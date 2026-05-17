//! One-shot importer: copies Jellyfin watch history into binkflix's
//! `watch_progress` table. Invoked as `binkflix import-jellyfin <path>`.

use anyhow::{bail, Context, Result};
use chrono::{NaiveDateTime, Utc};
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Select};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{FromRow, SqlitePool};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const JF_EPISODE: &str = "MediaBrowser.Controller.Entities.TV.Episode";
const JF_MOVIE: &str = "MediaBrowser.Controller.Entities.Movies.Movie";
const COMPLETION_RATIO: f64 = 0.9;

#[derive(FromRow)]
struct JfUser {
    id: String,
    username: String,
}

#[derive(FromRow)]
struct JfWatch {
    kind: String,
    name: Option<String>,
    series_name: Option<String>,
    season: Option<i64>,
    episode: Option<i64>,
    year: Option<i64>,
    runtime_ticks: Option<i64>,
    position_ticks: i64,
    played: i64,
    last_played: Option<String>,
}

#[derive(FromRow)]
struct BfMedia {
    id: String,
    kind: String,
    title_lc: String,
    year: Option<i64>,
    runtime_minutes: Option<i64>,
    season_number: Option<i64>,
    episode_number: Option<i64>,
    show_title_lc: Option<String>,
}

pub async fn run(jf_path: PathBuf) -> Result<()> {
    if !jf_path.exists() {
        bail!("Jellyfin DB not found at {}", jf_path.display());
    }

    let bf_path = PathBuf::from(
        std::env::var("BINKFLIX_DB").unwrap_or_else(|_| "./data/binkflix.db".into()),
    );
    if !bf_path.exists() {
        bail!("binkflix DB not found at {}", bf_path.display());
    }

    println!("Source (Jellyfin): {}", jf_path.display());
    println!("Target (binkflix): {}\n", bf_path.display());

    let jf = connect_sqlite(&jf_path, true).await?;
    let bf = connect_sqlite(&bf_path, false).await?;

    let users: Vec<JfUser> = sqlx::query_as(
        "SELECT Id AS id, Username AS username
         FROM Users ORDER BY Username COLLATE NOCASE",
    )
    .fetch_all(&jf)
    .await
    .context("listing Jellyfin users")?;
    if users.is_empty() {
        bail!("no users found in Jellyfin DB");
    }
    let user_labels: Vec<String> = users.iter().map(|u| u.username.clone()).collect();
    let pick = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Source Jellyfin user")
        .items(&user_labels)
        .default(0)
        .interact()?;
    let jf_user = &users[pick];
    println!();

    let target_sub = pick_target_user_sub(&bf).await?;
    println!();

    // Pull Jellyfin watch rows.
    let jf_rows: Vec<JfWatch> = sqlx::query_as(
        r#"
        SELECT bi.Type                  AS kind,
               bi.Name                  AS name,
               bi.SeriesName            AS series_name,
               bi.ParentIndexNumber     AS season,
               bi.IndexNumber           AS episode,
               bi.ProductionYear        AS year,
               bi.RunTimeTicks          AS runtime_ticks,
               ud.PlaybackPositionTicks AS position_ticks,
               ud.Played                AS played,
               ud.LastPlayedDate        AS last_played
        FROM UserData ud
        JOIN BaseItems bi ON bi.Id = ud.ItemId
        WHERE ud.UserId = ?
          AND bi.Type IN (?, ?)
          AND (ud.PlaybackPositionTicks > 0 OR ud.Played = 1)
        "#,
    )
    .bind(&jf_user.id)
    .bind(JF_EPISODE)
    .bind(JF_MOVIE)
    .fetch_all(&jf)
    .await
    .context("fetching Jellyfin UserData")?;

    println!("Jellyfin rows to consider: {}", jf_rows.len());

    // Build binkflix indexes. Soft-deleted rows are intentionally excluded —
    // importing onto a soft-deleted media id would resurface obsolete state.
    let bf_media: Vec<BfMedia> = sqlx::query_as(
        r#"
        SELECT m.id                AS "id",
               m.kind              AS "kind",
               lower(m.title)      AS "title_lc",
               m.year              AS "year",
               m.runtime_minutes   AS "runtime_minutes",
               m.season_number     AS "season_number",
               m.episode_number    AS "episode_number",
               lower(s.title)      AS "show_title_lc"
        FROM media m
        LEFT JOIN shows s ON s.id = m.show_id AND s.deleted_at IS NULL
        WHERE m.deleted_at IS NULL
        "#,
    )
    .fetch_all(&bf)
    .await
    .context("loading binkflix media index")?;

    let mut ep_index: HashMap<(String, i64, i64), (String, Option<i64>)> = HashMap::new();
    let mut movie_index: HashMap<(String, i64), (String, Option<i64>)> = HashMap::new();
    let mut movie_titleonly: HashMap<String, (String, Option<i64>)> = HashMap::new();

    for m in &bf_media {
        match m.kind.as_str() {
            "episode" => {
                if let (Some(show), Some(s), Some(e)) =
                    (&m.show_title_lc, m.season_number, m.episode_number)
                {
                    ep_index.insert(
                        (show.clone(), s, e),
                        (m.id.clone(), m.runtime_minutes),
                    );
                }
            }
            "movie" => {
                if let Some(year) = m.year {
                    movie_index.insert(
                        (m.title_lc.clone(), year),
                        (m.id.clone(), m.runtime_minutes),
                    );
                }
                movie_titleonly
                    .entry(m.title_lc.clone())
                    .or_insert_with(|| (m.id.clone(), m.runtime_minutes));
            }
            _ => {}
        }
    }

    // Pre-fetch existing watch_progress for the target user_sub so we can
    // forecast skip-newer counts in the preview.
    let existing: HashMap<String, i64> = sqlx::query_as::<_, (String, i64)>(
        "SELECT media_id, updated_at FROM watch_progress WHERE user_sub = ?",
    )
    .bind(&target_sub)
    .fetch_all(&bf)
    .await?
    .into_iter()
    .collect();

    // Match + compute.
    struct PlanRow {
        media_id: String,
        position_secs: f64,
        duration_secs: f64,
        completed: bool,
        updated_at: i64,
    }

    let mut planned: Vec<PlanRow> = Vec::new();
    let mut unmatched: Vec<String> = Vec::new();
    let mut ep_count = 0usize;
    let mut mv_count = 0usize;

    for r in &jf_rows {
        let lookup = match r.kind.as_str() {
            t if t == JF_EPISODE => {
                let show = r.series_name.as_deref().unwrap_or("");
                let (Some(s), Some(e)) = (r.season, r.episode) else {
                    unmatched.push(describe_jf(r));
                    continue;
                };
                ep_index.get(&(show.to_lowercase(), s, e)).cloned()
            }
            t if t == JF_MOVIE => {
                let name = r.name.as_deref().unwrap_or("").to_lowercase();
                let by_year = r
                    .year
                    .and_then(|y| movie_index.get(&(name.clone(), y)).cloned());
                by_year.or_else(|| movie_titleonly.get(&name).cloned())
            }
            _ => None,
        };

        let Some((media_id, runtime_minutes)) = lookup else {
            unmatched.push(describe_jf(r));
            continue;
        };

        let mut position_secs = r.position_ticks as f64 / 10_000_000.0;
        let duration_secs = r
            .runtime_ticks
            .map(|t| t as f64 / 10_000_000.0)
            .filter(|d| *d > 0.0)
            .or_else(|| runtime_minutes.map(|m| m as f64 * 60.0))
            .unwrap_or(0.0);

        let completed = r.played != 0
            || (duration_secs > 0.0 && position_secs / duration_secs > COMPLETION_RATIO);

        // Played=1 with no recorded position: fill the bar so binkflix shows
        // it as fully watched (matches mark_watched behaviour).
        if completed && position_secs <= 0.0 && duration_secs > 0.0 {
            position_secs = duration_secs;
        }

        let updated_at = parse_jf_date(r.last_played.as_deref())
            .unwrap_or_else(|| Utc::now().timestamp());

        match r.kind.as_str() {
            t if t == JF_EPISODE => ep_count += 1,
            t if t == JF_MOVIE => mv_count += 1,
            _ => {}
        }

        planned.push(PlanRow {
            media_id,
            position_secs,
            duration_secs,
            completed,
            updated_at,
        });
    }

    // Forecast skip-newer / overwrite counts against the pre-fetched existing map.
    let mut will_overwrite = 0usize;
    let mut will_skip_newer = 0usize;
    let mut will_insert = 0usize;
    for p in &planned {
        match existing.get(&p.media_id) {
            None => will_insert += 1,
            Some(&existing_ts) if p.updated_at > existing_ts => will_overwrite += 1,
            Some(_) => will_skip_newer += 1,
        }
    }

    println!(
        "Matched: {} episodes, {} movies   (unmatched: {})",
        ep_count,
        mv_count,
        unmatched.len()
    );
    if !unmatched.is_empty() {
        let n = unmatched.len().min(10);
        println!("Sample unmatched (first {}):", n);
        for u in unmatched.iter().take(n) {
            println!("  - {}", u);
        }
        if unmatched.len() > n {
            println!("  … {} more", unmatched.len() - n);
        }
    }
    println!(
        "\nPlan: insert {}, overwrite {}, skip-newer {}",
        will_insert, will_overwrite, will_skip_newer
    );

    if planned.is_empty() {
        println!("\nNothing to do.");
        return Ok(());
    }

    let go = Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt(format!(
            "Apply {} rows to watch_progress (user_sub={})?",
            planned.len(),
            target_sub
        ))
        .default(false)
        .interact()?;
    if !go {
        println!("Aborted.");
        return Ok(());
    }

    let mut tx = bf.begin().await?;
    let mut inserted = 0u64;
    let mut updated = 0u64;
    let mut skipped = 0u64;

    for p in &planned {
        let before = existing.get(&p.media_id).copied();
        let res = sqlx::query(
            r#"
            INSERT INTO watch_progress (user_sub, media_id, position_secs, duration_secs, completed, updated_at)
            VALUES (?, ?, ?, ?, ?, ?)
            ON CONFLICT(user_sub, media_id) DO UPDATE SET
                position_secs = excluded.position_secs,
                duration_secs = excluded.duration_secs,
                completed     = excluded.completed,
                updated_at    = excluded.updated_at
            WHERE excluded.updated_at > watch_progress.updated_at
            "#,
        )
        .bind(&target_sub)
        .bind(&p.media_id)
        .bind(p.position_secs)
        .bind(p.duration_secs)
        .bind(p.completed as i64)
        .bind(p.updated_at)
        .execute(&mut *tx)
        .await?;

        match (before, res.rows_affected()) {
            (None, n) if n > 0 => inserted += 1,
            (Some(_), n) if n > 0 => updated += 1,
            _ => skipped += 1,
        }
    }

    tx.commit().await?;

    println!(
        "\nDone. Inserted {}, updated {}, skipped-newer {}, unmatched {}.",
        inserted,
        updated,
        skipped,
        unmatched.len()
    );

    Ok(())
}

async fn connect_sqlite(path: &Path, read_only: bool) -> Result<SqlitePool> {
    // Build the connect options directly rather than via the `sqlite://…`
    // URL form: the URL parser interprets `?` query parameters, so a path
    // containing literal `?cache=shared&mode=rwc` could override settings
    // we set later. `filename(path)` takes the path verbatim.
    let opts = SqliteConnectOptions::new()
        .filename(path)
        .read_only(read_only)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(2)
        .connect_with(opts)
        .await
        .with_context(|| format!("opening {}", path.display()))?;
    Ok(pool)
}

async fn pick_target_user_sub(bf: &SqlitePool) -> Result<String> {
    // Union of every sub binkflix knows about (any with watch rows + any seen
    // by auth recently) joined to the optional users table for a friendly
    // login. Pre-users-table installs will get NULL logins and just see subs.
    let mut rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT x.user_sub, u.login
         FROM (SELECT user_sub FROM watch_progress
               UNION SELECT user_sub FROM users) x
         LEFT JOIN users u ON u.user_sub = x.user_sub
         GROUP BY x.user_sub
         ORDER BY (u.login IS NULL), u.login COLLATE NOCASE, x.user_sub",
    )
    .fetch_all(bf)
    .await?;
    if !rows.iter().any(|(s, _)| s == "dev") {
        rows.insert(0, ("dev".into(), None));
    }
    let labels: Vec<String> = rows
        .iter()
        .map(|(sub, login)| match login {
            Some(l) => format!("{} ({})", l, short_sub(sub)),
            None => sub.clone(),
        })
        .collect();
    let mut items: Vec<String> = labels.clone();
    items.push("(enter custom)".into());
    let default_idx = rows.iter().position(|(s, _)| s == "dev").unwrap_or(0);
    let pick = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Target binkflix user_sub")
        .items(&items)
        .default(default_idx)
        .interact()?;
    if pick == items.len() - 1 {
        let custom: String = Input::with_theme(&ColorfulTheme::default())
            .with_prompt("user_sub")
            .interact_text()?;
        let trimmed = custom.trim().to_string();
        if trimmed.is_empty() {
            bail!("empty user_sub");
        }
        Ok(trimmed)
    } else {
        Ok(rows[pick].0.clone())
    }
}

fn short_sub(s: &str) -> String {
    if s.len() > 8 {
        format!("{}…", &s[..8])
    } else {
        s.to_string()
    }
}

fn describe_jf(r: &JfWatch) -> String {
    if r.kind == JF_EPISODE {
        let show = r.series_name.as_deref().unwrap_or("?");
        let s = r.season.unwrap_or(0);
        let e = r.episode.unwrap_or(0);
        format!("{} S{:02}E{:02}", show, s, e)
    } else {
        let n = r.name.as_deref().unwrap_or("?");
        match r.year {
            Some(y) => format!("{} ({})", n, y),
            None => n.to_string(),
        }
    }
}

fn parse_jf_date(s: Option<&str>) -> Option<i64> {
    let s = s?;
    for fmt in &["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some(dt.and_utc().timestamp());
        }
    }
    None
}
