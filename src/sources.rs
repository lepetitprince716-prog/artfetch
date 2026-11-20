use crate::filter::{check_backfill, extract_platform_handles};
use crate::http::{HttpEngine, expand_tilde, gelbooru_credentials};
use crate::model::{
    ArtistCounts, ArtistProfile, CandidateItem, RatingPolicy, RatingTier, normalize_rating_code,
};
use regex::Regex;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::LazyLock;
use tokio::process::Command;
use url::Url;

static EH_GALLERY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"e-hentai\.org/g/(\d+)/([0-9a-f]+)").expect("valid regex"));
static GELBOORU_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"page=post&(?:amp;)?s=view&(?:amp;)?id=(\d+)").expect("regex"));
static GELBOORU_IMG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?:property="og:image"\s+content="|href=")(https://img\d*\.gelbooru\.com/[^"]*images/[^"]+)""#,
    )
    .expect("regex")
});
static GELBOORU_RATING_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Rating:\s*([A-Za-z]+)").expect("regex"));
static GELBOORU_SIZE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Size:\s*(\d+)x(\d+)").expect("regex"));

pub fn resolve_bin(name: &str) -> Option<PathBuf> {
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let cand = dir.join(name);
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    let local = expand_tilde(&format!("~/.local/bin/{name}"));
    if local.is_file() {
        return Some(local);
    }
    None
}

pub fn twitter_orig_url(raw: &str) -> String {
    let Ok(mut parsed) = Url::parse(raw) else {
        return raw.to_string();
    };
    let host_ok = parsed
        .host_str()
        .is_some_and(|h| h.contains("pbs.twimg.com"));
    if !host_ok || !parsed.path().contains("/media/") {
        return raw.to_string();
    }
    let mut pairs: Vec<(String, String)> = parsed
        .query_pairs()
        .filter(|(k, _)| k != "name")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    pairs.push(("name".to_string(), "orig".to_string()));
    parsed.query_pairs_mut().clear().extend_pairs(pairs);
    parsed.into()
}

pub fn iso_from_epoch(epoch_secs: i64) -> Option<String> {
    if epoch_secs <= 0 {
        return None;
    }
    let days = epoch_secs.div_euclid(86_400);
    let rem = epoch_secs.rem_euclid(86_400);
    let hour = rem / 3600;
    let minute = (rem % 3600) / 60;
    let second = rem % 60;

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + i64::from(month <= 2);

    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"
    ))
}

fn url_encode(input: &str) -> String {
    url::form_urlencoded::byte_serialize(input.as_bytes()).collect()
}

fn booru_rating_qualifier(style: &str, tier: RatingTier) -> &'static str {
    match (style, tier) {
        ("danbooru", RatingTier::Sfw) => " rating:g,s",
        ("danbooru", RatingTier::Ecchi) => " -rating:e",
        ("danbooru", RatingTier::MatureOnly) => " rating:q,e",
        ("dapi", RatingTier::Sfw) => " -rating:questionable -rating:explicit",
        ("dapi", RatingTier::Ecchi) => " -rating:explicit",
        ("moebooru" | "e621", RatingTier::Sfw) => " rating:s",
        ("moebooru" | "e621", RatingTier::Ecchi) => " -rating:e",
        ("moebooru" | "e621", RatingTier::MatureOnly) => " -rating:s",
        _ => "",
    }
}

pub async fn check_all_status(engine: &HttpEngine) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();

    let http_probes = [
        (
            "danbooru",
            "https://danbooru.donmai.us/counts/posts.json?tags=1girl",
            "REST JSON",
        ),
        (
            "safebooru_org",
            "https://safebooru.org/index.php?page=dapi&s=post&q=index&json=1&limit=1&tags=1girl",
            "DAPI JSON",
        ),
        (
            "tbib",
            "https://tbib.org/index.php?page=dapi&s=post&q=index&json=1&limit=1&tags=1girl",
            "DAPI JSON",
        ),
        (
            "yandere",
            "https://yande.re/post.json?limit=1",
            "Moebooru REST",
        ),
        (
            "konachan",
            "https://konachan.com/post.json?limit=1",
            "Moebooru REST",
        ),
        (
            "e621",
            "https://e621.net/posts.json?limit=1&tags=solo",
            "e621 REST",
        ),
        (
            "bluesky",
            "https://public.api.bsky.app/xrpc/app.bsky.actor.getProfile?actor=bsky.app",
            "AppView XRPC",
        ),
        (
            "mastodon",
            "https://baraag.net/api/v1/instance",
            "Mastodon REST",
        ),
    ];

    let futs = http_probes.iter().map(|(name, url, backend)| {
        let eng = engine.clone();
        let name = (*name).to_string();
        let url = (*url).to_string();
        let backend = (*backend).to_string();
        async move {
            match eng.get_text(&url).await {
                Ok(body) => (
                    name,
                    json!({ "ok": !body.trim().is_empty(), "backend": backend }),
                ),
                Err(e) => (
                    name,
                    json!({ "ok": false, "backend": backend, "reason": e }),
                ),
            }
        }
    });

    for (k, v) in futures_util::future::join_all(futs).await {
        out.insert(k, v);
    }

    let (gb_key, gb_uid) = gelbooru_credentials();
    let gb_status = if let (Some(k), Some(u)) = (gb_key, gb_uid) {
        let url = format!(
            "https://gelbooru.com/index.php?page=dapi&s=post&q=index&json=1&tags=1girl&limit=1&api_key={}&user_id={}",
            url_encode(&k),
            url_encode(&u)
        );
        match engine.get_text(&url).await {
            Ok(txt) => {
                let ok = txt.contains("\"post\"") || txt.contains("\"@attributes\"");
                json!({ "ok": ok, "mode": "dapi", "api_keys_set": true })
            }
            Err(e) => json!({ "ok": false, "mode": "dapi", "reason": e }),
        }
    } else {
        match engine
            .get_text("https://gelbooru.com/index.php?page=post&s=list&tags=1girl")
            .await
        {
            Ok(txt) => json!({
                "ok": txt.to_ascii_lowercase().contains("gelbooru"),
                "mode": "html_fallback",
                "api_keys_set": false
            }),
            Err(e) => json!({ "ok": false, "mode": "html_fallback", "reason": e }),
        }
    };
    out.insert("gelbooru".to_string(), gb_status);

    // Synthetic placeholder gallery id: verifies gdata endpoint reachability (returns a
    // `gmetadata` JSON key) without embedding a real gallery entry in public source code.
    let eh_payload = json!({
        "method": "gdata",
        "gidlist": [[1, "0000000000"]],
        "namespace": 1
    });
    let eh_status = match engine
        .post_json("https://api.e-hentai.org/api.php", &eh_payload)
        .await
    {
        Ok(val) => json!({ "ok": val.get("gmetadata").is_some(), "backend": "gdata + gallery-dl" }),
        Err(e) => json!({ "ok": false, "reason": e }),
    };
    out.insert("ehentai".to_string(), eh_status);

    let gdl = resolve_bin("gallery-dl");
    let gdl_cfg = expand_tilde("~/.config/gallery-dl/config.json");
    let (mut has_oauth, mut has_cookies) = (false, false);
    if let Ok(raw_cfg) = std::fs::read_to_string(gdl_cfg)
        && let Ok(doc) = serde_json::from_str::<Value>(&raw_cfg)
        && let Some(px) = doc.pointer("/extractor/pixiv")
    {
        has_oauth = px
            .get("refresh-token")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty());
        has_cookies = px.get("cookies").is_some() || px.get("cookies-from-browser").is_some();
    }
    out.insert(
        "pixiv".to_string(),
        json!({
            "ok": gdl.is_some() && has_oauth,
            "gallery_dl_found": gdl.is_some(),
            "oauth_configured": has_oauth,
            "r18_cookies_configured": has_cookies,
        }),
    );
    out.insert(
        "artstation".to_string(),
        json!({ "ok": gdl.is_some(), "backend": "gallery-dl" }),
    );

    let tw_status = if let Some(tw_bin) = resolve_bin("twitter") {
        match Command::new(tw_bin)
            .args(["status", "--json"])
            .output()
            .await
        {
            Ok(out_p) if out_p.status.success() => {
                let parsed: Value = serde_json::from_slice(&out_p.stdout).unwrap_or_default();
                let authed = parsed
                    .pointer("/data/authenticated")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                json!({ "ok": authed, "authenticated": authed, "backend": "twitter-cli" })
            }
            _ => json!({ "ok": false, "backend": "twitter-cli" }),
        }
    } else {
        json!({ "ok": false, "reason": "twitter binary not found" })
    };
    out.insert("twitter".to_string(), tw_status);

    out
}

pub async fn probe_danbooru(
    engine: &HttpEngine,
    tag: &str,
    since: Option<&str>,
    limit: usize,
    policy: &RatingPolicy,
) -> Result<Vec<CandidateItem>, String> {
    let mut q_tags = format!("{tag}{}", booru_rating_qualifier("danbooru", policy.tier));
    if let Some(s) = since {
        q_tags.push_str(&format!(" date:>={s}"));
    }
    let per_page = limit.clamp(20, 200);
    let mut items = Vec::new();

    for page in 1..=10 {
        let url = format!(
            "https://danbooru.donmai.us/posts.json?tags={}&limit={per_page}&page={page}",
            url_encode(&q_tags)
        );
        let text = engine.get_text(&url).await?;
        let rows: Vec<Value> =
            serde_json::from_str(&text).map_err(|e| format!("danbooru JSON error: {e}"))?;
        if rows.is_empty() {
            break;
        }
        let row_len = rows.len();
        for post in rows {
            if post
                .get("is_banned")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                || post
                    .get("is_deleted")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            {
                continue;
            }
            let Some(file_url) = post.get("file_url").and_then(Value::as_str) else {
                continue;
            };
            let ext = post
                .get("file_ext")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_ascii_lowercase();
            if matches!(ext.as_str(), "mp4" | "webm" | "zip") {
                continue;
            }
            let id = post
                .get("id")
                .and_then(Value::as_i64)
                .map(|v| v.to_string())
                .unwrap_or_default();
            let src = post
                .get("source")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty());
            let (src_year, backfill) = check_backfill(since, src);
            let rating = normalize_rating_code(
                post.get("rating")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown"),
            );
            let mut tag_vec: Vec<String> = Vec::new();
            for field in ["tag_string_general", "tag_string_meta"] {
                if let Some(ts) = post.get(field).and_then(Value::as_str) {
                    tag_vec.extend(ts.split_whitespace().map(str::to_string));
                }
            }
            items.push(CandidateItem {
                id: id.clone(),
                source: "danbooru".to_string(),
                created_at: post
                    .get("created_at")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                rating: rating.to_string(),
                width: post
                    .get("image_width")
                    .and_then(Value::as_u64)
                    .map(|v| v as u32),
                height: post
                    .get("image_height")
                    .and_then(Value::as_u64)
                    .map(|v| v as u32),
                file_ext: (!ext.is_empty()).then_some(ext),
                file_size: post.get("file_size").and_then(Value::as_u64),
                md5: post.get("md5").and_then(Value::as_str).map(str::to_string),
                score: post.get("score").and_then(Value::as_i64),
                fav_count: post.get("fav_count").and_then(Value::as_u64),
                image_url: Some(file_url.to_string()),
                source_url: Some(
                    src.map(str::to_string)
                        .unwrap_or_else(|| format!("https://danbooru.donmai.us/posts/{id}")),
                ),
                parent_id: post
                    .get("parent_id")
                    .and_then(Value::as_i64)
                    .map(|v| v.to_string()),
                inferred_source_year: src_year,
                is_backfill_suspect: backfill,
                tags: tag_vec.clone(),
                raw_tags: tag_vec,
                ..Default::default()
            });
            if items.len() >= limit {
                return Ok(items);
            }
        }
        if row_len < per_page {
            break;
        }
    }
    Ok(items)
}

#[allow(clippy::too_many_arguments)]
pub async fn probe_dapi(
    engine: &HttpEngine,
    source_name: &str,
    base_url: &str,
    tag: &str,
    since: Option<&str>,
    limit: usize,
    policy: &RatingPolicy,
    creds: Option<(&str, &str)>,
) -> Result<Vec<CandidateItem>, String> {
    if source_name == "safebooru_org" && policy.tier == RatingTier::MatureOnly {
        return Ok(Vec::new());
    }
    let qual = if source_name == "safebooru_org" {
        ""
    } else {
        booru_rating_qualifier("dapi", policy.tier)
    };
    let q_tags = format!("{tag}{qual}");
    let per_page = limit.clamp(20, 100);
    let mut out = Vec::new();

    for pid in 0..10 {
        let mut url = format!(
            "{base_url}/index.php?page=dapi&s=post&q=index&json=1&tags={}&limit={per_page}&pid={pid}",
            url_encode(&q_tags)
        );
        if let Some((k, u)) = creds {
            url.push_str(&format!(
                "&api_key={}&user_id={}",
                url_encode(k),
                url_encode(u)
            ));
        }
        let text = engine.get_text(&url).await?;
        if text.trim().is_empty() {
            break;
        }
        let parsed: Value =
            serde_json::from_str(&text).map_err(|e| format!("{source_name} JSON error: {e}"))?;
        let rows: Vec<Value> = if let Some(arr) = parsed.get("post").and_then(Value::as_array) {
            arr.clone()
        } else if let Some(arr) = parsed.as_array() {
            arr.clone()
        } else {
            break;
        };
        if rows.is_empty() {
            break;
        }
        let row_len = rows.len();
        let mut reached_cutoff = false;

        for p in rows {
            let raw_r = p.get("rating").and_then(Value::as_str).unwrap_or(
                if source_name == "safebooru_org" {
                    "general"
                } else {
                    "unknown"
                },
            );
            let rating = normalize_rating_code(raw_r);
            if !policy.probe_ratings.contains(rating) {
                continue;
            }
            let file_url = p
                .get("file_url")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    if source_name == "tbib" {
                        let dir = p.get("directory")?;
                        let img = p.get("image")?.as_str()?;
                        let dir_s = dir
                            .as_i64()
                            .map(|d| d.to_string())
                            .or_else(|| dir.as_str().map(str::to_string))?;
                        Some(format!("https://tbib.org//images/{dir_s}/{img}"))
                    } else {
                        None
                    }
                });
            let Some(file_url) = file_url else {
                continue;
            };
            let ext = file_url
                .split('?')
                .next()
                .and_then(|s| s.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()))
                .unwrap_or_default();
            if matches!(ext.as_str(), "mp4" | "webm" | "zip") {
                continue;
            }
            let iso_created = p
                .get("change")
                .and_then(Value::as_i64)
                .and_then(iso_from_epoch)
                .or_else(|| {
                    p.get("created_at")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                });
            if let (Some(s), Some(c)) = (since, iso_created.as_deref())
                && c.len() >= 10
                && c.starts_with("20")
                && &c[..10] < s
            {
                reached_cutoff = true;
                continue;
            }
            let id = p
                .get("id")
                .and_then(Value::as_i64)
                .map(|v| v.to_string())
                .or_else(|| p.get("id").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_default();
            let src = p
                .get("source")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty());
            let (src_year, backfill) = check_backfill(since, src);
            let parent_id = p
                .get("parent_id")
                .and_then(Value::as_i64)
                .filter(|v| *v > 0)
                .map(|v| v.to_string());
            let tag_vec: Vec<String> = p
                .get("tags")
                .and_then(Value::as_str)
                .map(|s| s.split_whitespace().map(str::to_string).collect())
                .unwrap_or_default();

            out.push(CandidateItem {
                id: id.clone(),
                source: source_name.to_string(),
                created_at: iso_created,
                rating: rating.to_string(),
                width: p.get("width").and_then(Value::as_u64).map(|v| v as u32),
                height: p.get("height").and_then(Value::as_u64).map(|v| v as u32),
                file_ext: (!ext.is_empty()).then_some(ext),
                md5: p
                    .get("md5")
                    .or_else(|| p.get("hash"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                score: p.get("score").and_then(Value::as_i64),
                image_url: Some(file_url),
                source_url: Some(
                    src.map(str::to_string).unwrap_or_else(|| {
                        format!("{base_url}/index.php?page=post&s=view&id={id}")
                    }),
                ),
                parent_id,
                inferred_source_year: src_year,
                is_backfill_suspect: backfill,
                tags: tag_vec.clone(),
                raw_tags: tag_vec,
                ..Default::default()
            });
            if out.len() >= limit {
                return Ok(out);
            }
        }
        if reached_cutoff || row_len < per_page {
            break;
        }
    }
    Ok(out)
}

pub async fn probe_gelbooru(
    engine: &HttpEngine,
    tag: &str,
    since: Option<&str>,
    limit: usize,
    policy: &RatingPolicy,
) -> Result<Vec<CandidateItem>, String> {
    let (api_key, user_id) = gelbooru_credentials();
    if let (Some(k), Some(u)) = (api_key.as_deref(), user_id.as_deref()) {
        return probe_dapi(
            engine,
            "gelbooru",
            "https://gelbooru.com",
            tag,
            since,
            limit,
            policy,
            Some((k, u)),
        )
        .await;
    }

    let q_tags = if !policy.allow_mature {
        format!("{tag} rating:general")
    } else {
        tag.to_string()
    };
    let list_url = format!(
        "https://gelbooru.com/index.php?page=post&s=list&tags={}",
        url_encode(&q_tags)
    );
    let html = engine.get_text(&list_url).await?;
    let mut seen = HashSet::new();
    let mut ids = Vec::new();
    for cap in GELBOORU_ID_RE.captures_iter(&html) {
        let pid = cap[1].to_string();
        if seen.insert(pid.clone()) {
            ids.push(pid);
            if ids.len() >= limit {
                break;
            }
        }
    }

    let mut out = Vec::new();
    for pid in ids {
        let view_url = format!("https://gelbooru.com/index.php?page=post&s=view&id={pid}");
        let Ok(vhtml) = engine.get_text(&view_url).await else {
            continue;
        };
        let img_url = GELBOORU_IMG_RE.captures(&vhtml).map(|c| c[1].to_string());
        let rating = GELBOORU_RATING_RE
            .captures(&vhtml)
            .map(|c| normalize_rating_code(&c[1]))
            .unwrap_or("unknown");
        if !policy.probe_ratings.contains(rating) {
            continue;
        }
        let (width, height) = GELBOORU_SIZE_RE
            .captures(&vhtml)
            .map(|c| (c[1].parse::<u32>().ok(), c[2].parse::<u32>().ok()))
            .unwrap_or((None, None));
        out.push(CandidateItem {
            id: pid,
            source: "gelbooru".to_string(),
            rating: rating.to_string(),
            width,
            height,
            image_url: img_url,
            source_url: Some(view_url),
            ..Default::default()
        });
    }
    Ok(out)
}

pub async fn probe_moebooru(
    engine: &HttpEngine,
    source_name: &str,
    base_url: &str,
    tag: &str,
    since: Option<&str>,
    limit: usize,
    policy: &RatingPolicy,
) -> Result<Vec<CandidateItem>, String> {
    let q_tags = format!("{tag}{}", booru_rating_qualifier("moebooru", policy.tier));
    let per_page = limit.clamp(20, 100);
    let mut items = Vec::new();

    for page in 1..=10 {
        let url = format!(
            "{base_url}/post.json?tags={}&limit={per_page}&page={page}",
            url_encode(&q_tags)
        );
        let text = engine.get_text(&url).await?;
        let rows: Vec<Value> =
            serde_json::from_str(&text).map_err(|e| format!("{source_name} JSON error: {e}"))?;
        if rows.is_empty() {
            break;
        }
        let row_len = rows.len();
        let mut reached_cutoff = false;
        for post in rows {
            let Some(file_url) = post.get("file_url").and_then(Value::as_str) else {
                continue;
            };
            let rating = normalize_rating_code(
                post.get("rating")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown"),
            );
            if !policy.probe_ratings.contains(rating) {
                continue;
            }
            let iso_created = post
                .get("created_at")
                .and_then(Value::as_i64)
                .and_then(iso_from_epoch);
            if let (Some(s), Some(c)) = (since, iso_created.as_deref())
                && c.len() >= 10
                && &c[..10] < s
            {
                reached_cutoff = true;
                continue;
            }
            let id = post
                .get("id")
                .and_then(Value::as_i64)
                .map(|v| v.to_string())
                .unwrap_or_default();
            let src = post
                .get("source")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty());
            let (src_year, backfill) = check_backfill(since, src);
            let tag_vec: Vec<String> = post
                .get("tags")
                .and_then(Value::as_str)
                .map(|s| s.split_whitespace().map(str::to_string).collect())
                .unwrap_or_default();

            items.push(CandidateItem {
                id: id.clone(),
                source: source_name.to_string(),
                created_at: iso_created,
                rating: rating.to_string(),
                width: post.get("width").and_then(Value::as_u64).map(|v| v as u32),
                height: post.get("height").and_then(Value::as_u64).map(|v| v as u32),
                file_ext: post
                    .get("file_ext")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                file_size: post.get("file_size").and_then(Value::as_u64),
                md5: post.get("md5").and_then(Value::as_str).map(str::to_string),
                score: post.get("score").and_then(Value::as_i64),
                image_url: Some(file_url.to_string()),
                source_url: Some(
                    src.map(str::to_string)
                        .unwrap_or_else(|| format!("{base_url}/post/show/{id}")),
                ),
                parent_id: post
                    .get("parent_id")
                    .and_then(Value::as_i64)
                    .map(|v| v.to_string()),
                inferred_source_year: src_year,
                is_backfill_suspect: backfill,
                tags: tag_vec.clone(),
                raw_tags: tag_vec,
                ..Default::default()
            });
            if items.len() >= limit {
                return Ok(items);
            }
        }
        if reached_cutoff || row_len < per_page {
            break;
        }
    }
    Ok(items)
}

pub async fn probe_e621(
    engine: &HttpEngine,
    tag: &str,
    since: Option<&str>,
    limit: usize,
    policy: &RatingPolicy,
) -> Result<Vec<CandidateItem>, String> {
    let mut q_tags = format!("{tag}{}", booru_rating_qualifier("e621", policy.tier));
    if let Some(s) = since {
        q_tags.push_str(&format!(" date:>={s}"));
    }
    let per_page = limit.clamp(20, 200);
    let mut items = Vec::new();

    for page in 1..=10 {
        let url = format!(
            "https://e621.net/posts.json?tags={}&limit={per_page}&page={page}",
            url_encode(&q_tags)
        );
        let text = engine.get_text(&url).await?;
        let parsed: Value =
            serde_json::from_str(&text).map_err(|e| format!("e621 JSON error: {e}"))?;
        let Some(rows) = parsed.get("posts").and_then(Value::as_array) else {
            break;
        };
        if rows.is_empty() {
            break;
        }
        let row_len = rows.len();
        for post in rows {
            if post
                .pointer("/flags/deleted")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                continue;
            }
            let Some(file_url) = post.pointer("/file/url").and_then(Value::as_str) else {
                continue;
            };
            let ext = post
                .pointer("/file/ext")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_ascii_lowercase();
            if matches!(ext.as_str(), "mp4" | "webm" | "swf" | "zip") {
                continue;
            }
            let raw_r = post
                .get("rating")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let rating = if raw_r.eq_ignore_ascii_case("s") {
                "general"
            } else {
                normalize_rating_code(raw_r)
            };
            if !policy.probe_ratings.contains(rating) {
                continue;
            }
            let id = post
                .get("id")
                .and_then(Value::as_i64)
                .map(|v| v.to_string())
                .unwrap_or_default();
            let src = post
                .get("sources")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(Value::as_str);
            let (src_year, backfill) = check_backfill(since, src);
            let mut tag_vec = Vec::new();
            if let Some(tmap) = post.get("tags").and_then(Value::as_object) {
                for vals in tmap.values() {
                    if let Some(arr) = vals.as_array() {
                        tag_vec.extend(arr.iter().filter_map(Value::as_str).map(str::to_string));
                    }
                }
            }
            items.push(CandidateItem {
                id: id.clone(),
                source: "e621".to_string(),
                created_at: post
                    .get("created_at")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                rating: rating.to_string(),
                width: post
                    .pointer("/file/width")
                    .and_then(Value::as_u64)
                    .map(|v| v as u32),
                height: post
                    .pointer("/file/height")
                    .and_then(Value::as_u64)
                    .map(|v| v as u32),
                file_ext: (!ext.is_empty()).then_some(ext),
                file_size: post.pointer("/file/size").and_then(Value::as_u64),
                md5: post
                    .pointer("/file/md5")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                score: post.pointer("/score/total").and_then(Value::as_i64),
                fav_count: post.get("fav_count").and_then(Value::as_u64),
                image_url: Some(file_url.to_string()),
                source_url: Some(
                    src.map(str::to_string)
                        .unwrap_or_else(|| format!("https://e621.net/posts/{id}")),
                ),
                parent_id: post
                    .pointer("/relationships/parent_id")
                    .and_then(Value::as_i64)
                    .map(|v| v.to_string()),
                inferred_source_year: src_year,
                is_backfill_suspect: backfill,
                tags: tag_vec.clone(),
                raw_tags: tag_vec,
                ..Default::default()
            });
            if items.len() >= limit {
                return Ok(items);
            }
        }
        if row_len < per_page {
            break;
        }
    }
    Ok(items)
}

pub async fn probe_bluesky(
    engine: &HttpEngine,
    handle: &str,
    since: Option<&str>,
    limit: usize,
    policy: &RatingPolicy,
) -> Result<Vec<CandidateItem>, String> {
    let actor = handle.trim_start_matches('@');
    let mut items = Vec::new();
    let mut cursor: Option<String> = None;

    while items.len() < limit {
        let batch = (limit - items.len()).clamp(10, 100);
        let mut url = format!(
            "https://public.api.bsky.app/xrpc/app.bsky.feed.getAuthorFeed?actor={}&limit={batch}&filter=posts_with_media",
            url_encode(actor)
        );
        if let Some(c) = cursor.as_deref() {
            url.push_str(&format!("&cursor={}", url_encode(c)));
        }
        let text = engine.get_text(&url).await?;
        let parsed: Value =
            serde_json::from_str(&text).map_err(|e| format!("bluesky JSON error: {e}"))?;
        let Some(feed) = parsed.get("feed").and_then(Value::as_array) else {
            break;
        };
        if feed.is_empty() {
            break;
        }
        let mut reached_cutoff = false;
        for entry in feed {
            if entry.get("reason").is_some() {
                continue;
            }
            let Some(post) = entry.get("post") else {
                continue;
            };
            let cdate = post
                .pointer("/record/createdAt")
                .and_then(Value::as_str)
                .unwrap_or("");
            if let Some(s) = since
                && cdate.len() >= 10
                && &cdate[..10] < s
            {
                reached_cutoff = true;
                continue;
            }
            let mut rating = "unknown";
            if let Some(labels) = post.get("labels").and_then(Value::as_array) {
                for lb in labels {
                    let v = lb
                        .get("val")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_ascii_lowercase();
                    if v == "porn" {
                        rating = "explicit";
                        break;
                    }
                    if matches!(v.as_str(), "sexual" | "nudity" | "graphic-media") {
                        rating = "questionable";
                    }
                }
            }
            if !policy.probe_ratings.contains(rating) {
                continue;
            }
            let imgs = post
                .pointer("/embed/images")
                .and_then(Value::as_array)
                .or_else(|| {
                    post.pointer("/embed/media/images")
                        .and_then(Value::as_array)
                });
            let Some(imgs) = imgs else {
                continue;
            };
            let rkey = post
                .get("uri")
                .and_then(Value::as_str)
                .and_then(|u| u.rsplit('/').next())
                .unwrap_or("");
            let author_h = post
                .pointer("/author/handle")
                .and_then(Value::as_str)
                .unwrap_or(actor);

            for (idx, img) in imgs.iter().enumerate() {
                let Some(fullsize) = img.get("fullsize").and_then(Value::as_str) else {
                    continue;
                };
                let raw_w = img.pointer("/aspectRatio/width").and_then(Value::as_u64);
                let raw_h = img.pointer("/aspectRatio/height").and_then(Value::as_u64);
                let px_w = raw_w.filter(|v| *v >= 100).map(|v| v as u32);
                let px_h = raw_h.filter(|v| *v >= 100).map(|v| v as u32);
                items.push(CandidateItem {
                    id: format!("{rkey}_{idx}"),
                    source: "bluesky".to_string(),
                    created_at: (!cdate.is_empty()).then(|| cdate.to_string()),
                    rating: rating.to_string(),
                    width: px_w,
                    height: px_h,
                    image_url: Some(fullsize.to_string()),
                    source_url: Some(format!("https://bsky.app/profile/{author_h}/post/{rkey}")),
                    ..Default::default()
                });
                if items.len() >= limit {
                    return Ok(items);
                }
            }
        }
        cursor = parsed
            .get("cursor")
            .and_then(Value::as_str)
            .map(str::to_string);
        if cursor.is_none() || reached_cutoff {
            break;
        }
    }
    Ok(items)
}

pub async fn probe_mastodon(
    engine: &HttpEngine,
    target: &str,
    since: Option<&str>,
    limit: usize,
    policy: &RatingPolicy,
) -> Result<Vec<CandidateItem>, String> {
    let candidates: Vec<(&str, String)> = if let Some((h, a)) = target.split_once(':')
        && !target.starts_with("http")
    {
        let host = if h.to_ascii_lowercase().contains("pawoo") {
            "pawoo.net"
        } else {
            "baraag.net"
        };
        vec![(host, a.trim_start_matches('@').to_string())]
    } else {
        let acct = target.trim().trim_start_matches('@').to_string();
        vec![("baraag.net", acct.clone()), ("pawoo.net", acct)]
    };

    let mut found: Option<(&str, String, String)> = None;
    for (host, acct) in &candidates {
        let lk_url = format!(
            "https://{host}/api/v1/accounts/lookup?acct={}",
            url_encode(acct)
        );
        if let Ok(txt) = engine.get_text(&lk_url).await
            && let Ok(doc) = serde_json::from_str::<Value>(&txt)
            && let Some(id) = doc.get("id").and_then(Value::as_str)
        {
            found = Some((host, id.to_string(), acct.clone()));
            break;
        }
    }
    let Some((host, account_id, acct)) = found else {
        return Err(format!(
            "mastodon account not found on baraag/pawoo: {target}"
        ));
    };

    let mut items = Vec::new();
    let mut max_id: Option<String> = None;
    for _ in 0..10 {
        let mut url = format!(
            "https://{host}/api/v1/accounts/{account_id}/statuses?only_media=true&exclude_reblogs=true&limit=40"
        );
        if let Some(m) = max_id.as_deref() {
            url.push_str(&format!("&max_id={}", url_encode(m)));
        }
        let txt = engine.get_text(&url).await?;
        let rows: Vec<Value> =
            serde_json::from_str(&txt).map_err(|e| format!("mastodon JSON error: {e}"))?;
        if rows.is_empty() {
            break;
        }
        let row_len = rows.len();
        let mut reached_cutoff = false;
        for st in &rows {
            let cdate = st.get("created_at").and_then(Value::as_str).unwrap_or("");
            if let Some(s) = since
                && cdate.len() >= 10
                && &cdate[..10] < s
            {
                reached_cutoff = true;
                continue;
            }
            let sensitive = st
                .get("sensitive")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let rating = if sensitive { "questionable" } else { "unknown" };
            if !policy.probe_ratings.contains(rating) {
                continue;
            }
            let stid = st.get("id").and_then(Value::as_str).unwrap_or("");
            let Some(atts) = st.get("media_attachments").and_then(Value::as_array) else {
                continue;
            };
            for (idx, att) in atts.iter().enumerate() {
                if att.get("type").and_then(Value::as_str) != Some("image") {
                    continue;
                }
                let Some(img_url) = att.get("url").and_then(Value::as_str) else {
                    continue;
                };
                items.push(CandidateItem {
                    id: format!("{stid}_{idx}"),
                    source: "mastodon".to_string(),
                    created_at: (!cdate.is_empty()).then(|| cdate.to_string()),
                    rating: rating.to_string(),
                    width: att
                        .pointer("/meta/original/width")
                        .and_then(Value::as_u64)
                        .map(|v| v as u32),
                    height: att
                        .pointer("/meta/original/height")
                        .and_then(Value::as_u64)
                        .map(|v| v as u32),
                    image_url: Some(img_url.to_string()),
                    source_url: Some(
                        st.get("url")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .unwrap_or_else(|| format!("https://{host}/@{acct}/{stid}")),
                    ),
                    parent_id: (atts.len() > 1).then(|| stid.to_string()),
                    ..Default::default()
                });
                if items.len() >= limit {
                    return Ok(items);
                }
            }
        }
        max_id = rows
            .last()
            .and_then(|r| r.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string);
        if max_id.is_none() || reached_cutoff || row_len < 20 {
            break;
        }
    }
    Ok(items)
}

pub async fn probe_twitter(
    handle: &str,
    since: Option<&str>,
    limit: usize,
) -> Result<Vec<CandidateItem>, String> {
    let tw_bin = resolve_bin("twitter").ok_or_else(|| "twitter CLI not found".to_string())?;
    let clean = handle.trim_start_matches('@');
    let mut cmd = Command::new(tw_bin);
    cmd.args([
        "search",
        "--from",
        clean,
        "--has",
        "images",
        "--exclude",
        "retweets",
        "-t",
        "latest",
        "-n",
        &limit.to_string(),
        "--json",
    ]);
    if let Some(s) = since {
        cmd.args(["--since", s]);
    }
    let out = cmd
        .output()
        .await
        .map_err(|e| format!("failed spawning twitter CLI: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "twitter CLI failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let payload: Value =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("twitter JSON error: {e}"))?;
    let mut items = Vec::new();
    if let Some(tweets) = payload.get("data").and_then(Value::as_array) {
        for tw in tweets {
            let tid = tw.get("id").and_then(Value::as_str).unwrap_or("");
            let Some(media_arr) = tw.get("media").and_then(Value::as_array) else {
                continue;
            };
            for (idx, m) in media_arr.iter().enumerate() {
                if m.get("type").and_then(Value::as_str) != Some("photo") {
                    continue;
                }
                let Some(u) = m.get("url").and_then(Value::as_str) else {
                    continue;
                };
                items.push(CandidateItem {
                    id: format!("{tid}_{idx}"),
                    source: "twitter".to_string(),
                    created_at: tw
                        .get("createdAtISO")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    rating: "unknown".to_string(),
                    width: m.get("width").and_then(Value::as_u64).map(|v| v as u32),
                    height: m.get("height").and_then(Value::as_u64).map(|v| v as u32),
                    image_url: Some(twitter_orig_url(u)),
                    source_url: Some(format!("https://x.com/{clean}/status/{tid}")),
                    score: tw.pointer("/metrics/likes").and_then(Value::as_i64),
                    ..Default::default()
                });
            }
        }
    }
    Ok(items)
}

pub async fn probe_pixiv(
    user_id: &str,
    since: Option<&str>,
    limit: usize,
    policy: &RatingPolicy,
) -> Result<Vec<CandidateItem>, String> {
    let gdl = resolve_bin("gallery-dl").ok_or_else(|| "gallery-dl not found".to_string())?;
    let url = format!("https://www.pixiv.net/users/{}/artworks", user_id.trim());
    let out = Command::new(gdl)
        .args(["--dump-json", "--range", &format!("1-{limit}"), &url])
        .output()
        .await
        .map_err(|e| format!("failed spawning gallery-dl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "gallery-dl pixiv failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let raw: Vec<Value> =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("pixiv JSON error: {e}"))?;
    let mut items = Vec::new();
    for entry in raw {
        let Some(arr) = entry.as_array() else {
            continue;
        };
        if arr.len() < 3 || arr[0].as_i64() != Some(3) {
            continue;
        }
        let Some(img_url) = arr[1].as_str() else {
            continue;
        };
        let Some(meta) = arr[2].as_object() else {
            continue;
        };
        let cdate = meta
            .get("create_date")
            .or_else(|| meta.get("date"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if let Some(s) = since
            && cdate.len() >= 10
            && &cdate[..10] < s
        {
            continue;
        }
        let x_restrict = meta.get("x_restrict").and_then(Value::as_i64).unwrap_or(0);
        let rating = if x_restrict >= 1 {
            "explicit"
        } else {
            "general"
        };
        if !policy.probe_ratings.contains(rating) {
            continue;
        }
        let ai_type = meta.get("ai_type").and_then(Value::as_i64).unwrap_or(0);
        let pid = meta
            .get("id")
            .and_then(Value::as_i64)
            .map(|v| v.to_string())
            .unwrap_or_default();
        let num = meta.get("num").and_then(Value::as_i64).unwrap_or(0);
        let page_count = meta.get("page_count").and_then(Value::as_i64).unwrap_or(1);
        let tag_vec: Vec<String> = meta
            .get("tags")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        items.push(CandidateItem {
            id: format!("{pid}_p{num}"),
            source: "pixiv".to_string(),
            created_at: (!cdate.is_empty()).then(|| cdate.to_string()),
            rating: rating.to_string(),
            width: meta.get("width").and_then(Value::as_u64).map(|v| v as u32),
            height: meta.get("height").and_then(Value::as_u64).map(|v| v as u32),
            image_url: Some(img_url.to_string()),
            source_url: Some(format!("https://www.pixiv.net/artworks/{pid}")),
            parent_id: (page_count > 1).then_some(pid),
            title: meta
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_string),
            is_ai: ai_type == 2,
            tags: tag_vec.clone(),
            raw_tags: tag_vec,
            ..Default::default()
        });
    }
    Ok(items)
}

pub async fn probe_artstation(
    handle: &str,
    since: Option<&str>,
    limit: usize,
    policy: &RatingPolicy,
) -> Result<Vec<CandidateItem>, String> {
    let gdl = resolve_bin("gallery-dl").ok_or_else(|| "gallery-dl not found".to_string())?;
    let slug = handle
        .trim()
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(handle);
    let url = format!("https://www.artstation.com/{slug}");
    let out = Command::new(gdl)
        .args(["--dump-json", "--range", &format!("1-{limit}"), &url])
        .output()
        .await
        .map_err(|e| format!("failed spawning gallery-dl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "gallery-dl artstation failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let raw: Vec<Value> =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("artstation JSON error: {e}"))?;
    let mut items = Vec::new();
    for entry in raw {
        let Some(arr) = entry.as_array() else {
            continue;
        };
        if arr.len() < 3 || arr[0].as_i64() != Some(3) {
            continue;
        }
        let Some(img_url) = arr[1].as_str() else {
            continue;
        };
        let meta = &arr[2];
        if meta.pointer("/asset/asset_type").and_then(Value::as_str) != Some("image")
            || !meta
                .pointer("/asset/has_image")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        {
            continue;
        }
        let cdate = meta
            .get("created_at")
            .or_else(|| meta.get("date"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if let Some(s) = since
            && cdate.len() >= 10
            && &cdate[..10] < s
        {
            continue;
        }
        let adult = meta
            .get("adult_content")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || meta
                .get("admin_adult_content")
                .and_then(Value::as_bool)
                .unwrap_or(false);
        let rating = if adult { "explicit" } else { "general" };
        if !policy.probe_ratings.contains(rating) {
            continue;
        }
        let hid = meta.get("hash_id").and_then(Value::as_str).unwrap_or("");
        let aid = meta
            .pointer("/asset/id")
            .and_then(Value::as_i64)
            .map(|v| v.to_string())
            .unwrap_or_else(|| items.len().to_string());
        let acount = meta
            .get("assets_count")
            .and_then(Value::as_i64)
            .unwrap_or(1);

        items.push(CandidateItem {
            id: if hid.is_empty() {
                aid
            } else {
                format!("{hid}_{aid}")
            },
            source: "artstation".to_string(),
            created_at: (!cdate.is_empty()).then(|| cdate.to_string()),
            rating: rating.to_string(),
            width: meta
                .pointer("/asset/width")
                .and_then(Value::as_u64)
                .map(|v| v as u32),
            height: meta
                .pointer("/asset/height")
                .and_then(Value::as_u64)
                .map(|v| v as u32),
            file_ext: meta
                .get("extension")
                .and_then(Value::as_str)
                .map(str::to_string),
            image_url: Some(img_url.to_string()),
            source_url: Some(
                meta.get("permalink")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("https://www.artstation.com/artwork/{hid}")),
            ),
            parent_id: (acount > 1 && !hid.is_empty()).then(|| hid.to_string()),
            title: meta
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_string),
            ..Default::default()
        });
        if items.len() >= limit {
            break;
        }
    }
    Ok(items)
}

async fn enrich_ehentai_gids(
    engine: &HttpEngine,
    pairs: &[(u64, String)],
) -> Result<Vec<CandidateItem>, String> {
    let mut out = Vec::new();
    for chunk in pairs.chunks(25) {
        let gidlist: Vec<Value> = chunk.iter().map(|(g, t)| json!([*g, t])).collect();
        let res = engine
            .post_json(
                "https://api.e-hentai.org/api.php",
                &json!({ "method": "gdata", "gidlist": gidlist, "namespace": 1 }),
            )
            .await?;
        if let Some(gmeta) = res.get("gmetadata").and_then(Value::as_array) {
            for g in gmeta {
                if g.get("error").is_some() {
                    continue;
                }
                let gid = g
                    .get("gid")
                    .and_then(Value::as_u64)
                    .map(|v| v.to_string())
                    .unwrap_or_default();
                let tok = g.get("token").and_then(Value::as_str).unwrap_or("");
                let fcount = g
                    .get("filecount")
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse::<u32>().ok())
                    .or_else(|| g.get("filecount").and_then(Value::as_u64).map(|v| v as u32))
                    .unwrap_or(0);
                let fsize = g.get("filesize").and_then(Value::as_u64).unwrap_or(0);
                let posted = g
                    .get("posted")
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse::<i64>().ok())
                    .or_else(|| g.get("posted").and_then(Value::as_i64));
                let tag_vec: Vec<String> = g
                    .get("tags")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();

                out.push(CandidateItem {
                    id: gid.clone(),
                    source: "ehentai".to_string(),
                    created_at: posted.and_then(iso_from_epoch),
                    rating: "explicit".to_string(),
                    title: g.get("title").and_then(Value::as_str).map(str::to_string),
                    category: g
                        .get("category")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    filecount: Some(fcount),
                    file_size: Some(fsize),
                    avg_kb_per_file: (fcount > 0).then(|| {
                        ((fsize as f64 / 1024.0 / f64::from(fcount)) * 10.0).round() / 10.0
                    }),
                    source_url: Some(format!("https://e-hentai.org/g/{gid}/{tok}/")),
                    tags: tag_vec.clone(),
                    raw_tags: tag_vec,
                    ..Default::default()
                });
            }
        }
    }
    Ok(out)
}

pub async fn probe_ehentai(
    engine: &HttpEngine,
    target: &str,
    limit: usize,
) -> Result<Vec<CandidateItem>, String> {
    if let Some(cap) = EH_GALLERY_RE.captures(target) {
        let gid = cap[1].parse::<u64>().map_err(|e| e.to_string())?;
        return enrich_ehentai_gids(engine, &[(gid, cap[2].to_string())]).await;
    }
    let gdl = resolve_bin("gallery-dl").ok_or_else(|| "gallery-dl not found".to_string())?;
    let search_url = if target.starts_with("http://") || target.starts_with("https://") {
        target.to_string()
    } else if target.contains(':') {
        format!("https://e-hentai.org/tag/{}", url_encode(target))
    } else {
        format!("https://e-hentai.org/?f_search={}", url_encode(target))
    };
    let out = Command::new(gdl)
        .args(["--dump-json", "--range", &format!("1-{limit}"), &search_url])
        .output()
        .await
        .map_err(|e| format!("failed spawning gallery-dl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "gallery-dl ehentai failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let raw: Vec<Value> =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("ehentai JSON error: {e}"))?;
    let mut pairs = Vec::new();
    let mut seen = HashSet::new();
    for entry in raw {
        if let Some(arr) = entry.as_array()
            && arr.len() >= 2
            && let Some(surl) = arr[1].as_str()
            && let Some(cap) = EH_GALLERY_RE.captures(surl)
            && let Ok(gid) = cap[1].parse::<u64>()
            && seen.insert(gid)
        {
            pairs.push((gid, cap[2].to_string()));
            if pairs.len() >= limit {
                break;
            }
        }
    }
    enrich_ehentai_gids(engine, &pairs).await
}

pub async fn resolve_artist(
    engine: &HttpEngine,
    query: &str,
    allow_mature: bool,
) -> Result<ArtistProfile, String> {
    let raw_q = query.trim();
    let clean_q = raw_q
        .trim_start_matches('@')
        .trim_end_matches('+')
        .trim()
        .replace(' ', "_")
        .to_ascii_lowercase();

    let mut canonical_tag: Option<String> = None;
    let mut alias_from: Option<String> = None;
    let mut other_names: Vec<String> = Vec::new();
    let mut artist_urls: Vec<String> = Vec::new();

    if !clean_q.is_empty() && !clean_q.starts_with("http") {
        let al_url = format!(
            "https://danbooru.donmai.us/tag_aliases.json?search[antecedent_name]={}&search[status]=active",
            url_encode(&clean_q)
        );
        if let Ok(txt) = engine.get_text(&al_url).await
            && let Ok(arr) = serde_json::from_str::<Vec<Value>>(&txt)
            && let Some(cons) = arr
                .first()
                .and_then(|a| a.get("consequent_name"))
                .and_then(Value::as_str)
        {
            alias_from = Some(clean_q.clone());
            canonical_tag = Some(cons.to_string());
        }
    }

    let lookup_term = canonical_tag.as_deref().unwrap_or(raw_q);
    let art_url = format!(
        "https://danbooru.donmai.us/artists.json?search[any_name_or_url_matches]={}&only=id,name,other_names,urls&limit=5",
        url_encode(lookup_term)
    );
    if let Ok(txt) = engine.get_text(&art_url).await
        && let Ok(artists) = serde_json::from_str::<Vec<Value>>(&txt)
        && !artists.is_empty()
    {
        let target_match = canonical_tag.as_deref().unwrap_or(&clean_q);
        let picked = artists
            .iter()
            .find(|a| {
                a.get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|n| n.eq_ignore_ascii_case(target_match))
            })
            .unwrap_or(&artists[0]);
        if let Some(n) = picked.get("name").and_then(Value::as_str) {
            canonical_tag = Some(n.to_string());
        }
        other_names = picked
            .get("other_names")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        artist_urls = picked
            .get("urls")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter(|u| u.get("is_active").and_then(Value::as_bool).unwrap_or(true))
                    .filter_map(|u| u.get("url").and_then(Value::as_str).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
    }

    if canonical_tag.is_none() && !clean_q.is_empty() && !clean_q.starts_with("http") {
        let mut patterns = vec![format!("*{clean_q}*")];
        if clean_q.len() >= 6 {
            patterns.push(format!("*{}*", &clean_q[1..]));
        }
        for pat in patterns {
            let tg_url = format!(
                "https://danbooru.donmai.us/tags.json?search[name_matches]={}&search[category]=1&search[order]=count&limit=5",
                url_encode(&pat)
            );
            if let Ok(txt) = engine.get_text(&tg_url).await
                && let Ok(tags) = serde_json::from_str::<Vec<Value>>(&txt)
                && let Some(first_name) = tags
                    .first()
                    .and_then(|t| t.get("name"))
                    .and_then(Value::as_str)
            {
                canonical_tag = Some(first_name.to_string());
                let art2_url = format!(
                    "https://danbooru.donmai.us/artists.json?search[name]={}&only=id,name,other_names,urls&limit=1",
                    url_encode(first_name)
                );
                if let Ok(t2) = engine.get_text(&art2_url).await
                    && let Ok(a2) = serde_json::from_str::<Vec<Value>>(&t2)
                    && let Some(p2) = a2.first()
                {
                    other_names = p2
                        .get("other_names")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                    artist_urls = p2
                        .get("urls")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter(|u| {
                                    u.get("is_active").and_then(Value::as_bool).unwrap_or(true)
                                })
                                .filter_map(|u| {
                                    u.get("url").and_then(Value::as_str).map(str::to_string)
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                }
                break;
            }
        }
    }

    let (platforms, manual_urls) =
        extract_platform_handles(&artist_urls, canonical_tag.as_deref().or(Some(&clean_q)));

    let (mut db_total, mut db_sfw) = (0u64, 0u64);
    if let Some(ctag) = canonical_tag.as_deref() {
        let u_all = format!(
            "https://danbooru.donmai.us/counts/posts.json?tags={}",
            url_encode(ctag)
        );
        let u_sfw = format!(
            "https://danbooru.donmai.us/counts/posts.json?tags={}",
            url_encode(&format!("{ctag} rating:g,s"))
        );
        let (r_all, r_sfw) = tokio::join!(engine.get_text(&u_all), engine.get_text(&u_sfw));
        if let Ok(t) = r_all
            && let Ok(v) = serde_json::from_str::<Value>(&t)
        {
            db_total = v
                .pointer("/counts/posts")
                .and_then(Value::as_u64)
                .unwrap_or(0);
        }
        if let Ok(t) = r_sfw
            && let Ok(v) = serde_json::from_str::<Value>(&t)
        {
            db_sfw = v
                .pointer("/counts/posts")
                .and_then(Value::as_u64)
                .unwrap_or(0);
        }
    }

    let mut gb_total = None;
    let (gb_key, gb_uid) = gelbooru_credentials();
    let gb_lookup = canonical_tag.as_deref().unwrap_or(&clean_q);
    if let (Some(k), Some(u)) = (gb_key, gb_uid)
        && !gb_lookup.is_empty()
        && !gb_lookup.starts_with("http")
    {
        let gb_url = format!(
            "https://gelbooru.com/index.php?page=dapi&s=tag&q=index&json=1&name={}&api_key={}&user_id={}",
            url_encode(gb_lookup),
            url_encode(&k),
            url_encode(&u)
        );
        if let Ok(txt) = engine.get_text(&gb_url).await
            && let Ok(v) = serde_json::from_str::<Value>(&txt)
            && let Some(cnt) = v
                .get("tag")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(|t| t.get("count"))
                .and_then(Value::as_u64)
        {
            gb_total = Some(cnt);
        }
    }

    let mut resolved_targets = BTreeMap::new();
    let tag_target = canonical_tag.clone().unwrap_or_else(|| {
        if clean_q.starts_with("http") {
            String::new()
        } else {
            clean_q
        }
    });
    if !tag_target.is_empty() {
        for src in [
            "danbooru",
            "gelbooru",
            "safebooru_org",
            "tbib",
            "yandere",
            "konachan",
            "e621",
        ] {
            resolved_targets.insert(src.to_string(), tag_target.clone());
        }
        if allow_mature {
            resolved_targets.insert(
                "ehentai".to_string(),
                format!("artist:{}", tag_target.replace('_', " ")),
            );
        }
    }
    for (pk, pv) in platforms {
        resolved_targets.insert(pk, pv);
    }

    Ok(ArtistProfile {
        schema: "artfetch.artist_profile/v1".to_string(),
        content_rating: if allow_mature {
            "mature".to_string()
        } else {
            "sfw".to_string()
        },
        query: raw_q.to_string(),
        canonical_tag,
        alias_from,
        other_names,
        counts: ArtistCounts {
            danbooru_total: db_total,
            danbooru_sfw: db_sfw,
            danbooru_mature: db_total.saturating_sub(db_sfw),
            danbooru_sfw_ratio: (db_total > 0)
                .then(|| ((db_sfw as f64 / db_total as f64) * 10_000.0).round() / 10_000.0),
            gelbooru_total: gb_total,
        },
        resolved_targets,
        manual_or_browser_urls: manual_urls,
    })
}

pub async fn probe_single_source(
    engine: &HttpEngine,
    src: &str,
    target: &str,
    since: Option<&str>,
    limit: usize,
    policy: &RatingPolicy,
) -> Result<Vec<CandidateItem>, String> {
    match src {
        "danbooru" => probe_danbooru(engine, target, since, limit, policy).await,
        "gelbooru" => probe_gelbooru(engine, target, since, limit, policy).await,
        "safebooru_org" => {
            probe_dapi(
                engine,
                "safebooru_org",
                "https://safebooru.org",
                target,
                since,
                limit,
                policy,
                None,
            )
            .await
        }
        "tbib" => {
            probe_dapi(
                engine,
                "tbib",
                "https://tbib.org",
                target,
                since,
                limit,
                policy,
                None,
            )
            .await
        }
        "yandere" => {
            probe_moebooru(
                engine,
                "yandere",
                "https://yande.re",
                target,
                since,
                limit,
                policy,
            )
            .await
        }
        "konachan" => {
            probe_moebooru(
                engine,
                "konachan",
                "https://konachan.com",
                target,
                since,
                limit,
                policy,
            )
            .await
        }
        "e621" => probe_e621(engine, target, since, limit, policy).await,
        "bluesky" => probe_bluesky(engine, target, since, limit, policy).await,
        "mastodon" => probe_mastodon(engine, target, since, limit, policy).await,
        "twitter" => probe_twitter(target, since, limit).await,
        "pixiv" => probe_pixiv(target, since, limit, policy).await,
        "artstation" => probe_artstation(target, since, limit, policy).await,
        "ehentai" => {
            if !policy.allow_mature {
                return Err(
                    "ehentai requires --allow-mature or a non-sfw --rating-tier".to_string()
                );
            }
            probe_ehentai(engine, target, limit).await
        }
        other => Err(format!("unsupported source '{other}'")),
    }
}
