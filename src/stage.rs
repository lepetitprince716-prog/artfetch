use crate::model::{RatingPolicy, RatingTier};
use crate::safe_path::FileName;
use image::{DynamicImage, GenericImageView, ImageReader, RgbImage, imageops::FilterType};
use jpeg_encoder::{ColorType, Encoder, SamplingFactor};
use rayon::prelude::*;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

static PIXIV_FILENAME_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|[^0-9])(\d{7,11})_p(\d+)(?:[^0-9]|$)").expect("valid regex")
});

const SFW_EXPLICIT_MAX: f64 = 0.15;
const SFW_QUESTIONABLE_MAX: f64 = 0.25;

const DEFECT_THRESHOLDS: &[(&str, f64)] = &[
    ("monochrome", 0.50),
    ("greyscale", 0.50),
    ("sketch", 0.45),
    ("lineart", 0.45),
    ("comic", 0.45),
    ("4koma", 0.45),
    ("multiple_views", 0.50),
];

const CENSOR_THRESHOLDS: &[(&str, f64)] = &[("mosaic_censoring", 0.40), ("bar_censoring", 0.40)];

pub fn extract_pixiv_id_from_filename(filename: &str) -> Option<(String, u32)> {
    let cap = PIXIV_FILENAME_RE.captures(filename)?;
    let page = cap[2].parse::<u32>().ok()?;
    Some((cap[1].to_string(), page))
}

pub fn flatten_to_white_rgb(img: DynamicImage) -> RgbImage {
    match img {
        DynamicImage::ImageRgb8(rgb) => rgb,
        other => {
            let rgba = other.to_rgba8();
            let (w, h) = rgba.dimensions();
            let mut rgb = RgbImage::new(w, h);
            for (src_px, dst_px) in rgba.pixels().zip(rgb.pixels_mut()) {
                let a = u16::from(src_px[3]);
                let inv = 255u16.saturating_sub(a);
                dst_px[0] = (((u16::from(src_px[0]) * a) + (255 * inv) + 127) / 255) as u8;
                dst_px[1] = (((u16::from(src_px[1]) * a) + (255 * inv) + 127) / 255) as u8;
                dst_px[2] = (((u16::from(src_px[2]) * a) + (255 * inv) + 127) / 255) as u8;
            }
            rgb
        }
    }
}

pub fn compute_dhash64(img: &DynamicImage) -> u64 {
    let small = img
        .grayscale()
        .resize_exact(9, 8, FilterType::Lanczos3)
        .to_luma8();
    let mut bits = 0u64;
    for y in 0..8 {
        for x in 0..8 {
            let left = small.get_pixel(x, y)[0];
            let right = small.get_pixel(x + 1, y)[0];
            bits = (bits << 1) | u64::from(left < right);
        }
    }
    bits
}

pub fn hamming64(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

#[derive(Debug, Clone)]
pub struct StagedJpegInfo {
    pub orig_wh: (u32, u32),
    pub staged_wh: (u32, u32),
    pub sha256: String,
}

pub fn write_training_jpeg(
    src_path: &Path,
    dest_path: &Path,
    max_edge: u32,
    quality: u8,
) -> Result<StagedJpegInfo, String> {
    let decoded = ImageReader::open(src_path)
        .map_err(|e| format!("open {} failed: {e}", src_path.display()))?
        .with_guessed_format()
        .map_err(|e| format!("format guess {} failed: {e}", src_path.display()))?
        .decode()
        .map_err(|e| format!("decode {} failed: {e}", src_path.display()))?;

    let orig_wh = decoded.dimensions();
    let rgb = flatten_to_white_rgb(decoded);
    let long_edge = orig_wh.0.max(orig_wh.1);
    let final_rgb = if long_edge > max_edge {
        let scale = f64::from(max_edge) / f64::from(long_edge);
        let nw = ((f64::from(orig_wh.0) * scale).round() as u32).max(1);
        let nh = ((f64::from(orig_wh.1) * scale).round() as u32).max(1);
        image::imageops::resize(&rgb, nw, nh, FilterType::Lanczos3)
    } else {
        rgb
    };

    let staged_wh = final_rgb.dimensions();
    if staged_wh.0 > u32::from(u16::MAX) || staged_wh.1 > u32::from(u16::MAX) {
        return Err("image dimensions exceed JPEG u16 limit".to_string());
    }

    let mut encoded = Vec::with_capacity((staged_wh.0 as usize) * (staged_wh.1 as usize) / 2);
    let mut enc = Encoder::new(&mut encoded, quality);
    enc.set_sampling_factor(SamplingFactor::R_4_4_4);
    enc.encode(
        final_rgb.as_raw(),
        staged_wh.0 as u16,
        staged_wh.1 as u16,
        ColorType::Rgb,
    )
    .map_err(|e| format!("JPEG encode failed: {e}"))?;

    let digest = hex::encode(Sha256::digest(&encoded));
    let part_name = format!(
        ".{}.part",
        dest_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("staged.jpg")
    );
    let part_path = dest_path.with_file_name(part_name);
    std::fs::write(&part_path, &encoded)
        .map_err(|e| format!("write {} failed: {e}", part_path.display()))?;
    std::fs::rename(&part_path, dest_path)
        .map_err(|e| format!("rename {} failed: {e}", dest_path.display()))?;

    Ok(StagedJpegInfo {
        orig_wh,
        staged_wh,
        sha256: digest,
    })
}

#[derive(Debug, Clone)]
pub struct LocalCandidate {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub dhash: u64,
    pub pixiv_id: Option<String>,
    pub provenance_urls: Vec<String>,
    pub tagger_rating: Option<BTreeMap<String, f64>>,
    pub merged_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RejectedFile {
    pub path: String,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DroppedDuplicate {
    pub path: String,
    pub duplicate_of: String,
    pub hamming_distance: u32,
}

pub fn evaluate_tagger_entry(
    entry: Option<&Value>,
    policy: &RatingPolicy,
    exclude_censored: bool,
    check_completion: bool,
) -> (bool, Vec<String>, Option<BTreeMap<String, f64>>) {
    let Some(obj) = entry else {
        return (false, vec!["tagger_missing_entry".to_string()], None);
    };
    if obj.get("ok").and_then(Value::as_bool) != Some(true) {
        let r = obj
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("tagger_not_ok");
        return (false, vec![format!("tagger_failed:{r}")], None);
    }
    let exp = obj
        .pointer("/rating/explicit")
        .and_then(Value::as_f64)
        .unwrap_or(1.0);
    let quest = obj
        .pointer("/rating/questionable")
        .and_then(Value::as_f64)
        .unwrap_or(1.0);

    let mut reasons = Vec::new();
    match policy.tier {
        RatingTier::Sfw => {
            if !exp.is_finite()
                || !quest.is_finite()
                || exp >= SFW_EXPLICIT_MAX
                || quest >= SFW_QUESTIONABLE_MAX
            {
                reasons.push("tagger_not_sfw".to_string());
            }
        }
        RatingTier::Ecchi => {
            if !exp.is_finite() || exp >= SFW_EXPLICIT_MAX {
                reasons.push("tagger_explicit_exceeds_ecchi".to_string());
            }
        }
        RatingTier::MatureOnly => {
            if !exp.is_finite()
                || !quest.is_finite()
                || (exp < SFW_EXPLICIT_MAX && quest < SFW_QUESTIONABLE_MAX)
            {
                reasons.push("tagger_too_sfw_for_mature_only".to_string());
            }
        }
        RatingTier::All => {}
    }

    if let Some(gen_map) = obj.get("general").and_then(Value::as_object) {
        if check_completion {
            for (tag, thresh) in DEFECT_THRESHOLDS {
                if gen_map.get(*tag).and_then(Value::as_f64).unwrap_or(0.0) >= *thresh {
                    reasons.push(format!("tagger_defect:{tag}"));
                }
            }
        }
        if exclude_censored {
            for (tag, thresh) in CENSOR_THRESHOLDS {
                if gen_map.get(*tag).and_then(Value::as_f64).unwrap_or(0.0) >= *thresh {
                    reasons.push(format!("tagger_censored:{tag}"));
                }
            }
        }
    }

    let mut clean_r = BTreeMap::new();
    if let Some(rmap) = obj.get("rating").and_then(Value::as_object) {
        for (k, v) in rmap {
            if let Some(f) = v.as_f64() {
                clean_r.insert(k.clone(), (f * 10_000.0).round() / 10_000.0);
            }
        }
    }
    (reasons.is_empty(), reasons, Some(clean_r))
}

pub fn dedupe_by_dhash(
    mut candidates: Vec<LocalCandidate>,
    max_hamming: u32,
) -> (Vec<LocalCandidate>, Vec<DroppedDuplicate>) {
    candidates.sort_by(|a, b| {
        let area_a = u64::from(a.width) * u64::from(a.height);
        let area_b = u64::from(b.width) * u64::from(b.height);
        area_b.cmp(&area_a).then_with(|| a.path.cmp(&b.path))
    });

    let mut keepers: Vec<LocalCandidate> = Vec::new();
    let mut dropped: Vec<DroppedDuplicate> = Vec::new();

    for mut cand in candidates {
        let mut matched_idx = None;
        let mut matched_dist = u32::MAX;
        for (idx, kp) in keepers.iter().enumerate() {
            let dist = hamming64(cand.dhash, kp.dhash);
            if dist <= max_hamming {
                matched_idx = Some(idx);
                matched_dist = dist;
                break;
            }
        }
        if let Some(idx) = matched_idx {
            let p_str = cand.path.display().to_string();
            for u in cand.provenance_urls {
                if !keepers[idx].provenance_urls.contains(&u) {
                    keepers[idx].provenance_urls.push(u);
                }
            }
            keepers[idx].merged_paths.push(p_str.clone());
            dropped.push(DroppedDuplicate {
                path: p_str,
                duplicate_of: keepers[idx].path.display().to_string(),
                hamming_distance: matched_dist,
            });
        } else {
            cand.merged_paths = vec![cand.path.display().to_string()];
            keepers.push(cand);
        }
    }
    (keepers, dropped)
}

pub fn merge_manifest_atomic(manifest_path: &Path, new_records: &[Value]) -> Result<(), String> {
    let mut by_file: BTreeMap<String, Value> = BTreeMap::new();
    if let Ok(txt) = std::fs::read_to_string(manifest_path) {
        for line in txt.lines() {
            if let Ok(v) = serde_json::from_str::<Value>(line)
                && let Some(f) = v.get("file").and_then(Value::as_str)
            {
                by_file.insert(f.to_string(), v);
            }
        }
    }
    for rec in new_records {
        let Some(f) = rec.get("file").and_then(Value::as_str) else {
            continue;
        };
        let mut merged = rec.clone();
        if let Some(prev) = by_file.get(f) {
            for arr_key in ["all_seen_urls", "all_seen_sources"] {
                let mut combined: Vec<String> = Vec::new();
                for obj in [prev, rec] {
                    if let Some(arr) = obj.get(arr_key).and_then(Value::as_array) {
                        for s in arr.iter().filter_map(Value::as_str) {
                            if !combined.contains(&s.to_string()) {
                                combined.push(s.to_string());
                            }
                        }
                    }
                }
                if !combined.is_empty() {
                    if arr_key == "all_seen_sources" {
                        combined.sort();
                    }
                    merged[arr_key] = json!(combined);
                }
            }
        }
        by_file.insert(f.to_string(), merged);
    }
    let mut out_txt = String::new();
    for v in by_file.values() {
        out_txt.push_str(&serde_json::to_string(v).unwrap());
        out_txt.push('\n');
    }
    let part = manifest_path.with_file_name(".manifest.jsonl.part");
    std::fs::write(&part, out_txt).map_err(|e| format!("write manifest part failed: {e}"))?;
    std::fs::rename(&part, manifest_path).map_err(|e| format!("rename manifest failed: {e}"))?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct StageConfig {
    pub in_dirs: Vec<PathBuf>,
    pub out_dir: PathBuf,
    pub policy: RatingPolicy,
    pub exclude_censored: bool,
    pub allow_unpolished: bool,
    pub allow_unverified_sfw: bool,
    pub min_long_edge: u32,
    pub min_short_edge: u32,
    pub max_edge: u32,
    pub jpeg_quality: u8,
    pub dhash_max_dist: u32,
    pub prefix: String,
    pub trigger: Option<String>,
    pub tagger_json: Option<PathBuf>,
    pub run: bool,
}

fn collect_image_files(dir: &Path, out: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut sorted: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|x| x.path())).collect();
    sorted.sort();
    for p in sorted {
        let Some(name) = p.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        if p.is_dir() {
            collect_image_files(&p, out, seen);
        } else if p.is_file() {
            let ext = p
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "webp" | "bmp") {
                let canon = p.canonicalize().unwrap_or_else(|_| p.clone());
                if seen.insert(canon.clone()) {
                    out.push(canon);
                }
            }
        }
    }
}

pub fn execute_stage(cfg: &StageConfig) -> Result<Value, String> {
    let prefix = FileName::parse(&cfg.prefix).map_err(|e| format!("--prefix {e}"))?;
    for d in &cfg.in_dirs {
        if !d.is_dir() {
            return Err(format!("--in-dir is not a directory: {}", d.display()));
        }
    }
    if cfg.run
        && cfg.out_dir.exists()
        && std::fs::read_dir(&cfg.out_dir)
            .map(|mut i| i.next().is_some())
            .unwrap_or(false)
    {
        return Err(format!(
            "--out-dir must be empty or non-existent: {}",
            cfg.out_dir.display()
        ));
    }

    let mut prov_map: HashMap<String, Value> = HashMap::new();
    for d in &cfg.in_dirs {
        let mf = d.join("manifest.jsonl");
        if let Ok(text) = std::fs::read_to_string(mf) {
            for line in text.lines() {
                if let Ok(row) = serde_json::from_str::<Value>(line)
                    && let Some(f) = row.get("file").and_then(Value::as_str)
                {
                    let base = Path::new(f)
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or(f)
                        .to_string();
                    prov_map.insert(base, row);
                }
            }
        }
    }

    let mut raw_files = Vec::new();
    let mut seen = HashSet::new();
    for d in &cfg.in_dirs {
        collect_image_files(d, &mut raw_files, &mut seen);
    }

    // Parallel decode + dHash across all CPU cores via Rayon!
    let scanned_results: Vec<Result<LocalCandidate, RejectedFile>> = raw_files
        .par_iter()
        .map(|p| {
            let decoded = ImageReader::open(p)
                .and_then(|r| r.with_guessed_format())
                .map_err(|e| RejectedFile {
                    path: p.display().to_string(),
                    reasons: vec![format!("io_error:{e}")],
                })?
                .decode()
                .map_err(|e| RejectedFile {
                    path: p.display().to_string(),
                    reasons: vec![format!("corrupt_image:{e}")],
                })?;

            let (w, h) = decoded.dimensions();
            let long_edge = w.max(h);
            let short_edge = w.min(h);
            let aspect = if h > 0 {
                f64::from(w) / f64::from(h)
            } else {
                0.0
            };
            let mut reasons = Vec::new();
            if long_edge < cfg.min_long_edge {
                reasons.push("low_resolution".to_string());
            }
            if short_edge < cfg.min_short_edge {
                reasons.push("short_edge_too_small".to_string());
            }
            if !(0.33..=3.0).contains(&aspect) {
                reasons.push("extreme_aspect_ratio".to_string());
            }

            let fname = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
            let prov = prov_map.get(fname);
            if cfg.exclude_censored
                && prov
                    .and_then(|v| v.get("censorship"))
                    .and_then(Value::as_str)
                    == Some("censored")
            {
                reasons.push("censored_manifest".to_string());
            }
            if !reasons.is_empty() {
                return Err(RejectedFile {
                    path: p.display().to_string(),
                    reasons,
                });
            }

            let dhash = compute_dhash64(&decoded);
            let px_id = extract_pixiv_id_from_filename(fname).map(|(id, _)| id);
            let mut urls = Vec::new();
            if let Some(pr) = prov {
                if let Some(arr) = pr.get("all_seen_urls").and_then(Value::as_array) {
                    for u in arr.iter().filter_map(Value::as_str) {
                        if !urls.contains(&u.to_string()) {
                            urls.push(u.to_string());
                        }
                    }
                }
                if let Some(su) = pr.get("source_url").and_then(Value::as_str)
                    && !urls.contains(&su.to_string())
                {
                    urls.push(su.to_string());
                }
            }
            if let Some(ref pid) = px_id {
                let pu = format!("https://www.pixiv.net/artworks/{pid}");
                if !urls.contains(&pu) {
                    urls.push(pu);
                }
            }

            Ok(LocalCandidate {
                path: p.clone(),
                width: w,
                height: h,
                dhash,
                pixiv_id: px_id,
                provenance_urls: urls,
                tagger_rating: None,
                merged_paths: Vec::new(),
            })
        })
        .collect();

    let mut geom_passed = Vec::new();
    let mut rejected = Vec::new();
    for res in scanned_results {
        match res {
            Ok(c) => geom_passed.push(c),
            Err(r) => rejected.push(r),
        }
    }

    let mut tagger_passed = Vec::new();
    if let Some(ref tpath) = cfg.tagger_json {
        let raw_t = std::fs::read_to_string(tpath)
            .map_err(|e| format!("failed reading tagger JSON {}: {e}", tpath.display()))?;
        let tdoc: Value =
            serde_json::from_str(&raw_t).map_err(|e| format!("invalid tagger JSON: {e}"))?;
        let results_obj = tdoc.get("results").and_then(Value::as_object);

        for mut cand in geom_passed {
            let p_str = cand.path.display().to_string();
            let entry = results_obj.and_then(|m| m.get(&p_str));
            let (ok, reasons, ratings) = evaluate_tagger_entry(
                entry,
                &cfg.policy,
                cfg.exclude_censored,
                !cfg.allow_unpolished,
            );
            if ok {
                cand.tagger_rating = ratings;
                tagger_passed.push(cand);
            } else {
                rejected.push(RejectedFile {
                    path: p_str,
                    reasons,
                });
            }
        }
    } else {
        if cfg.policy.tier == RatingTier::Sfw && cfg.run && !cfg.allow_unverified_sfw {
            return Err(
                "SFW stage --run requires --tagger-json for visual rating verification (or pass --allow-unverified-sfw to label output as 'sfw_unverified')"
                    .to_string(),
            );
        }
        tagger_passed = geom_passed;
    }

    let (keepers, dhash_dropped) = dedupe_by_dhash(tagger_passed, cfg.dhash_max_dist);
    let content_tag = if cfg.policy.tier == RatingTier::Sfw && cfg.tagger_json.is_none() {
        "sfw_unverified"
    } else {
        cfg.policy.content_rating_label()
    };

    if !cfg.run {
        return Ok(json!({
            "mode": "PLAN",
            "content_rating": content_tag,
            "rating_tier": cfg.policy.tier.as_str(),
            "out_dir": cfg.out_dir.display().to_string(),
            "scanned_files": raw_files.len(),
            "rejected_count": rejected.len(),
            "dhash_duplicates_dropped": dhash_dropped.len(),
            "keepers_count": keepers.len(),
            "tagger_applied": cfg.tagger_json.is_some(),
        }));
    }

    std::fs::create_dir_all(&cfg.out_dir)
        .map_err(|e| format!("create_dir_all {} failed: {e}", cfg.out_dir.display()))?;

    // Parallel Lanczos3 resize + 4:4:4 JPEG encoding across all CPU cores!
    let encoded_results: Result<Vec<Value>, String> = keepers
        .par_iter()
        .enumerate()
        .map(|(i, kp)| {
            let idx = i + 1;
            let item_id = format!("{}_{idx:03}", prefix.as_str());
            let jpg_name = format!("{item_id}.jpg");
            let dest = FileName::parse(&jpg_name)?.path_in(&cfg.out_dir)?;
            let info = write_training_jpeg(&kp.path, &dest, cfg.max_edge, cfg.jpeg_quality)?;
            Ok(json!({
                "num_id": format!("#{idx:02}"),
                "id": item_id,
                "file": jpg_name,
                "sha256": info.sha256,
                "orig_wh": [info.orig_wh.0, info.orig_wh.1],
                "staged_wh": [info.staged_wh.0, info.staged_wh.1],
                "source_file": kp.path.file_name().and_then(|s| s.to_str()).unwrap_or(""),
                "pixiv_id": kp.pixiv_id,
                "tagger_rating": kp.tagger_rating,
                "provenance_urls": kp.provenance_urls,
            }))
        })
        .collect();

    let staged_entries = encoded_results?;
    let mut sha_lines = String::new();
    let mut skeleton_items = Vec::new();
    for entry in &staged_entries {
        let sha = entry.get("sha256").and_then(Value::as_str).unwrap_or("");
        let file = entry.get("file").and_then(Value::as_str).unwrap_or("");
        let id = entry.get("id").and_then(Value::as_str).unwrap_or("");
        sha_lines.push_str(&format!("{sha}  {file}\n"));
        skeleton_items.push(json!({ "id": id, "caption": "" }));
    }

    std::fs::write(cfg.out_dir.join("SHA256SUMS"), sha_lines)
        .map_err(|e| format!("write SHA256SUMS failed: {e}"))?;
    std::fs::write(
        cfg.out_dir.join("dataset_skeleton.json"),
        serde_json::to_string_pretty(&json!({
            "content_rating": content_tag,
            "items": skeleton_items,
        }))
        .unwrap()
            + "\n",
    )
    .map_err(|e| format!("write dataset_skeleton.json failed: {e}"))?;

    let trig_prefix = cfg
        .trigger
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|t| format!("{t}. "))
        .unwrap_or_default();
    std::fs::write(
        cfg.out_dir.join("prep_template.json"),
        serde_json::to_string_pretty(&json!({
            "content_rating": content_tag,
            "items": "dataset_skeleton.json",
            "source_dir": cfg.out_dir.display().to_string(),
            "dest_dir": format!("{}_prep", cfg.out_dir.display()),
            "caption_template": format!("{trig_prefix}Anime-style digital illustration. {{caption}}"),
            "image_ext": ".jpg",
        }))
        .unwrap()
            + "\n",
    )
    .map_err(|e| format!("write prep_template.json failed: {e}"))?;

    let manifest_doc = json!({
        "schema": "artfetch.staging_manifest/v1",
        "content_rating": content_tag,
        "rating_tier": cfg.policy.tier.as_str(),
        "preprocessing": {
            "max_edge_px": cfg.max_edge,
            "resampling": "Lanczos3",
            "format": "JPEG (.jpg)",
            "jpeg_quality": cfg.jpeg_quality,
            "chroma_subsampling": "4:4:4 (subsampling=0)"
        },
        "summary": {
            "scanned_files": raw_files.len(),
            "kept_count": staged_entries.len(),
            "rejected_count": rejected.len(),
            "dhash_duplicates_dropped": dhash_dropped.len(),
        },
        "kept": staged_entries,
        "dhash_dropped": dhash_dropped,
        "rejected": rejected,
    });
    std::fs::write(
        cfg.out_dir.join("staging_manifest.json"),
        serde_json::to_string_pretty(&manifest_doc).unwrap() + "\n",
    )
    .map_err(|e| format!("write staging_manifest.json failed: {e}"))?;

    Ok(json!({
        "mode": "RUN",
        "content_rating": content_tag,
        "kept_count": staged_entries.len(),
        "out_dir": cfg.out_dir.display().to_string(),
        "manifest": cfg.out_dir.join("staging_manifest.json").display().to_string(),
        "prep_template": cfg.out_dir.join("prep_template.json").display().to_string(),
    }))
}
