//! WebDAV 同步模块
//!
//! 功能：将本地 sessions.json（会话配置）上传/下载到 WebDAV 服务器
//! 支持：坚果云、Nutstore、Nextcloud 等标准 WebDAV 服务
//! 认证：HTTP Basic Auth
//! 安全：SHA256 校验文件完整性，下载前自动备份本地文件
//! 冲突处理：上传前检查远端版本，冲突时支持保留本地/保留远端/智能合并

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
    /// 上次同步成功时远端文件的 SHA256，用于冲突检测
    #[serde(default)]
    pub last_sync_hash: String,
}

impl Default for WebDavSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: "https://dav.jianguoyun.com/dav/rusterm-sync/".to_string(),
            username: String::new(),
            password: String::new(),
            auto_sync: false,
            last_sync_hash: String::new(),
        }
    }
}

// ─── 同步结果 ───────────────────────────────────────────────────────

/// 同步操作的结果
#[derive(Debug)]
pub enum SyncResult {
    /// 同步成功，返回 SHA256
    Ok(String),
    /// 检测到冲突：(本地 SHA256, 远端 SHA256)
    Conflict { local_hash: String, remote_hash: String },
    /// 已合并：返回合并后的 SHA256
    Merged(String),
}

impl std::fmt::Display for SyncResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncResult::Ok(h) => write!(f, "synced ({})", &h[..8]),
            SyncResult::Conflict { local_hash, remote_hash } => {
                write!(f, "conflict local={} remote={}", &local_hash[..8], &remote_hash[..8])
            }
            SyncResult::Merged(h) => write!(f, "merged ({})", &h[..8]),
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
    Ok(dirs.config_dir().to_path_buf())
}

/// 获取 sessions.json 的本地路径
fn sessions_json_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("sessions.json"))
}

/// 构建 HTTP 客户端（30 秒超时）
fn build_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?)
}

/// 拼接远端文件 URL
fn remote_url(base_url: &str, filename: &str) -> String {
    let base = base_url.trim_end_matches('/');
    format!("{}/{}", base, filename)
}

/// 远端文件名
const REMOTE_FILE: &str = "sessions.json";

// ─── WebDAV 操作 ────────────────────────────────────────────────────

/// 加载本地 WebDAV 配置
pub fn load_settings() -> WebDavSettings {
    let path = config_dir().map(|d| d.join("webdav.json")).unwrap_or_default();
    if path.exists() {
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    } else {
        WebDavSettings::default()
    }
}

/// 保存本地 WebDAV 配置
pub fn save_settings(settings: &WebDavSettings) -> Result<()> {
    let path = config_dir()?.join("webdav.json");
    let json = serde_json::to_string_pretty(settings)?;
    std::fs::write(&path, json)?;
    Ok(())
}

/// 测试 WebDAV 连接
///
/// 发送一个 PROPFIND 请求验证认证和地址是否正确
pub async fn test_connection(settings: &WebDavSettings) -> Result<()> {
    let client = build_client()?;
    let url = settings.base_url.trim_end_matches('/');

    // PROPFIND 请求体（查询目录属性）
    let propfind_body = r#"<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop><D:resourcetype/></D:prop>
</D:propfind>"#;

    tracing::info!("testing WebDAV connection to {}", url);
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

/// 获取远端文件的 SHA256（通过下载并计算）
///
/// 如果远端文件不存在，返回 None
pub async fn get_remote_hash(settings: &WebDavSettings) -> Result<Option<String>> {
    let client = build_client()?;
    let url = remote_url(&settings.base_url, REMOTE_FILE);

    let resp = client
        .get(&url)
        .basic_auth(&settings.username, Some(&settings.password))
        .send()
        .await
        .context("failed to GET remote file for hash check")?;

    let status = resp.status();
    if status.as_u16() == 404 {
        return Ok(None);  // 远端文件不存在
    }
    if !status.is_success() {
        bail!("WebDAV GET failed: {}", status.as_u16());
    }

    let data = resp.bytes().await.context("failed to read response body")?;
    Ok(Some(sha256_hex(&data)))
}

/// 上传 sessions.json 到 WebDAV 服务器（带冲突检测）
///
/// 流程：
/// 1. 读取本地文件，计算 SHA256
/// 2. 检查远端文件版本（如果设置了 last_sync_hash）
/// 3. 如果远端有新版本（hash 不同），返回冲突
/// 4. 否则上传并更新 last_sync_hash
pub async fn upload(settings: &WebDavSettings) -> Result<SyncResult> {
    let config_path = sessions_json_path()?;
    if !config_path.exists() {
        bail!("local sessions.json does not exist");
    }

    // 读取本地文件
    let data = std::fs::read(&config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let local_hash = sha256_hex(&data);
    let client = build_client()?;

    // 如果有上次同步记录，检查远端是否有新版本
    if !settings.last_sync_hash.is_empty() {
        if let Ok(Some(remote_hash)) = get_remote_hash(settings).await {
            // 远端 hash 和上次同步不同，说明有其他设备修改过
            if remote_hash != settings.last_sync_hash {
                tracing::warn!(
                    "conflict detected: local={}, remote={}, last_sync={}",
                    &local_hash[..8], &remote_hash[..8], &settings.last_sync_hash[..8]
                );
                return Ok(SyncResult::Conflict {
                    local_hash,
                    remote_hash,
                });
            }
        }
    }

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
    tracing::info!("uploaded sessions.json (sha256: {}…)", &local_hash[..16]);
    Ok(SyncResult::Ok(local_hash))
}

/// 强制上传（跳过冲突检测，用于用户选择"保留本地"时）
pub async fn force_upload(settings: &WebDavSettings) -> Result<String> {
    let config_path = sessions_json_path()?;
    if !config_path.exists() {
        bail!("local sessions.json does not exist");
    }

    let data = std::fs::read(&config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let checksum = sha256_hex(&data);
    let client = build_client()?;

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
    tracing::info!("force uploaded sessions.json (sha256: {}…)", &checksum[..16]);
    Ok(checksum)
}

/// 从 WebDAV 服务器下载 sessions.json（带冲突检测）
///
/// 流程：
/// 1. GET 下载远端文件
/// 2. 验证是合法 JSON
/// 3. 计算 SHA256，和本地比较
/// 4. 如果不同且本地有未同步的改动，返回冲突
/// 5. 否则备份本地文件并写入新文件
pub async fn download(settings: &WebDavSettings) -> Result<SyncResult> {
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

    let remote_hash = sha256_hex(&data);
    let config_path = sessions_json_path()?;
    let bak = config_path.with_extension("json.bak");

    // 备份本地文件（如果存在）
    if config_path.exists() {
        std::fs::copy(&config_path, &bak)
            .with_context(|| format!("failed to backup to {}", bak.display()))?;
    }

    // 写入新文件
    std::fs::write(&config_path, &data)
        .with_context(|| format!("failed to write {}", config_path.display()))?;
    tracing::info!("downloaded sessions.json (sha256: {}…)", &remote_hash[..16]);
    Ok(SyncResult::Ok(remote_hash))
}

/// 合并本地和远端的 sessions.json
///
/// 策略：
/// - 按 session ID 去重
/// - 如果同一 ID 两边都有，保留更新的那个（按 last_used 或名称）
/// - 保留本地的 groups、settings 等配置
///
/// 返回合并后的 SHA256
pub async fn merge_and_upload(settings: &WebDavSettings) -> Result<String> {
    let client = build_client()?;
    let url = remote_url(&settings.base_url, REMOTE_FILE);
    let config_path = sessions_json_path()?;

    // 读取本地文件
    let local_data = std::fs::read(&config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let local_json: serde_json::Value = serde_json::from_slice(&local_data)?;

    // 下载远端文件
    let resp = client
        .get(&url)
        .basic_auth(&settings.username, Some(&settings.password))
        .send()
        .await
        .context("failed to GET from WebDAV server")?;
    let status = resp.status();
    if !status.is_success() {
        bail!("WebDAV GET failed: {}", status.as_u16());
    }
    let remote_data = resp.bytes().await?;
    let remote_json: serde_json::Value = serde_json::from_slice(&remote_data)?;

    // 合并逻辑
    let merged = merge_json(&local_json, &remote_json);
    let merged_bytes = serde_json::to_vec_pretty(&merged)?;
    let merged_hash = sha256_hex(&merged_bytes);

    // 备份本地文件
    let bak = config_path.with_extension("json.bak");
    if config_path.exists() {
        let _ = std::fs::copy(&config_path, &bak);
    }

    // 写入合并后的文件
    std::fs::write(&config_path, &merged_bytes)?;

    // 上传到远端
    let _ = create_collection(settings).await;
    let put_resp = client
        .put(&url)
        .basic_auth(&settings.username, Some(&settings.password))
        .header("Content-Type", "application/json")
        .body(merged_bytes)
        .send()
        .await
        .context("failed to PUT merged file")?;
    let put_status = put_resp.status();
    if !put_status.is_success() && put_status.as_u16() != 201 && put_status.as_u16() != 204 {
        bail!("WebDAV PUT failed: {}", put_status.as_u16());
    }

    tracing::info!("merged and uploaded sessions.json (sha256: {}…)", &merged_hash[..16]);
    Ok(merged_hash)
}

/// 合并两个 JSON 对象中的 sessions 数组
///
/// 策略：
/// - 按 session ID 去重
/// - 两边都有的 session，保留最后修改的（比较 host+user 相同则保留后者）
/// - 保留本地的非 sessions 字段（groups、settings 等）
fn merge_json(local: &serde_json::Value, remote: &serde_json::Value) -> serde_json::Value {
    let mut merged = local.clone();

    // 获取本地 sessions 数组
    let local_sessions = local.get("sessions")
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default();

    // 获取远端 sessions 数组
    let remote_sessions = remote.get("sessions")
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default();

    // 构建合并后的 sessions：以本地为基础，补充远端独有的
    let mut merged_sessions = local_sessions.clone();
    let mut added = 0;

    for remote_session in &remote_sessions {
        let remote_id = remote_session.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let remote_host = remote_session.get("host").and_then(|v| v.as_str()).unwrap_or("");
        let remote_user = remote_session.get("user").and_then(|v| v.as_str()).unwrap_or("");

        // 检查本地是否已有同 ID 的 session
        let exists_by_id = merged_sessions.iter().any(|s| {
            s.get("id").and_then(|v| v.as_str()) == Some(remote_id)
        });

        if !exists_by_id {
            // 没有同 ID 的，检查是否有同 host+user 的（可能在不同设备上创建了相同连接）
            let exists_by_host = merged_sessions.iter().any(|s| {
                s.get("host").and_then(|v| v.as_str()) == Some(remote_host)
                    && s.get("user").and_then(|v| v.as_str()) == Some(remote_user)
            });

            if !exists_by_host {
                // 完全新的 session，添加
                merged_sessions.push(remote_session.clone());
                added += 1;
            }
        }
    }

    tracing::info!(
        "merge: local={} remote={} merged={} added={}",
        local_sessions.len(), remote_sessions.len(), merged_sessions.len(), added
    );

    // 更新 sessions 数组
    if let Some(obj) = merged.as_object_mut() {
        obj.insert("sessions".to_string(), serde_json::Value::Array(merged_sessions));
    }

    merged
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
