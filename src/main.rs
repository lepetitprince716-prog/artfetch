use artfetch::filter::{
    FilterOptions, annotate_and_filter, dedupe_cross_platform_items, dedupe_parent_items,
};
use artfetch::http::{HttpEngine, HttpLimits};
use artfetch::model::{ALL_SOURCES, ArtistProfile, CandidateItem, RatingPolicy};
use artfetch::safe_path::FileName;
use artfetch::sources::{check_all_status, probe_single_source, resolve_artist, resolve_bin};
use artfetch::stage::{StageConfig, execute_stage, merge_manifest_atomic};
use clap::{Args, Parser, Subcommand};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::Semaphore;

#[derive(Parser)]
#[command(
    name = "artfetch",
    version,
    about = "Fast multi-source illustration discovery, completion-gated fetcher, and perceptual dataset stager"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Commands,
}

#[derive(Args, Clone)]
struct SharedFilterFlags {
    #[arg(long, help = "Rating tier: sfw, ecchi, mature_only, or all")]
    rating_tier: Option<String>,
    #[arg(
        long,
        visible_alias = "allow-nsfw",
        help = "Allow mature/explicit items"
    )]
    allow_mature: bool,
    #[arg(
        long,
        help = "Keep finished illustrations only (exclude AI, sketches, monochrome, comics)"
    )]
    training_ready: bool,
    #[arg(long, help = "Exclude mosaic/bar censored works")]
    exclude_censored: bool,
    #[arg(long, default_value_t = 1024)]
    min_long_edge: u32,
    #[arg(long, default_value_t = 640)]
    min_short_edge: u32,
}

#[derive(Subcommand)]
enum Commands {
    #[command(about = "Concurrently check availability and auth state across all 13 channels")]
    Status,

    #[command(about = "Normalize artist handle/tag, cross-platform URLs, and post counts")]
    Resolve {
        query: String,
        #[arg(long, visible_alias = "allow-nsfw")]
        allow_mature: bool,
        #[arg(long)]
        out: Option<PathBuf>,
    },

    #[command(about = "Concurrently inspect single- or multi-channel metadata (read-only)")]
    Probe {
        #[arg(long, help = "Channel name, comma-separated list, or 'auto'")]
        source: String,
        #[arg(long)]
        target: String,
        #[arg(long)]
        profile: Option<PathBuf>,
        #[arg(long)]
        since: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        dedupe_parent: bool,
        #[arg(long)]
        manifest: Option<PathBuf>,
        #[command(flatten)]
        filters: SharedFilterFlags,
    },

    #[command(
        about = "Plan or concurrently download images with per-host rate limiting and atomic writes"
    )]
    Fetch {
        #[arg(long, help = "Channel name, comma-separated list, or 'auto'")]
        source: String,
        #[arg(long)]
        target: String,
        #[arg(long)]
        profile: Option<PathBuf>,
        #[arg(long)]
        since: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        dedupe_parent: bool,
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value_t = 6)]
        concurrency: usize,
        #[arg(long, default_value_t = 2)]
        per_host: usize,
        #[arg(
            long,
            default_value_t = 256,
            value_parser = clap::value_parser!(u64).range(1..=1 << 20),
            help = "Refuse any single download larger than this many MiB once decoded"
        )]
        max_download_mib: u64,
        #[arg(long)]
        run: bool,
        #[command(flatten)]
        filters: SharedFilterFlags,
    },

    #[command(
        about = "Multi-core parallel perceptual deduplication (dHash) and 1536px 4:4:4 JPEG staging"
    )]
    Stage {
        #[arg(long, num_args = 1.., required = true)]
        in_dir: Vec<PathBuf>,
        #[arg(long)]
        out_dir: PathBuf,
        #[arg(long)]
        rating_tier: Option<String>,
        #[arg(long, visible_alias = "allow-nsfw")]
        allow_mature: bool,
        #[arg(long)]
        exclude_censored: bool,
        #[arg(long)]
        allow_unpolished: bool,
        #[arg(long, default_value_t = 1024)]
        min_long_edge: u32,
        #[arg(long, default_value_t = 640)]
        min_short_edge: u32,
        #[arg(long, default_value_t = 1536)]
        max_edge: u32,
        #[arg(long, default_value_t = 93)]
        jpeg_quality: u8,
        #[arg(long, default_value_t = 6)]
        dhash_max_dist: u32,
        #[arg(long, default_value = "img")]
        prefix: String,
        #[arg(long)]
        trigger: Option<String>,
        #[arg(long, help = "Optional external vision tagger batch JSON file")]
        tagger_json: Option<PathBuf>,
        #[arg(
            long,
            help = "Allow SFW staging without --tagger-json (marks output as sfw_unverified)"
        )]
        allow_unverified_sfw: bool,
        #[arg(long)]
        run: bool,
    },
}

async fn resolve_channel_pairs(
    engine: &HttpEngine,
    source_arg: &str,
    target: &str,
    profile_path: Option<&Path>,
    policy: &RatingPolicy,
) -> Result<Vec<(String, String)>, String> {
    if source_arg.trim().eq_ignore_ascii_case("auto") {
        let prof: ArtistProfile = if let Some(p) = profile_path {
            let txt = std::fs::read_to_string(p)
                .map_err(|e| format!("failed reading profile {}: {e}", p.display()))?;
            serde_json::from_str(&txt).map_err(|e| format!("invalid profile JSON: {e}"))?
        } else {
            resolve_artist(engine, target, policy.allow_mature).await?
        };
        let auto_order = [
            "danbooru",
            "gelbooru",
            "yandere",
            "konachan",
            "e621",
            "twitter",
            "bluesky",
            "pixiv",
            "artstation",
            "mastodon",
        ];
        let pairs: Vec<(String, String)> = auto_order
            .iter()
            .filter_map(|s| {
                prof.resolved_targets
                    .get(*s)
                    .map(|t| ((*s).to_string(), t.clone()))
            })
            .collect();
        if pairs.is_empty() {
            return Err(format!(
                "--source auto resolved zero targets for '{target}'"
            ));
        }
        return Ok(pairs);
    }

    let mut pairs = Vec::new();
    for raw in source_arg.split(',') {
        let s = raw.trim();
        if s.is_empty() {
            continue;
        }
        if !ALL_SOURCES.contains(&s) {
            return Err(format!("unsupported source '{s}'"));
        }
        pairs.push((s.to_string(), target.to_string()));
    }
    if pairs.is_empty() {
        return Err("no valid --source specified".to_string());
    }
    Ok(pairs)
}

#[allow(clippy::too_many_arguments)]
async fn run_multi_probe(
    engine: &HttpEngine,
    source_arg: &str,
    target: &str,
    profile_path: Option<&Path>,
    since: Option<&str>,
    limit: usize,
    dedupe_parent: bool,
    filters: &SharedFilterFlags,
) -> Result<(Vec<CandidateItem>, BTreeMap<String, String>, RatingPolicy), String> {
    let policy = RatingPolicy::resolve(filters.rating_tier.as_deref(), filters.allow_mature)?;
    let pairs = resolve_channel_pairs(engine, source_arg, target, profile_path, &policy).await?;
    let is_multi = pairs.len() > 1;

    let futs = pairs.into_iter().map(|(src, tgt)| {
        let eng = engine.clone();
        let pol = policy.clone();
        let since_owned = since.map(str::to_string);
        async move {
            let res =
                probe_single_source(&eng, &src, &tgt, since_owned.as_deref(), limit, &pol).await;
            (src, res)
        }
    });

    let results = futures_util::future::join_all(futs).await;
    let mut raw_items = Vec::new();
    let mut channel_errors = BTreeMap::new();
    for (src, res) in results {
        match res {
            Ok(v) => raw_items.extend(v),
            Err(e) => {
                if !is_multi {
                    return Err(e);
                }
                channel_errors.insert(src, e);
            }
        }
    }

    let fopts = FilterOptions {
        training_ready: filters.training_ready,
        exclude_censored: filters.exclude_censored,
        min_long_edge: filters.min_long_edge,
        min_short_edge: filters.min_short_edge,
        ..Default::default()
    };
    let filtered = annotate_and_filter(raw_items, &policy, &fopts);
    let deduped = if is_multi {
        dedupe_cross_platform_items(filtered)
    } else if dedupe_parent {
        dedupe_parent_items(filtered)
    } else {
        filtered
    };

    Ok((deduped, channel_errors, policy))
}

fn download_referer(source: &str, source_url: Option<&str>) -> Option<String> {
    match source {
        "pixiv" => Some("https://www.pixiv.net/".to_string()),
        "danbooru" => Some("https://danbooru.donmai.us/".to_string()),
        "gelbooru" => Some("https://gelbooru.com/".to_string()),
        "yandere" => Some("https://yande.re/".to_string()),
        "konachan" => Some("https://konachan.com/".to_string()),
        "e621" => Some("https://e621.net/".to_string()),
        "twitter" | "bluesky" | "mastodon" => None,
        _ => source_url.map(str::to_string),
    }
}

/// `.ext` from the URL path when it is 1-5 ASCII letters or digits, else `.jpg`.
fn image_extension(url: &str) -> String {
    url.split(['?', '#'])
        .next()
        .and_then(|p| Path::new(p).extension())
        .and_then(|s| s.to_str())
        .filter(|e| (1..=5).contains(&e.len()) && e.bytes().all(|b| b.is_ascii_alphanumeric()))
        .map(|e| format!(".{}", e.to_ascii_lowercase()))
        .unwrap_or_else(|| ".jpg".to_string())
}

async fn fetch_ehentai_isolated(
    items: &[CandidateItem],
    out_dir: &Path,
    limit: usize,
) -> Result<Vec<Value>, String> {
    let gdl = resolve_bin("gallery-dl").ok_or_else(|| "gallery-dl not found".to_string())?;
    let eh_dir = out_dir.join("ehentai");
    tokio::fs::create_dir_all(&eh_dir)
        .await
        .map_err(|e| format!("mkdir {} failed: {e}", eh_dir.display()))?;

    let mut known: HashSet<String> = std::fs::read_dir(&eh_dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .collect();
    let mut remaining = limit.max(1);

    for it in items {
        let Some(gurl) = it.source_url.as_deref() else {
            continue;
        };
        if remaining == 0 {
            break;
        }
        let out = Command::new(&gdl)
            .args([
                "-D",
                &eh_dir.display().to_string(),
                "--write-metadata",
                "--retries",
                "3",
                "--range",
                &format!("1-{remaining}"),
                gurl,
            ])
            .output()
            .await
            .map_err(|e| format!("gallery-dl spawn error: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "gallery-dl ehentai failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let now_files: HashSet<String> = std::fs::read_dir(&eh_dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|e| {
                let p = e.ok()?.path();
                let ext = p.extension()?.to_str()?.to_ascii_lowercase();
                if matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "webp" | "gif") {
                    p.file_name()?.to_str().map(str::to_string)
                } else {
                    None
                }
            })
            .collect();
        let added = now_files.difference(&known).count();
        known.extend(now_files);
        remaining = remaining.saturating_sub(added);
    }

    let mut recs = Vec::new();
    let mut names: Vec<String> = known.into_iter().collect();
    names.sort();
    for name in names {
        let stem = Path::new(&name)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(&name);
        recs.push(json!({
            "id": stem,
            "source": "ehentai",
            "rating": "explicit",
            "file": format!("ehentai/{name}"),
            "content_rating": "mature",
        }));
    }
    Ok(recs)
}

#[tokio::main]
async fn main() {
    if let Err(err) = real_main().await {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

async fn real_main() -> Result<(), String> {
    let cli = Cli::parse();
    match cli.cmd {
        Commands::Status => {
            let engine = HttpEngine::new(2, 12)?;
            let st = check_all_status(&engine).await;
            println!("{}", serde_json::to_string_pretty(&st).unwrap());
        }
        Commands::Resolve {
            query,
            allow_mature,
            out,
        } => {
            let engine = HttpEngine::new(2, 15)?;
            let prof = resolve_artist(&engine, &query, allow_mature).await?;
            let rendered = serde_json::to_string_pretty(&prof).unwrap();
            if let Some(out_path) = out {
                if let Some(parent) = out_path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                std::fs::write(&out_path, format!("{rendered}\n")).map_err(|e| e.to_string())?;
            }
            println!("{rendered}");
        }
        Commands::Probe {
            source,
            target,
            profile,
            since,
            limit,
            dedupe_parent,
            manifest,
            filters,
        } => {
            let engine = HttpEngine::new(2, 20)?;
            let (items, channel_errors, policy) = run_multi_probe(
                &engine,
                &source,
                &target,
                profile.as_deref(),
                since.as_deref(),
                limit,
                dedupe_parent,
                &filters,
            )
            .await?;

            if let Some(mpath) = manifest {
                if let Some(parent) = mpath.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                let mut lines = String::new();
                for it in &items {
                    lines.push_str(&serde_json::to_string(it).unwrap());
                    lines.push('\n');
                }
                std::fs::write(&mpath, lines).map_err(|e| e.to_string())?;
            }

            let mut out = json!({
                "count": items.len(),
                "rating_tier": policy.tier.as_str(),
                "items": items,
            });
            if !channel_errors.is_empty() {
                out["channel_errors"] = json!(channel_errors);
            }
            println!("{}", serde_json::to_string_pretty(&out).unwrap());
        }
        Commands::Fetch {
            source,
            target,
            profile,
            since,
            limit,
            dedupe_parent,
            out,
            concurrency,
            per_host,
            max_download_mib,
            run,
            filters,
        } => {
            let limits = HttpLimits {
                max_download_bytes: max_download_mib << 20,
                ..HttpLimits::default()
            };
            let engine = HttpEngine::with_limits(per_host, 45, limits)?;
            let (items, channel_errors, policy) = run_multi_probe(
                &engine,
                &source,
                &target,
                profile.as_deref(),
                since.as_deref(),
                limit,
                dedupe_parent,
                &filters,
            )
            .await?;

            let eh_items: Vec<CandidateItem> = items
                .iter()
                .filter(|i| i.source == "ehentai")
                .cloned()
                .collect();
            let direct_items: Vec<CandidateItem> = items
                .into_iter()
                .filter(|i| i.source != "ehentai" && i.image_url.is_some())
                .collect();

            if !run {
                let sample_urls: Vec<String> = direct_items
                    .iter()
                    .take(5)
                    .filter_map(|i| i.image_url.clone())
                    .collect();
                let mut plan = json!({
                    "mode": "PLAN",
                    "source": source,
                    "target": target,
                    "rating_tier": policy.tier.as_str(),
                    "allow_mature": policy.allow_mature,
                    "out_dir": out.display().to_string(),
                    "candidate_count": direct_items.len(),
                    "sample_urls": sample_urls,
                });
                if !eh_items.is_empty() {
                    let est: u32 = eh_items
                        .iter()
                        .map(|g| g.filecount.unwrap_or(0).min(limit as u32))
                        .sum();
                    plan["estimated_ehentai_files"] = json!(est);
                }
                if !channel_errors.is_empty() {
                    plan["channel_errors"] = json!(channel_errors);
                }
                println!("{}", serde_json::to_string_pretty(&plan).unwrap());
                return Ok(());
            }

            tokio::fs::create_dir_all(&out)
                .await
                .map_err(|e| format!("mkdir {} failed: {e}", out.display()))?;

            let mut all_records = Vec::new();
            if !eh_items.is_empty() {
                let eh_recs = fetch_ehentai_isolated(&eh_items, &out, limit).await?;
                all_records.extend(eh_recs);
            }

            let global_sem = Arc::new(Semaphore::new(concurrency.max(1)));
            let content_tag = policy.content_rating_label().to_string();
            let dl_futs = direct_items.into_iter().map(|it| {
                let sem = global_sem.clone();
                let eng = engine.clone();
                let out_dir = out.clone();
                let ctag = content_tag.clone();
                async move {
                    let _permit = sem.acquire().await.map_err(|e| e.to_string())?;
                    let url = it.image_url.clone().unwrap();
                    let fname = format!("{}_{}{}", it.source, it.id, image_extension(&url));
                    let dest = FileName::parse(&fname)
                        .and_then(|n| n.path_in(&out_dir))
                        .map_err(|e| format!("refusing {} item {:?}: {e}", it.source, it.id))?;
                    let referer = download_referer(&it.source, it.source_url.as_deref());
                    let sha256 = eng.download_atomic(&url, referer.as_deref(), &dest).await?;
                    let mut rec = serde_json::to_value(&it).unwrap();
                    rec["file"] = json!(fname);
                    rec["sha256"] = json!(sha256);
                    rec["content_rating"] = json!(ctag);
                    Ok::<Value, String>(rec)
                }
            });

            let mut dl_errors = Vec::new();
            for res in futures_util::future::join_all(dl_futs).await {
                match res {
                    Ok(rec) => all_records.push(rec),
                    Err(e) => dl_errors.push(e),
                }
            }

            let manifest_path = out.join("manifest.jsonl");
            merge_manifest_atomic(&manifest_path, &all_records)?;

            let mut summary = json!({
                "mode": "RUN",
                "content_rating": content_tag,
                "downloaded": all_records.len(),
                "failed_downloads": dl_errors.len(),
                "out_dir": out.display().to_string(),
                "manifest": manifest_path.display().to_string(),
            });
            if !dl_errors.is_empty() {
                summary["download_errors"] = json!(dl_errors);
            }
            if !channel_errors.is_empty() {
                summary["channel_errors"] = json!(channel_errors);
            }
            println!("{}", serde_json::to_string_pretty(&summary).unwrap());
        }
        Commands::Stage {
            in_dir,
            out_dir,
            rating_tier,
            allow_mature,
            exclude_censored,
            allow_unpolished,
            min_long_edge,
            min_short_edge,
            max_edge,
            jpeg_quality,
            dhash_max_dist,
            prefix,
            trigger,
            tagger_json,
            allow_unverified_sfw,
            run,
        } => {
            let policy = RatingPolicy::resolve(rating_tier.as_deref(), allow_mature)?;
            let cfg = StageConfig {
                in_dirs: in_dir,
                out_dir,
                policy,
                exclude_censored,
                allow_unpolished,
                allow_unverified_sfw,
                min_long_edge,
                min_short_edge,
                max_edge,
                jpeg_quality,
                dhash_max_dist,
                prefix,
                trigger,
                tagger_json,
                run,
            };
            let res = execute_stage(&cfg)?;
            println!("{}", serde_json::to_string_pretty(&res).unwrap());
        }
    }
    Ok(())
}
