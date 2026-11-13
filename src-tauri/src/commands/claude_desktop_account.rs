//! Claude Desktop（macOS）官方登录的多账号切换 + 工作区同步。
//!
//! 切换 / 同步要退出并重新打开 Desktop、复制浏览器状态，耗时几秒，都放到阻塞线程里跑。

use std::sync::Arc;

use crate::services::claude_desktop_account::{
    ClaudeDesktopAccountManager, DesktopAccountsStatus, SwitchReport, WorkspaceReport,
};

pub struct ClaudeDesktopAccountState(pub Arc<ClaudeDesktopAccountManager>);

async fn blocking<T, F>(state: &ClaudeDesktopAccountState, f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(&ClaudeDesktopAccountManager) -> Result<T, crate::error::AppError> + Send + 'static,
{
    let manager = state.0.clone();
    tokio::task::spawn_blocking(move || f(&manager))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn claude_desktop_accounts_status(
    state: tauri::State<'_, ClaudeDesktopAccountState>,
) -> Result<DesktopAccountsStatus, String> {
    blocking(&state, |manager| Ok(manager.status())).await
}

#[tauri::command]
pub async fn claude_desktop_account_switch(
    label: String,
    state: tauri::State<'_, ClaudeDesktopAccountState>,
) -> Result<SwitchReport, String> {
    blocking(&state, move |manager| manager.switch(label.trim())).await
}

#[tauri::command]
pub async fn claude_desktop_account_save_current(
    label: String,
    email: Option<String>,
    state: tauri::State<'_, ClaudeDesktopAccountState>,
) -> Result<String, String> {
    blocking(&state, move |manager| {
        manager.save_current(label.trim(), email)
    })
    .await
}

#[tauri::command]
pub async fn claude_desktop_account_remove(
    label: String,
    state: tauri::State<'_, ClaudeDesktopAccountState>,
) -> Result<(), String> {
    blocking(&state, move |manager| manager.remove(label.trim())).await
}

#[tauri::command]
pub async fn claude_desktop_workspace_sync(
    state: tauri::State<'_, ClaudeDesktopAccountState>,
) -> Result<WorkspaceReport, String> {
    blocking(&state, |manager| manager.sync_workspace()).await
}

#[tauri::command]
pub async fn claude_desktop_account_capture_start(
    label: String,
    email: Option<String>,
    state: tauri::State<'_, ClaudeDesktopAccountState>,
) -> Result<(), String> {
    blocking(&state, move |manager| {
        manager.start_capture(label.trim(), email)
    })
    .await
}

#[tauri::command]
pub fn claude_desktop_account_capture_cancel(state: tauri::State<'_, ClaudeDesktopAccountState>) {
    state.0.cancel_capture();
}

#[tauri::command]
pub fn claude_desktop_account_capture_dismiss(state: tauri::State<'_, ClaudeDesktopAccountState>) {
    state.0.dismiss_capture();
}
