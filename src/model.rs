use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

pub const ALL_SOURCES: &[&str] = &[
    "twitter",
    "bluesky",
    "danbooru",
    "gelbooru",
    "safebooru_org",
    "tbib",
    "yandere",
    "konachan",
    "e621",
    "pixiv",
    "ehentai",
    "artstation",
    "mastodon",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RatingTier {
    Sfw,
    Ecchi,
    MatureOnly,
    All,
}

impl RatingTier {
    pub fn parse_str(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "sfw" => Ok(Self::Sfw),
            "ecchi" => Ok(Self::Ecchi),
            "mature" | "mature_only" | "nsfw_only" => Ok(Self::MatureOnly),
            "all" => Ok(Self::All),
            other => Err(format!(
                "unsupported rating tier '{other}'; expected sfw, ecchi, mature_only, or all"
            )),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Sfw => "sfw",
            Self::Ecchi => "ecchi",
            Self::MatureOnly => "nsfw_only",
            Self::All => "all",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RatingPolicy {
    pub tier: RatingTier,
    pub allow_mature: bool,
    pub probe_ratings: HashSet<&'static str>,
}

impl RatingPolicy {
    pub fn resolve(rating_tier: Option<&str>, allow_mature: bool) -> Result<Self, String> {
        let tier = match rating_tier {
            None => {
                if allow_mature {
                    RatingTier::All
                } else {
                    RatingTier::Sfw
                }
            }
            Some(raw) => {
                let parsed = RatingTier::parse_str(raw)?;
                if parsed == RatingTier::Sfw && allow_mature {
                    return Err(
                        "conflicting flags: --rating-tier sfw cannot be combined with --allow-mature"
                            .to_string(),
                    );
                }
                parsed
            }
        };

        let probe_ratings: HashSet<&'static str> = match tier {
            RatingTier::Sfw => ["general", "sensitive", "unknown"].into_iter().collect(),
            RatingTier::Ecchi => ["general", "sensitive", "questionable", "unknown"]
                .into_iter()
                .collect(),
            RatingTier::MatureOnly => ["questionable", "explicit", "unknown"]
                .into_iter()
                .collect(),
            RatingTier::All => [
                "general",
                "sensitive",
                "questionable",
                "explicit",
                "unknown",
            ]
            .into_iter()
            .collect(),
        };

        Ok(Self {
            tier,
            allow_mature: tier != RatingTier::Sfw,
            probe_ratings,
        })
    }

    pub fn content_rating_label(&self) -> &'static str {
        if self.allow_mature { "mature" } else { "sfw" }
    }
}

pub fn normalize_rating_code(raw: &str) -> &'static str {
    match raw.trim().to_ascii_lowercase().as_str() {
        "g" | "general" | "safe" => "general",
        "s" | "sensitive" => "sensitive",
        "q" | "questionable" => "questionable",
        "e" | "explicit" => "explicit",
        _ => "unknown",
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CandidateItem {
    pub id: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    pub rating: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_ext: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub md5: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fav_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filecount: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avg_kb_per_file: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inferred_source_year: Option<i32>,
    #[serde(default)]
    pub is_backfill_suspect: bool,
    #[serde(default)]
    pub is_ai: bool,
    #[serde(default)]
    pub censorship: String,
    #[serde(default)]
    pub is_finished_candidate: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reject_reasons: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub all_seen_sources: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub all_seen_urls: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(skip)]
    pub raw_tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtistCounts {
    pub danbooru_total: u64,
    pub danbooru_sfw: u64,
    pub danbooru_mature: u64,
    pub danbooru_sfw_ratio: Option<f64>,
    pub gelbooru_total: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtistProfile {
    pub schema: String,
    pub content_rating: String,
    pub query: String,
    pub canonical_tag: Option<String>,
    pub alias_from: Option<String>,
    pub other_names: Vec<String>,
    pub counts: ArtistCounts,
    pub resolved_targets: BTreeMap<String, String>,
    pub manual_or_browser_urls: Vec<String>,
}
