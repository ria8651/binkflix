use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub struct MovieNfo {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default, rename = "originaltitle")]
    pub original_title: Option<String>,
    #[serde(default)]
    pub year: Option<i64>,
    #[serde(default)]
    pub plot: Option<String>,
    #[serde(default)]
    pub runtime: Option<i64>,
    #[serde(default)]
    pub genre: Vec<String>,
    #[serde(default)]
    pub uniqueid: Vec<UniqueId>,
    #[serde(default)]
    pub mpaa: Option<String>,
    #[serde(default)]
    pub tagline: Option<String>,
    #[serde(default)]
    pub premiered: Option<String>,
    #[serde(default)]
    pub studio: Vec<String>,
    #[serde(default)]
    pub director: Vec<String>,
    /// Kodi NFO uses `<credits>` for writers; one element per writer.
    #[serde(default)]
    pub credits: Vec<String>,
    /// Flat `<rating>` element — typically a single average.
    #[serde(default)]
    pub rating: Option<f64>,
    /// Newer `<ratings>` block carrying per-source ratings.
    #[serde(default)]
    pub ratings: Option<Ratings>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub struct TvShowNfo {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default, rename = "originaltitle")]
    pub original_title: Option<String>,
    #[serde(default)]
    pub year: Option<i64>,
    #[serde(default)]
    pub premiered: Option<String>,
    #[serde(default)]
    pub plot: Option<String>,
    #[serde(default)]
    pub genre: Vec<String>,
    #[serde(default)]
    pub uniqueid: Vec<UniqueId>,
    #[serde(default)]
    pub mpaa: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub enddate: Option<String>,
    #[serde(default)]
    pub studio: Vec<String>,
    #[serde(default)]
    pub rating: Option<f64>,
    #[serde(default)]
    pub ratings: Option<Ratings>,
}

impl TvShowNfo {
    pub fn imdb_id(&self) -> Option<&str> { imdb(&self.uniqueid) }
    pub fn tmdb_id(&self) -> Option<&str> { tmdb(&self.uniqueid) }

    /// Year from `<year>` or derived from `<premiered>` (first 4 chars of YYYY-MM-DD).
    pub fn year_or_premiered(&self) -> Option<i64> {
        if let Some(y) = self.year { return Some(y); }
        self.premiered
            .as_deref()
            .and_then(|s| s.get(..4))
            .and_then(|y| y.parse().ok())
    }

    pub fn primary_rating(&self) -> Option<(f64, Option<i64>, String)> {
        primary_rating(self.ratings.as_ref(), self.rating)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub struct EpisodeNfo {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub season: Option<i64>,
    #[serde(default)]
    pub episode: Option<i64>,
    #[serde(default)]
    pub plot: Option<String>,
    #[serde(default)]
    pub runtime: Option<i64>,
    #[serde(default)]
    pub aired: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UniqueId {
    #[serde(rename = "@type", default)]
    pub kind: String,
    #[serde(rename = "$value", default)]
    pub value: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct Ratings {
    #[serde(default, rename = "rating")]
    pub items: Vec<RatingItem>,
}

#[derive(Debug, Default, Deserialize)]
pub struct RatingItem {
    #[serde(rename = "@name", default)]
    pub name: Option<String>,
    /// Kodi writes this as `default="true"`. quick-xml-de parses the literal
    /// "true"/"false" into a bool; missing attribute → None.
    #[serde(rename = "@default", default)]
    pub default: Option<bool>,
    #[serde(default)]
    pub value: Option<f64>,
    #[serde(default)]
    pub votes: Option<i64>,
}

fn imdb<'a>(ids: &'a [UniqueId]) -> Option<&'a str> {
    ids.iter()
        .find(|u| u.kind.eq_ignore_ascii_case("imdb"))
        .map(|u| u.value.as_str())
}

fn tmdb<'a>(ids: &'a [UniqueId]) -> Option<&'a str> {
    ids.iter()
        .find(|u| u.kind.eq_ignore_ascii_case("tmdb"))
        .map(|u| u.value.as_str())
}

impl MovieNfo {
    pub fn imdb_id(&self) -> Option<&str> { imdb(&self.uniqueid) }
    pub fn tmdb_id(&self) -> Option<&str> { tmdb(&self.uniqueid) }

    pub fn primary_rating(&self) -> Option<(f64, Option<i64>, String)> {
        primary_rating(self.ratings.as_ref(), self.rating)
    }
}

/// Pick the most representative rating across the `<ratings>` block and the
/// flat `<rating>` fallback. Prefers a `default="true"` entry; otherwise the
/// first item with a non-zero value; otherwise the flat `<rating>`.
fn primary_rating(
    ratings: Option<&Ratings>,
    flat: Option<f64>,
) -> Option<(f64, Option<i64>, String)> {
    if let Some(r) = ratings {
        let pick = r
            .items
            .iter()
            .find(|it| it.default == Some(true) && it.value.is_some())
            .or_else(|| r.items.iter().find(|it| it.value.unwrap_or(0.0) > 0.0));
        if let Some(it) = pick {
            if let Some(v) = it.value {
                let source = it.name.clone().unwrap_or_else(|| "nfo".to_string());
                return Some((v, it.votes, source));
            }
        }
    }
    flat.map(|v| (v, None, "nfo".to_string()))
}


/// Drop the `Some("")` Kodi writes for self-closing elements like `<mpaa />`
/// so downstream code can treat "present" as "non-empty".
fn nilify(s: &mut Option<String>) {
    if matches!(s.as_deref(), Some(v) if v.trim().is_empty()) {
        *s = None;
    }
}

fn clean_strings(v: &mut Vec<String>) {
    v.retain(|s| !s.trim().is_empty());
}

pub fn parse_movie_nfo(path: &Path) -> anyhow::Result<MovieNfo> {
    let xml = std::fs::read_to_string(path)?;
    let mut nfo: MovieNfo = quick_xml::de::from_str(&xml)?;
    nilify(&mut nfo.title);
    nilify(&mut nfo.original_title);
    nilify(&mut nfo.plot);
    nilify(&mut nfo.mpaa);
    nilify(&mut nfo.tagline);
    nilify(&mut nfo.premiered);
    clean_strings(&mut nfo.genre);
    clean_strings(&mut nfo.studio);
    clean_strings(&mut nfo.director);
    clean_strings(&mut nfo.credits);
    Ok(nfo)
}

pub fn parse_tvshow_nfo(path: &Path) -> anyhow::Result<TvShowNfo> {
    let xml = std::fs::read_to_string(path)?;
    let mut nfo: TvShowNfo = quick_xml::de::from_str(&xml)?;
    nilify(&mut nfo.title);
    nilify(&mut nfo.original_title);
    nilify(&mut nfo.plot);
    nilify(&mut nfo.premiered);
    nilify(&mut nfo.mpaa);
    nilify(&mut nfo.status);
    nilify(&mut nfo.enddate);
    clean_strings(&mut nfo.genre);
    clean_strings(&mut nfo.studio);
    Ok(nfo)
}

pub fn parse_episode_nfo(path: &Path) -> anyhow::Result<EpisodeNfo> {
    let xml = std::fs::read_to_string(path)?;
    let mut nfo: EpisodeNfo = quick_xml::de::from_str(&xml)?;
    nilify(&mut nfo.title);
    nilify(&mut nfo.plot);
    nilify(&mut nfo.aired);
    Ok(nfo)
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum NfoKind {
    Movie,
    TvShow,
    Episode,
}

/// Peek at an NFO file's root element to decide what kind of metadata it holds.
/// Skips XML declaration, comments, and whitespace. Returns None for unknown roots.
pub fn detect_nfo_kind(path: &Path) -> Option<NfoKind> {
    let text = std::fs::read_to_string(path).ok()?;
    let bytes = text.as_bytes();
    let mut i = 0;
    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'<' {
            return None;
        }
        match bytes.get(i + 1) {
            Some(b'?') | Some(b'!') => {
                // XML decl, DOCTYPE, comment — skip to next '>'.
                while i < bytes.len() && bytes[i] != b'>' {
                    i += 1;
                }
                i += 1;
                continue;
            }
            _ => {}
        }
        let start = i + 1;
        let mut end = start;
        while end < bytes.len()
            && !bytes[end].is_ascii_whitespace()
            && bytes[end] != b'>'
            && bytes[end] != b'/'
        {
            end += 1;
        }
        let name = text.get(start..end)?.to_ascii_lowercase();
        return match name.as_str() {
            "movie" => Some(NfoKind::Movie),
            "tvshow" => Some(NfoKind::TvShow),
            "episodedetails" => Some(NfoKind::Episode),
            _ => None,
        };
    }
}
