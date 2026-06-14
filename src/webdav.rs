//! WebDAV 同步模块
//!
//! 功能：将本地 sessions.json（会话配置）上传/下载到 WebDAV 服务器
//! 支持：坚果云、Nutstore、Nextcloud 等标准 WebDAV 服务
//! 认证：HTTP Basic Auth
//! 安全：SHA256 校验文件完整性，下载前自动备份本地文件

use anyhow::{bail, Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

// ─── WebDAV 配置结构体 ──────────────────────────────────────────────
// 保存在 ~/.config/rusterm/webdav.json
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebDavSettings {
    pub enabled: bool,          // 是否启用同步
    pub base_url: String,       // WebDAV 服务器地址，如 https://dav.jianguoyun.com/dav/rusterm-sync/
    pub username: String,       // 用户名（坚果云邮箱）
    pub password: String,       // 密码（第三方应用专用密码，不是登录密码）
    #[serde(default)]
    pub auto_sync: bool,        // 是否自动同步（修改会话时自动上传）
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

// ─── 工具函数 ───────────────────────────────────────────────────────

/// 计算 SHA256 哈希值（用于校验文件完整性）
fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

/// 获取配置目录路径 ~/.config/rusterm/
fn config_dir() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("dev", "rusterm", "rusterm")
        .context("could not determine project config directory")?;
    let dir = dirs.config_dir().to_path_buf();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create config dir {}", dir.display()))?;
    Ok(dir)
}

/// webdav.json 的路径
fn settings_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("webdav.json"))
}

/// sessions.json 的路径（本地会话配置文件）
fn sessions_json_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("sessions.json"))
}

/// 远端文件名（上传/下载的文件名）
const REMOTE_FILE: &str = "sessions.json";

// ─── 配置读写 ───────────────────────────────────────────────────────

/// 从 webdav.json 加载 WebDAV 配置，文件不存在则返回默认值
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

/// 保存 WebDAV 配置到 webdav.json
pub fn save_settings(settings: &WebDavSettings) -> Result<()> {
    let path = settings_path()?;
    let raw = serde_json::to_string_pretty(settings)?;
    std::fs::write(&path, raw)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

// ─── URL 和 HTTP 客户端 ─────────────────────────────────────────────

/// 拼接远端文件 URL，如 https://dav.example.com/dav/ + sessions.json
fn remote_url(base_url: &str, filename: &str) -> String {
    let base = base_url.trim_end_matches('/');
    format!("{}/{}", base, filename)
}

/// 创建 HTTP 客户端，超时 30 秒
fn build_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client")
}

// ─── 核心操作 ───────────────────────────────────────────────────────

/// 测试 WebDAV 连接
///
/// 流程：
/// 1. MKCOL 创建远端目录（已存在则忽略 405 错误）
/// 2. PROPFIND 验证目录是否可访问
///
/// 成功返回 Ok(())，失败返回错误信息
pub async fn test_connection(settings: &WebDavSettings) -> Result<()> {
    let client = build_client()?;
    // 去掉末尾的 /，避免 MKCOL 时路径问题
    let url = settings.base_url.trim_end_matches('/');
    tracing::info!("test_connection url={} user={}", url, settings.username);

    // 第一步：MKCOL 创建目录
    // MKCOL 是 WebDAV 的"创建目录"命令
    // 405 = 目录已存在，也算成功
    tracing::info!("MKCOL {}", url);
    let mkcol_resp = client
        .request(reqwest::Method::from_bytes(b"MKCOL").unwrap(), url)
        .basic_auth(&settings.username, Some(&settings.password))
        .send()
        .await
        .context("failed to send MKCOL request")?;
    let mkcol_status = mkcol_resp.status().as_u16();
    tracing::info!("MKCOL response: {}", mkcol_status);

    // 第二步：PROPFIND 验证目录
    // PROPFIND 是 WebDAV 的"查询属性"命令，类似 HTTP GET 但只返回元数据
    // Depth: 0 表示只查当前目录，不查子目录
    let propfind_body = r#"<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop><D:resourcetype/></D:prop>
</D:propfind>"#;
    let resp = client
        .request(reqwest::Method::from_bytes(b"PROPFIND").unwrap(), url)
        .basic_auth(&settings.username, Some(&settings.password))
        .header("Depth", "0")
        .header("Content-Type", "application/xml")
        .body(propfind_body)
        .send()
        .await
        .context("failed to send PROPFIND request")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    tracing::info!("PROPFIND response: {} {}", status.as_u16(), &body[..body.len().min(200)]);

    // 200 和 207 都是 WebDAV 的正常响应
    if status.is_success() || status.as_u16() == 207 {
        Ok(())
    } else {
        bail!("WebDAV PROPFIND failed: {} {}", status.as_u16(), body);
    }
}

/// 上传 sessions.json 到 WebDAV 服务器
///
/// 流程：
/// 1. 读取本地 sessions.json
/// 2. 计算 SHA256 哈希
/// 3. MKCOL 确保远端目录存在
/// 4. PUT 上传文件
/// 5. 返回 SHA256（用于 UI 显示）
pub async fn upload(settings: &WebDavSettings) -> Result<String> {
    let config_path = sessions_json_path()?;
    if !config_path.exists() {
        bail!("local sessions.json does not exist");
    }

    // 读取本地文件
    let data = std::fs::read(&config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let checksum = sha256_hex(&data);
    let client = build_client()?;

    // 确保远端目录存在（MKCOL，已存在则忽略）
    let _ = create_collection(settings).await;

    // PUT 上传文件
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

    // 200/201/204 都是 PUT 成功的状态码
    if !status.is_success() && status.as_u16() != 201 && status.as_u16() != 204 {
        let body = resp.text().await.unwrap_or_default();
        bail!("WebDAV PUT failed: {} {}", status.as_u16(), body);
    }
    tracing::info!("uploaded sessions.json (sha256: {}…)", &checksum[..16]);
    Ok(checksum)
}

/// 从 WebDAV 服务器下载 sessions.json
///
/// 流程：
/// 1. GET 下载远端文件
/// 2. 验证是合法 JSON
/// 3. 计算 SHA256 哈希
/// 4. 备份本地文件为 sessions.json.bak
/// 5. 写入新文件
/// 6. 返回 SHA256
pub async fn download(settings: &WebDavSettings) -> Result<String> {
    let client = build_client()?;
    let url = remote_url(&settings.base_url, REMOTE_FILE);

    // GET 下载
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

    // 读取响应内容
    let data = resp.bytes().await.context("failed to read response body")?;

    // 验证是合法 JSON（防止下载到错误页面）
    serde_json::from_slice::<serde_json::Value>(&data)
        .context("downloaded content is not valid JSON")?;

    let checksum = sha256_hex(&data);
    let config_path = sessions_json_path()?;

    // 备份本地文件（如果存在）
    if config_path.exists() {
        let bak = config_path.with_extension("json.bak");
        std::fs::copy(&config_path, &bak)
            .with_context(|| format!("failed to backup to {}", bak.display()))?;
    }

    // 写入新文件
    std::fs::write(&config_path, &data)
        .with_context(|| format!("failed to write {}", config_path.display()))?;
    tracing::info!("downloaded sessions.json (sha256: {}…)", &checksum[..16]);
    Ok(checksum)
}

/// MKCOL 创建远端目录
///
/// MKCOL 是 WebDAV 专有的"创建目录"命令
/// - 201 = 创建成功
/// - 405 = 目录已存在（也算成功）
/// - 其他状态码 = 失败
pub async fn create_collection(settings: &WebDavSettings) -> Result<()> {
    let client = build_client()?;
    let url = settings.base_url.trim_end_matches('/');
    tracing::info!("MKCOL {}", url);
    let resp = client
        .request(reqwest::Method::from_bytes(b"MKCOL").unwrap(), url)
        .basic_auth(&settings.username, Some(&settings.password))
        .send()
        .await
        .context("failed to send MKCOL request")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    tracing::info!("MKCOL response: {} {}", status.as_u16(), body);

    // 201 创建成功，405 已存在
    if status.is_success() || status.as_u16() == 405 {
        Ok(())
    } else {
        bail!("WebDAV MKCOL failed: {} {}", status.as_u16(), body);
    }
}
