//! Unified playback markers: one store, many producers.
//!
//! [`media_markers`](../../migrations/0024_markers.sql) is written by several
//! independent producers — embedded chapters (cheap, per-file; see
//! [`chapters_to_markers`]), audio-fingerprint matching (per-season; the
//! `audio` source), an optional silence/black refinement, and manual edits —
//! and read by exactly one consumer: the player (scrub-bar ticks + the
//! "Skip Intro/Credits" button). Producers share this schema, not a code
//! path: chapters fit the scanner's per-file essential pass, while audio
//! matching is a season-scoped pass that needs every episode first.
//!
//! Markers are per-media. A shared intro that only some episodes of a season
//! carry produces rows only on the episodes that actually contain it.

use super::AppState;
use super::media_info::RawChapter;
use crate::types::{Marker, MarkerKind, MarkersResponse};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;
use tokio::process::Command;
use tokio::sync::OnceCell;

/// Chapters/segments shorter than this are container artifacts (or too brief
/// to be worth a tick) and are dropped.
const MIN_MARKER_SECS: f64 = 5.0;

// ── Audio-fingerprint matching tunables ────────────────────────────────────

/// Chromaprint default sub-fingerprint period: at 11025 Hz with a 4096-sample
/// frame stepped by 1/3 there are ~8.06 items/sec. Empirically confirmed
/// (fpcalc 1.6.0) constant across file lengths, with a ~2–3 s tail the
/// fingerprint never reaches. Index i maps to media time `i * FP_ITEM_SECS`.
const FP_ITEM_SECS: f64 = 0.12383;
/// Bump to invalidate every cached fingerprint (e.g. if the fpcalc args or
/// `FP_ITEM_SECS` assumption change). Stored per row in `media_fingerprints`;
/// the scanner reads it to decide whether a season's fingerprints are current.
pub const FP_ALGO_VERSION: i64 = 1;
/// Cap the analysed span. Covers feature-length runtimes; bounds fpcalc time
/// and cache size (~8 KB per fingerprinted minute).
const FP_MAX_LENGTH_SECS: u32 = 3600;
/// Two sub-fingerprints "match" if at most this many of their 32 bits differ —
/// tolerant of re-encode noise between episodes while staying specific.
const HAMMING_BITS: u32 = 6;
/// Minimum shared-segment length worth a marker (~15 s) — shorter shared runs
/// (stings, logos) are dropped.
const MIN_SEGMENT_SECS: f64 = 15.0;
/// Bridge this many consecutive mismatches inside an otherwise-aligned run
/// (~1 s) so a brief divergence doesn't split one intro into two.
const MAX_GAP_ITEMS: usize = 8;
/// Merge per-episode runs separated by less than this (~5 s) into one segment.
const MERGE_GAP_ITEMS: usize = 40;
/// Cap indexed positions per distinct sub-fingerprint value so a constant
/// (silence) value can't blow up the offset histogram.
const MAX_VALUE_POSITIONS: usize = 8;
/// Candidate alignment offsets to extend per episode pair (one per recurring
/// segment: recap, intro, credits, …).
const TOP_OFFSETS: usize = 6;
/// An offset needs at least this many anchor hits to be a candidate. The
/// contiguous-run + MIN_SEGMENT length check downstream rejects noise, so this
/// is just a cheap pre-filter.
const MIN_ANCHOR_HITS: u32 = 10;
/// All-pairs correlation is O(N²); cap season size to avoid pathological work
/// on mis-grouped specials. Logged when it bites.
pub const MAX_SEASON_EPISODES: usize = 60;

fn min_segment_items() -> usize {
    (MIN_SEGMENT_SECS / FP_ITEM_SECS) as usize
}

fn kind_from_str(s: &str) -> Option<MarkerKind> {
    Some(match s {
        "intro" => MarkerKind::Intro,
        "recap" => MarkerKind::Recap,
        "outro" => MarkerKind::Outro,
        "credits" => MarkerKind::Credits,
        "chapter" => MarkerKind::Chapter,
        _ => return None,
    })
}

/// All markers for a media, ordered by start time. An unknown `kind` string
/// (e.g. left by a since-removed producer) is skipped rather than failing the
/// read — same defensive stance as `media_info::load` on bad `probe_json`.
pub async fn load(pool: &SqlitePool, media_id: &str) -> anyhow::Result<MarkersResponse> {
    let rows: Vec<(String, f64, f64, Option<String>, String, f64)> = sqlx::query_as(
        "SELECT kind, start_secs, end_secs, title, source, confidence
         FROM media_markers WHERE media_id = ? ORDER BY start_secs",
    )
    .bind(media_id)
    .fetch_all(pool)
    .await?;
    let markers = rows
        .into_iter()
        .filter_map(|(kind, start_secs, end_secs, title, source, confidence)| {
            Some(Marker {
                kind: kind_from_str(&kind)?,
                start_secs,
                end_secs,
                title,
                source,
                confidence,
            })
        })
        .collect();
    Ok(MarkersResponse { markers })
}

/// Markers validated against the live file. Reuses the probe's validate-on-read
/// ([`super::media_info::load_fresh`]): when the file's `(mtime, size)` differs
/// from the signature it was last derived against, that fires the single-file
/// refresh (`run_essential`), which re-derives `probe_json` *and* the chapter
/// markers together under the shared refresh lock. We don't need the probe
/// result here — only the side effect of refreshing stale rows — then we read
/// markers back. Audio-detected markers are season-scoped and are *not*
/// refreshed here (they're re-derived by the scanner's audio-match phase).
pub async fn load_fresh(state: &AppState, media_id: &str) -> anyhow::Result<MarkersResponse> {
    let _ = super::media_info::load_fresh(state, media_id).await?;
    load(&state.pool, media_id).await
}

/// Replace all markers for `media_id` that came from a single `source`,
/// atomically. Rows from other sources (and `manual` edits) are left
/// untouched, so a chapter re-derive never wipes audio-detected markers and a
/// season re-analysis never wipes chapter ticks.
pub async fn store_markers(
    pool: &SqlitePool,
    media_id: &str,
    source: &str,
    markers: &[Marker],
) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM media_markers WHERE media_id = ? AND source = ?")
        .bind(media_id)
        .bind(source)
        .execute(&mut *tx)
        .await?;
    for m in markers {
        sqlx::query(
            "INSERT OR REPLACE INTO media_markers
                (media_id, kind, start_secs, end_secs, title, source, confidence)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(media_id)
        .bind(m.kind.as_str())
        .bind(m.start_secs)
        .bind(m.end_secs)
        .bind(&m.title)
        .bind(source)
        .bind(m.confidence)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Turn embedded chapters into markers. A titled chapter is classified by
/// keyword (high confidence); an *untitled* or generically-named chapter
/// ("Chapter 3") becomes a navigational `Chapter` tick and is deliberately
/// **not** position-guessed into a skippable kind — guessing would put a
/// bogus "Skip Intro" over a movie's first real chapter. Whole-file and
/// sub-`MIN_MARKER_SECS` chapters are dropped as container artifacts.
pub fn chapters_to_markers(chapters: &[RawChapter], duration: f64) -> Vec<Marker> {
    chapters
        .iter()
        .filter_map(|c| {
            let len = c.end - c.start;
            if len < MIN_MARKER_SECS {
                return None;
            }
            // A single chapter spanning ~the whole file is an artifact, not a
            // real segment boundary.
            if duration > 0.0 && len >= duration * 0.95 {
                return None;
            }
            let (kind, confidence) = classify_chapter_title(c.title.as_deref());
            Some(Marker {
                kind,
                start_secs: c.start,
                end_secs: c.end,
                title: c.title.clone(),
                source: "chapter".to_string(),
                confidence,
            })
        })
        .collect()
}

/// Keyword classification for a chapter title. No keyword → a navigational
/// `Chapter` (full confidence as a tick, but not skippable). Short ambiguous
/// aliases ("op"/"ed") are intentionally omitted — as bare substrings they
/// match "operation"/"edited" and would misfire.
fn classify_chapter_title(title: Option<&str>) -> (MarkerKind, f64) {
    if let Some(t) = title {
        let lc = t.to_ascii_lowercase();
        let has = |kw: &str| lc.contains(kw);
        if has("recap") || has("previously") {
            return (MarkerKind::Recap, 1.0);
        }
        if has("intro") || has("opening") || has("title sequence") || has("main titles") {
            return (MarkerKind::Intro, 1.0);
        }
        if has("credit") || has("ending") || has("outro") {
            return (MarkerKind::Credits, 1.0);
        }
    }
    (MarkerKind::Chapter, 1.0)
}

/// Classify a `(start, end)` segment by where it sits in the runtime: near the
/// head → `Intro`, near the tail → `Credits`, otherwise a recurring `Chapter`
/// (e.g. a mid-episode character-info interstitial). Near-start = first
/// `max(20%, 5 min)`; near-end = last `max(15%, 4 min)`. Returns `Chapter`
/// when `duration` is unknown. (Recap-vs-intro splitting of two adjacent
/// head segments is left for a future refinement.)
fn classify_by_position(start: f64, end: f64, duration: f64) -> MarkerKind {
    if duration <= 0.0 {
        return MarkerKind::Chapter;
    }
    let near_start_zone = (duration * 0.20).max(300.0);
    let near_end_zone = (duration * 0.15).max(240.0);
    if start <= near_start_zone {
        MarkerKind::Intro
    } else if end >= duration - near_end_zone {
        MarkerKind::Credits
    } else {
        MarkerKind::Chapter
    }
}

// ── Audio-fingerprint matching ──────────────────────────────────────────────

/// Whether audio-fingerprint intro/outro detection is available this process.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FpcalcStatus {
    Available,
    Missing,
}

static FPCALC: OnceCell<FpcalcStatus> = OnceCell::const_new();

/// Process-wide, memoized probe for `fpcalc` (Chromaprint). Disabled — with a
/// one-time warning — when the binary isn't on PATH or `BINKFLIX_AUDIO_MATCH=0`.
/// Mirrors the hwenc startup-probe: callers (startup logging + the scanner's
/// season pass) all read the same cached verdict. When `Missing`, embedded
/// chapters still produce markers and the player still works.
pub async fn fpcalc_status() -> FpcalcStatus {
    *FPCALC.get_or_init(detect_fpcalc).await
}

async fn detect_fpcalc() -> FpcalcStatus {
    if std::env::var("BINKFLIX_AUDIO_MATCH").ok().as_deref() == Some("0") {
        tracing::info!("audio-match: disabled (BINKFLIX_AUDIO_MATCH=0)");
        return FpcalcStatus::Missing;
    }
    match Command::new("fpcalc").arg("-version").output().await {
        Ok(o) if o.status.success() => {
            tracing::info!("audio-match: fpcalc detected; season intro/outro detection enabled");
            FpcalcStatus::Available
        }
        _ => {
            tracing::warn!(
                "audio-match: `fpcalc` not found on PATH — intro/outro audio detection DISABLED \
                 (embedded chapters still apply). Install libchromaprint-tools to enable."
            );
            FpcalcStatus::Missing
        }
    }
}

fn decode_fp(bytes: &[u8]) -> Vec<u32> {
    bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn encode_fp(fp: &[u32]) -> Vec<u8> {
    let mut v = Vec::with_capacity(fp.len() * 4);
    for x in fp {
        v.extend_from_slice(&x.to_le_bytes());
    }
    v
}

/// Raw whole-file Chromaprint fingerprint via `fpcalc`. Values are unsigned
/// 32-bit, so parsed as `u32` (some exceed `i32::MAX`).
async fn fingerprint(video: &Path) -> anyhow::Result<Vec<u32>> {
    let out = Command::new("fpcalc")
        .args(["-raw", "-json", "-length"])
        .arg(FP_MAX_LENGTH_SECS.to_string())
        .arg(video)
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!("fpcalc failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    #[derive(serde::Deserialize)]
    struct Out {
        #[serde(default)]
        fingerprint: Vec<u32>,
    }
    let parsed: Out = serde_json::from_slice(&out.stdout)?;
    Ok(parsed.fingerprint)
}

/// Fingerprint for `media_id`, reusing the cached `media_fingerprints` row when
/// its `(mtime, size, algo)` still match the live file — so a season re-run
/// only pays fpcalc on the file(s) that actually changed or are new.
pub async fn ensure_fingerprint(
    pool: &SqlitePool,
    media_id: &str,
    video: &Path,
    sig: (i64, i64),
) -> anyhow::Result<Vec<u32>> {
    let row: Option<(Option<i64>, Option<i64>, i64, Vec<u8>)> = sqlx::query_as(
        "SELECT content_mtime, content_size, fp_algo_version, raw
         FROM media_fingerprints WHERE media_id = ?",
    )
    .bind(media_id)
    .fetch_optional(pool)
    .await?;
    if let Some((Some(m), Some(s), ver, raw)) = row {
        if (m, s) == sig && ver == FP_ALGO_VERSION {
            return Ok(decode_fp(&raw));
        }
    }
    let fp = fingerprint(video).await?;
    sqlx::query(
        "INSERT OR REPLACE INTO media_fingerprints
            (media_id, content_mtime, content_size, fp_algo_version, raw)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(media_id)
    .bind(sig.0)
    .bind(sig.1)
    .bind(FP_ALGO_VERSION)
    .bind(encode_fp(&fp))
    .execute(pool)
    .await?;
    Ok(fp)
}

/// One episode's fingerprint + runtime, fed to [`analyze_season`].
pub struct SeasonEpisode {
    pub media_id: String,
    pub duration: f64,
    pub fp: Vec<u32>,
}

/// Valid `i` range where both `a[i]` and `b[i + d]` exist.
fn overlap_range(la: usize, lb: usize, d: isize) -> (usize, usize) {
    let i_lo = if d < 0 { (-d) as usize } else { 0 };
    let i_hi = (lb as isize - d).max(0) as usize; // i < lb - d
    (i_lo.min(la), i_hi.min(la))
}

/// Find shared sub-fingerprint runs between two episodes. Returns each run as
/// `(range in a, range in b)` in index space. Strategy: index `b`'s values,
/// histogram candidate alignment offsets from anchor hits, then for the top
/// offsets walk the overlap extending Hamming-close runs (bridging brief gaps).
fn find_pair_segments(a: &[u32], b: &[u32]) -> Vec<(Range<usize>, Range<usize>)> {
    let min_items = min_segment_items();
    if a.len() < min_items || b.len() < min_items {
        return Vec::new();
    }

    let mut index: HashMap<u32, Vec<usize>> = HashMap::new();
    for (j, &v) in b.iter().enumerate() {
        let e = index.entry(v).or_default();
        if e.len() < MAX_VALUE_POSITIONS {
            e.push(j);
        }
    }

    let mut hist: HashMap<isize, u32> = HashMap::new();
    for (i, &v) in a.iter().enumerate() {
        if let Some(list) = index.get(&v) {
            for &j in list {
                *hist.entry(j as isize - i as isize).or_default() += 1;
            }
        }
    }

    let mut offsets: Vec<(isize, u32)> =
        hist.into_iter().filter(|&(_, c)| c >= MIN_ANCHOR_HITS).collect();
    offsets.sort_by(|x, y| y.1.cmp(&x.1));
    offsets.truncate(TOP_OFFSETS);

    let mut result = Vec::new();
    for (d, _) in offsets {
        let (i_lo, i_hi) = overlap_range(a.len(), b.len(), d);
        let mut run_start: Option<usize> = None;
        let mut last_match = i_lo;
        let mut gap = 0usize;
        let close = |run_start: &mut Option<usize>, last_match: usize, out: &mut Vec<_>| {
            if let Some(s) = run_start.take() {
                let e = last_match + 1;
                if e - s >= min_items {
                    out.push((
                        s..e,
                        (s as isize + d) as usize..(e as isize + d) as usize,
                    ));
                }
            }
        };
        for i in i_lo..i_hi {
            let j = (i as isize + d) as usize;
            if (a[i] ^ b[j]).count_ones() <= HAMMING_BITS {
                if run_start.is_none() {
                    run_start = Some(i);
                }
                last_match = i;
                gap = 0;
            } else if run_start.is_some() {
                gap += 1;
                if gap > MAX_GAP_ITEMS {
                    close(&mut run_start, last_match, &mut result);
                    gap = 0;
                }
            }
        }
        close(&mut run_start, last_match, &mut result);
    }
    result
}

/// Merge a single episode's raw runs (each tagged with the partner episode it
/// came from) into deduped segments with the set of corroborating partners.
fn merge_runs(runs: &mut [(usize, usize, usize)]) -> Vec<(usize, usize, Vec<usize>)> {
    runs.sort_by_key(|r| r.0);
    let mut out: Vec<(usize, usize, Vec<usize>)> = Vec::new();
    for &(s, e, p) in runs.iter() {
        if let Some(last) = out.last_mut() {
            if s <= last.1 + MERGE_GAP_ITEMS {
                last.1 = last.1.max(e);
                if !last.2.contains(&p) {
                    last.2.push(p);
                }
                continue;
            }
        }
        out.push((s, e, vec![p]));
    }
    out
}

/// Correlate every episode pair in a season and return, per episode, the
/// markers for segments it shares with at least one other episode (quorum of
/// 2 — a single matching pair already means two episodes carry it). Confidence
/// scales with how many other episodes corroborate the segment.
pub fn analyze_season(eps: &[SeasonEpisode]) -> Vec<(String, Vec<Marker>)> {
    let n = eps.len();
    let mut raw: Vec<Vec<(usize, usize, usize)>> = vec![Vec::new(); n];
    for i in 0..n {
        for j in (i + 1)..n {
            for (ra, rb) in find_pair_segments(&eps[i].fp, &eps[j].fp) {
                raw[i].push((ra.start, ra.end, j));
                raw[j].push((rb.start, rb.end, i));
            }
        }
    }

    let denom = (n.saturating_sub(1)).max(1) as f64;
    let mut out = Vec::with_capacity(n);
    for (i, ep) in eps.iter().enumerate() {
        let mut markers = Vec::new();
        for (s_idx, e_idx, partners) in merge_runs(&mut raw[i]) {
            let start = s_idx as f64 * FP_ITEM_SECS;
            let end = e_idx as f64 * FP_ITEM_SECS;
            if end - start < MIN_SEGMENT_SECS {
                continue;
            }
            let kind = classify_by_position(start, end, ep.duration);
            let title = match kind {
                MarkerKind::Chapter => Some("Recurring segment".to_string()),
                _ => None,
            };
            markers.push(Marker {
                kind,
                start_secs: start,
                end_secs: end,
                title,
                source: "audio".to_string(),
                confidence: (partners.len() as f64 / denom).clamp(0.0, 1.0),
            });
        }
        out.push((ep.media_id.clone(), markers));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random sub-fingerprint stream (a simple LCG) so
    /// tests don't need real audio or `Math.random`.
    fn lcg_stream(seed: u32, n: usize) -> Vec<u32> {
        let mut x = seed;
        (0..n)
            .map(|_| {
                x = x.wrapping_mul(1664525).wrapping_add(1013904223);
                x
            })
            .collect()
    }

    #[test]
    fn detects_shared_intro_and_ignores_unique_episode() {
        // ~27 s of identical "intro", then per-episode-distinct bodies.
        let intro = lcg_stream(42, 220);
        let with_intro = |body_seed: u32| {
            let mut fp = intro.clone();
            fp.extend(lcg_stream(body_seed, 2000));
            fp
        };
        let eps = vec![
            SeasonEpisode { media_id: "a".into(), duration: 1500.0, fp: with_intro(7) },
            SeasonEpisode { media_id: "b".into(), duration: 1500.0, fp: with_intro(99) },
            // No shared content with anyone.
            SeasonEpisode { media_id: "c".into(), duration: 1500.0, fp: lcg_stream(500, 2200) },
        ];
        let by_id: HashMap<String, Vec<Marker>> = analyze_season(&eps).into_iter().collect();

        for id in ["a", "b"] {
            let ms = &by_id[id];
            assert_eq!(ms.len(), 1, "episode {id} should get exactly the shared intro");
            assert_eq!(ms[0].kind, MarkerKind::Intro, "near-start shared segment → Intro");
            assert!(ms[0].start_secs < 2.0, "intro starts near 0, got {}", ms[0].start_secs);
            assert!(ms[0].end_secs > MIN_SEGMENT_SECS, "intro ≥ min length, got {}", ms[0].end_secs);
        }
        assert!(by_id["c"].is_empty(), "a unique episode must get no shared markers");
    }

    #[test]
    fn chapter_classifier_keywords_positions_and_artifacts() {
        let dur = 1800.0;
        let chapters = [
            RawChapter { start: 0.0, end: 80.0, title: Some("Opening Credits".into()) },
            RawChapter { start: 80.0, end: 1500.0, title: Some("Chapter 2".into()) },
            RawChapter { start: 1700.0, end: 1800.0, title: Some("End Credits".into()) },
            RawChapter { start: 200.0, end: 203.0, title: Some("blip".into()) },
        ];
        let ms = chapters_to_markers(&chapters, dur);
        assert_eq!(ms.len(), 3, "the <5 s 'blip' chapter is dropped");
        assert_eq!(ms[0].kind, MarkerKind::Intro, "'Opening …' keyword → Intro");
        // Untitled/generic chapters are navigational ticks, never position-guessed
        // into a skippable kind.
        assert_eq!(ms[1].kind, MarkerKind::Chapter, "'Chapter 2' stays a generic tick");
        assert_eq!(ms[2].kind, MarkerKind::Credits, "'End Credits' → Credits");
    }

    #[test]
    fn whole_file_chapter_is_dropped_as_artifact() {
        let ms = chapters_to_markers(
            &[RawChapter { start: 0.0, end: 1799.0, title: None }],
            1800.0,
        );
        assert!(ms.is_empty(), "a single whole-file chapter is a container artifact");
    }

    /// Real end-to-end through the actual `fpcalc` binary: two synthetic
    /// "episodes" that share a byte-identical 25 s intro (then diverge) must
    /// both yield an Intro marker near 0. Ignored by default — needs ffmpeg +
    /// fpcalc and writes temp files. Run with:
    ///   cargo test --features server -- --ignored real_fpcalc
    #[tokio::test]
    #[ignore]
    async fn real_fpcalc_detects_shared_intro() {
        use std::process::Command as Sync;
        let dir = std::env::temp_dir().join("binkflix_fp_e2e");
        let _ = std::fs::create_dir_all(&dir);
        let p = |n: &str| dir.join(n).to_string_lossy().into_owned();
        let ff = |args: &[&str]| {
            let ok = Sync::new("ffmpeg")
                .arg("-y")
                .args(args)
                .status()
                .expect("ffmpeg must be installed for this test")
                .success();
            assert!(ok, "ffmpeg failed for args: {args:?}");
        };
        // Fixed intro reused verbatim by both episodes (byte-identical audio →
        // identical fingerprints). Distinct, seeded bodies for the rest.
        ff(&["-f", "lavfi", "-i", "anoisesrc=d=25:c=pink:a=0.5", "-ar", "22050", "-ac", "1", &p("intro.wav")]);
        ff(&["-f", "lavfi", "-i", "anoisesrc=d=20:c=pink:a=0.5:seed=11", "-ar", "22050", "-ac", "1", &p("b1.wav")]);
        ff(&["-f", "lavfi", "-i", "anoisesrc=d=20:c=pink:a=0.5:seed=22", "-ar", "22050", "-ac", "1", &p("b2.wav")]);
        ff(&["-i", &p("intro.wav"), "-i", &p("b1.wav"), "-filter_complex", "[0:a][1:a]concat=n=2:v=0:a=1[a]", "-map", "[a]", &p("ep1.wav")]);
        ff(&["-i", &p("intro.wav"), "-i", &p("b2.wav"), "-filter_complex", "[0:a][1:a]concat=n=2:v=0:a=1[a]", "-map", "[a]", &p("ep2.wav")]);

        let fp1 = fingerprint(Path::new(&p("ep1.wav"))).await.expect("fpcalc ep1");
        let fp2 = fingerprint(Path::new(&p("ep2.wav"))).await.expect("fpcalc ep2");
        assert!(!fp1.is_empty() && !fp2.is_empty(), "fpcalc produced fingerprints");

        let eps = vec![
            SeasonEpisode { media_id: "ep1".into(), duration: 45.0, fp: fp1 },
            SeasonEpisode { media_id: "ep2".into(), duration: 45.0, fp: fp2 },
        ];
        let by_id: HashMap<String, Vec<Marker>> = analyze_season(&eps).into_iter().collect();
        for id in ["ep1", "ep2"] {
            let intro = by_id[id].iter().find(|m| m.kind == MarkerKind::Intro);
            let m = intro.unwrap_or_else(|| panic!("{id}: expected a detected Intro, got {:?}", by_id[id]));
            assert!(m.start_secs < 3.0, "{id}: intro starts near 0, got {}", m.start_secs);
            assert!(m.end_secs > MIN_SEGMENT_SECS, "{id}: intro spans the shared region, got {}", m.end_secs);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
