//! 官方配套插件的最低版本校验独立于日常更新偏好。
//!
//! ── 下限是「插件协议代际」，不是网关产品版本 ──────────────────
//! 这里原先要求插件版本 **不低于网关自身版本**（`CURRENT_VERSION`）。那条
//! 判据只在「插件与网关同一发版节奏」时成立；一旦网关单独发版（本 fork 的
//! 3.5.x 就是这样：只改了报表、更新源与构建，`plugin_host/**` 一行未动，
//! `agent2api-provider-protocol` 仍钉 `v3.4.0`），它会把上一代**完全可用**的
//! 插件一律判成「低于要求」，结果是所有提供商在启动时被 `start_pack` 直接
//! 跳过、转发面全部 503。
//!
//! 真正决定兼容性的是**通信协议代际**：`agent2api-provider-protocol` 的
//! `PROTOCOL_VERSION`（握手时交换），以及 `install::validate_manifest` 对入口
//! 二进制的复核。两者都通过，插件就能跑。因此下限写成一个固定常量
//! [`MIN_PLUGIN_VERSION`]，**只在插件面向网关的 RPC 面发生变化时才上调**。
//!
//! 上游 3.5.0-beta.1 走的是同一条路（模块头改成「兼容性只由公共接口决定，
//! 不比较插件与主程序的产品版本号」），此处保留版本闸门的形状是为了不拆掉
//! 「补齐插件」的既有链路。

use serde_json::{json, Value};

use super::install::{InstalledPack, PackRegistry};
use super::{operations, update, PackSummary};
use crate::server::core::task_state::{self, Claim, ManualBackoff};
use crate::server::core::update::compare_versions;
use crate::server::logging;

pub(super) const RETRY_INTERVAL_MS: i64 = 5 * 60_000;
const OFFICIAL_PACKS: &[&str] = &["agent2api-providers", "agent2api-web-providers"];

/// 官方配套插件的最低可用版本（**插件协议代际**，与网关产品版本解耦）。
///
/// `3.4.0` = `agent2api-provider-protocol` 当前所钉 tag 的那一代；其后各代
/// （3.4.1 / 3.4.2 / 3.4.3 …）都建立在这套协议之上，彼此可互换。
/// 改这个值的唯一理由是**插件面向网关的 RPC 面**不兼容地变了 —— 那时新旧
/// 插件本就不能混用，上调下限、让旧插件走「补齐插件」才是对的。
pub(super) const MIN_PLUGIN_VERSION: &str = "3.4.0";

pub(super) fn minimum_version(id: &str) -> Option<&'static str> {
    OFFICIAL_PACKS.contains(&id).then_some(MIN_PLUGIN_VERSION)
}

pub(super) fn version_error(id: &str, version: &str) -> Option<String> {
    let minimum = minimum_version(id)?;
    if matches!(compare_versions(version, minimum), Some(0 | 1)) {
        return None;
    }
    Some(format!(
        "插件「{id}」v{version} 低于当前网关要求的 v{minimum}，补齐前暂不加载；请在插件页查看进度或重试"
    ))
}

pub(crate) fn provider_error(provider: &str) -> Option<String> {
    let registry = PackRegistry::load();
    let owner = super::ownership::owners(&registry);
    let id = owner.get(provider)?;
    let pack = registry.packs.iter().find(|pack| &pack.id == id)?;
    version_error(&pack.id, &pack.version)
}

/// 补齐任务的持久化键。带下限版本而不是网关版本：网关发版不该让一个
/// 已经下载好的候选包失效（口径见模块头）。
fn key(id: &str) -> String {
    format!("pluginRequirement:{id}:{MIN_PLUGIN_VERSION}:{}", super::host_target())
}

fn cached_asset(state: &task_state::TaskState, id: &str) -> Option<update::PackAsset> {
    let asset: update::PackAsset = serde_json::from_value(state.value.as_ref()?.clone()).ok()?;
    let minimum = minimum_version(id)?;
    (matches!(compare_versions(&asset.version, minimum), Some(0 | 1))
        && asset.name == update::asset_name(id, &asset.version, &super::host_target())
        && asset.require_signed)
        .then_some(asset)
}

pub(super) fn update_for(pack: &PackSummary) -> Option<Value> {
    if !pack.update_required {
        return None;
    }
    let state = task_state::read(&key(&pack.id)).unwrap_or_default();
    let asset = cached_asset(&state, &pack.id);
    let task = operations::snapshot()["operations"].as_array()
        .and_then(|tasks| tasks.iter().find(|task| task["id"] == pack.id
            && task["requiredVersion"].as_str() == Some(MIN_PLUGIN_VERSION)))
        .cloned()
        .unwrap_or(Value::Null);
    let operation_error = task["error"].as_str().or_else(|| task["hostError"].as_str());
    let error = state.last_error.as_deref().or(operation_error);
    let operation_state = task_state::read(&format!("pluginOperation:{}", pack.id)).unwrap_or_default();
    let reason = error.map(|error| update::describe_error(error, state.retry_at)).unwrap_or_else(|| {
        if pack.signed {
            "正在自动补齐当前网关所需的官方插件；日常更新开关不影响本次修复".to_string()
        } else {
            "未签名包不会被自动覆盖，请点击补齐插件或从文件安装符合最低版本要求的包".to_string()
        }
    });
    Some(json!({
        "id": pack.id, "current": pack.version, "latest": asset.as_ref().map(|asset| &asset.version),
        "required": true, "requiredVersion": pack.required_version,
        "repairing": state.running() || operations::active(&pack.id),
        "available": asset.is_some(), "checked": state.last_success_at > 0,
        "checkedAt": state.last_success_at, "retryAt": state.retry_at.max(operation_state.retry_at),
        "asset": asset.as_ref().map(|asset| json!({
            "target": super::host_target(), "url": asset.url, "size": asset.size,
            "current": true, "requireSigned": true,
        })),
        "page": asset.as_ref().map(|asset| &asset.page),
        "prerelease": asset.as_ref().is_some_and(|asset| {
            crate::server::core::update::is_prerelease_version(&asset.version)
        }),
        "reason": reason, "error": error.is_some(),
    }))
}

async fn find_asset(id: &str) -> Result<update::PackAsset, String> {
    let mut detail = update::PluginDetail {
        protocol: agent2api_provider_protocol::PROTOCOL_VERSION,
        repo: update::FALLBACK_REPO.to_string(),
        tag_template: if id == "agent2api-web-providers" { "web-v{version}" } else { "v{version}" }.to_string(),
        asset_template: "{id}-{version}-{target}.zip".to_string(),
        homepage: String::new(),
        require_signed: true,
    };
    match update::read_index_result().await {
        Ok(entries) => {
            if let Some(entry) = entries.into_iter().find(|entry| entry.id == id) {
                if !entry.official {
                    return Err(format!("插件「{id}」在当前索引中不是官方包，未自动替换"));
                }
                if let Some(remote) = update::read_detail(&entry.manifest).await {
                    detail = remote;
                }
                if !entry.repo.is_empty() {
                    detail.repo = entry.repo;
                }
            }
        }
        Err(error) => logging::verbose("[Plugin]", &format!("插件索引读取失败，尝试内置官方配套源：{error}")),
    }
    let minimum = minimum_version(id).ok_or_else(|| "这个插件不属于官方配套包".to_string())?;
    update::required_release(&detail.repo, id, &detail, minimum, &super::host_target()).await
}

async fn resolve(id: &str, manual: bool) -> Result<update::PackAsset, String> {
    let state = task_state::read(&key(id))?;
    if state.last_error.is_none()
        && logging::now_ms().saturating_sub(state.last_success_at) < RETRY_INTERVAL_MS
    {
        if let Some(asset) = cached_asset(&state, id) {
            return Ok(asset);
        }
    }
    let guard = match task_state::claim(
        &key(id), RETRY_INTERVAL_MS, manual, ManualBackoff::Respect, 60_000,
    )? {
        Claim::Acquired(guard) => guard,
        Claim::Deferred(state) => return Err(state.last_error.as_deref()
            .map(|error| update::describe_error(error, state.retry_at))
            .unwrap_or_else(|| update::waiting_message(&state))),
    };
    match find_asset(id).await {
        Ok(asset) => {
            guard.finish(
                true, format!("找到官方配套插件 v{}", asset.version),
                Some(json!(asset)), 0, RETRY_INTERVAL_MS,
            )?;
            Ok(asset)
        }
        Err(error) => {
            guard.finish(false, error.clone(), None, 0, RETRY_INTERVAL_MS)?;
            Err(error)
        }
    }
}

fn installed(id: &str, automatic: bool) -> Result<InstalledPack, String> {
    let pack = PackRegistry::load().packs.into_iter().find(|pack| pack.id == id)
        .ok_or_else(|| "还没有安装这个插件包".to_string())?;
    if version_error(id, &pack.version).is_none() {
        return Err("插件已满足当前网关的最低版本要求，不需要补齐".to_string());
    }
    if automatic && !pack.signed {
        return Err("未签名包需要用户手动确认更新，未自动覆盖".to_string());
    }
    Ok(pack)
}

pub(super) async fn start(id: &str, manual: bool) -> Result<operations::Operation, String> {
    installed(id, !manual)?;
    if operations::active(id) {
        return Err("这个插件正在下载或安装，请稍候".to_string());
    }
    let operation_state = task_state::read(&format!("pluginOperation:{id}"))?;
    if operation_state.running() || operation_state.retry_at > logging::now_ms() {
        return Err(operation_state.waiting_message());
    }
    let asset = resolve(id, manual).await?;
    installed(id, !manual)?;
    operations::start_required(id, asset, !manual)
}

pub(super) async fn apply() {
    let Ok(states) = task_state::read_all() else { return };
    let now = logging::now_ms();
    let packs = PackRegistry::load();
    for pack in packs.packs {
        if !pack.signed || version_error(&pack.id, &pack.version).is_none()
            || operations::active(&pack.id)
            || states.get(&key(&pack.id)).is_some_and(|state| state.running() || now < state.due_at())
            || states.get(&format!("pluginOperation:{}", pack.id))
                .is_some_and(|state| state.running() || now < state.retry_at)
        {
            continue;
        }
        if let Err(error) = start(&pack.id, false).await {
            logging::verbose("[Plugin]", &format!("最低版本补齐 {}：{error}", pack.id));
            continue;
        }
        while operations::active(&pack.id) {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }
}

pub(super) fn clear_cooldown() {
    for id in OFFICIAL_PACKS {
        let _ = task_state::clear_cooldown(&key(id));
        let _ = task_state::store_value(&key(id), Value::Null);
        let _ = task_state::schedule_now(&key(id));
        let _ = task_state::clear_cooldown(&format!("pluginOperation:{id}"));
        let _ = task_state::schedule_now(&format!("pluginOperation:{id}"));
    }
    super::catalog::wake();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本 fork 的痛点：网关单独发版后，上一代插件必须仍然可用。
    ///
    /// 3.5.x 只动了报表、更新源与 CI，`plugin_host/**` 与协议 tag 都没变；
    /// 若下限继续跟着 `CURRENT_VERSION` 走，这些包会被判成「太旧」而拒绝加载，
    /// 表现为所有提供商 503。这条用例把「产品版本 ≠ 兼容判据」钉住。
    #[test]
    fn older_but_compatible_packs_still_pass() {
        for version in ["3.4.0", "3.4.1", "3.4.2", "3.4.3", "3.5.0", "3.6.1"] {
            assert!(
                version_error("agent2api-providers", version).is_none(),
                "v{version} 应当通过兼容校验（下限是插件协议代际，不是网关版本）"
            );
        }
    }

    #[test]
    fn packs_below_the_protocol_generation_are_rejected() {
        for version in ["3.3.0", "3.3.0-beta.6", "3.0.9"] {
            assert!(
                version_error("agent2api-providers", version).is_some(),
                "v{version} 属于旧协议代际，应当被判为需要补齐"
            );
        }
    }

    #[test]
    fn non_official_packs_are_not_version_gated() {
        assert!(minimum_version("some-third-party-pack").is_none());
        assert!(version_error("some-third-party-pack", "0.0.1").is_none());
    }

    /// 下限必须与网关产品版本解耦：网关升到任何版本，这个值都不应跟着变。
    #[test]
    fn floor_is_independent_of_the_gateway_version() {
        assert_eq!(minimum_version("agent2api-providers"), Some(MIN_PLUGIN_VERSION));
        assert_eq!(minimum_version("agent2api-web-providers"), Some(MIN_PLUGIN_VERSION));
        assert_ne!(
            MIN_PLUGIN_VERSION,
            crate::server::core::update::CURRENT_VERSION,
            "下限不应等于网关版本；若两者偶然相等，这条断言会提醒你确认是不是又绑回去了"
        );
    }
}
