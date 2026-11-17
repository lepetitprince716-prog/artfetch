use futures_util::StreamExt;
use reqwest::{Client, StatusCode};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;
use url::Url;

const DEFAULT_UA: &str = "artfetch/0.1 (multi-source illustration dataset tool)";

pub fn expand_tilde(path_str: &str) -> PathBuf {
    if let Some(rest) = path_str.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path_str)
}

pub fn load_env_file_defaults() -> HashMap<String, String> {
    let target = std::env::var("ARTFETCH_ENV_FILE")
        .map(|p| expand_tilde(&p))
        .unwrap_or_else(|_| expand_tilde("~/.config/artfetch/credentials.env"));
    let mut map = HashMap::new();
    let Ok(content) = std::fs::read_to_string(target) else {
        return map;
    };
    for raw_line in content.lines() {
        let mut line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(stripped) = line.strip_prefix("export ") {
            line = stripped.trim();
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let key = k.trim();
        let mut val = v.trim();
        if val.len() >= 2
            && ((val.starts_with('"') && val.ends_with('"'))
                || (val.starts_with('\'') && val.ends_with('\'')))
        {
            val = &val[1..val.len() - 1];
        }
        if !key.is_empty() {
            map.insert(key.to_string(), val.to_string());
        }
    }
    map
}

pub fn gelbooru_credentials() -> (Option<String>, Option<String>) {
    let defaults = load_env_file_defaults();
    let api_key = std::env::var("GELBOORU_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            defaults
                .get("GELBOORU_API_KEY")
                .cloned()
                .filter(|s| !s.is_empty())
        });
    let user_id = std::env::var("GELBOORU_USER_ID")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            defaults
                .get("GELBOORU_USER_ID")
                .cloned()
                .filter(|s| !s.is_empty())
        });
    (api_key, user_id)
}

#[derive(Clone)]
pub struct HttpEngine {
    client: Client,
    per_host_limit: usize,
    host_semaphores: Arc<Mutex<HashMap<String, Arc<Semaphore>>>>,
}

impl HttpEngine {
    pub fn new(per_host_limit: usize, timeout_secs: u64) -> Result<Self, String> {
        let client = Client::builder()
            .user_agent(DEFAULT_UA)
            .timeout(Duration::from_secs(timeout_secs))
            .connect_timeout(Duration::from_secs(10))
            .pool_max_idle_per_host(4)
            .build()
            .map_err(|e| format!("failed to build HTTP client: {e}"))?;
        Ok(Self {
            client,
            per_host_limit: per_host_limit.max(1),
            host_semaphores: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    fn host_semaphore(&self, url: &str) -> Arc<Semaphore> {
        let host = Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
            .unwrap_or_else(|| "default".to_string());
        let mut guard = self.host_semaphores.lock().expect("host mutex");
        guard
            .entry(host)
            .or_insert_with(|| Arc::new(Semaphore::new(self.per_host_limit)))
            .clone()
    }

    fn should_retry(status: StatusCode) -> bool {
        status == StatusCode::TOO_MANY_REQUESTS
            || status == StatusCode::REQUEST_TIMEOUT
            || status.is_server_error()
    }

    pub fn redact_url(raw: &str) -> String {
        let Ok(mut parsed) = Url::parse(raw) else {
            return raw.to_string();
        };
        let has_secret = parsed.query_pairs().any(|(k, _)| {
            matches!(
                k.to_ascii_lowercase().as_str(),
                "api_key" | "user_id" | "token" | "key" | "access_token"
            )
        });
        if !has_secret {
            return raw.to_string();
        }
        let clean_pairs: Vec<(String, String)> = parsed
            .query_pairs()
            .map(|(k, v)| {
                if matches!(
                    k.to_ascii_lowercase().as_str(),
                    "api_key" | "user_id" | "token" | "key" | "access_token"
                ) {
                    (k.into_owned(), "REDACTED".to_string())
                } else {
                    (k.into_owned(), v.into_owned())
                }
            })
            .collect();
        parsed.query_pairs_mut().clear().extend_pairs(clean_pairs);
        parsed.into()
    }

    pub async fn get_text(&self, url: &str) -> Result<String, String> {
        let sem = self.host_semaphore(url);
        let _permit = sem.acquire().await.map_err(|e| e.to_string())?;
        let safe_url = Self::redact_url(url);
        let mut last_err = String::new();

        for attempt in 0..3u64 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(350 * (1 << attempt))).await;
            }
            match self
                .client
                .get(url)
                .header("Accept", "application/json, text/html;q=0.9")
                .send()
                .await
            {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        return resp
                            .text()
                            .await
                            .map_err(|e| format!("body read failed for {safe_url}: {e}"));
                    }
                    last_err = format!("HTTP {} for {safe_url}", status.as_u16());
                    if !Self::should_retry(status) {
                        return Err(last_err);
                    }
                }
                Err(_) => {
                    last_err = format!("request failed for {safe_url}");
                }
            }
        }
        Err(last_err)
    }

    pub async fn post_json(
        &self,
        url: &str,
        payload: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let sem = self.host_semaphore(url);
        let _permit = sem.acquire().await.map_err(|e| e.to_string())?;
        let mut last_err = String::new();

        for attempt in 0..3u64 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(400 * (1 << attempt))).await;
            }
            match self.client.post(url).json(payload).send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        return resp
                            .json::<serde_json::Value>()
                            .await
                            .map_err(|e| format!("JSON decode failed for {url}: {e}"));
                    }
                    last_err = format!("HTTP {} for {}", status.as_u16(), url);
                    if !Self::should_retry(status) {
                        return Err(last_err);
                    }
                }
                Err(e) => {
                    last_err = format!("POST error for {url}: {e}");
                }
            }
        }
        Err(last_err)
    }

    pub async fn download_atomic(
        &self,
        url: &str,
        referer: Option<&str>,
        dest: &Path,
    ) -> Result<String, String> {
        if dest.is_file() {
            let bytes = tokio::fs::read(dest)
                .await
                .map_err(|e| format!("failed reading existing {}: {e}", dest.display()))?;
            if !bytes.is_empty() {
                return Ok(hex::encode(Sha256::digest(&bytes)));
            }
        }

        let sem = self.host_semaphore(url);
        let _permit = sem.acquire().await.map_err(|e| e.to_string())?;
        let part_name = format!(
            ".{}.part",
            dest.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("download")
        );
        let part_path = dest.with_file_name(part_name);
        let mut last_err = String::new();

        let safe_url = Self::redact_url(url);
        for attempt in 0..3u64 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(500 * (1 << attempt))).await;
            }
            let mut req = self.client.get(url);
            if let Some(r) = referer.filter(|s| !s.is_empty()) {
                req = req.header("Referer", r);
            }
            match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if !status.is_success() {
                        last_err = format!("HTTP {} downloading {}", status.as_u16(), safe_url);
                        if !Self::should_retry(status) {
                            return Err(last_err);
                        }
                        continue;
                    }
                    let mut file = tokio::fs::File::create(&part_path)
                        .await
                        .map_err(|e| format!("create {} failed: {e}", part_path.display()))?;
                    let mut hasher = Sha256::new();
                    let mut stream = resp.bytes_stream();
                    let mut stream_ok = true;
                    while let Some(chunk_res) = stream.next().await {
                        match chunk_res {
                            Ok(chunk) => {
                                hasher.update(&chunk);
                                if let Err(e) = file.write_all(&chunk).await {
                                    last_err = format!("write failed: {e}");
                                    stream_ok = false;
                                    break;
                                }
                            }
                            Err(e) => {
                                last_err = format!("stream error on {url}: {e}");
                                stream_ok = false;
                                break;
                            }
                        }
                    }
                    if stream_ok && file.flush().await.is_ok() {
                        drop(file);
                        tokio::fs::rename(&part_path, dest)
                            .await
                            .map_err(|e| format!("atomic rename failed: {e}"))?;
                        return Ok(hex::encode(hasher.finalize()));
                    }
                }
                Err(e) => {
                    last_err = format!("download request failed for {url}: {e}");
                }
            }
        }
        Err(last_err)
    }
}
