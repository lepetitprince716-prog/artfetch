use artfetch::filter::{
    FilterOptions, annotate_and_filter, classify_censorship, dedupe_cross_platform_items,
    dedupe_parent_items, extract_platform_handles, extract_source_year,
};
use artfetch::http::HttpEngine;
use artfetch::model::{CandidateItem, RatingPolicy, RatingTier};
use artfetch::sources::twitter_orig_url;
use artfetch::stage::{
    StageConfig, execute_stage, extract_pixiv_id_from_filename, merge_manifest_atomic,
};
use image::{DynamicImage, Rgba, RgbaImage};
use serde_json::json;

#[test]
fn rating_policy_conflict_and_tiers() {
    assert!(RatingPolicy::resolve(Some("sfw"), true).is_err());
    let sfw = RatingPolicy::resolve(None, false).unwrap();
    assert_eq!(sfw.tier, RatingTier::Sfw);
    assert!(!sfw.allow_mature);

    let ecchi = RatingPolicy::resolve(Some("ecchi"), false).unwrap();
    assert!(ecchi.allow_mature);
    assert!(ecchi.probe_ratings.contains("questionable"));
    assert!(!ecchi.probe_ratings.contains("explicit"));

    let mature_only = RatingPolicy::resolve(Some("mature_only"), true).unwrap();
    assert!(!mature_only.probe_ratings.contains("general"));
    assert!(mature_only.probe_ratings.contains("explicit"));
}

#[test]
fn source_year_and_twitter_orig_upgrade() {
    assert_eq!(
        extract_source_year(Some("https://x.com/artist_demo/status/2098634019613356364")),
        Some(2026)
    );
    assert_eq!(
        extract_source_year(Some(
            "https://i.pximg.net/img-original/img/2025/08/14/12/00/00/123456_p0.png"
        )),
        Some(2025)
    );
    let orig = twitter_orig_url("https://pbs.twimg.com/media/ABC123xyz.jpg?name=large&format=jpg");
    assert!(orig.contains("name=orig"));
}

#[test]
fn training_filter_occurs_before_dedup_and_preserves_multi_image_tweets() {
    let policy = RatingPolicy::resolve(Some("all"), true).unwrap();
    let opts = FilterOptions {
        training_ready: true,
        exclude_censored: true,
        min_long_edge: 1024,
        min_short_edge: 640,
        ..Default::default()
    };

    // Cluster 900: parent 900 is mosaic_censoring (larger), child 901 is uncensored (slightly smaller).
    // Filter-first guarantees 901 survives instead of 900 winning dedup and then being dropped!
    let censored_parent = CandidateItem {
        id: "900".into(),
        source: "danbooru".into(),
        rating: "explicit".into(),
        width: Some(3000),
        height: Some(4000),
        image_url: Some("https://cdn.example.org/900.png".into()),
        raw_tags: vec!["1girl".into(), "mosaic_censoring".into()],
        ..Default::default()
    };
    let uncensored_child = CandidateItem {
        id: "901".into(),
        source: "danbooru".into(),
        parent_id: Some("900".into()),
        rating: "explicit".into(),
        width: Some(2400),
        height: Some(3200),
        md5: Some("0123456789abcdef0123456789abcdef".into()),
        image_url: Some("https://cdn.example.org/901.png".into()),
        raw_tags: vec!["1girl".into(), "uncensored".into()],
        ..Default::default()
    };
    // Cross-source duplicate of 901 on yandere with higher resolution -> should win and merge sources!
    let yandere_master = CandidateItem {
        id: "555".into(),
        source: "yandere".into(),
        rating: "explicit".into(),
        width: Some(3600),
        height: Some(4800),
        md5: Some("0123456789abcdef0123456789abcdef".into()),
        image_url: Some("https://cdn.example.org/y555.png".into()),
        raw_tags: vec!["1girl".into(), "uncensored".into()],
        ..Default::default()
    };
    // Sketch item -> rejected by training_ready.
    let sketch_item = CandidateItem {
        id: "902".into(),
        source: "danbooru".into(),
        rating: "general".into(),
        width: Some(2000),
        height: Some(2000),
        image_url: Some("https://cdn.example.org/902.png".into()),
        raw_tags: vec!["sketch".into(), "monochrome".into()],
        ..Default::default()
    };
    // Multi-image tweet (2 distinct media hashes under same tweet status URL) -> BOTH must survive!
    let tw1 = CandidateItem {
        id: "777_0".into(),
        source: "twitter".into(),
        rating: "unknown".into(),
        width: Some(2048),
        height: Some(2048),
        image_url: Some("https://pbs.twimg.com/media/MEDIA_ONE.jpg?name=orig".into()),
        source_url: Some("https://x.com/demo/status/777".into()),
        ..Default::default()
    };
    let tw2 = CandidateItem {
        id: "777_1".into(),
        source: "twitter".into(),
        rating: "unknown".into(),
        width: Some(2048),
        height: Some(2048),
        image_url: Some("https://pbs.twimg.com/media/MEDIA_TWO.jpg?name=orig".into()),
        source_url: Some("https://x.com/demo/status/777".into()),
        ..Default::default()
    };

    let filtered = annotate_and_filter(
        vec![
            censored_parent,
            uncensored_child,
            yandere_master,
            sketch_item,
            tw1,
            tw2,
        ],
        &policy,
        &opts,
    );
    let deduped = dedupe_cross_platform_items(filtered);
    assert_eq!(deduped.len(), 3);
    assert_eq!(deduped[0].source, "yandere");
    assert_eq!(
        deduped[0].all_seen_sources,
        vec!["danbooru".to_string(), "yandere".to_string()]
    );
    assert_eq!(deduped[1].id, "777_0");
    assert_eq!(deduped[2].id, "777_1");
}

#[test]
fn parent_dedup_namespaces_by_source_and_strips_sfw_tags() {
    let sfw_pol = RatingPolicy::resolve(Some("sfw"), false).unwrap();
    let d1 = CandidateItem {
        id: "100".into(),
        source: "danbooru".into(),
        rating: "general".into(),
        width: Some(1500),
        height: Some(1500),
        tags: vec!["tag_a".into()],
        raw_tags: vec!["tag_a".into()],
        ..Default::default()
    };
    let g1 = CandidateItem {
        id: "100".into(),
        source: "gelbooru".into(),
        rating: "general".into(),
        width: Some(1500),
        height: Some(1500),
        tags: vec!["tag_b".into()],
        raw_tags: vec!["tag_b".into()],
        ..Default::default()
    };
    let filtered = annotate_and_filter(vec![d1, g1], &sfw_pol, &FilterOptions::default());
    assert!(filtered.iter().all(|i| i.tags.is_empty()));
    let collapsed = dedupe_parent_items(filtered);
    assert_eq!(collapsed.len(), 2);
}

#[test]
fn url_handle_extraction_and_censorship_tri_state() {
    assert_eq!(
        classify_censorship(&["uncensored".to_string(), "mosaic_censoring".to_string()]),
        "censored"
    );
    assert_eq!(
        classify_censorship(&["uncensored".to_string()]),
        "uncensored"
    );
    assert_eq!(classify_censorship(&["1girl".to_string()]), "unknown");

    let urls = vec![
        "https://x.com/old_alt_account".to_string(),
        "https://x.com/demo_artist".to_string(),
        "https://www.pixiv.net/en/users/9876543".to_string(),
        "https://bsky.app/profile/demo.bsky.social".to_string(),
        "https://www.artstation.com/demo_portfolio".to_string(),
        "https://baraag.net/@demo_masto".to_string(),
        "https://demo.fanbox.cc/".to_string(),
    ];
    let (platforms, manual) = extract_platform_handles(&urls, Some("demo_artist"));
    assert_eq!(
        platforms.get("twitter").map(String::as_str),
        Some("demo_artist")
    );
    assert_eq!(platforms.get("pixiv").map(String::as_str), Some("9876543"));
    assert_eq!(
        platforms.get("bluesky").map(String::as_str),
        Some("demo.bsky.social")
    );
    assert_eq!(
        platforms.get("artstation").map(String::as_str),
        Some("demo_portfolio")
    );
    assert_eq!(
        platforms.get("mastodon").map(String::as_str),
        Some("baraag:demo_masto")
    );
    assert_eq!(manual.len(), 1);
    assert_eq!(
        extract_pixiv_id_from_filename("123456789_p2 - Illustration Title.png"),
        Some(("123456789".to_string(), 2))
    );
}

#[test]
fn parallel_stage_resizes_dedups_and_writes_prep_templates() {
    let dir = tempfile::tempdir().unwrap();
    let in_dir = dir.path().join("inbox");
    let out_dir = dir.path().join("staged");
    std::fs::create_dir_all(&in_dir).unwrap();

    // Image A: 2000x1600 gradient PNG with alpha channel + Pixiv filename
    let mut img_a = RgbaImage::new(2000, 1600);
    for (x, y, px) in img_a.enumerate_pixels_mut() {
        *px = Rgba([(x % 256) as u8, (y % 256) as u8, 140, 255]);
    }
    let path_a = in_dir.join("109876543_p0_master.png");
    DynamicImage::ImageRgba8(img_a.clone())
        .save(&path_a)
        .unwrap();

    // Image A duplicate at lower resolution (1200x960) -> should collapse via dHash!
    let img_a_small = DynamicImage::ImageRgba8(img_a).resize_exact(
        1200,
        960,
        image::imageops::FilterType::Lanczos3,
    );
    let path_a_dup = in_dir.join("twitter_reencode.jpg");
    img_a_small.to_rgb8().save(&path_a_dup).unwrap();

    // Image B: distinct pattern (vertical stripes, 1400x1800)
    let mut img_b = RgbaImage::new(1400, 1800);
    for (x, _y, px) in img_b.enumerate_pixels_mut() {
        let v = if (x / 120) % 2 == 0 { 240 } else { 20 };
        *px = Rgba([v, 255 - v, 80, 255]);
    }
    let path_b = in_dir.join("distinct_b.png");
    DynamicImage::ImageRgba8(img_b).save(&path_b).unwrap();

    // Image C: too small (400x400) -> rejected by geometry
    let img_c = RgbaImage::from_pixel(400, 400, Rgba([100, 100, 100, 255]));
    let path_c = in_dir.join("tiny_thumb.png");
    DynamicImage::ImageRgba8(img_c).save(&path_c).unwrap();

    // Write simulated vision tagger JSON
    let p_a_canon = path_a.canonicalize().unwrap().display().to_string();
    let p_dup_canon = path_a_dup.canonicalize().unwrap().display().to_string();
    let p_b_canon = path_b.canonicalize().unwrap().display().to_string();
    let tagger_path = dir.path().join("tagger.json");
    let tagger_doc = json!({
        "ok": true,
        "results": {
            p_a_canon: { "ok": true, "rating": { "general": 0.91, "sensitive": 0.08, "questionable": 0.005, "explicit": 0.001 }, "general": {} },
            p_dup_canon: { "ok": true, "rating": { "general": 0.90, "sensitive": 0.09, "questionable": 0.005, "explicit": 0.001 }, "general": {} },
            p_b_canon: { "ok": true, "rating": { "general": 0.85, "sensitive": 0.14, "questionable": 0.01, "explicit": 0.001 }, "general": { "sketch": 0.80 } }
        }
    });
    std::fs::write(&tagger_path, serde_json::to_string(&tagger_doc).unwrap()).unwrap();

    let cfg = StageConfig {
        in_dirs: vec![in_dir.clone()],
        out_dir: out_dir.clone(),
        policy: RatingPolicy::resolve(Some("sfw"), false).unwrap(),
        exclude_censored: true,
        allow_unpolished: false,
        allow_unverified_sfw: false,
        min_long_edge: 1024,
        min_short_edge: 640,
        max_edge: 1536,
        jpeg_quality: 93,
        dhash_max_dist: 6,
        prefix: "demo".into(),
        trigger: Some("@demo_artist".into()),
        tagger_json: Some(tagger_path),
        run: true,
    };

    // B4: SFW stage --run without tagger_json must fail closed unless allow_unverified_sfw is set!
    let mut unverified_cfg = cfg.clone();
    unverified_cfg.out_dir = dir.path().join("unverified_fail");
    unverified_cfg.tagger_json = None;
    assert!(execute_stage(&unverified_cfg).is_err());

    unverified_cfg.allow_unverified_sfw = true;
    let unv_res = execute_stage(&unverified_cfg).unwrap();
    assert_eq!(unv_res["content_rating"], "sfw_unverified");

    let res = execute_stage(&cfg).unwrap();
    assert_eq!(res["kept_count"], 1);
    assert!(out_dir.join("demo_001.jpg").is_file());
    assert!(out_dir.join("SHA256SUMS").is_file());
    assert!(out_dir.join("prep_template.json").is_file());

    let staged_img = image::open(out_dir.join("demo_001.jpg")).unwrap();
    assert_eq!(staged_img.width(), 1536);
    assert_eq!(staged_img.height(), 1229);

    // Re-running against non-empty out_dir must fail cleanly.
    assert!(execute_stage(&cfg).is_err());
}

#[test]
fn b1_parent_priority_b3_manifest_merge_and_url_redaction() {
    // B1: Root post (`parent_id: None`) beats higher-resolution child variant across platforms.
    let root_db = CandidateItem {
        id: "10".into(),
        source: "danbooru".into(),
        parent_id: None,
        width: Some(2000),
        height: Some(2000),
        md5: Some("fedcba9876543210fedcba9876543210".into()),
        ..Default::default()
    };
    let child_ym = CandidateItem {
        id: "20".into(),
        source: "yandere".into(),
        parent_id: Some("19".into()),
        width: Some(2500),
        height: Some(2500),
        md5: Some("fedcba9876543210fedcba9876543210".into()),
        ..Default::default()
    };
    let out = dedupe_cross_platform_items(vec![child_ym, root_db]);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].source, "danbooru");

    // B3: merge_manifest_atomic preserves prior all_seen_urls & all_seen_sources.
    let dir = tempfile::tempdir().unwrap();
    let mf = dir.path().join("manifest.jsonl");
    merge_manifest_atomic(
        &mf,
        &[json!({"file": "a.png", "all_seen_urls": ["https://u1.example.org"], "all_seen_sources": ["danbooru"]})],
    )
    .unwrap();
    merge_manifest_atomic(
        &mf,
        &[json!({"file": "a.png", "all_seen_urls": ["https://u2.example.org"], "all_seen_sources": ["yandere"]})],
    )
    .unwrap();
    let line = std::fs::read_to_string(&mf).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(
        parsed["all_seen_urls"],
        json!(["https://u1.example.org", "https://u2.example.org"])
    );
    assert_eq!(parsed["all_seen_sources"], json!(["danbooru", "yandere"]));

    // Secret redaction in error URLs.
    let red = HttpEngine::redact_url(
        "https://gelbooru.com/index.php?api_key=SECRET123&user_id=9999&tags=1girl",
    );
    assert!(!red.contains("SECRET123"));
    assert!(!red.contains("9999"));
    assert!(red.contains("REDACTED"));
}
