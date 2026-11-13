//! Claude Desktop（macOS）官方登录的专用命令。列表、删除走授权中心的通用 `auth_*` 命令
//! （provider id `claude_desktop`）；切换账号靠启用绑定了它的 Claude Desktop 官方卡。
//! 这里放 Desktop 特有的：运行状态、收录当前登录、登录另一个账号、同步工作区、查额度。

use std::sync::Arc;

use crate::services::claude_desktop_account::{
    ClaudeDesktopAccountManager, DesktopLiveState, ImportOutcome, WorkspaceReport,
};
use crate::services::subscription::ClaudeAccountQuota;
use crate::store::AppState;

/// 托盘 / 卡片额度缓存里 Desktop 账号的键，和 Claude Code 账号的键分开。
pub fn claude_desktop_quota_cache_key(account_id: &str) -> String {
    format!("desktop:{account_id}")
}

fn manager() -> Arc<ClaudeDesktopAccountManager> {
    ClaudeDesktopAccountManager::global()
}

async fn blocking<T, F>(job: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(&ClaudeDesktopAccountManager) -> Result<T, crate::error::AppError> + Send + 'static,
{
    let manager = manager();
    tokio::task::spawn_blocking(move || job(&manager))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn claude_desktop_account_live_state() -> Result<DesktopLiveState, String> {
    blocking(|manager| Ok(manager.live_state())).await
}

/// 收录 Desktop 当前登录的账号（已收录的直接返回）。Desktop 会短暂重启。
#[tauri::command]
pub async fn claude_desktop_account_import_current() -> Result<ImportOutcome, String> {
    manager()
        .import_current()
        .await
        .map_err(|error| error.to_string())
}

/// 登录另一个账号：Desktop 退出登录后重新打开，用户登录后自动收录；进度看 live state。
#[tauri::command]
pub async fn claude_desktop_account_capture_start() -> Result<(), String> {
    let manager = manager();
    tokio::task::spawn_blocking(move || manager.start_capture())
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn claude_desktop_account_capture_cancel() {
    manager().cancel_capture();
}

#[tauri::command]
pub fn claude_desktop_account_capture_dismiss() {
    manager().dismiss_capture();
}

/// 把所有账号的会话、定时任务、工作区预设并进当前登录的账号（Desktop 在运行时会重启）。
#[tauri::command]
pub async fn claude_desktop_workspace_sync() -> Result<WorkspaceReport, String> {
    blocking(|manager| manager.sync_workspace()).await
}

/// 这个 Desktop 账号的额度。其他账号的令牌快过期时自动续期（写回 CC Switch 的档案）；
/// 结果按账号写进托盘缓存，卡片和托盘浮窗看到的是同一份。
#[tauri::command(rename_all = "camelCase")]
pub async fn get_claude_desktop_account_quota(
    app: tauri::AppHandle,
    app_state: tauri::State<'_, AppState>,
    account_id: String,
) -> Result<ClaudeAccountQuota, String> {
    let account_id = account_id.trim().to_string();
    let result = manager().quota(&account_id).await;
    if let Ok(quota) = &result {
        app_state.usage_cache.put_claude_account(
            claude_desktop_quota_cache_key(&account_id),
            quota.quota.clone(),
        );
        crate::tray::schedule_tray_refresh(&app);
    }
    result
}
