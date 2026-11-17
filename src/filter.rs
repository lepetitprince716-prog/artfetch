use crate::model::{CandidateItem, RatingPolicy};
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::LazyLock;

const AI_REJECT_TAGS: &[&str] = &[
    "ai-generated",
    "ai_generated",
    "ai-assisted",
    "ai_assisted",
    "novelai",
    "stable_diffusion",
    "midjourney",
    "nai_diffusion",
    "other:ai_generated",
    "other:ai generated",
];

const LOW_COMPLETION_TAGS: &[&str] = &[
    "sketch",
    "rough_sketch",
    "unfinished",
    "wip",
    "work_in_progress",
    "doodle",
    "lineart",
    "outline",
    "flat_color",
    "monochrome",
    "greyscale",
    "spot_color",
    "pixel_art",
    "oekaki",
];

const NON_ILLUST_LAYOUT_TAGS: &[&str] = &[
    "comic",
    "4koma",
    "2koma",
    "3koma",
    "manga",
    "silent_comic",
    "speech_bubble",
    "thought_bubble",
    "character_sheet",
    "reference_sheet",
    "multiple_views",
    "concept_art",
    "chart",
    "diagram",
    "meme",
    "parody_meme",
    "photo_(medium)",
    "real_life",
    "cosplay_photo",
    "animated",
    "video",
    "ugoira",
    "text_focus",
];

const CENSORED_TAGS: &[&str] = &[
    "mosaic_censoring",
    "bar_censoring",
    "censored",
    "heavy_censoring",
    "light_censoring",
    "heart_censoring",
    "steam_censoring",
    "convenient_censoring",
    "other:mosaic_censorship",
    "other:mosaic censorship",
];

const UNCENSORED_TAGS: &[&str] = &[
    "uncensored",
    "decensored",
    "other:uncensored",
    "other:decensored",
];

const EHENTAI_TRAINING_CATEGORIES: &[&str] =
    &["Artist CG", "Image Set", "Non-H", "Western", "Misc"];

static PXIMG_PAGE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"pximg\.net/.+?/(\d{6,12}_p\d+)").expect("valid regex"));
static TWIMG_MEDIA_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"pbs\.twimg\.com/media/([A-Za-z0-9_-]+)").expect("valid regex"));
static X_STATUS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"https?://(?:x\.com|twitter\.com)/[^/]+/status/(\d+)").expect("valid regex")
});
static PXIMG_DATE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/img/(\d{4})/(\d{2})/(\d{2})/").expect("valid regex"));
static TWITTER_URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)https?://(?:www\.)?(?:x\.com|twitter\.com)/([A-Za-z0-9_]{1,30})(?:[/?#]|$)")
        .expect("valid regex")
});
static PIXIV_USER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)pixiv\.net/(?:en/)?(?:users/|member(?:_illust)?\.php\?id=)(\d+)")
        .expect("valid regex")
});
static BSKY_URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)bsky\.app/profile/([A-Za-z0-9_.:-]+)").expect("valid regex"));
static ARTSTATION_URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)artstation\.com/(?:artist/)?([A-Za-z0-9_-]+)(?:[/?#]|$)").expect("valid regex")
});
static MASTODON_URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)https?://(baraag\.net|pawoo\.net)/(?:@|users/)([A-Za-z0-9_]+)")
        .expect("valid regex")
});
static MANUAL_PORTAL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(fanbox\.cc|fantia\.jp|patreon\.com|subscribestar\.adult|skeb\.jp|newgrounds\.com|deviantart\.com)",
    )
    .expect("valid regex")
});

const TWITTER_EPOCH_MS: u64 = 1_288_834_974_657;

pub fn extract_source_year(source_url: Option<&str>) -> Option<i32> {
    let url = source_url?;
    if let Some(cap) = X_STATUS_RE.captures(url)
        && let Ok(snowflake) = cap[1].parse::<u64>()
    {
        let unix_secs = ((snowflake >> 22) + TWITTER_EPOCH_MS) / 1000;
        let days = (unix_secs / 86_400) as i64;
        // Civil year from days since 1970-01-01 (Howard Hinnant algorithm).
        let z = days + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = (z - era * 146_097) as u64;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
        let y = (yoe as i64) + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let year = y + i64::from(mp >= 10);
        return Some(year as i32);
    }
    if let Some(cap) = PXIMG_DATE_RE.captures(url)
        && let Ok(year) = cap[1].parse::<i32>()
    {
        return Some(year);
    }
    None
}

pub fn check_backfill(since: Option<&str>, source_url: Option<&str>) -> (Option<i32>, bool) {
    let src_year = extract_source_year(source_url);
    let since_year = since
        .and_then(|s| s.get(0..4))
        .and_then(|y| y.parse::<i32>().ok());
    let suspect = match (since_year, src_year) {
        (Some(sy), Some(iy)) => iy < sy,
        _ => false,
    };
    (src_year, suspect)
}

fn normalized_tag_set(tags: &[String]) -> HashSet<String> {
    let mut out = HashSet::with_capacity(tags.len() * 2);
    for t in tags {
        let low = t.trim().to_ascii_lowercase();
        if low.is_empty() {
            continue;
        }
        out.insert(low.replace(' ', "_"));
        out.insert(low);
    }
    out
}

pub fn classify_censorship(tags: &[String]) -> &'static str {
    let norm = normalized_tag_set(tags);
    let hard = [
        "mosaic_censoring",
        "bar_censoring",
        "heavy_censoring",
        "other:mosaic_censorship",
    ];
    if hard.iter().any(|k| norm.contains(*k)) {
        return "censored";
    }
    if UNCENSORED_TAGS.iter().any(|k| norm.contains(*k)) {
        return "uncensored";
    }
    if CENSORED_TAGS.iter().any(|k| norm.contains(*k)) {
        return "censored";
    }
    "unknown"
}

#[derive(Debug, Clone)]
pub struct FilterOptions {
    pub training_ready: bool,
    pub exclude_censored: bool,
    pub min_long_edge: u32,
    pub min_short_edge: u32,
    pub min_aspect: f64,
    pub max_aspect: f64,
}

impl Default for FilterOptions {
    fn default() -> Self {
        Self {
            training_ready: false,
            exclude_censored: false,
            min_long_edge: 1024,
            min_short_edge: 640,
            min_aspect: 0.33,
            max_aspect: 3.0,
        }
    }
}

pub fn evaluate_training_candidate(
    item: &CandidateItem,
    opts: &FilterOptions,
) -> (bool, Vec<String>, String) {
    let tag_source = if !item.raw_tags.is_empty() {
        &item.raw_tags
    } else {
        &item.tags
    };
    let norm = normalized_tag_set(tag_source);
    let censorship = if item.censorship.is_empty() {
        classify_censorship(tag_source).to_string()
    } else {
        item.censorship.clone()
    };

    let mut reasons = Vec::new();
    if item.is_ai || AI_REJECT_TAGS.iter().any(|k| norm.contains(*k)) {
        reasons.push("ai_generated".to_string());
    }
    if LOW_COMPLETION_TAGS.iter().any(|k| norm.contains(*k)) {
        reasons.push("low_completion".to_string());
    }
    if NON_ILLUST_LAYOUT_TAGS.iter().any(|k| norm.contains(*k)) {
        reasons.push("non_illust_layout".to_string());
    }

    let ext = item
        .file_ext
        .as_deref()
        .unwrap_or_else(|| {
            item.image_url
                .as_deref()
                .and_then(|u| u.split('?').next())
                .and_then(|p| p.rsplit_once('.').map(|(_, e)| e))
                .unwrap_or("")
        })
        .trim_start_matches('.')
        .to_ascii_lowercase();
    if matches!(
        ext.as_str(),
        "mp4" | "webm" | "zip" | "gif" | "ugoira" | "swf"
    ) {
        reasons.push("animated_or_archive".to_string());
    }

    if item.source == "ehentai"
        && let Some(cat) = item.category.as_deref()
        && !cat.is_empty()
        && !EHENTAI_TRAINING_CATEGORIES.contains(&cat)
        && !norm.contains("other:full_color")
    {
        reasons.push("ehentai_non_cg_category".to_string());
    }

    if let (Some(w), Some(h)) = (item.width, item.height)
        && w >= 100
        && h >= 100
    {
        let long_edge = w.max(h);
        let short_edge = w.min(h);
        let aspect = f64::from(w) / f64::from(h);
        if long_edge < opts.min_long_edge {
            reasons.push("low_resolution".to_string());
        }
        if short_edge < opts.min_short_edge {
            reasons.push("short_edge_too_small".to_string());
        }
        if aspect < opts.min_aspect || aspect > opts.max_aspect {
            reasons.push("extreme_aspect_ratio".to_string());
        }
    }

    if opts.exclude_censored && censorship == "censored" {
        reasons.push("censored".to_string());
    }

    (reasons.is_empty(), reasons, censorship)
}

pub fn annotate_and_filter(
    items: Vec<CandidateItem>,
    policy: &RatingPolicy,
    opts: &FilterOptions,
) -> Vec<CandidateItem> {
    let mut out = Vec::with_capacity(items.len());
    for mut item in items {
        let rating_low = item.rating.to_ascii_lowercase();
        if !policy.probe_ratings.contains(rating_low.as_str()) {
            continue;
        }
        let (is_ok, reasons, censorship) = evaluate_training_candidate(&item, opts);
        if opts.exclude_censored && censorship == "censored" {
            continue;
        }
        if opts.training_ready && !is_ok {
            continue;
        }
        item.censorship = censorship;
        item.is_finished_candidate = is_ok;
        item.reject_reasons = reasons;
        if !policy.allow_mature {
            item.tags.clear();
        }
        item.raw_tags.clear();
        out.push(item);
    }
    out
}

pub fn dedupe_parent_items(items: Vec<CandidateItem>) -> Vec<CandidateItem> {
    let mut best: HashMap<(String, String), CandidateItem> = HashMap::new();
    let mut order: Vec<(String, String)> = Vec::new();

    for item in items {
        let cluster = item.parent_id.clone().unwrap_or_else(|| item.id.clone());
        let key = (item.source.clone(), cluster);
        if let Some(cur) = best.get(&key) {
            let cur_score = (
                u8::from(cur.parent_id.is_none()),
                u64::from(cur.width.unwrap_or(0)) * u64::from(cur.height.unwrap_or(0)),
            );
            let new_score = (
                u8::from(item.parent_id.is_none()),
                u64::from(item.width.unwrap_or(0)) * u64::from(item.height.unwrap_or(0)),
            );
            if new_score > cur_score {
                best.insert(key, item);
            }
        } else {
            order.push(key.clone());
            best.insert(key, item);
        }
    }

    order.into_iter().filter_map(|k| best.remove(&k)).collect()
}

pub fn canonical_artwork_key(item: &CandidateItem) -> Option<String> {
    if let Some(md5) = item.md5.as_deref() {
        let clean = md5.trim().to_ascii_lowercase();
        if clean.len() == 32 && clean.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Some(format!("md5:{clean}"));
        }
    }
    for url in [item.image_url.as_deref(), item.source_url.as_deref()]
        .into_iter()
        .flatten()
    {
        if let Some(cap) = PXIMG_PAGE_RE.captures(url) {
            return Some(format!("pximg:{}", &cap[1]));
        }
        if let Some(cap) = TWIMG_MEDIA_RE.captures(url) {
            return Some(format!("twimg:{}", &cap[1]));
        }
    }
    if item.source == "pixiv" && item.id.contains("_p") {
        return Some(format!("pximg:{}", item.id));
    }
    None
}

pub fn dedupe_cross_platform_items(items: Vec<CandidateItem>) -> Vec<CandidateItem> {
    let parent_collapsed = dedupe_parent_items(items);
    let mut winners: HashMap<String, CandidateItem> = HashMap::new();
    let mut sources_map: HashMap<String, BTreeSet<String>> = HashMap::new();
    let mut urls_map: HashMap<String, Vec<String>> = HashMap::new();
    let mut order: Vec<String> = Vec::new();

    for (idx, item) in parent_collapsed.into_iter().enumerate() {
        let key = canonical_artwork_key(&item)
            .unwrap_or_else(|| format!("unique:{}:{}:{}", item.source, item.id, idx));
        let item_urls: Vec<String> = [item.source_url.clone(), item.image_url.clone()]
            .into_iter()
            .flatten()
            .filter(|s| !s.is_empty())
            .collect();

        sources_map
            .entry(key.clone())
            .or_default()
            .insert(item.source.clone());
        let url_list = urls_map.entry(key.clone()).or_default();
        for u in item_urls {
            if !url_list.contains(&u) {
                url_list.push(u);
            }
        }

        if let Some(cur) = winners.get(&key) {
            let cur_rank = (
                u8::from(cur.parent_id.is_none()),
                u64::from(cur.width.unwrap_or(0)) * u64::from(cur.height.unwrap_or(0)),
            );
            let new_rank = (
                u8::from(item.parent_id.is_none()),
                u64::from(item.width.unwrap_or(0)) * u64::from(item.height.unwrap_or(0)),
            );
            if new_rank > cur_rank {
                winners.insert(key, item);
            }
        } else {
            order.push(key.clone());
            winners.insert(key, item);
        }
    }

    order
        .into_iter()
        .filter_map(|k| {
            let mut w = winners.remove(&k)?;
            if let Some(srcs) = sources_map.remove(&k) {
                w.all_seen_sources = srcs.into_iter().collect();
            }
            if let Some(urls) = urls_map.remove(&k) {
                w.all_seen_urls = urls;
            }
            Some(w)
        })
        .collect()
}

pub fn extract_platform_handles(
    urls: &[String],
    preferred_name: Option<&str>,
) -> (BTreeMap<String, String>, Vec<String>) {
    let twitter_reserved: HashSet<&str> = [
        "i", "intent", "home", "explore", "search", "hashtag", "settings", "share", "status",
    ]
    .into_iter()
    .collect();
    let artstation_reserved: HashSet<&str> = [
        "artwork",
        "projects",
        "blogs",
        "prints",
        "marketplace",
        "learning",
        "search",
    ]
    .into_iter()
    .collect();

    let norm_alnum = |s: &str| -> String {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_lowercase())
            .collect()
    };
    let pref = preferred_name.map(norm_alnum).unwrap_or_default();

    let mut platforms: BTreeMap<String, String> = BTreeMap::new();
    let mut manual_urls = Vec::new();
    let mut put = |key: &str, cand: String| {
        if !platforms.contains_key(key) || (!pref.is_empty() && norm_alnum(&cand) == pref) {
            platforms.insert(key.to_string(), cand);
        }
    };

    for raw in urls {
        let u = raw.trim();
        if u.is_empty() {
            continue;
        }
        if let Some(cap) = TWITTER_URL_RE.captures(u) {
            let h = &cap[1];
            if !twitter_reserved.contains(h.to_ascii_lowercase().as_str()) {
                put("twitter", h.to_string());
            }
        }
        if let Some(cap) = PIXIV_USER_RE.captures(u) {
            put("pixiv", cap[1].to_string());
        }
        if let Some(cap) = BSKY_URL_RE.captures(u) {
            put("bluesky", cap[1].to_string());
        }
        if let Some(cap) = ARTSTATION_URL_RE.captures(u) {
            let slug = &cap[1];
            if !artstation_reserved.contains(slug.to_ascii_lowercase().as_str()) {
                put("artstation", slug.to_string());
            }
        }
        if let Some(cap) = MASTODON_URL_RE.captures(u) {
            let host = if cap[1].to_ascii_lowercase().contains("baraag") {
                "baraag"
            } else {
                "pawoo"
            };
            put("mastodon", format!("{host}:{}", &cap[2]));
        }
        if MANUAL_PORTAL_RE.is_match(u) && !manual_urls.contains(&u.to_string()) {
            manual_urls.push(u.to_string());
        }
    }

    (platforms, manual_urls)
}
