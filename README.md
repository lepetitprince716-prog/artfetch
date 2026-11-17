# artfetch

Fast, single-binary Rust CLI for multi-source illustration discovery, completion-gated dataset harvesting, cross-platform deduplication, and multi-core perceptual dataset staging (`1536px` 4:4:4 JPEG).

## Features

- **13 Supported Discovery & Metadata Channels**:
  - **Booru & Archive APIs**: `danbooru`, `gelbooru` (official DAPI w/ HTML fallback), `safebooru_org`, `tbib`, `yandere`, `konachan`, `e621`.
  - **Social & Portfolio Feeds**: `bluesky` (public AppView XRPC), `mastodon` (`baraag.net` / `pawoo.net` public REST), `artstation` (via `gallery-dl`), `pixiv` (via `gallery-dl` OAuth/cookies), `twitter` (via `twitter` CLI `?name=orig`), and `ehentai` (batch `gdata` API + isolated `gallery-dl`).
- **Creator Alias & Handle Resolution (`resolve`)**:
  - Resolves misspelled handles, active tag aliases, cross-platform artist profile links (X/Twitter, Pixiv ID, Bluesky, ArtStation, Baraag/Pawoo, Skeb/Fanbox/Patreon), and SFW vs mature post ratios into a structured `artfetch.artist_profile/v1` JSON document.
- **Training-Grade High-Completion Filtering (`--training-ready`)**:
  - Blocks AI-generated entries (`ai-generated`, `novelai`, `stable_diffusion`, Pixiv `ai_type == 2`, E-Hentai `other:ai generated`), unfinished roughs (`sketch`, `wip`, `lineart`, `monochrome`, `greyscale`, `flat_color`), and non-illustration layouts (`comic`, `4koma`, `manga`, `speech_bubble`, `character_sheet`, `multiple_views`, animated formats).
  - Enforces minimum long/short pixel edges, aspect-ratio bounds (`0.33..=3.0`), explicit rating tiers (`sfw`, `ecchi`, `mature_only`, `all`), and tri-state censorship classification (`censored` | `uncensored` | `unknown` with `--exclude-censored`).
  - Applies **filter-before-dedup** ordering so clean/uncensored variants are never discarded behind filtered parent posts, and deduplicates across platforms via `md5`, `pximg` page keys, and `twimg` media keys without collapsing multi-image social posts.
- **Resilient Concurrent Fetcher (`fetch`)**:
  - `PLAN` mode by default; `--run` streams downloads over pooled HTTP/2 connections with bounded global (`--concurrency`) and per-host (`--per-host`) semaphores, exponential retry backoff on `429`/`5xx`, inline `SHA-256`, and crash-safe `.part` atomic renames.
- **Multi-Core Perceptual Dataset Stager (`stage`)**:
  - Uses `rayon` across all CPU cores to decode local folders or browser exports (extracting Pixiv IDs from filenames automatically), flatten alpha onto white RGB, compute 64-bit horizontal `dHash` (`Hamming <= 6` near-duplicate collapse), optionally apply external vision-tagger JSON scores (`--tagger-json`), and encode `Lanczos3` `<= 1536px` `4:4:4` (`subsampling=0`) JPEGs with `SHA256SUMS`, `staging_manifest.json`, `dataset_skeleton.json`, and `prep_template.json`.

## Installation

```bash
cargo install --path .
```

## Configuration

`artfetch` requires zero configuration for public keyless endpoints (`danbooru`, `safebooru_org`, `tbib`, `yandere`, `konachan`, `e621`, `bluesky`, `mastodon`, `ehentai` metadata).

Optional credentials are read from environment variables or `~/.config/artfetch/credentials.env` (override path with `ARTFETCH_ENV_FILE`):

```ini
GELBOORU_API_KEY=your_api_key
GELBOORU_USER_ID=your_user_id
```

## Quick Start

```bash
# 1. Concurrently verify availability across all 13 channels
artfetch status

# 2. Resolve an artist alias or handle into canonical tags and platform targets
artfetch resolve "dishwasher1910"

# 3. Probe finished high-resolution works across multiple channels (read-only)
artfetch probe \
  --source danbooru,gelbooru,yandere \
  --target "dishwasher1910" \
  --training-ready \
  --since 2026-01-01 \
  --limit 40

# 4. Plan or execute bounded concurrent downloads (--run writes files)
artfetch fetch \
  --source danbooru,yandere \
  --target "dishwasher1910" \
  --rating-tier ecchi \
  --training-ready \
  --exclude-censored \
  --out ./harvest_raw \
  --concurrency 6 \
  --per-host 2

# 5. Stage local folders into deduplicated 1536px 4:4:4 training JPEGs
artfetch stage \
  --in-dir ./harvest_raw \
  --out-dir ./dataset_1536 \
  --allow-mature \
  --prefix dish26 \
  --trigger "@dishwasher1910" \
  --run
```

## License

Licensed under the Apache License, Version 2.0 ([LICENSE](LICENSE)).
