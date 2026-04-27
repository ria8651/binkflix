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
    pub uniqueid: Vec<UniqueId>,
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
}

#[derive(Debug, Deserialize)]
pub struct UniqueId {
    #[serde(rename = "@type", default)]
    pub kind: String,
    #[serde(rename = "$value", default)]
    pub value: String,
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
}


pub fn parse_movie_nfo(path: &Path) -> anyhow::Result<MovieNfo> {
    let xml = std::fs::read_to_string(path)?;
    Ok(quick_xml::de::from_str(&xml)?)
}

pub fn parse_tvshow_nfo(path: &Path) -> anyhow::Result<TvShowNfo> {
    let xml = std::fs::read_to_string(path)?;
    Ok(quick_xml::de::from_str(&xml)?)
}

pub fn parse_episode_nfo(path: &Path) -> anyhow::Result<EpisodeNfo> {
    let xml = std::fs::read_to_string(path)?;
    Ok(quick_xml::de::from_str(&xml)?)
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

