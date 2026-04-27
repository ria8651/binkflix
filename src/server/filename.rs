//! Heuristic parsing of media filenames when no NFO sidecar is present.
//!
//! Inspired by Jellyfin / Sonarr / `guessit`. Not exhaustive — we cover the
//! common shapes (release-group dumps, tidy "Title (Year).mkv", `SxxEyy`,
//! `1x02`, daily dates) and leave the rest to whatever `infer_season_episode`
//! can salvage.

const YEAR_MIN: i64 = 1900;
const YEAR_MAX: i64 = 2099;

/// Lowercase tokens that mark the boundary between the title and release
/// metadata. The parser cuts the stem at the first one it sees.
const JUNK_TOKENS: &[&str] = &[
    // resolution
    "2160p", "1080p", "720p", "480p", "360p", "4k", "uhd",
    // source
    "bluray", "blu-ray", "bdrip", "brrip", "bdremux", "remux",
    "webrip", "web-dl", "webdl", "web", "hdtv", "pdtv", "dvdrip", "dvd",
    "hdrip", "hddvd", "vhsrip", "tvrip", "cam", "ts", "telesync",
    // video codec
    "x264", "x265", "h264", "h265", "h.264", "h.265", "hevc", "avc",
    "xvid", "divx", "vp9", "av1",
    // audio codec
    "aac", "ac3", "eac3", "ddp", "dd5", "dd2", "dts", "dts-hd", "truehd",
    "atmos", "flac", "mp3", "opus",
    // bit depth / hdr
    "10bit", "8bit", "hdr", "hdr10", "hdr10+", "dolby", "dovi", "dv",
    // edition / status
    "proper", "repack", "extended", "unrated", "remastered", "imax",
    "internal", "limited", "directors", "director's", "uncut", "theatrical",
    // origin
    "multi", "dual", "subbed", "dubbed",
];

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ParsedMovie {
    pub title: String,
    pub year: Option<i64>,
}

/// Find a `SxxEyy` (or `S01.E02`, `S01 E02`, `S01-E02`, `S01_E02`) tag.
/// Returns `(season, episode, start_byte, end_byte)` for the matched span
/// (so callers can strip it from the title).
fn find_sxxeyy(s: &str) -> Option<(i64, i64, usize, usize)> {
    let lower = s.to_ascii_lowercase();
    let b = lower.as_bytes();
    let mut i = 0;
    while i < b.len() {
        // Need 's' preceded by start-of-string or a non-alnum boundary.
        if b[i] == b's'
            && (i == 0 || !b[i - 1].is_ascii_alphanumeric())
            && i + 1 < b.len()
            && b[i + 1].is_ascii_digit()
        {
            let s_start = i + 1;
            let mut s_end = s_start;
            while s_end < b.len() && b[s_end].is_ascii_digit() {
                s_end += 1;
            }
            // Allow one separator between season and 'e'.
            let mut j = s_end;
            if j < b.len() && matches!(b[j], b'.' | b'_' | b' ' | b'-') {
                j += 1;
            }
            if j < b.len()
                && b[j] == b'e'
                && j + 1 < b.len()
                && b[j + 1].is_ascii_digit()
            {
                let e_start = j + 1;
                let mut e_end = e_start;
                while e_end < b.len() && b[e_end].is_ascii_digit() {
                    e_end += 1;
                }
                if let (Ok(season), Ok(episode)) = (
                    lower[s_start..s_end].parse::<i64>(),
                    lower[e_start..e_end].parse::<i64>(),
                ) {
                    // Consume trailing extra-episode tags so we strip `S01E01E02` / `S01E01-E02` cleanly.
                    let mut tail = e_end;
                    loop {
                        let mut k = tail;
                        if k < b.len() && (b[k] == b'-' || b[k] == b'e') {
                            if b[k] == b'-' {
                                k += 1;
                            }
                            if k < b.len() && b[k] == b'e' {
                                k += 1;
                                let digits_start = k;
                                while k < b.len() && b[k].is_ascii_digit() {
                                    k += 1;
                                }
                                if k > digits_start {
                                    tail = k;
                                    continue;
                                }
                            }
                        }
                        break;
                    }
                    return Some((season, episode, i, tail));
                }
            }
        }
        i += 1;
    }
    None
}

/// `1x02`, `01x02`. Bounded so resolutions like `1920x1080` don't match.
fn find_nxmm(s: &str) -> Option<(i64, i64, usize, usize)> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() {
            let n_start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if i < b.len() && (b[i] == b'x' || b[i] == b'X') && i + 1 < b.len() && b[i + 1].is_ascii_digit() {
                let n_end = i;
                let m_start = i + 1;
                let mut m_end = m_start;
                while m_end < b.len() && b[m_end].is_ascii_digit() {
                    m_end += 1;
                }
                let prev_ok = n_start == 0 || !b[n_start - 1].is_ascii_alphanumeric();
                let next_ok = m_end == b.len() || !b[m_end].is_ascii_alphanumeric();
                if prev_ok && next_ok {
                    if let (Ok(s_num), Ok(e_num)) = (
                        s[n_start..n_end].parse::<i64>(),
                        s[m_start..m_end].parse::<i64>(),
                    ) {
                        // Reject obvious resolutions: both dims big, e.g. 1920x1080, 1280x720.
                        let looks_like_resolution =
                            (s_num >= 480 && e_num >= 240) || s_num > 100 || e_num >= 1000;
                        if !looks_like_resolution && s_num > 0 && e_num > 0 {
                            return Some((s_num, e_num, n_start, m_end));
                        }
                    }
                }
            }
        } else {
            i += 1;
        }
    }
    None
}

/// `YYYY-MM-DD` or `YYYY.MM.DD`. Returns season=YYYY, episode=MMDD (Sonarr).
fn find_date(s: &str) -> Option<(i64, i64, usize, usize)> {
    let b = s.as_bytes();
    if b.len() < 10 {
        return None;
    }
    let mut i = 0;
    while i + 10 <= b.len() {
        let prev_ok = i == 0 || !b[i - 1].is_ascii_alphanumeric();
        let end = i + 10;
        let next_ok = end == b.len() || !b[end].is_ascii_alphanumeric();
        if prev_ok
            && next_ok
            && b[i..i + 4].iter().all(|c| c.is_ascii_digit())
            && (b[i + 4] == b'-' || b[i + 4] == b'.')
            && b[i + 5].is_ascii_digit()
            && b[i + 6].is_ascii_digit()
            && b[i + 4] == b[i + 7]
            && b[i + 8].is_ascii_digit()
            && b[i + 9].is_ascii_digit()
        {
            if let (Ok(y), Ok(mo), Ok(d)) = (
                s[i..i + 4].parse::<i64>(),
                s[i + 5..i + 7].parse::<i64>(),
                s[i + 8..i + 10].parse::<i64>(),
            ) {
                if (YEAR_MIN..=YEAR_MAX).contains(&y) && (1..=12).contains(&mo) && (1..=31).contains(&d) {
                    return Some((y, mo * 100 + d, i, i + 10));
                }
            }
        }
        i += 1;
    }
    None
}

/// Try every supported episode pattern. Returns `(season, episode)` only —
/// callers that need the matched span use [`find_episode_span`].
pub fn parse_episode(stem: &str) -> Option<(i64, i64)> {
    find_episode_span(stem).map(|(s, e, _, _)| (s, e))
}

/// As [`parse_episode`] but also returns the matched byte span in `stem`.
pub fn find_episode_span(stem: &str) -> Option<(i64, i64, usize, usize)> {
    if let Some(m) = find_sxxeyy(stem) {
        return Some(m);
    }
    if let Some(m) = find_nxmm(stem) {
        return Some(m);
    }
    if let Some(m) = find_date(stem) {
        return Some(m);
    }
    None
}

/// Replace dots/underscores with spaces — but only when the stem is clearly
/// "release style" (more dots than spaces). Preserves real titles like
/// `The Matrix.mkv` which have a single dot for the extension only (already
/// stripped before we see them here, but the heuristic guards against
/// punctuation in genuine titles).
fn normalize_separators(s: &str) -> String {
    let dots = s.bytes().filter(|c| *c == b'.').count();
    let unders = s.bytes().filter(|c| *c == b'_').count();
    let spaces = s.bytes().filter(|c| *c == b' ').count();
    if dots + unders > spaces {
        s.replace(['.', '_'], " ")
    } else {
        s.replace('_', " ")
    }
}

/// Find the byte index of the first junk token (case-insensitive, on word
/// boundaries). Returns `s.len()` if none.
fn first_junk_index(s: &str) -> usize {
    let lower = s.to_ascii_lowercase();
    let b = lower.as_bytes();
    let mut best = s.len();
    for token in JUNK_TOKENS {
        let tb = token.as_bytes();
        let mut start = 0;
        while let Some(pos) = lower[start..].find(token) {
            let abs = start + pos;
            let prev_ok = abs == 0 || !b[abs - 1].is_ascii_alphanumeric();
            let after = abs + tb.len();
            let next_ok = after == b.len() || !b[after].is_ascii_alphanumeric();
            if prev_ok && next_ok && abs < best {
                best = abs;
                break;
            }
            start = abs + 1;
        }
    }
    best
}

/// Find a 4-digit year in `(YYYY)`, `[YYYY]`, or surrounded by separators.
/// Returns `(year, start, end)` for the **last** valid year that appears
/// before any junk token — so `2001 A Space Odyssey 1968` picks 1968 but
/// `Avatar 2009 1080p` picks 2009 (the `1080p` is junk, not a year).
fn find_year(s: &str) -> Option<(i64, usize, usize)> {
    let b = s.as_bytes();
    let cutoff = first_junk_index(s);
    let mut best: Option<(i64, usize, usize)> = None;
    let mut i = 0;
    while i + 4 <= cutoff {
        if b[i].is_ascii_digit() {
            // Only consider exactly-4-digit runs.
            let start = i;
            let mut end = start;
            while end < b.len() && b[end].is_ascii_digit() {
                end += 1;
            }
            if end - start == 4 {
                let prev_ok = start == 0
                    || matches!(b[start - 1], b'(' | b'[' | b'{' | b' ' | b'.' | b'-' | b'_');
                let next_ok = end == b.len()
                    || matches!(b[end], b')' | b']' | b'}' | b' ' | b'.' | b'-' | b'_');
                if prev_ok && next_ok {
                    if let Ok(y) = s[start..end].parse::<i64>() {
                        if (YEAR_MIN..=YEAR_MAX).contains(&y) {
                            // Take the rightmost; "Title 1968" beats "2001 Title 1968"
                            // when both look like years, but we only consider years
                            // that come at-or-after a space (i.e. not the very first
                            // token), so `2001 A Space Odyssey 1968` → 1968 wins.
                            let is_first_token = s[..start].trim().is_empty();
                            if !is_first_token {
                                best = Some((y, start, end));
                            } else if best.is_none() {
                                // Lone year-as-title is fine.
                                best = Some((y, start, end));
                            }
                        }
                    }
                }
            }
            i = end;
        } else {
            i += 1;
        }
    }
    best
}

/// Strip surrounding/embedded balanced bracket runs `[...]`, `(...)`, `{...}`.
fn strip_brackets(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0u32;
    for c in s.chars() {
        match c {
            '[' | '(' | '{' => depth += 1,
            ']' | ')' | '}' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = true;
    for c in s.chars() {
        if c.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(c);
            last_space = false;
        }
    }
    out.trim().to_string()
}

/// Trailing release group: `-RARBG`, `-YIFY`, `-GROUP`. We strip if the
/// remainder after the last `-` looks like a tag (no spaces, ≤ 12 chars,
/// alphanumerics or `.`).
fn strip_release_group(s: &str) -> String {
    let trimmed = s.trim();
    if let Some(idx) = trimmed.rfind('-') {
        let tag = &trimmed[idx + 1..];
        let looks_like_tag = !tag.is_empty()
            && tag.len() <= 12
            && tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '.');
        if looks_like_tag {
            return trimmed[..idx].to_string();
        }
    }
    trimmed.to_string()
}

/// Title-case each whitespace-separated word — only applied when the input
/// is entirely lowercase, to avoid wrecking already-cased titles like `iPhone`.
fn maybe_titlecase(s: &str) -> String {
    if s.chars().any(|c| c.is_uppercase()) {
        return s.to_string();
    }
    s.split(' ')
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// General-purpose cleanup: dot/underscore → space, strip junk-tokens-onward,
/// strip brackets, strip release group, collapse whitespace, title-case if
/// all-lowercase. Year is **not** removed here — callers that want it gone
/// should use [`parse_movie`].
pub fn clean_title(s: &str) -> String {
    let normalized = normalize_separators(s);
    let cut = first_junk_index(&normalized);
    let head = &normalized[..cut];
    let head = strip_brackets(head);
    let head = strip_release_group(&head);
    let head = collapse_ws(&head);
    maybe_titlecase(&head)
}

/// Parse a movie filename stem into a clean title and (optional) year.
pub fn parse_movie(stem: &str) -> ParsedMovie {
    let normalized = normalize_separators(stem);
    let (year, head_end) = match find_year(&normalized) {
        Some((y, start, _end)) => (Some(y), start),
        None => (None, normalized.len()),
    };
    // Cut at year-start OR junk-start, whichever is earlier.
    let junk = first_junk_index(&normalized);
    let cut = head_end.min(junk);
    let head = &normalized[..cut];
    let head = strip_brackets(head);
    let head = strip_release_group(&head);
    let head = collapse_ws(&head);
    let head = maybe_titlecase(&head);
    ParsedMovie { title: head, year }
}

/// Episode title from a filename: strip the SxxEyy/1x02/date tag and
/// everything before it (release prefix is usually the show name, which
/// already lives on the show row), then run the normal cleanup. If nothing
/// meaningful is left, returns `Episode {n}`.
pub fn clean_episode_title(stem: &str, episode: i64) -> String {
    let after = match find_episode_span(stem) {
        Some((_, _, _start, end)) => &stem[end..],
        None => stem,
    };
    let cleaned = clean_title(after);
    // Strip a leading separator that's left behind after cutting the tag.
    let cleaned = cleaned.trim_start_matches(|c: char| !c.is_alphanumeric()).to_string();
    let cleaned = collapse_ws(&cleaned);
    if cleaned.is_empty() || cleaned.chars().all(|c| !c.is_alphabetic()) {
        format!("Episode {episode}")
    } else {
        cleaned
    }
}

/// Sort-friendly transform: drop bracketed prefixes like `[REPACK]`, lowercase,
/// strip a leading article ("the ", "a ", "an "), strip leading non-alphanumerics.
/// Used by `ORDER BY` so "The Matrix" sorts as "matrix" and "[REPACK] Avatar"
/// sorts as "avatar".
pub fn sort_title(s: &str) -> String {
    let stripped = strip_brackets(s);
    let lower = collapse_ws(&stripped).to_lowercase();
    let trimmed = lower.trim_start_matches(|c: char| !c.is_alphanumeric()).to_string();
    for prefix in ["the ", "a ", "an "] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return rest.trim().to_string();
        }
    }
    trimmed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sxxeyy_basic() {
        assert_eq!(parse_episode("Show.Name.S01E02.1080p.WEB-DL.x265-GROUP"), Some((1, 2)));
        assert_eq!(parse_episode("show s01e02"), Some((1, 2)));
        assert_eq!(parse_episode("Show S1E2"), Some((1, 2)));
        assert_eq!(parse_episode("Show.S10E15"), Some((10, 15)));
    }

    #[test]
    fn sxxeyy_with_separators() {
        assert_eq!(parse_episode("Show.Name.S01.E02.mkv"), Some((1, 2)));
        assert_eq!(parse_episode("Show Name S01 E02"), Some((1, 2)));
        assert_eq!(parse_episode("Show-S01-E02"), Some((1, 2)));
        assert_eq!(parse_episode("Show_S01_E02"), Some((1, 2)));
    }

    #[test]
    fn sxxeyy_multi_episode() {
        // First episode wins, span includes the full multi-ep tag.
        assert_eq!(parse_episode("Show.S01E01E02"), Some((1, 1)));
        assert_eq!(parse_episode("Show.S01E01-E03"), Some((1, 1)));
    }

    #[test]
    fn nxmm_basic() {
        assert_eq!(parse_episode("Show Name 1x02"), Some((1, 2)));
        assert_eq!(parse_episode("Show.Name.01x02.mkv"), Some((1, 2)));
    }

    #[test]
    fn nxmm_rejects_resolution() {
        assert_eq!(parse_episode("Movie.1920x1080.mkv"), None);
        assert_eq!(parse_episode("Movie.1280x720.mkv"), None);
    }

    #[test]
    fn date_based() {
        assert_eq!(parse_episode("Daily Show 2024-01-15"), Some((2024, 115)));
        assert_eq!(parse_episode("Daily.Show.2024.01.15.mkv"), Some((2024, 115)));
    }

    #[test]
    fn movie_release_dump() {
        let p = parse_movie("The.Matrix.1999.1080p.BluRay.x264-RARBG");
        assert_eq!(p.title, "The Matrix");
        assert_eq!(p.year, Some(1999));
    }

    #[test]
    fn movie_with_brackets() {
        let p = parse_movie("Avatar (2009) [1080p]");
        assert_eq!(p.title, "Avatar");
        assert_eq!(p.year, Some(2009));
    }

    #[test]
    fn movie_year_in_title() {
        let p = parse_movie("2001 A Space Odyssey (1968)");
        assert_eq!(p.title, "2001 A Space Odyssey");
        assert_eq!(p.year, Some(1968));
    }

    #[test]
    fn movie_no_year() {
        let p = parse_movie("Movie Name");
        assert_eq!(p.title, "Movie Name");
        assert_eq!(p.year, None);
    }

    #[test]
    fn movie_dotted_no_year() {
        let p = parse_movie("Some.Indie.Movie");
        assert_eq!(p.title, "Some Indie Movie");
        assert_eq!(p.year, None);
    }

    #[test]
    fn movie_titlecase_lowercase_input() {
        let p = parse_movie("the.matrix.1999.1080p");
        assert_eq!(p.title, "The Matrix");
        assert_eq!(p.year, Some(1999));
    }

    #[test]
    fn movie_preserves_existing_case() {
        let p = parse_movie("iPhone Story (2015)");
        assert_eq!(p.title, "iPhone Story");
    }

    #[test]
    fn episode_title_stripping() {
        assert_eq!(
            clean_episode_title("Show.Name.S01E02.The.Pilot.1080p.WEB-DL.x265-GROUP", 2),
            "The Pilot"
        );
    }

    #[test]
    fn episode_title_fallback_when_empty() {
        assert_eq!(
            clean_episode_title("Show.Name.S01E02.1080p.WEB-DL", 2),
            "Episode 2"
        );
    }

    #[test]
    fn episode_title_no_tag() {
        // No SxxEyy → whole thing is the title.
        assert_eq!(clean_episode_title("Just A Title", 1), "Just A Title");
    }

    #[test]
    fn show_title_clean() {
        assert_eq!(clean_title("Breaking.Bad.(2008)"), "Breaking Bad");
        assert_eq!(clean_title("breaking_bad"), "Breaking Bad");
    }

    #[test]
    fn sort_title_articles() {
        assert_eq!(sort_title("The Matrix"), "matrix");
        assert_eq!(sort_title("A Bug's Life"), "bug's life");
        assert_eq!(sort_title("An American Tail"), "american tail");
        assert_eq!(sort_title("Anaconda"), "anaconda");
    }

    #[test]
    fn sort_title_brackets() {
        assert_eq!(sort_title("[REPACK] Avatar"), "avatar");
        assert_eq!(sort_title("(2009) The Matrix"), "matrix");
    }
}
