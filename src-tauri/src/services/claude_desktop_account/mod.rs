//! Claude Desktop（macOS）官方登录的多账号切换，切换时把本机的 Code 工作区一并同步过去。
//!
//! Desktop 只有一个登录槽位：账号身份在 `config.json` 的 `oauth:tokenCache(V2)` /
//! `lastKnownAccountUuid`，再加上 Cookie 和 `Local Storage` / `Session Storage` / `IndexedDB`
//! 这些浏览器状态。切换就是：退出 Desktop → 把当前账号的这些东西存回它的档案 → 合并工作区 →
//! 装上目标账号的档案 → 重新打开 Desktop。每次切换前都会写一份回滚归档，中途失败自动还原。
//!
//! 「工作区同步」指 Code 页里按账号分开存放的内容：
//! - `claude-code-sessions/<account>/<org>/local_*.json` 会话索引（按 `lastActivityAt` 新的赢）；
//! - 同目录的 `archived-sessions.idx`（跟着会话自己的 `isArchived` 重建）；
//! - 同目录的 `scheduled-tasks.json` 定时任务（按 id 三方合并）；
//! - `ccd-session-presets.json` 里按账号存放的工作区文件夹预设。
//!
//! 会话正文在 `~/.claude/projects/`，本来就不分账号，不需要搬。
//!
//! 档案目录沿用 claude-acc（<https://github.com/ohmaseclaro/claude-acc>，MIT）的
//! `~/.claude-acc/profiles/<label>/` 格式，与 claude-acc、ai-usagebar
//! （<https://github.com/akitaonrails/ai-usagebar>，MIT）互通：用它们保存过的账号这里直接可用。

pub mod app;
pub mod merge;
pub mod token;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::AppError;
use app::AppControl;

const CONFIG_JSON: &str = "config.json";
const SESSIONS_DIR: &str = "claude-code-sessions";
const PRESETS_JSON: &str = "ccd-session-presets.json";
const COOKIE_FILES: [&str; 2] = ["Cookies", "Cookies-journal"];
const LEVELDB_DIRS: [&str; 3] = ["Local Storage", "Session Storage", "IndexedDB"];
/// 远程控制桥接状态：里面的会话 id 很快失效，切换时删掉、从不存进档案。
const BRIDGE_FILE: &str = "bridge-state.json";
/// 按账号存放的浏览器扩展设备登记，只做增量合并。
const DEVICE_REGISTRY: &str = "ant-device-registry.json";

const TOKEN_CACHE: &str = "config-tokenCache";
const TOKEN_CACHE_V2: &str = "config-tokenCacheV2";
const DESKTOP_STATE: &str = "desktop-state";
const META_JSON: &str = "meta.json";
/// 回滚归档保留份数。
const KEEP_BACKUPS: usize = 10;

/// 添加账号时等用户登录、等组织目录出现的时间。
const LOGIN_WAIT: Duration = Duration::from_secs(5 * 60);
const ORG_WAIT: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_secs(2);

fn err(message: impl Into<String>) -> AppError {
    AppError::Message(message.into())
}

// ───────────────────────────── 路径 ─────────────────────────────

#[derive(Debug, Clone)]
pub struct Paths {
    pub data_dir: PathBuf,
    pub profiles_dir: PathBuf,
    pub backups_dir: PathBuf,
}

impl Paths {
    pub fn resolve() -> Self {
        let home = crate::config::get_home_dir();
        let store = home.join(".claude-acc");
        Self {
            data_dir: home.join("Library/Application Support/Claude"),
            profiles_dir: store.join("profiles"),
            backups_dir: store.join("backups"),
        }
    }

    #[cfg(test)]
    pub fn at(data_dir: PathBuf, store: &Path) -> Self {
        Self {
            data_dir,
            profiles_dir: store.join("profiles"),
            backups_dir: store.join("backups"),
        }
    }

    pub fn installed(&self) -> bool {
        self.data_dir.is_dir()
    }
    fn config_json(&self) -> PathBuf {
        self.data_dir.join(CONFIG_JSON)
    }
    fn sessions_root(&self) -> PathBuf {
        self.data_dir.join(SESSIONS_DIR)
    }
    fn presets_path(&self) -> PathBuf {
        self.data_dir.join(PRESETS_JSON)
    }
    fn profile_dir(&self, label: &str) -> PathBuf {
        self.profiles_dir.join(label)
    }
    /// 与 claude-acc / ai-usagebar 共用的同步账本。
    fn synced_path(&self) -> PathBuf {
        self.backups_dir.with_file_name("synced.json")
    }
    /// 添加账号前把当前登录整份备份到这里，取消或超时就还原。
    fn prelogin_dir(&self) -> PathBuf {
        self.backups_dir.with_file_name("prelogin-backup")
    }
}

// ───────────────────────────── 档案 ─────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileMeta {
    pub label: String,
    pub email: Option<String>,
    pub account_uuid: String,
    pub org_uuid: Option<String>,
    pub saved_at: Option<i64>,
    pub has_credentials: bool,
    pub has_desktop_state: bool,
}

#[derive(Deserialize)]
struct RawMeta {
    email: Option<String>,
    #[serde(rename = "accountUuid")]
    account_uuid: Option<String>,
    #[serde(rename = "orgUuid")]
    org_uuid: Option<String>,
    #[serde(rename = "savedAt")]
    saved_at: Option<i64>,
}

/// 所有档案，按名字排序。单个 `meta.json` 坏了只跳过它。
pub fn load_profiles(profiles_dir: &Path) -> Vec<ProfileMeta> {
    let Ok(entries) = std::fs::read_dir(profiles_dir) else {
        return Vec::new();
    };
    let mut profiles: Vec<ProfileMeta> = entries
        .flatten()
        .filter_map(|entry| {
            let dir = entry.path();
            let label = dir.file_name()?.to_str()?.to_string();
            let raw: RawMeta =
                serde_json::from_slice(&std::fs::read(dir.join(META_JSON)).ok()?).ok()?;
            Some(ProfileMeta {
                label,
                email: raw.email.filter(|e| !e.is_empty()),
                account_uuid: raw.account_uuid.filter(|u| !u.is_empty())?,
                org_uuid: raw.org_uuid.filter(|u| !u.is_empty()),
                saved_at: raw.saved_at,
                has_credentials: dir.join(TOKEN_CACHE).is_file()
                    || dir.join(TOKEN_CACHE_V2).is_file(),
                has_desktop_state: dir.join(DESKTOP_STATE).is_dir(),
            })
        })
        .collect();
    profiles.sort_by(|a, b| a.label.cmp(&b.label));
    profiles
}

/// 档案名就是目录名：不能有路径分隔符、不能以点开头。
pub fn validate_label(label: &str) -> Result<(), AppError> {
    let ok = !label.is_empty()
        && label.chars().count() <= 64
        && !label.starts_with('.')
        && !label
            .chars()
            .any(|c| c == '/' || c == '\\' || c == ':' || c.is_control());
    if ok {
        Ok(())
    } else {
        Err(err(format!(
            "账号名称 {label:?} 无效：不能为空、不能以点开头、不能包含 / \\ : 或控制字符，最长 64 个字符"
        )))
    }
}

fn read_config(paths: &Paths) -> Option<Vec<u8>> {
    std::fs::read(paths.config_json()).ok()
}

fn active_account(paths: &Paths) -> Option<String> {
    merge::logged_in_account(&read_config(paths)?)
}

fn label_for_uuid<'a>(profiles: &'a [ProfileMeta], uuid: &str) -> Option<&'a str> {
    profiles
        .iter()
        .find(|p| p.account_uuid == uuid)
        .map(|p| p.label.as_str())
}

/// 账号在会话树里的所有组织目录，再加上档案里记的那个。
fn account_orgs(paths: &Paths, account_uuid: &str, meta_org: Option<&str>) -> Vec<String> {
    let mut orgs: BTreeSet<String> = std::fs::read_dir(paths.sessions_root().join(account_uuid))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
        .collect();
    if let Some(org) = meta_org {
        orgs.insert(org.to_string());
    }
    orgs.into_iter().collect()
}

fn session_count(paths: &Paths, account_uuid: &str) -> usize {
    let names: BTreeSet<String> = account_orgs(paths, account_uuid, None)
        .iter()
        .flat_map(|org| {
            merge::local_session_files(&paths.sessions_root().join(account_uuid).join(org))
        })
        .filter_map(|path| merge::file_name(&path))
        .collect();
    names.len()
}

// ───────────────────────────── 状态 ─────────────────────────────

/// 一个保存过的 Desktop 账号。`id` 是账号 UUID：卡片绑定存的就是它，换台电脑也认得出。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopAccount {
    pub id: String,
    /// 档案目录名（默认是邮箱）
    pub label: String,
    pub email: Option<String>,
    pub org_uuid: Option<String>,
    /// 秒
    pub saved_at: Option<i64>,
    pub session_count: usize,
    pub is_active: bool,
    /// 有凭据也有浏览器状态，能切过去
    pub usable: bool,
}

impl DesktopAccount {
    /// 界面上显示的名字：有邮箱用邮箱，否则用档案名。
    pub fn display_name(&self) -> String {
        self.email.clone().unwrap_or_else(|| self.label.clone())
    }
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CapturePhase {
    #[default]
    Idle,
    Preparing,
    WaitingLogin,
    WaitingOrg,
    Saving,
    Done,
    Refreshed,
    Cancelled,
    TimedOut,
    Failed,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureStatus {
    pub phase: CapturePhase,
    /// 收录成的账号（完成后才有）
    pub account_id: Option<String>,
    pub label: Option<String>,
    pub message: Option<String>,
    pub started_at: Option<i64>,
}

impl CaptureStatus {
    fn in_progress(&self) -> bool {
        matches!(
            self.phase,
            CapturePhase::Preparing
                | CapturePhase::WaitingLogin
                | CapturePhase::WaitingOrg
                | CapturePhase::Saving
        )
    }
}

/// Desktop 现在的样子，授权中心轮询它。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopLiveState {
    /// 只有 macOS 支持
    pub supported: bool,
    /// 本机装过并运行过 Claude Desktop
    pub installed: bool,
    pub running: bool,
    /// Desktop 登着的账号 UUID
    pub active_account_id: Option<String>,
    /// Desktop 已登录，但这个账号还没收录
    pub active_unmanaged: bool,
    pub capture: CaptureStatus,
    /// 正在切换 / 同步 / 添加
    pub busy: bool,
}

// ───────────────────────────── 结果 ─────────────────────────────

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceReport {
    pub sessions_copied: usize,
    pub sessions_updated: usize,
    pub sessions_skipped_deleted: usize,
    pub routines_added: usize,
    pub routines_updated: usize,
    pub routine_conflicts: usize,
    pub presets_added: usize,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SwitchReport {
    pub label: String,
    pub previous_label: Option<String>,
    pub workspace: WorkspaceReport,
    pub archive: Option<String>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportOutcome {
    pub account: DesktopAccount,
    /// false：这个账号之前就收录过
    pub created: bool,
}

// ───────────────────────────── 管理器 ─────────────────────────────

pub struct ClaudeDesktopAccountManager {
    paths: Paths,
    app: Arc<dyn AppControl>,
    busy: Arc<AtomicBool>,
    capture: Arc<Mutex<CaptureStatus>>,
    cancel: Arc<AtomicBool>,
    /// 账号 UUID → 从 Anthropic 查到的邮箱、套餐（只在内存里，查一次用到退出）
    info: Mutex<BTreeMap<String, token::AccountInfo>>,
}

/// 同一时间只做一件事；离开作用域自动放开。
pub(crate) struct BusyGuard(Arc<AtomicBool>);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

static GLOBAL: std::sync::OnceLock<Arc<ClaudeDesktopAccountManager>> = std::sync::OnceLock::new();

impl ClaudeDesktopAccountManager {
    pub fn new(paths: Paths, app: Arc<dyn AppControl>) -> Self {
        Self {
            paths,
            app,
            busy: Arc::new(AtomicBool::new(false)),
            capture: Arc::new(Mutex::new(CaptureStatus::default())),
            cancel: Arc::new(AtomicBool::new(false)),
            info: Mutex::new(BTreeMap::new()),
        }
    }

    /// 全进程共用一个：命令、切卡、托盘都经它，切换和续期才能互斥。
    pub fn global() -> Arc<Self> {
        GLOBAL
            .get_or_init(|| Arc::new(Self::new(Paths::resolve(), Arc::new(app::DesktopApp))))
            .clone()
    }

    fn ensure_supported(&self) -> Result<(), AppError> {
        if !cfg!(target_os = "macos") && !cfg!(test) {
            return Err(err("Claude Desktop 多账号切换目前只支持 macOS"));
        }
        if !self.paths.installed() {
            return Err(err(format!(
                "没有找到 Claude Desktop 的数据目录 {}，请先安装并登录一次 Claude Desktop",
                self.paths.data_dir.display()
            )));
        }
        Ok(())
    }

    fn acquire(&self) -> Result<BusyGuard, AppError> {
        if self
            .busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(err("另一个 Claude Desktop 账号操作正在进行，请稍后再试"));
        }
        Ok(BusyGuard(self.busy.clone()))
    }

    fn set_capture(&self, update: impl FnOnce(&mut CaptureStatus)) {
        set_capture(&self.capture, update);
    }

    fn view(&self, profile: &ProfileMeta, active: Option<&str>) -> DesktopAccount {
        let cached_email = self.info.lock().ok().and_then(|info| {
            info.get(&profile.account_uuid)
                .and_then(|i| i.email.clone())
        });
        DesktopAccount {
            id: profile.account_uuid.clone(),
            label: profile.label.clone(),
            email: profile.email.clone().or(cached_email),
            org_uuid: profile.org_uuid.clone(),
            saved_at: profile.saved_at,
            session_count: session_count(&self.paths, &profile.account_uuid),
            is_active: active == Some(profile.account_uuid.as_str()),
            usable: profile.has_credentials && profile.has_desktop_state,
        }
    }

    /// 收录过的账号；同一个账号存了多份档案时只列一份。
    pub fn accounts(&self) -> Vec<DesktopAccount> {
        let active = active_account(&self.paths);
        let mut seen = BTreeSet::new();
        load_profiles(&self.paths.profiles_dir)
            .iter()
            .filter(|profile| seen.insert(profile.account_uuid.clone()))
            .map(|profile| self.view(profile, active.as_deref()))
            .collect()
    }

    pub fn account(&self, account_id: &str) -> Option<DesktopAccount> {
        self.accounts().into_iter().find(|a| a.id == account_id)
    }

    /// Desktop 现在登着的账号 UUID。
    pub fn active_account_id(&self) -> Option<String> {
        active_account(&self.paths)
    }

    pub fn live_state(&self) -> DesktopLiveState {
        let supported = cfg!(target_os = "macos");
        let installed = self.paths.installed();
        let active = active_account(&self.paths);
        let profiles = load_profiles(&self.paths.profiles_dir);
        let managed = active
            .as_deref()
            .is_some_and(|uuid| label_for_uuid(&profiles, uuid).is_some());
        DesktopLiveState {
            supported,
            installed,
            running: supported && installed && self.app.is_running().unwrap_or(false),
            active_unmanaged: active.is_some() && !managed,
            active_account_id: active,
            capture: self.capture.lock().map(|c| c.clone()).unwrap_or_default(),
            busy: self.busy.load(Ordering::SeqCst),
        }
    }

    fn label_of(&self, account_id: &str) -> Result<String, AppError> {
        load_profiles(&self.paths.profiles_dir)
            .into_iter()
            .find(|p| p.account_uuid == account_id)
            .map(|p| p.label)
            .ok_or_else(|| err("这个 Claude Desktop 账号不在这台电脑上，请先在授权中心收录"))
    }

    /// 切到账号 `account_id`：退出 Desktop、存回当前账号、合并工作区、装上目标账号，
    /// 在重新打开前执行 `before_relaunch`（切卡时写 Desktop 的配置），失败就整体回滚。
    pub fn switch_to(
        &self,
        account_id: &str,
        before_relaunch: Option<&mut dyn FnMut() -> Result<(), AppError>>,
    ) -> Result<SwitchReport, AppError> {
        self.ensure_supported()?;
        let _guard = self.acquire()?;
        let label = self.label_of(account_id)?;
        switch_profile(&self.paths, self.app.as_ref(), &label, before_relaunch)
    }

    /// 收录 Desktop 当前登录的账号（名字用 Anthropic 账号资料里的邮箱）。已收录的直接返回。
    /// Desktop 会短暂重启，以便安全地复制浏览器状态。
    pub async fn import_current(self: &Arc<Self>) -> Result<ImportOutcome, AppError> {
        self.ensure_supported()?;
        let account =
            active_account(&self.paths).ok_or_else(|| err("Claude Desktop 当前没有登录"))?;
        if let Some(existing) = self.account(&account) {
            return Ok(ImportOutcome {
                account: existing,
                created: false,
            });
        }
        let info = self.lookup_info(&account, true).await;
        let manager = self.clone();
        let account_id = account.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = manager.acquire()?;
            let label = unique_label(&manager.paths, info.as_ref(), &account_id);
            let email = info.and_then(|i| i.email);
            save_current_profile(&manager.paths, manager.app.as_ref(), &label, email)?;
            Ok(ImportOutcome {
                account: manager
                    .account(&account_id)
                    .ok_or_else(|| err("账号收录后没有读到"))?,
                created: true,
            })
        })
        .await
        .map_err(|e| err(e.to_string()))?
    }

    /// 只同步工作区：把所有账号的会话 / 定时任务 / 预设并进当前登录的账号。
    pub fn sync_workspace(&self) -> Result<WorkspaceReport, AppError> {
        self.ensure_supported()?;
        let _guard = self.acquire()?;
        let account = active_account(&self.paths)
            .ok_or_else(|| err("Claude Desktop 当前没有登录，无法同步工作区"))?;
        let profiles = load_profiles(&self.paths.profiles_dir);
        let meta_org = profiles
            .iter()
            .find(|p| p.account_uuid == account)
            .and_then(|p| p.org_uuid.clone());
        let was_running = self.app.is_running()?;
        if was_running {
            self.app.quit()?;
        }
        let result = merge_workspace_into(&self.paths, &account, meta_org.as_deref());
        if was_running {
            let relaunch = self.app.relaunch();
            if let (Ok(_), Err(error)) = (&result, relaunch) {
                return Err(error);
            }
        }
        result
    }

    /// 删除这个账号的档案（不能删当前登录的那个：那是唯一一份实时登录）。
    pub fn remove(&self, account_id: &str) -> Result<(), AppError> {
        let _guard = self.acquire()?;
        if active_account(&self.paths).as_deref() == Some(account_id) {
            return Err(err(
                "不能删除 Claude Desktop 当前登录的账号，请先切换到其他账号",
            ));
        }
        let doomed: Vec<String> = load_profiles(&self.paths.profiles_dir)
            .into_iter()
            .filter(|p| p.account_uuid == account_id)
            .map(|p| p.label)
            .collect();
        if doomed.is_empty() {
            return Err(err("没有这个 Claude Desktop 账号"));
        }
        for label in doomed {
            remove_if_present(&self.paths.profile_dir(&label))?;
        }
        Ok(())
    }

    /// 开始添加新账号：Desktop 退出登录后重新打开，等用户登录新账号，再收录（名字用邮箱）。
    /// 在后台线程里跑，进度看 [`Self::live_state`] 的 `capture`。
    pub fn start_capture(self: &Arc<Self>) -> Result<(), AppError> {
        self.ensure_supported()?;
        if self.live_state().active_unmanaged {
            return Err(err(
                "Claude Desktop 当前登录的账号还没收录，请先收录它，否则登录另一个账号后它会丢失",
            ));
        }
        let guard = self.acquire()?;
        self.cancel.store(false, Ordering::SeqCst);
        self.set_capture(|c| {
            *c = CaptureStatus {
                phase: CapturePhase::Preparing,
                started_at: Some(chrono::Utc::now().timestamp_millis()),
                ..CaptureStatus::default()
            }
        });
        let manager = self.clone();
        std::thread::spawn(move || {
            let _guard = guard;
            let outcome = capture_profile(&manager, &manager.capture, &manager.cancel);
            set_capture(&manager.capture, |c| match outcome {
                Ok(CaptureOutcome::Captured {
                    account_id,
                    label,
                    note,
                }) => {
                    c.phase = CapturePhase::Done;
                    c.account_id = Some(account_id);
                    c.label = Some(label);
                    c.message = note;
                }
                Ok(CaptureOutcome::Refreshed { account_id, label }) => {
                    c.phase = CapturePhase::Refreshed;
                    c.account_id = Some(account_id);
                    c.label = Some(label);
                }
                Ok(CaptureOutcome::Cancelled) => c.phase = CapturePhase::Cancelled,
                Ok(CaptureOutcome::TimedOut) => c.phase = CapturePhase::TimedOut,
                Err(error) => {
                    log::warn!("Claude Desktop 添加账号失败: {error}");
                    c.phase = CapturePhase::Failed;
                    c.message = Some(error.to_string());
                }
            });
        });
        Ok(())
    }

    pub fn cancel_capture(&self) {
        let in_progress = self
            .capture
            .lock()
            .map(|c| c.in_progress())
            .unwrap_or(false);
        if in_progress {
            self.cancel.store(true, Ordering::SeqCst);
        }
    }

    /// 前端看过结果后清掉，回到空闲。
    pub fn dismiss_capture(&self) {
        self.set_capture(|c| {
            if !c.in_progress() {
                *c = CaptureStatus::default();
            }
        });
    }

    // ───────────── 令牌、账号资料、额度 ─────────────

    /// 读这个账号能查用量的令牌：当前登录的从 config.json 只读，其他的从档案读，
    /// 快过期时在切换锁里续期并写回档案（`allow_refresh`）。
    async fn usable_token(
        self: &Arc<Self>,
        account_id: &str,
        allow_refresh: bool,
    ) -> Result<token::DesktopToken, String> {
        let manager = self.clone();
        let id = account_id.to_string();
        let (key, source, current) = tokio::task::spawn_blocking(move || {
            let key = token::safe_storage_key().map_err(|e| e.to_string())?;
            let source = manager.token_source(&id)?;
            let current = token::read_token(&key, &source).map_err(|e| e.to_string())?;
            Ok::<_, String>((key, source, current))
        })
        .await
        .map_err(|e| e.to_string())??;
        let now = chrono::Utc::now().timestamp_millis();
        if !current.needs_refresh(now) {
            return Ok(current);
        }
        if matches!(source, token::TokenSource::Live(_)) {
            // 当前登录的号由 Desktop 自己续期，这里只读
            return Ok(current);
        }
        if !allow_refresh {
            return Ok(current);
        }
        // 续期会轮换 refresh token：拿切换锁，期间不会有切换去读这份档案
        let _guard = self.acquire().map_err(|e| e.to_string())?;
        // 拿到锁时它可能刚被切成当前账号：那就交给 Desktop
        if self.active_account_id().as_deref() == Some(account_id) {
            return Ok(current);
        }
        let fresh = {
            let source = source.clone();
            tokio::task::spawn_blocking(move || token::read_token(&key, &source))
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?
        };
        if !fresh.needs_refresh(now) {
            return Ok(fresh);
        }
        let renewed = token::refresh(&fresh, now).await?;
        let to_write = renewed.clone();
        tokio::task::spawn_blocking(move || token::write_back(&key, &source, &to_write))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        Ok(renewed)
    }

    fn token_source(&self, account_id: &str) -> Result<token::TokenSource, String> {
        if active_account(&self.paths).as_deref() == Some(account_id) {
            return Ok(token::TokenSource::Live(self.paths.config_json()));
        }
        let label = self.label_of(account_id).map_err(|e| e.to_string())?;
        Ok(token::TokenSource::Profile(self.paths.profile_dir(&label)))
    }

    /// 查账号的邮箱、套餐（缓存在内存里）。`force` 时重新查。
    async fn lookup_info(
        self: &Arc<Self>,
        account_id: &str,
        allow_refresh: bool,
    ) -> Option<token::AccountInfo> {
        if let Some(info) = self
            .info
            .lock()
            .ok()
            .and_then(|m| m.get(account_id).cloned())
        {
            return Some(info);
        }
        let token = self.usable_token(account_id, allow_refresh).await.ok()?;
        let info = token::fetch_account_info(&token.access_token).await.ok()?;
        if let Ok(mut map) = self.info.lock() {
            map.insert(account_id.to_string(), info.clone());
        }
        Some(info)
    }

    /// 这个账号的用量（和 Claude Code 绑号卡同一个接口）和套餐。
    pub async fn quota(
        self: &Arc<Self>,
        account_id: &str,
    ) -> Result<crate::services::subscription::ClaudeAccountQuota, String> {
        use crate::services::subscription::{
            ClaudeAccountQuota, CredentialStatus, SubscriptionQuota,
        };
        let token = match self.usable_token(account_id, true).await {
            Ok(token) => token,
            Err(message) => {
                return Ok(ClaudeAccountQuota {
                    quota: SubscriptionQuota::error("claude", CredentialStatus::NotFound, message),
                    plan: None,
                })
            }
        };
        if token
            .expires_at
            .is_some_and(|expires| expires <= chrono::Utc::now().timestamp_millis())
        {
            return Ok(ClaudeAccountQuota {
                quota: SubscriptionQuota::error(
                    "claude",
                    CredentialStatus::Expired,
                    "Claude Desktop 的登录已过期，打开 Claude Desktop 后会自动续期".to_string(),
                ),
                plan: None,
            });
        }
        let quota = crate::services::subscription::query_claude_quota(&token.access_token).await?;
        let info = match self
            .info
            .lock()
            .ok()
            .and_then(|m| m.get(account_id).cloned())
        {
            Some(info) => Some(info),
            None => {
                let fetched = token::fetch_account_info(&token.access_token).await.ok();
                if let (Some(info), Ok(mut map)) = (&fetched, self.info.lock()) {
                    map.insert(account_id.to_string(), info.clone());
                }
                fetched
            }
        };
        Ok(ClaudeAccountQuota {
            quota,
            plan: info
                .filter(|info| info.subscription_type.is_some())
                .map(|info| crate::services::claude_account::AccountPlan {
                    subscription_type: info.subscription_type,
                    rate_limit_tier: info.rate_limit_tier,
                }),
        })
    }
}

fn set_capture(capture: &Mutex<CaptureStatus>, update: impl FnOnce(&mut CaptureStatus)) {
    if let Ok(mut status) = capture.lock() {
        update(&mut status);
    }
}

// ───────────────────────────── 官方卡 ─────────────────────────────

/// 授权中心里 Claude Desktop 账号的认证供应商标识（卡片绑定存的 `authProvider`）。
pub const AUTH_PROVIDER: &str = "claude_desktop";

/// Claude Desktop 官方卡绑定的账号 UUID；没绑（沿用 Desktop 现在的登录）或不是官方卡时为 `None`。
pub fn bound_account(provider: &crate::provider::Provider) -> Option<String> {
    if !crate::claude_desktop_config::is_official_provider(provider) {
        return None;
    }
    provider
        .meta
        .as_ref()?
        .managed_account_id_for(AUTH_PROVIDER)
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
}

/// 切到（或重新应用）Claude Desktop 卡片 `target` 时的登录切换，对应 Claude Code 的
/// `claude_official::with_login_switch`：卡绑了本机的 Desktop 账号、且 Desktop 现在不是这个号时，
/// 退出 Desktop、换上这个号，在 Desktop 重新打开之前执行 `write`（写 Desktop 的配置、移动卡片指针）；
/// `write` 失败就把登录整体回滚。账号不在这台电脑上（别的设备同步来的卡）时只写配置、记日志。
pub fn with_desktop_login<R>(
    target: Option<&crate::provider::Provider>,
    write: impl FnOnce() -> Result<R, AppError>,
) -> Result<R, AppError> {
    let Some(account) = target.and_then(bound_account) else {
        return write();
    };
    let manager = ClaudeDesktopAccountManager::global();
    if !cfg!(target_os = "macos") || !manager.paths.installed() {
        return write();
    }
    if manager.label_of(&account).is_err() {
        log::warn!("Claude Desktop 官方卡绑定的账号 {account} 不在这台电脑上，只切换配置");
        return write();
    }
    if manager.active_account_id().as_deref() == Some(account.as_str()) {
        return write();
    }
    let mut write = Some(write);
    let mut output = None;
    let mut hook = || -> Result<(), AppError> {
        if let Some(write) = write.take() {
            output = Some(write()?);
        }
        Ok(())
    };
    manager.switch_to(&account, Some(&mut hook))?;
    output.ok_or_else(|| err("切换 Claude Desktop 账号后没有写入配置"))
}

/// 新账号的档案名：邮箱（没查到就用 `claude-<UUID 前 8 位>`），重名时加序号。
fn unique_label(paths: &Paths, info: Option<&token::AccountInfo>, account_uuid: &str) -> String {
    let base = info
        .and_then(|i| i.email.clone())
        .filter(|email| validate_label(email).is_ok())
        .unwrap_or_else(|| {
            format!(
                "claude-{}",
                account_uuid.chars().take(8).collect::<String>()
            )
        });
    let mut label = base.clone();
    let mut n = 2;
    while paths.profile_dir(&label).exists() {
        label = format!("{base}-{n}");
        n += 1;
    }
    label
}

// ───────────────────────────── 切换 ─────────────────────────────

fn switch_profile(
    paths: &Paths,
    app: &dyn AppControl,
    label: &str,
    mut before_relaunch: Option<&mut dyn FnMut() -> Result<(), AppError>>,
) -> Result<SwitchReport, AppError> {
    let profiles = load_profiles(&paths.profiles_dir);
    let target = profiles
        .iter()
        .find(|p| p.label == label)
        .cloned()
        .ok_or_else(|| err(format!("没有名为 {label:?} 的 Claude Desktop 账号")))?;
    let profile_dir = paths.profile_dir(label);
    let token_cache = non_empty_file(&profile_dir.join(TOKEN_CACHE))?;
    let token_cache_v2 = non_empty_file(&profile_dir.join(TOKEN_CACHE_V2))?;
    if token_cache.is_none() && token_cache_v2.is_none() {
        return Err(err(format!(
            "账号 {label:?} 没有保存的登录凭据，请重新添加这个账号"
        )));
    }
    if !profile_dir.join(DESKTOP_STATE).is_dir() {
        return Err(err(format!(
            "账号 {label:?} 没有保存的浏览器状态，请重新添加这个账号"
        )));
    }

    let active = active_account(paths);
    if active.as_deref() == Some(target.account_uuid.as_str()) {
        // 已经是这个号：不用重启 Desktop，只做切卡要做的事
        if let Some(hook) = before_relaunch.as_mut() {
            hook()?;
        }
        return Ok(SwitchReport {
            label: label.to_string(),
            previous_label: Some(label.to_string()),
            ..SwitchReport::default()
        });
    }
    if let Some(uuid) = &active {
        if label_for_uuid(&profiles, uuid).is_none() {
            return Err(err(
                "Claude Desktop 当前登录的账号还没有保存，切走会丢掉它的登录。请先「保存当前登录」",
            ));
        }
    }

    // 先退出：SQLite / LevelDB / config.json 必须静止才能归档和复制
    if let Err(error) = app.quit() {
        return Err(match app.relaunch() {
            Ok(()) => error,
            Err(relaunch) => err(format!(
                "{error}；Claude Desktop 也没能重新打开: {relaunch}"
            )),
        });
    }

    let mut notes = Vec::new();
    let archive = paths
        .backups_dir
        .join(format!("switch-{}-{label}.tar.gz", timestamp()));
    let members = archive_members(paths);
    let mut archived = false;
    let mut previous_label = None;
    let mut workspace = WorkspaceReport::default();

    let result = (|| -> Result<(), AppError> {
        if !members.is_empty() {
            app.archive(&archive, &paths.data_dir, &members)?;
            archived = true;
            prune_archives(&paths.backups_dir, KEEP_BACKUPS, &mut notes);
        }
        // 退出后再认一次旧账号（Desktop 退出时会重写 config.json），而且必须在改写凭据之前
        // 快照它，否则旧账号的令牌会被记到新账号名下。
        let profiles = load_profiles(&paths.profiles_dir);
        if let Some(outgoing) = active_account(paths)
            .and_then(|uuid| label_for_uuid(&profiles, &uuid).map(str::to_string))
        {
            snapshot_profile(paths, &outgoing, &mut notes)?;
            previous_label = Some(outgoing);
        }

        workspace = merge_workspace_into(paths, &target.account_uuid, target.org_uuid.as_deref())?;

        let config =
            std::fs::read(paths.config_json()).map_err(|e| AppError::io(paths.config_json(), e))?;
        let swapped = merge::swap_config_tokens(
            &config,
            token_cache.as_deref(),
            token_cache_v2.as_deref(),
            &target.account_uuid,
        )?;
        crate::config::atomic_write(&paths.config_json(), &swapped)?;
        restore_desktop_state(paths, label)?;

        let bridge = paths.data_dir.join(BRIDGE_FILE);
        if bridge.is_file() {
            if let Err(error) = std::fs::remove_file(&bridge) {
                notes.push(format!("无法清除 {BRIDGE_FILE}: {error}"));
            }
        }
        restore_device_registry(paths, label, &mut notes);
        if let Some(hook) = before_relaunch.as_mut() {
            hook()?;
        }
        Ok(())
    })();

    let mut result = result;
    if result.is_err() && archived {
        if let Err(rollback) = app.restore(&archive, &paths.data_dir, &identity_members()) {
            let original = result.unwrap_err();
            result = Err(err(format!("{original}；自动回滚不完整: {rollback}")));
        }
    }
    let relaunch = app.relaunch();
    match (result, relaunch) {
        (Ok(()), Ok(())) => Ok(SwitchReport {
            label: label.to_string(),
            previous_label,
            workspace,
            archive: archived.then(|| archive.display().to_string()),
            notes,
        }),
        (Ok(()), Err(error)) | (Err(error), Ok(())) => Err(error),
        (Err(error), Err(relaunch)) => Err(err(format!(
            "{error}；Claude Desktop 也没能重新打开: {relaunch}"
        ))),
    }
}

fn save_current_profile(
    paths: &Paths,
    app: &dyn AppControl,
    label: &str,
    email: Option<String>,
) -> Result<String, AppError> {
    let config = read_config(paths).ok_or_else(|| err("读不到 Claude Desktop 的 config.json"))?;
    let account =
        merge::logged_in_account(&config).ok_or_else(|| err("Claude Desktop 当前没有登录"))?;
    let profiles = load_profiles(&paths.profiles_dir);
    if let Some(existing) = label_for_uuid(&profiles, &account) {
        return Err(err(format!("当前登录的账号已经保存为 {existing:?}")));
    }
    if paths.profile_dir(label).exists() {
        return Err(err(format!("已经有名为 {label:?} 的账号，请换一个名字")));
    }
    let org = resolve_org(paths, &account, &config);

    let was_running = app.is_running()?;
    if was_running {
        app.quit()?;
    }
    let mut notes = Vec::new();
    let result = (|| {
        snapshot_profile(paths, label, &mut notes)?;
        write_meta(paths, label, email.as_deref(), &account, org.as_deref())
    })();
    if result.is_err() {
        let _ = remove_if_present(&paths.profile_dir(label));
    }
    let relaunch = if was_running { app.relaunch() } else { Ok(()) };
    result?;
    relaunch?;
    Ok(label.to_string())
}

/// 账号多个组织时，优先 config 里出现过的；还分不出来就取最近改过的目录。
fn resolve_org(paths: &Paths, account_uuid: &str, config: &[u8]) -> Option<String> {
    let dir = paths.sessions_root().join(account_uuid);
    let mut orgs = account_orgs(paths, account_uuid, None);
    if orgs.len() > 1 {
        let known = merge::orgs_in_config(config);
        let narrowed: Vec<String> = orgs.iter().filter(|o| known.contains(o)).cloned().collect();
        if narrowed.len() == 1 {
            return narrowed.into_iter().next();
        }
        orgs.sort_by_key(|org| {
            std::fs::metadata(dir.join(org))
                .and_then(|m| m.modified())
                .ok()
        });
    }
    orgs.pop()
}

// ───────────────────────────── 工作区合并 ─────────────────────────────

/// 把所有账号的工作区并进 `account_uuid` 的每个组织目录。只在 Desktop 退出时调用。
fn merge_workspace_into(
    paths: &Paths,
    account_uuid: &str,
    meta_org: Option<&str>,
) -> Result<WorkspaceReport, AppError> {
    let mut report = WorkspaceReport::default();
    let root = paths.sessions_root();
    let synced = load_synced(&paths.synced_path());
    let orgs = account_orgs(paths, account_uuid, meta_org);
    let presets_path = paths.presets_path();
    let mut presets = merge::read_json(&presets_path);
    // 合并前先折叠：得到每个账号自己删过什么（含这次之前一直累积的）
    let folded = merge::fold_deletions(&synced, &merge::current_state(&root, presets.as_ref()));
    let target_row = folded.get(account_uuid).cloned().unwrap_or_default();
    let mut canonical: Option<BTreeMap<String, Value>> = None;

    if orgs.is_empty() {
        report.notes.push(
            "这个账号还没有 Code 会话目录，会话和定时任务未同步；在 Claude Desktop 里打开一次 Code 页后再同步即可"
                .into(),
        );
    }
    for org in &orgs {
        let sessions =
            merge::plan_session_merge(&root, account_uuid, org, &target_row.deleted_sessions);
        for (source, destination) in sessions.copied.iter().chain(&sessions.updated) {
            copy_file(source, destination)?;
        }
        report.sessions_copied += sessions.copied.len();
        report.sessions_updated += sessions.updated.len();
        report.sessions_skipped_deleted = report
            .sessions_skipped_deleted
            .max(sessions.skipped_deleted);

        let dir = root.join(account_uuid).join(org);
        if let Some(bytes) = merge::rebuild_archived_index(&dir) {
            crate::config::atomic_write(&dir.join(merge::ARCHIVED_INDEX), &bytes)?;
        }

        let scheduled = merge::plan_scheduled_merge(
            &root,
            account_uuid,
            org,
            &synced,
            &target_row.deleted_routines,
        )?;
        let unchanged = std::fs::read(&scheduled.target)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            == serde_json::from_slice::<Value>(&scheduled.bytes).ok();
        let empty = scheduled.added == 0 && !scheduled.target.exists();
        if !unchanged && !empty {
            crate::config::atomic_write(&scheduled.target, &scheduled.bytes)?;
        }
        report.routines_added += scheduled.added;
        report.routines_updated += scheduled.updated;
        report.routine_conflicts += scheduled.conflicts;
        canonical
            .get_or_insert_with(BTreeMap::new)
            .extend(scheduled.canonical_routines);
    }

    if let Some(document) = &presets {
        if let Some(merged) =
            merge::plan_preset_merge(document, account_uuid, &target_row.deleted_presets)
        {
            let bytes = serde_json::to_vec_pretty(&merged.document)
                .map_err(|source| AppError::JsonSerialize { source })?;
            crate::config::atomic_write(&presets_path, &bytes)?;
            report.presets_added = merged.added;
            presets = Some(merged.document);
        }
    }

    // 记下每个账号现在有什么，下次才能分清「删掉了」和「从来没有」
    let mut next = merge::fold_deletions(&folded, &merge::current_state(&root, presets.as_ref()));
    let mut canonical = canonical.unwrap_or_else(|| merge::canonical_routines(&synced));
    let present: BTreeSet<String> = next
        .values()
        .flat_map(|row| row.routines.iter().cloned())
        .collect();
    canonical.retain(|id, _| present.contains(id));
    merge::set_canonical_routines(&mut next, &canonical);
    if let Err(error) = save_synced(&paths.synced_path(), &next) {
        report.notes.push(format!("无法写入同步账本: {error}"));
    }
    Ok(report)
}

fn load_synced(path: &Path) -> merge::Synced {
    std::fs::read(path)
        .map(|bytes| merge::parse_synced(&bytes))
        .unwrap_or_default()
}

fn save_synced(path: &Path, synced: &merge::Synced) -> Result<(), AppError> {
    let bytes = serde_json::to_vec(synced).map_err(|source| AppError::JsonSerialize { source })?;
    crate::config::atomic_write(path, &bytes)
}

// ───────────────────────────── 添加账号 ─────────────────────────────

/// 记住账号用的全部东西。添加账号前整份备份，没完成就原样放回。
const LOGIN_STATE_FILES: [&str; 4] = [CONFIG_JSON, "Cookies", "Cookies-journal", BRIDGE_FILE];

enum CaptureOutcome {
    Captured {
        account_id: String,
        label: String,
        note: Option<String>,
    },
    /// 登录的是已收录过的账号：刷新了它的档案
    Refreshed {
        account_id: String,
        label: String,
    },
    Cancelled,
    TimedOut,
}

fn capture_profile(
    manager: &Arc<ClaudeDesktopAccountManager>,
    capture: &Mutex<CaptureStatus>,
    cancel: &AtomicBool,
) -> Result<CaptureOutcome, AppError> {
    let paths = &manager.paths;
    let app = manager.app.as_ref();
    let profiles = load_profiles(&paths.profiles_dir);
    let config = read_config(paths).unwrap_or_default();
    let mut known_orgs: Vec<String> = profiles.iter().filter_map(|p| p.org_uuid.clone()).collect();
    known_orgs.extend(merge::orgs_in_config(&config));
    let outgoing = merge::logged_in_account(&config)
        .and_then(|uuid| label_for_uuid(&profiles, &uuid).map(str::to_string));

    app.quit()?;
    let mut notes = Vec::new();
    let relaunch_after = |error: AppError| match app.relaunch() {
        Ok(()) => error,
        Err(relaunch) => err(format!(
            "{error}；Claude Desktop 也没能重新打开: {relaunch}"
        )),
    };
    // 先把要退出登录的账号存回它的档案，之后切回去能还原到现在的样子
    if let Some(previous) = &outgoing {
        snapshot_profile(paths, previous, &mut notes).map_err(relaunch_after)?;
    }
    // 不管有没有档案，都整份备份一次，取消时用它还原
    backup_login_state(paths, &mut notes).map_err(relaunch_after)?;
    let restore_after = |error: AppError| {
        let restore = restore_login_state(paths);
        let relaunch = app.relaunch();
        match (restore, relaunch) {
            (Ok(()), Ok(())) => error,
            (Err(restore), _) => err(format!("{error}；还原之前的登录不完整: {restore}")),
            (Ok(()), Err(relaunch)) => err(format!(
                "{error}；已还原之前的登录，但 Claude Desktop 没能重新打开: {relaunch}"
            )),
        }
    };
    clear_login_state(paths).map_err(restore_after)?;
    app.relaunch().map_err(restore_after)?;

    let restore_previous = || -> Result<(), AppError> {
        app.quit()?;
        restore_login_state(paths)?;
        app.relaunch()
    };

    set_capture(capture, |c| c.phase = CapturePhase::WaitingLogin);
    let Some(account_uuid) = poll(LOGIN_WAIT, cancel, || {
        read_config(paths).and_then(|bytes| merge::logged_in_account(&bytes))
    }) else {
        let cancelled = cancel.load(Ordering::SeqCst);
        restore_previous()?;
        return Ok(if cancelled {
            CaptureOutcome::Cancelled
        } else {
            CaptureOutcome::TimedOut
        });
    };

    // 登录的是已经保存过的账号：当作重新登录，刷新那个档案
    let profiles = load_profiles(&paths.profiles_dir);
    let existing = label_for_uuid(&profiles, &account_uuid).map(str::to_string);

    let org = if existing.is_some() {
        None
    } else {
        set_capture(capture, |c| c.phase = CapturePhase::WaitingOrg);
        let account_dir = paths.sessions_root().join(&account_uuid);
        poll(ORG_WAIT, cancel, || {
            std::fs::read_dir(&account_dir)
                .ok()
                .and_then(|entries| {
                    entries
                        .flatten()
                        .filter(|e| e.path().is_dir())
                        .find_map(|e| e.file_name().to_str().map(str::to_string))
                })
                .or_else(|| {
                    read_config(paths)
                        .and_then(|bytes| merge::new_org_in_config(&bytes, &known_orgs))
                })
        })
    };
    if cancel.load(Ordering::SeqCst) {
        restore_previous()?;
        return Ok(CaptureOutcome::Cancelled);
    }

    // 新账号用邮箱命名：趁 Desktop 还开着，用它刚写下的令牌查一次账号资料（只读）
    let (label, email) = match &existing {
        Some(label) => (label.clone(), None),
        None => {
            let info = tauri::async_runtime::block_on(manager.lookup_info(&account_uuid, false));
            (
                unique_label(paths, info.as_ref(), &account_uuid),
                info.and_then(|i| i.email),
            )
        }
    };

    set_capture(capture, |c| c.phase = CapturePhase::Saving);
    app.quit()?;
    let result = (|| -> Result<Option<String>, AppError> {
        snapshot_profile(paths, &label, &mut notes)?;
        if existing.is_some() {
            return Ok(None);
        }
        write_meta(
            paths,
            &label,
            email.as_deref(),
            &account_uuid,
            org.as_deref(),
        )?;
        // 新账号第一次登录别是空侧边栏：把本机已有的工作区并进来
        let report = merge_workspace_into(paths, &account_uuid, org.as_deref())?;
        Ok(Some(format!(
            "已同步 {} 个会话、{} 个定时任务、{} 个工作区预设",
            report.sessions_copied + report.sessions_updated,
            report.routines_added + report.routines_updated,
            report.presets_added
        )))
    })();
    let note = match result {
        Ok(note) => note,
        Err(error) => {
            if existing.is_none() {
                let _ = remove_if_present(&paths.profile_dir(&label));
            }
            return Err(restore_after(error));
        }
    };
    if let Err(error) = app.relaunch() {
        log::warn!("账号已保存，但 Claude Desktop 没能重新打开: {error}");
    }
    Ok(match existing {
        Some(label) => CaptureOutcome::Refreshed {
            account_id: account_uuid,
            label,
        },
        None => CaptureOutcome::Captured {
            account_id: account_uuid,
            label,
            note,
        },
    })
}

fn poll<T>(
    limit: Duration,
    cancel: &AtomicBool,
    mut probe: impl FnMut() -> Option<T>,
) -> Option<T> {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if cancel.load(Ordering::SeqCst) {
            return None;
        }
        if let Some(value) = probe() {
            return Some(value);
        }
        std::thread::sleep(POLL.min(limit));
    }
    None
}

/// 唯一写 `meta.json` 的地方；切换从不改它。
fn write_meta(
    paths: &Paths,
    label: &str,
    email: Option<&str>,
    account_uuid: &str,
    org_uuid: Option<&str>,
) -> Result<(), AppError> {
    let meta = serde_json::json!({
        "label": label,
        "email": email.filter(|e| !e.trim().is_empty()).map(str::trim),
        "accountUuid": account_uuid,
        "orgUuid": org_uuid,
        "savedAt": chrono::Utc::now().timestamp(),
    });
    let bytes =
        serde_json::to_vec_pretty(&meta).map_err(|source| AppError::JsonSerialize { source })?;
    crate::config::atomic_write(&paths.profile_dir(label).join(META_JSON), &bytes)
}

fn backup_login_state(paths: &Paths, notes: &mut Vec<String>) -> Result<(), AppError> {
    let backup = paths.prelogin_dir();
    remove_if_present(&backup)?;
    std::fs::create_dir_all(&backup).map_err(|e| AppError::io(&backup, e))?;
    restrict(&backup, 0o700, notes);
    for name in LOGIN_STATE_FILES {
        let source = paths.data_dir.join(name);
        if source.is_file() {
            copy_file(&source, &backup.join(name))?;
        }
    }
    for name in LEVELDB_DIRS {
        let source = paths.data_dir.join(name);
        if source.is_dir() {
            copy_dir(&source, &backup.join(name))?;
        }
    }
    Ok(())
}

fn restore_login_state(paths: &Paths) -> Result<(), AppError> {
    let backup = paths.prelogin_dir();
    if !backup.is_dir() {
        return Err(err(format!("没有可还原的登录备份 {}", backup.display())));
    }
    for name in LOGIN_STATE_FILES {
        let source = backup.join(name);
        let live = paths.data_dir.join(name);
        if source.is_file() {
            copy_file(&source, &live)?;
        } else {
            remove_if_present(&live)?;
        }
    }
    for name in LEVELDB_DIRS {
        let source = backup.join(name);
        let live = paths.data_dir.join(name);
        if source.is_dir() {
            replace_dir(&source, &live)?;
        } else {
            remove_if_present(&live)?;
        }
    }
    Ok(())
}

fn clear_login_state(paths: &Paths) -> Result<(), AppError> {
    let config_json = paths.config_json();
    let bytes = std::fs::read(&config_json).map_err(|e| AppError::io(&config_json, e))?;
    crate::config::atomic_write(&config_json, &merge::clear_config_tokens(&bytes)?)?;
    for name in LOGIN_STATE_FILES.iter().filter(|n| **n != CONFIG_JSON) {
        remove_if_present(&paths.data_dir.join(name))?;
    }
    for name in LEVELDB_DIRS {
        remove_if_present(&paths.data_dir.join(name))?;
    }
    Ok(())
}

// ───────────────────────────── 档案读写 ─────────────────────────────

/// 把当前登录的凭据和浏览器状态存进档案 `label`。只能在 Desktop 退出后调用。
fn snapshot_profile(paths: &Paths, label: &str, notes: &mut Vec<String>) -> Result<(), AppError> {
    let profile_dir = paths.profile_dir(label);
    std::fs::create_dir_all(&profile_dir).map_err(|e| AppError::io(&profile_dir, e))?;
    restrict(&profile_dir, 0o700, notes);

    let bytes =
        std::fs::read(paths.config_json()).map_err(|e| AppError::io(paths.config_json(), e))?;
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|e| AppError::json(paths.config_json(), e))?;
    let token = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .filter(|blob| !blob.is_empty())
            .map(str::to_string)
    };
    let (cache, cache_v2) = (token("oauth:tokenCache"), token("oauth:tokenCacheV2"));
    if cache.is_none() && cache_v2.is_none() {
        return Err(err(
            "当前 Desktop 登录没有 OAuth 令牌缓存，拒绝覆盖它的档案",
        ));
    }
    for (name, blob) in [(TOKEN_CACHE, cache), (TOKEN_CACHE_V2, cache_v2)] {
        let path = profile_dir.join(name);
        match blob {
            Some(blob) => crate::config::atomic_write_private(&path, blob.as_bytes())?,
            None => remove_if_present(&path)?,
        }
    }

    let registry = paths.data_dir.join(DEVICE_REGISTRY);
    if registry.is_file() {
        copy_file(&registry, &profile_dir.join(DEVICE_REGISTRY))?;
    }

    // 先在旁边准备好新快照，再整体换上，失败时旧快照还在
    let state_dir = profile_dir.join(DESKTOP_STATE);
    let staged = tempfile::Builder::new()
        .prefix(".desktop-state.pending-")
        .tempdir_in(&profile_dir)
        .map_err(|e| AppError::io(&profile_dir, e))?;
    restrict(staged.path(), 0o700, notes);
    for name in COOKIE_FILES {
        let source = paths.data_dir.join(name);
        if source.is_file() {
            let destination = staged.path().join(name);
            copy_file(&source, &destination)?;
            restrict(&destination, 0o600, notes);
        }
    }
    for name in LEVELDB_DIRS {
        let source = paths.data_dir.join(name);
        if source.is_dir() {
            copy_dir(&source, &staged.path().join(name))?;
        }
    }
    let staged = staged.keep();
    let previous = profile_dir.join(".desktop-state.previous");
    remove_if_present(&previous)?;
    if state_dir.exists() {
        std::fs::rename(&state_dir, &previous).map_err(|e| AppError::io(&state_dir, e))?;
    }
    if let Err(error) = std::fs::rename(&staged, &state_dir) {
        if previous.exists() {
            let _ = std::fs::rename(&previous, &state_dir);
        }
        let _ = remove_if_present(&staged);
        return Err(AppError::io(&state_dir, error));
    }
    if let Err(error) = remove_if_present(&previous) {
        notes.push(format!("无法删除旧的浏览器状态快照: {error}"));
    }
    Ok(())
}

fn restore_desktop_state(paths: &Paths, label: &str) -> Result<(), AppError> {
    let state_dir = paths.profile_dir(label).join(DESKTOP_STATE);
    for name in COOKIE_FILES {
        let source = state_dir.join(name);
        let destination = paths.data_dir.join(name);
        if source.is_file() {
            copy_file(&source, &destination)?;
        } else {
            remove_if_present(&destination)?;
        }
    }
    for name in LEVELDB_DIRS {
        let source = state_dir.join(name);
        let destination = paths.data_dir.join(name);
        if source.is_dir() {
            replace_dir(&source, &destination)?;
        } else {
            remove_if_present(&destination)?;
        }
    }
    Ok(())
}

fn restore_device_registry(paths: &Paths, label: &str, notes: &mut Vec<String>) {
    let snapshot = paths.profile_dir(label).join(DEVICE_REGISTRY);
    let live = paths.data_dir.join(DEVICE_REGISTRY);
    let (Ok(saved), Ok(current)) = (std::fs::read(&snapshot), std::fs::read(&live)) else {
        return;
    };
    match merge::merge_device_registry(&current, &saved) {
        Ok(bytes) if bytes != current => {
            if let Err(error) = crate::config::atomic_write(&live, &bytes) {
                notes.push(format!("无法合并 {DEVICE_REGISTRY}: {error}"));
            }
        }
        Ok(_) => {}
        Err(error) => notes.push(format!("无法合并 {DEVICE_REGISTRY}: {error}")),
    }
}

/// 回滚归档的内容（相对数据目录）：切换会动到的身份文件和各账号的定时任务注册表、预设。
fn archive_members(paths: &Paths) -> Vec<String> {
    let mut members: Vec<String> = identity_members()
        .into_iter()
        .chain([PRESETS_JSON])
        .filter(|name| paths.data_dir.join(name).exists())
        .map(str::to_string)
        .collect();
    let mut extra: Vec<String> = merge::account_org_dirs(&paths.sessions_root())
        .iter()
        .flat_map(|dir| {
            [
                dir.join(merge::SCHEDULED_TASKS),
                dir.join(merge::ARCHIVED_INDEX),
            ]
        })
        .filter(|path| path.is_file())
        .filter_map(|path| {
            path.strip_prefix(&paths.data_dir)
                .ok()
                .map(|p| p.display().to_string())
        })
        .collect();
    extra.sort();
    members.extend(extra);
    members
}

fn identity_members() -> Vec<&'static str> {
    [CONFIG_JSON, DEVICE_REGISTRY, BRIDGE_FILE]
        .into_iter()
        .chain(COOKIE_FILES)
        .chain(LEVELDB_DIRS)
        .collect()
}

fn prune_archives(backups_dir: &Path, keep: usize, notes: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(backups_dir) else {
        return;
    };
    let mut archives: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("switch-") && n.ends_with(".tar.gz"))
        })
        .collect();
    if archives.len() <= keep.max(1) {
        return;
    }
    archives.sort();
    let doomed = archives.len() - keep.max(1);
    for path in archives.into_iter().take(doomed) {
        if let Err(error) = std::fs::remove_file(&path) {
            notes.push(format!("无法清理旧归档 {}: {error}", path.display()));
        }
    }
}

// ───────────────────────────── 文件工具 ─────────────────────────────

fn non_empty_file(path: &Path) -> Result<Option<String>, AppError> {
    match std::fs::read_to_string(path) {
        Ok(text) if !text.is_empty() => Ok(Some(text)),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(AppError::io(path, error)),
    }
}

fn copy_file(source: &Path, destination: &Path) -> Result<(), AppError> {
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;
    }
    std::fs::copy(source, destination).map_err(|e| AppError::io(source, e))?;
    // 保留源文件的修改时间：会话合并用它判断谁更新，复制出来的副本不能显得比源还新
    if let Ok(modified) = std::fs::metadata(source).and_then(|m| m.modified()) {
        if let Ok(file) = std::fs::File::options().write(true).open(destination) {
            let _ = file.set_modified(modified);
        }
    }
    Ok(())
}

pub(crate) fn remove_if_present(path: &Path) -> Result<(), AppError> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => {
            std::fs::remove_dir_all(path).map_err(|e| AppError::io(path, e))
        }
        Ok(_) => std::fs::remove_file(path).map_err(|e| AppError::io(path, e)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AppError::io(path, error)),
    }
}

/// LevelDB 不能逐文件合并（旧 manifest 配新日志就是坏库），先整个删掉再复制。
fn replace_dir(source: &Path, destination: &Path) -> Result<(), AppError> {
    remove_if_present(destination)?;
    copy_dir(source, destination)
}

fn copy_dir(source: &Path, destination: &Path) -> Result<(), AppError> {
    std::fs::create_dir_all(destination).map_err(|e| AppError::io(destination, e))?;
    for entry in std::fs::read_dir(source)
        .map_err(|e| AppError::io(source, e))?
        .flatten()
    {
        let child = entry.path();
        let target = destination.join(entry.file_name());
        let kind = entry.file_type().map_err(|e| AppError::io(&child, e))?;
        if kind.is_dir() {
            copy_dir(&child, &target)?;
        } else if kind.is_file() {
            std::fs::copy(&child, &target).map_err(|e| AppError::io(&child, e))?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn restrict(path: &Path, mode: u32, notes: &mut Vec<String>) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(error) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)) {
        notes.push(format!("无法收紧权限 {}: {error}", path.display()));
    }
}

#[cfg(not(unix))]
fn restrict(_path: &Path, _mode: u32, _notes: &mut Vec<String>) {}

fn timestamp() -> String {
    chrono::Local::now().format("%Y%m%d-%H%M%S%.3f").to_string()
}

#[cfg(test)]
mod tests;
