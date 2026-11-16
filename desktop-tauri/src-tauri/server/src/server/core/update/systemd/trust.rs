//! 升级程序自己的信任链：只认管理员配置的仓库或内置默认仓库，不读网关的缓存库，
//! 也不跟随上游索引（见 [`fetch`] 里的说明）。

use std::time::Duration;

use futures::StreamExt;
use serde_json::Value;

use crate::server::core::update::{integrity, version, UpdateError, MAX_INSTALLER_BYTES};

pub(super) struct VerifiedRelease {
    pub sha256: String,
    pub size: u64,
}

#[cfg(target_os = "linux")]
pub(super) fn verify(version: &str, token: Option<&str>) -> Result<VerifiedRelease, UpdateError> {
    verify_from(version, token, std::env::var("AGENT2API_UPDATER_REPO").ok().as_deref())
}

pub(super) fn verify_from(version: &str, token: Option<&str>, repository: Option<&str>) -> Result<VerifiedRelease, UpdateError> {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()
        .map_err(|error| UpdateError::new(format!("无法初始化升级校验：{error}")))?;
    runtime.block_on(fetch(version, token, repository))
}

async fn fetch(tag: &str, token: Option<&str>, repository: Option<&str>) -> Result<VerifiedRelease, UpdateError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("agent2api-systemd-updater")
        .build().map_err(|error| UpdateError::new(error.to_string()))?;
    // 仓库来源：`AGENT2API_UPDATER_REPO` > 内置 `DEFAULT_REPO`（本 fork）。
    //
    // ── 为什么不再读官方索引 ────────────────────────────────────
    // 索引里的 `app.repo` 指向**上游**构建仓库；本 fork 的 Release 不在那里，
    // 照索引走会去上游找一个本 fork 从未发布过的包（或更糟：装上官方版把
    // 本 fork 覆盖掉）。升级程序的信任链必须指向真正发布本 fork 产物的仓库，
    // 这个值只能来自内置常量或管理员显式配置。
    let repository = match repository.filter(|value| !value.trim().is_empty()) {
        Some(value) => value.trim().to_string(),
        None => version::DEFAULT_REPO.to_string(),
    };
    if !valid_repo(&repository) { return Err(UpdateError::new("管理员配置的更新仓库无效")); }
    let url = format!("https://api.github.com/repos/{repository}/releases/tags/v{tag}");
    let release: Value = serde_json::from_slice(&get(&client, &url, token, 2 * 1024 * 1024).await?)
        .map_err(|_| UpdateError::new("官方发布信息格式异常"))?;
    let expected_tag = format!("v{tag}");
    if release["draft"] == true || release["tag_name"].as_str() != Some(expected_tag.as_str()) {
        return Err(UpdateError::new("官方发布版本与待安装版本不符"));
    }
    let name = version::server_bundle_name().ok_or_else(|| UpdateError::new("当前架构没有服务端包"))?;
    let asset = release["assets"].as_array().and_then(|items| items.iter().find(|item| item["name"] == name))
        .ok_or_else(|| UpdateError::new("官方发布没有对应架构的服务端更新包"))?;
    let wanted_url = format!("https://github.com/{repository}/releases/download/v{tag}/{name}");
    if asset["browser_download_url"].as_str() != Some(wanted_url.as_str()) {
        return Err(UpdateError::new("官方更新包地址与版本不匹配"));
    }
    let size = asset["size"].as_u64().filter(|size| *size > 0 && *size <= MAX_INSTALLER_BYTES)
        .ok_or_else(|| UpdateError::new("官方更新包大小异常"))?;
    let sha256 = asset["digest"].as_str().and_then(integrity::parse_digest)
        .ok_or_else(|| UpdateError::new("未取得官方校验值，未修改现有程序；请稍后重试"))?;
    Ok(VerifiedRelease { sha256, size })
}

fn valid_repo(value: &str) -> bool {
    let parts: Vec<_> = value.split('/').collect();
    parts.len() == 2 && parts.iter().all(|part| !part.is_empty() && *part != "." && *part != ".."
        && part.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')))
}

async fn get(client: &reqwest::Client, url: &str, token: Option<&str>, limit: usize) -> Result<Vec<u8>, UpdateError> {
    let mut request = client.get(url);
    if let Some(token) = token.filter(|value| !value.is_empty()) {
        if token.len() > 255 || !token.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')) {
            return Err(UpdateError::new("GitHub 凭证格式不正确"));
        }
        request = request.bearer_auth(token);
    }
    let response = request.send().await.map_err(|error| UpdateError::new(format!("官方校验请求失败：{error}")))?;
    if !response.status().is_success() {
        return Err(UpdateError::new(format!("官方校验返回 HTTP {}，现有程序未修改", response.status().as_u16())));
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| UpdateError::new(error.to_string()))?;
        if bytes.len().saturating_add(chunk.len()) > limit { return Err(UpdateError::new("官方校验响应过大")); }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
