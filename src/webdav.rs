//! WebDAV sync module for rusterm session data.
//!
//! Upload/download `sessions.json` to a WebDAV server with Basic Auth,
//! SHA256 verification, and settings stored in `webdav.json`.

use anyhow::{bail, Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebDavSettings {
    pub enabled: bool,
    pub base_url: String,
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub auto_sync: bool,
}

impl Default for WebDavSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: "https://dav.jianguoyun.com/dav/rusterm-sync/".to_string(),
            username: String::new(),
            password: String::new(),
            auto_sync: false,
        }
    }
}

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

fn config_dir() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("dev", "rusterm", "rusterm")
        .context("could not determine project config directory")?;
    let dir = dirs.config_dir().to_path_buf();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create config dir {}", dir.display()))?;
    Ok(dir)
}

fn settings_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("webdav.json"))
}

fn sessions_json_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("sessions.json"))
}

const REMOTE_FILE: &str = "sessions.json";

pub fn load_settings() -> Result<WebDavSettings> {
    let path = settings_path()?;
    if path.exists() {
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let settings: WebDavSettings = serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        Ok(settings)
    } else {
        Ok(WebDavSettings::default())
    }
}

pub fn save_settings(settings: &WebDavSettings) -> Result<()> {
    let path = settings_path()?;
    let raw = serde_json::to_string_pretty(settings)?;
    std::fs::write(&path, raw)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn remote_url(base_url: &str, filename: &str) -> String {
    let base = base_url.trim_end_matches('/');
    format!("{}/{}", base, filename)
}

fn build_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client")
}

pub async fn test_connection(settings: &WebDavSettings) -> Result<()> {
    let client = build_client()?;
    let propfind_body = r#"<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop><D:resourcetype/></D:prop>
</D:propfind>"#;
    let resp = client
        .request(reqwest::Method::from_bytes(b"PROPFIND").unwrap(), &settings.base_url)
        .basic_auth(&settings.username, Some(&settings.password))
        .header("Depth", "0")
        .header("Content-Type", "application/xml")
        .body(propfind_body)
        .send()
        .await
        .context("failed to send PROPFIND request")?;
    let status = resp.status();
    if status.is_success() || status.as_u16() == 207 {
        Ok(())
    } else {
        let body = resp.text().await.unwrap_or_default();
        bail!("WebDAV PROPFIND failed: {} {}", status.as_u16(), body);
    }
}

pub async fn upload(settings: &WebDavSettings) -> Result<String> {
    let config_path = sessions_json_path()?;
    if !config_path.exists() {
        bail!("local sessions.json does not exist");
    }
    let data = std::fs::read(&config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let checksum = sha256_hex(&data);
    let client = build_client()?;
    // Ensure remote directory exists (MKCOL, ignore 405 = already exists)
    let _ = create_collection(settings).await;
    let url = remote_url(&settings.base_url, REMOTE_FILE);
    let resp = client
        .put(&url)
        .basic_auth(&settings.username, Some(&settings.password))
        .header("Content-Type", "application/json")
        .body(data)
        .send()
        .await
        .context("failed to PUT to WebDAV server")?;
    let status = resp.status();
    if !status.is_success() && status.as_u16() != 201 && status.as_u16() != 204 {
        let body = resp.text().await.unwrap_or_default();
        bail!("WebDAV PUT failed: {} {}", status.as_u16(), body);
    }
    tracing::info!("uploaded sessions.json (sha256: {}…)", &checksum[..16]);
    Ok(checksum)
}

pub async fn download(settings: &WebDavSettings) -> Result<String> {
    let client = build_client()?;
    let url = remote_url(&settings.base_url, REMOTE_FILE);
    let resp = client
        .get(&url)
        .basic_auth(&settings.username, Some(&settings.password))
        .send()
        .await
        .context("failed to GET from WebDAV server")?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("WebDAV GET failed: {} {}", status.as_u16(), body);
    }
    let data = resp.bytes().await.context("failed to read response body")?;
    serde_json::from_slice::<serde_json::Value>(&data)
        .context("downloaded content is not valid JSON")?;
    let checksum = sha256_hex(&data);
    let config_path = sessions_json_path()?;
    if config_path.exists() {
        let bak = config_path.with_extension("json.bak");
        std::fs::copy(&config_path, &bak)
            .with_context(|| format!("failed to backup to {}", bak.display()))?;
    }
    std::fs::write(&config_path, &data)
        .with_context(|| format!("failed to write {}", config_path.display()))?;
    tracing::info!("downloaded sessions.json (sha256: {}…)", &checksum[..16]);
    Ok(checksum)
}

pub async fn create_collection(settings: &WebDavSettings) -> Result<()> {
    let client = build_client()?;
    let resp = client
        .request(reqwest::Method::from_bytes(b"MKCOL").unwrap(), &settings.base_url)
        .basic_auth(&settings.username, Some(&settings.password))
        .send()
        .await
        .context("failed to send MKCOL request")?;
    let status = resp.status();
    if status.is_success() || status.as_u16() == 405 {
        Ok(())
    } else {
        let body = resp.text().await.unwrap_or_default();
        bail!("WebDAV MKCOL failed: {} {}", status.as_u16(), body);
    }
}
