//! Claude Desktop 多账号切换里「纯计算」的一半：决定哪些会话索引要复制、合并后的定时任务 /
//! 会话预设长什么样、`config.json` 改写成什么。
//!
//! 这里不碰 `$HOME`、不起进程，只接收显式的根目录或文件内容，所以整套逻辑能在临时目录里单测。
//!
//! 数据布局和「谁新用谁」的规则来自 claude-acc（<https://github.com/ohmaseclaro/claude-acc>，MIT）
//! 和 ai-usagebar（<https://github.com/akitaonrails/ai-usagebar>，MIT）的逆向结果；
//! 账本 `synced.json` 与两者共用同一格式，三个工具可以交替使用。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::error::AppError;

/// 每个账号 / 组织目录下的定时任务注册表。
pub const SCHEDULED_TASKS: &str = "scheduled-tasks.json";
/// 每个账号 / 组织目录下的「已归档会话」索引。
pub const ARCHIVED_INDEX: &str = "archived-sessions.idx";
/// Desktop 删除会话时留下的墓碑文件前缀（`deleted_<sessionUuid>`）。
const TOMBSTONE_PREFIX: &str = "deleted_";

// ───────────────────────────── 会话索引 ─────────────────────────────

/// 一次合并要复制进目标账号目录的会话索引。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SessionMerge {
    /// 目标从没见过的索引 `(source, destination)`。
    pub copied: Vec<(PathBuf, PathBuf)>,
    /// 目标已有、但别处那份更新（见 [`freshness`]）的索引。会话续聊后索引会前进，
    /// 不覆盖的话 Desktop 会从旧的续接点打开，看起来像最近的消息丢了。
    pub updated: Vec<(PathBuf, PathBuf)>,
    /// 因为目标账号自己删过（账本或墓碑）而没有带回来的会话数。
    pub skipped_deleted: usize,
}

/// 规划把所有账号的会话索引并进 `<sessions_root>/<account>/<org>`。
///
/// `deleted_here` 是目标账号自己删掉过的索引文件名：不再把它们从别的账号带回来，
/// 否则在 A 里删掉的会话，切回 A 时又会冒出来。
pub fn plan_session_merge(
    sessions_root: &Path,
    account_uuid: &str,
    org_uuid: &str,
    deleted_here: &BTreeSet<String>,
) -> SessionMerge {
    let target_dir = sessions_root.join(account_uuid).join(org_uuid);
    let tombstones = tombstones_everywhere(sessions_root);

    // 以目标路径为键：两个来源都有同一份索引时只排一次写，留最新的那份。
    let mut best: BTreeMap<PathBuf, (PathBuf, (i64, i64))> = BTreeMap::new();
    let mut skipped = BTreeSet::new();
    for source_dir in account_org_dirs(sessions_root) {
        if source_dir == target_dir {
            continue;
        }
        for source in local_session_files(&source_dir) {
            let Some(name) = file_name(&source) else {
                continue;
            };
            let destination = target_dir.join(&name);
            if !destination.exists()
                && (deleted_here.contains(&name) || tombstones.contains(session_uuid(&name)))
            {
                skipped.insert(name);
                continue;
            }
            let freshness = freshness(&source);
            match best.get(&destination) {
                Some((_, seen)) if *seen >= freshness => {}
                _ => {
                    best.insert(destination, (source, freshness));
                }
            }
        }
    }

    let mut merge = SessionMerge {
        skipped_deleted: skipped.len(),
        ..SessionMerge::default()
    };
    for (destination, (source, freshness)) in best {
        if !destination.exists() {
            merge.copied.push((source, destination));
        } else if freshness > self::freshness(&destination)
            && std::fs::read(&source).ok() != std::fs::read(&destination).ok()
        {
            merge.updated.push((source, destination));
        }
    }
    merge
}

/// 按目录里每个会话索引自己的 `isArchived` 重建 `archived-sessions.idx`。
///
/// 会话合并按「最新的那份赢」，归档状态跟着索引走，所以在别的账号里归档 / 取消归档过的会话，
/// 到这里也是同样的状态。返回 `None` 表示内容不变、不用写。
pub fn rebuild_archived_index(dir: &Path) -> Option<Vec<u8>> {
    let mut archived: Vec<String> = local_session_files(dir)
        .iter()
        .filter(|path| {
            read_json(path)
                .and_then(|value| value.get("isArchived").and_then(Value::as_bool))
                .unwrap_or(false)
        })
        .filter_map(|path| file_name(path))
        .map(|name| name.trim_end_matches(".json").to_string())
        .collect();
    archived.sort();

    let path = dir.join(ARCHIVED_INDEX);
    let mut document = read_json(&path)
        .filter(Value::is_object)
        .unwrap_or_else(|| serde_json::json!({ "v": 1 }));
    let mut previous: Vec<String> = document
        .get("archived")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    previous.sort();
    if previous == archived && path.exists() {
        return None;
    }
    if archived.is_empty() && !path.exists() {
        return None;
    }
    document["archived"] = Value::from(archived);
    serde_json::to_vec(&document).ok()
}

// ───────────────────────────── 同步账本 ─────────────────────────────

/// 某个账号上次合并后拥有的内容。字段和 claude-acc / ai-usagebar 的 `synced.json` 一致，
/// 新增的 `ccSwitchPresets` 对它们来说是未知字段，会被忽略。
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SyncedAccount {
    #[serde(default)]
    pub routines: BTreeSet<String>,
    /// 会话索引文件名（`local_<id>.json`）。
    #[serde(default)]
    pub sessions: BTreeSet<String>,
    /// 上次同步时看到的定时任务定义：三方合并的基线。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub routine_definitions: BTreeMap<String, Value>,
    /// 已成功对齐的定义；每行都存一份，保持与 claude-acc 兼容的扁平结构。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub canonical_routines: BTreeMap<String, Value>,
    /// Code 页「会话预设」（工作区文件夹）的 id。
    #[serde(
        default,
        rename = "ccSwitchPresets",
        skip_serializing_if = "BTreeSet::is_empty"
    )]
    pub presets: BTreeSet<String>,
    /// 在这个账号里删掉过的会话 / 定时任务 / 预设：累积记住，之后都不再从别的账号带回来。
    #[serde(
        default,
        rename = "ccSwitchDeletedSessions",
        skip_serializing_if = "BTreeSet::is_empty"
    )]
    pub deleted_sessions: BTreeSet<String>,
    #[serde(
        default,
        rename = "ccSwitchDeletedRoutines",
        skip_serializing_if = "BTreeSet::is_empty"
    )]
    pub deleted_routines: BTreeSet<String>,
    #[serde(
        default,
        rename = "ccSwitchDeletedPresets",
        skip_serializing_if = "BTreeSet::is_empty"
    )]
    pub deleted_presets: BTreeSet<String>,
}

/// 账号 UUID → 上次同步后的内容。
pub type Synced = BTreeMap<String, SyncedAccount>;

pub fn parse_synced(bytes: &[u8]) -> Synced {
    if let Ok(current) = serde_json::from_slice::<Synced>(bytes) {
        return current;
    }
    // 早期格式：账号 → 定时任务 id 列表
    serde_json::from_slice::<BTreeMap<String, BTreeSet<String>>>(bytes)
        .map(|old| {
            old.into_iter()
                .map(|(account, routines)| {
                    (
                        account,
                        SyncedAccount {
                            routines,
                            ..SyncedAccount::default()
                        },
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 只有所有账号都认同的定义才算「已对齐」；旧版本写的账本丢了这个字段时自然退回到不信任。
pub fn canonical_routines(synced: &Synced) -> BTreeMap<String, Value> {
    let mut accounts = synced.values();
    let Some(first) = accounts.next() else {
        return BTreeMap::new();
    };
    let mut canonical = first.canonical_routines.clone();
    for account in accounts {
        canonical.retain(|id, task| account.canonical_routines.get(id) == Some(task));
    }
    canonical
}

pub fn set_canonical_routines(synced: &mut Synced, canonical: &BTreeMap<String, Value>) {
    for account in synced.values_mut() {
        account.canonical_routines.clone_from(canonical);
    }
}

/// 合并后每个账号的实际内容，用来刷新账本。
pub fn current_state(sessions_root: &Path, presets: Option<&Value>) -> Synced {
    let mut out = Synced::new();
    for (account, path) in scheduled_registries(sessions_root) {
        let (tasks, _) = load_scheduled(&path);
        let row = out.entry(account).or_default();
        for task in tasks {
            let Some(id) = task_id(&task) else { continue };
            row.routines.insert(id.clone());
            row.routine_definitions.insert(id, task);
        }
    }
    for dir in account_org_dirs(sessions_root) {
        let Some(account) = dir.parent().and_then(file_name) else {
            continue;
        };
        out.entry(account).or_default().sessions.extend(
            local_session_files(&dir)
                .iter()
                .filter_map(|p| file_name(p)),
        );
    }
    if let Some(by_account) = presets
        .and_then(|doc| doc.get("byAccount"))
        .and_then(Value::as_object)
    {
        for (account, entry) in by_account {
            let ids = preset_list(entry).into_iter().filter_map(|p| task_id(&p));
            out.entry(account.clone()).or_default().presets.extend(ids);
        }
    }
    out
}

/// 把「上次记下有、现在没有」累积进每个账号的删除集合，返回带删除记录的当前状态。
///
/// 每次合并前后各折叠一次：合并前得到「目标账号删过什么」，合并后把它写回账本。
/// 只有上次同步时记录过的账号才会产生删除记录，第一次运行什么都不算删除。
pub fn fold_deletions(previous: &Synced, current: &Synced) -> Synced {
    let mut out = current.clone();
    for (account, before) in previous {
        let row = out.entry(account.clone()).or_default();
        fold(
            &mut row.deleted_sessions,
            &before.deleted_sessions,
            &before.sessions,
            &row.sessions,
        );
        fold(
            &mut row.deleted_routines,
            &before.deleted_routines,
            &before.routines,
            &row.routines,
        );
        fold(
            &mut row.deleted_presets,
            &before.deleted_presets,
            &before.presets,
            &row.presets,
        );
    }
    out
}

fn fold(
    deleted: &mut BTreeSet<String>,
    deleted_before: &BTreeSet<String>,
    present_before: &BTreeSet<String>,
    present_now: &BTreeSet<String>,
) {
    deleted.extend(deleted_before.iter().cloned());
    deleted.extend(present_before.difference(present_now).cloned());
    deleted.retain(|id| !present_now.contains(id));
}

// ───────────────────────────── 定时任务 ─────────────────────────────

/// 渲染好、还没写入的目标 `scheduled-tasks.json`。
#[derive(Debug, PartialEq, Eq)]
pub struct ScheduledMerge {
    pub target: PathBuf,
    pub bytes: Vec<u8>,
    pub added: usize,
    pub updated: usize,
    /// 同一个任务在多个账号里被各自改过：保留目标自己的那份，不替用户猜。
    pub conflicts: usize,
    pub canonical_routines: BTreeMap<String, Value>,
}

struct RoutineCandidate {
    account: String,
    task: Value,
    target: bool,
}

/// `Some(true)` 表示自上次同步后这个账号改过它；`None` 表示没有可信的基线。
fn task_changed(synced: &Synced, account: &str, id: &str, task: &Value) -> Option<bool> {
    let state = synced.get(account)?;
    match state.routine_definitions.get(id) {
        Some(previous) => Some(previous != task),
        None if state.routines.contains(id) => None,
        None => Some(true),
    }
}

fn unique_values<'a>(values: impl Iterator<Item = &'a Value>) -> Vec<Value> {
    let mut unique = Vec::new();
    for value in values {
        if !unique.contains(value) {
            unique.push(value.clone());
        }
    }
    unique
}

/// 按任务 `id` 做三方合并。任务脚本本身在 `~/.claude/scheduled-tasks/` 下、本来就是全局的，
/// 这里只合并按账号存放的注册表。目标账号自己删掉的任务不会被带回来。
pub fn plan_scheduled_merge(
    sessions_root: &Path,
    account_uuid: &str,
    org_uuid: &str,
    synced: &Synced,
    deleted_here: &BTreeSet<String>,
) -> Result<ScheduledMerge, AppError> {
    let target = sessions_root
        .join(account_uuid)
        .join(org_uuid)
        .join(SCHEDULED_TASKS);
    let target_document = read_json(&target).filter(Value::is_object);
    let (target_tasks, mut skips) = load_scheduled(&target);
    let prior_canonical = canonical_routines(synced);

    let mut candidates: BTreeMap<String, Vec<RoutineCandidate>> = BTreeMap::new();
    let mut order = Vec::new();
    let mut seen = BTreeSet::new();
    for task in target_tasks {
        let Some(id) = task_id(&task) else { continue };
        if seen.insert(id.clone()) {
            order.push(id.clone());
        }
        candidates.entry(id).or_default().push(RoutineCandidate {
            account: account_uuid.to_string(),
            task,
            target: true,
        });
    }
    for (account, source) in scheduled_registries(sessions_root) {
        if source == target {
            continue;
        }
        let (source_tasks, source_skips) = load_scheduled(&source);
        for task in source_tasks {
            let Some(id) = task_id(&task) else { continue };
            if deleted_here.contains(&id) {
                continue;
            }
            if seen.insert(id.clone()) {
                order.push(id.clone());
            }
            candidates.entry(id).or_default().push(RoutineCandidate {
                account: account.clone(),
                task,
                target: false,
            });
        }
        for (key, value) in source_skips {
            skips.entry(key).or_insert(value);
        }
    }

    let mut tasks = Vec::new();
    let mut canonical = BTreeMap::new();
    let (mut added, mut updated, mut conflicts) = (0usize, 0usize, 0usize);
    for id in order {
        let choices = &candidates[&id];
        let target_task = choices.iter().find(|c| c.target).map(|c| &c.task);
        let changed = unique_values(choices.iter().filter_map(|c| {
            task_changed(synced, &c.account, &id, &c.task)
                .is_some_and(|changed| changed)
                .then_some(&c.task)
        }));
        let fallback = || target_task.unwrap_or(&choices[0].task).clone();

        let (selected, resolved) = if let Some(previous) = prior_canonical.get(&id) {
            let unknown_divergence = choices.iter().any(|c| {
                task_changed(synced, &c.account, &id, &c.task).is_none() && c.task != *previous
            });
            if changed.len() == 1 && !unknown_divergence {
                (changed[0].clone(), true)
            } else if changed.is_empty() && !unknown_divergence {
                (previous.clone(), true)
            } else {
                (fallback(), false)
            }
        } else if changed.len() == 1 {
            (changed[0].clone(), true)
        } else if changed.len() > 1 {
            (fallback(), false)
        } else {
            let newest_at = choices.iter().map(|c| created_at(&c.task)).max();
            let newest = unique_values(
                choices
                    .iter()
                    .filter(|c| Some(created_at(&c.task)) == newest_at)
                    .map(|c| &c.task),
            );
            if newest.len() == 1 {
                (newest[0].clone(), true)
            } else {
                (fallback(), false)
            }
        };

        match target_task {
            None => added += 1,
            Some(existing) if *existing != selected => updated += 1,
            Some(_) => {}
        }
        if resolved {
            canonical.insert(id, selected.clone());
        } else {
            conflicts += 1;
        }
        tasks.push(selected);
    }

    // 保留目标注册表里其他顶层字段（Desktop 自己的迁移标记等），只替换这两项。
    let mut document = target_document.unwrap_or_else(|| Value::Object(Map::new()));
    document["scheduledTasks"] = Value::Array(tasks);
    document["recordedSkips"] = Value::Object(skips);
    Ok(ScheduledMerge {
        target,
        bytes: serde_json::to_vec(&document)
            .map_err(|source| AppError::JsonSerialize { source })?,
        added,
        updated,
        conflicts,
        canonical_routines: canonical,
    })
}

// ───────────────────────────── 会话预设（工作区） ─────────────────────────────

/// Code 页新建会话时可选的「工作区」预设（`ccd-session-presets.json` 的 `byAccount.<uuid>.presets`）。
#[derive(Debug, PartialEq)]
pub struct PresetMerge {
    pub document: Value,
    pub added: usize,
}

fn preset_list(entry: &Value) -> Vec<Value> {
    entry
        .get("presets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// 预设指向的目标（同一个文件夹在不同账号里可能是不同 id 的预设，按目标去重）。
fn preset_target_key(preset: &Value) -> Option<String> {
    let target = preset.get("target")?;
    let kind = target.get("kind").and_then(Value::as_str).unwrap_or("");
    let cwd = target.get("cwd").and_then(Value::as_str)?;
    Some(format!("{kind}\u{0}{cwd}"))
}

fn preset_recency(preset: &Value) -> i64 {
    preset
        .get("lastUsedAt")
        .map(json_i64)
        .unwrap_or(0)
        .max(created_at(preset))
}

/// 把其他账号的会话预设并进 `account_uuid`。只增不删：目标已有的预设（按 id 或按目标文件夹）
/// 原样保留；目标账号自己删掉过的（`deleted_here`）不会被带回来。`None` 表示没有变化。
pub fn plan_preset_merge(
    document: &Value,
    account_uuid: &str,
    deleted_here: &BTreeSet<String>,
) -> Option<PresetMerge> {
    let by_account = document.get("byAccount")?.as_object()?;
    let target_entry = by_account.get(account_uuid).cloned();
    let mut target_presets = target_entry.as_ref().map(preset_list).unwrap_or_default();

    let mut ids: BTreeSet<String> = target_presets.iter().filter_map(task_id).collect();
    let mut targets: BTreeSet<String> = target_presets
        .iter()
        .filter_map(preset_target_key)
        .collect();
    let mut incoming: Vec<Value> = by_account
        .iter()
        .filter(|(account, _)| account.as_str() != account_uuid)
        .flat_map(|(_, entry)| preset_list(entry))
        .collect();
    // 最近用过的优先，同一个文件夹只收一份
    incoming.sort_by_key(|preset| std::cmp::Reverse(preset_recency(preset)));

    let mut added = 0usize;
    for preset in incoming {
        let Some(id) = task_id(&preset) else { continue };
        if ids.contains(&id) || deleted_here.contains(&id) {
            continue;
        }
        if let Some(key) = preset_target_key(&preset) {
            if !targets.insert(key) {
                continue;
            }
        }
        ids.insert(id);
        target_presets.push(preset);
        added += 1;
    }
    if added == 0 {
        return None;
    }

    let mut document = document.clone();
    let entry = document["byAccount"]
        .as_object_mut()?
        .entry(account_uuid.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !entry.is_object() {
        *entry = Value::Object(Map::new());
    }
    entry["presets"] = Value::Array(target_presets);
    Some(PresetMerge { document, added })
}

// ───────────────────────────── config.json ─────────────────────────────

/// 改写 `config.json`，让 Desktop 下次启动时变成另一个账号：两份 OAuth 缓存加上
/// `lastKnownAccountUuid`。其他键（扩展白名单、窗口状态、开关）原样保留。
pub fn swap_config_tokens(
    existing: &[u8],
    token_cache: Option<&str>,
    token_cache_v2: Option<&str>,
    account_uuid: &str,
) -> Result<Vec<u8>, AppError> {
    let mut document: Value =
        serde_json::from_slice(existing).map_err(|source| AppError::json("config.json", source))?;
    let object = document
        .as_object_mut()
        .ok_or_else(|| AppError::Message("Claude Desktop config.json 不是 JSON 对象".into()))?;
    // 只写这个账号真有的那份：4.16 起只写 V2，把上一个账号的旧版缓存留下会装上过期凭据
    for (key, blob) in [
        ("oauth:tokenCache", token_cache),
        ("oauth:tokenCacheV2", token_cache_v2),
    ] {
        match blob {
            Some(blob) => {
                object.insert(key.into(), blob.into());
            }
            None => {
                object.remove(key);
            }
        }
    }
    object.insert("lastKnownAccountUuid".into(), account_uuid.into());
    serde_json::to_vec(&document).map_err(|source| AppError::JsonSerialize { source })
}

/// 去掉 Desktop 记住账号用的键，让它下次启动停在登录页。
pub fn clear_config_tokens(existing: &[u8]) -> Result<Vec<u8>, AppError> {
    let mut document: Value =
        serde_json::from_slice(existing).map_err(|source| AppError::json("config.json", source))?;
    let object = document
        .as_object_mut()
        .ok_or_else(|| AppError::Message("Claude Desktop config.json 不是 JSON 对象".into()))?;
    for key in [
        "oauth:tokenCache",
        "oauth:tokenCacheV2",
        "lastKnownAccountUuid",
    ] {
        object.remove(key);
    }
    serde_json::to_vec(&document).map_err(|source| AppError::JsonSerialize { source })
}

/// 已经完成登录的账号：要同时有 `lastKnownAccountUuid` 和至少一份令牌缓存，
/// 只看前者会抓到登录到一半的状态。
pub fn logged_in_account(config: &[u8]) -> Option<String> {
    let document: Value = serde_json::from_slice(config).ok()?;
    let has_token = ["oauth:tokenCacheV2", "oauth:tokenCache"]
        .iter()
        .any(|key| {
            document
                .get(*key)
                .and_then(Value::as_str)
                .is_some_and(|token| !token.is_empty())
        });
    if !has_token {
        return None;
    }
    document
        .get("lastKnownAccountUuid")?
        .as_str()
        .filter(|uuid| !uuid.is_empty())
        .map(str::to_string)
}

/// `dxt:allowlistEnabled:<org>` 里出现过的所有组织。
pub fn orgs_in_config(config: &[u8]) -> Vec<String> {
    const PREFIX: &str = "dxt:allowlistEnabled:";
    let Ok(Value::Object(object)) = serde_json::from_slice::<Value>(config) else {
        return Vec::new();
    };
    let mut orgs: Vec<String> = object
        .keys()
        .filter_map(|key| key.strip_prefix(PREFIX))
        .filter(|org| !org.is_empty())
        .map(str::to_string)
        .collect();
    orgs.sort();
    orgs.dedup();
    orgs
}

/// 新账号的组织：本机没见过的、唯一的那一个。
pub fn new_org_in_config(config: &[u8], known: &[String]) -> Option<String> {
    let mut unseen = orgs_in_config(config)
        .into_iter()
        .filter(|org| !known.contains(org));
    let candidate = unseen.next()?;
    unseen.next().is_none().then_some(candidate)
}

/// 把快照里的 `ant-device-registry.json` 并回实时文件：只补丢失的键，实时值永远优先。
pub fn merge_device_registry(live: &[u8], snapshot: &[u8]) -> Result<Vec<u8>, AppError> {
    let mut merged: Map<String, Value> = serde_json::from_slice(live)
        .map_err(|source| AppError::json("ant-device-registry.json", source))?;
    if let Ok(saved) = serde_json::from_slice::<Map<String, Value>>(snapshot) {
        for (account, registration) in saved {
            merged.entry(account).or_insert(registration);
        }
    }
    serde_json::to_vec(&merged).map_err(|source| AppError::JsonSerialize { source })
}

// ───────────────────────────── 工具函数 ─────────────────────────────

/// `<sessions_root>` 下所有 `<account>/<org>` 目录。
pub fn account_org_dirs(sessions_root: &Path) -> Vec<PathBuf> {
    let Ok(accounts) = std::fs::read_dir(sessions_root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for account in accounts.flatten() {
        let Ok(orgs) = std::fs::read_dir(account.path()) else {
            continue;
        };
        out.extend(orgs.flatten().map(|org| org.path()).filter(|p| p.is_dir()));
    }
    out.sort();
    out
}

/// 只要 `local_*.json`，不包括同目录下按 id 合并的账号级文件。
pub fn local_session_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("local_") && name.ends_with(".json"))
        })
        .collect();
    out.sort();
    out
}

fn tombstones_everywhere(sessions_root: &Path) -> BTreeSet<String> {
    account_org_dirs(sessions_root)
        .iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
        .filter_map(|name| name.strip_prefix(TOMBSTONE_PREFIX).map(str::to_string))
        .collect()
}

/// `local_<uuid>.json` → `<uuid>`
fn session_uuid(name: &str) -> &str {
    name.trim_start_matches("local_").trim_end_matches(".json")
}

fn scheduled_registries(sessions_root: &Path) -> Vec<(String, PathBuf)> {
    account_org_dirs(sessions_root)
        .into_iter()
        .filter_map(|dir| {
            let path = dir.join(SCHEDULED_TASKS);
            if !path.is_file() {
                return None;
            }
            let account = dir.parent().and_then(file_name)?;
            Some((account, path))
        })
        .collect()
}

fn load_scheduled(path: &Path) -> (Vec<Value>, Map<String, Value>) {
    let Some(value) = read_json(path) else {
        return (Vec::new(), Map::new());
    };
    let tasks = value
        .get("scheduledTasks")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let skips = value
        .get("recordedSkips")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    (tasks, skips)
}

pub fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

pub fn file_name(path: &Path) -> Option<String> {
    path.file_name()?.to_str().map(str::to_string)
}

fn task_id(task: &Value) -> Option<String> {
    task.get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

fn created_at(task: &Value) -> i64 {
    task.get("createdAt").map_or(0, json_i64)
}

/// 先比 `lastActivityAt`，相同再比文件修改时间：归档、改名这类改动不推进 `lastActivityAt`，
/// 但会改文件。复制时会保留源文件的修改时间，所以同步过的两份时间相同、不会来回覆盖。
fn freshness(path: &Path) -> (i64, i64) {
    let modified = std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as i64);
    (last_activity(path), modified)
}

fn last_activity(path: &Path) -> i64 {
    read_json(path)
        .and_then(|value| value.get("lastActivityAt").map(json_i64))
        .unwrap_or(0)
}

fn json_i64(value: &Value) -> i64 {
    value
        .as_i64()
        .or_else(|| value.as_f64().map(|n| n as i64))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write(path: &Path, value: &Value) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
    }

    fn session(root: &Path, account: &str, org: &str, id: &str, activity: i64, archived: bool) {
        write(
            &root
                .join(account)
                .join(org)
                .join(format!("local_{id}.json")),
            &json!({ "sessionId": format!("local_{id}"), "lastActivityAt": activity, "isArchived": archived }),
        );
    }

    #[test]
    fn session_merge_copies_new_and_newer_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        session(root, "A", "o", "s1", 10, false);
        session(root, "A", "o", "s2", 10, false);
        session(root, "B", "o", "s2", 20, false);
        session(root, "B", "o", "s3", 5, false);
        session(root, "C", "p", "s3", 50, false);

        let merge = plan_session_merge(root, "A", "o", &BTreeSet::new());
        let copied: Vec<_> = merge.copied.iter().map(|(s, _)| s.clone()).collect();
        assert_eq!(copied, vec![root.join("C/p/local_s3.json")]);
        let updated: Vec<_> = merge.updated.iter().map(|(s, _)| s.clone()).collect();
        assert_eq!(updated, vec![root.join("B/o/local_s2.json")]);
    }

    #[test]
    fn session_merge_respects_local_deletions_and_tombstones() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("A/o")).unwrap();
        session(root, "B", "o", "gone", 10, false);
        session(root, "B", "o", "tomb", 10, false);
        session(root, "B", "o", "keep", 10, false);
        std::fs::write(root.join("B/o/deleted_tomb"), "x").unwrap();

        let deleted = BTreeSet::from(["local_gone.json".to_string()]);
        let merge = plan_session_merge(root, "A", "o", &deleted);
        assert_eq!(merge.copied.len(), 1);
        assert!(merge.copied[0].0.ends_with("local_keep.json"));
        assert_eq!(merge.skipped_deleted, 2);
    }

    #[test]
    fn deletions_accumulate_across_folds() {
        let mut before = Synced::new();
        before.entry("A".into()).or_default().sessions = BTreeSet::from(["s1".into(), "s2".into()]);
        before.entry("A".into()).or_default().routines = BTreeSet::from(["r1".into()]);
        // A 删了 s2 和 r1
        let mut now = Synced::new();
        now.entry("A".into()).or_default().sessions = BTreeSet::from(["s1".into()]);
        let folded = fold_deletions(&before, &now);
        assert_eq!(
            folded["A"].deleted_sessions,
            BTreeSet::from(["s2".to_string()])
        );
        assert_eq!(
            folded["A"].deleted_routines,
            BTreeSet::from(["r1".to_string()])
        );
        // 再折叠一次（s2 已经不在「上次有」里了）删除记录也还在
        let again = fold_deletions(&folded, &now);
        assert_eq!(
            again["A"].deleted_sessions,
            BTreeSet::from(["s2".to_string()])
        );
        // 第一次运行（没有记录）不算删除
        assert!(fold_deletions(&Synced::new(), &now)["A"]
            .deleted_sessions
            .is_empty());
    }

    #[test]
    fn session_merge_uses_mtime_when_activity_ties() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        session(root, "A", "o", "s1", 10, false);
        session(root, "B", "o", "s1", 10, true);
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(root.join("A/o/local_s1.json"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        let merge = plan_session_merge(root, "A", "o", &BTreeSet::new());
        assert_eq!(merge.updated.len(), 1);
        // 反过来，目标更新时不覆盖
        let merge = plan_session_merge(root, "B", "o", &BTreeSet::new());
        assert!(merge.updated.is_empty());
    }

    #[test]
    fn archived_index_follows_session_flags() {
        let dir = tempfile::tempdir().unwrap();
        let org = dir.path().join("A/o");
        session(dir.path(), "A", "o", "s1", 1, true);
        session(dir.path(), "A", "o", "s2", 1, false);
        write(
            &org.join(ARCHIVED_INDEX),
            &json!({"v":1,"archived":["local_s2"]}),
        );

        let bytes = rebuild_archived_index(&org).expect("changed");
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value, json!({"v":1,"archived":["local_s1"]}));

        std::fs::write(org.join(ARCHIVED_INDEX), bytes).unwrap();
        assert!(rebuild_archived_index(&org).is_none());
    }

    #[test]
    fn scheduled_merge_unions_and_keeps_extra_keys() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            &root.join("A/o").join(SCHEDULED_TASKS),
            &json!({"scheduledTasks":[{"id":"t1","createdAt":1}],"recordedSkips":{},"stamp":true}),
        );
        write(
            &root.join("B/o").join(SCHEDULED_TASKS),
            &json!({"scheduledTasks":[{"id":"t2","createdAt":2}],"recordedSkips":{"k":1}}),
        );
        let merge = plan_scheduled_merge(root, "A", "o", &Synced::new(), &BTreeSet::new()).unwrap();
        let value: Value = serde_json::from_slice(&merge.bytes).unwrap();
        assert_eq!(merge.added, 1);
        assert_eq!(value["stamp"], json!(true));
        assert_eq!(value["recordedSkips"], json!({"k":1}));
        assert_eq!(value["scheduledTasks"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn scheduled_merge_does_not_resurrect_local_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            &root.join("A/o").join(SCHEDULED_TASKS),
            &json!({"scheduledTasks":[],"recordedSkips":{}}),
        );
        write(
            &root.join("B/o").join(SCHEDULED_TASKS),
            &json!({"scheduledTasks":[{"id":"t1"}],"recordedSkips":{}}),
        );
        let deleted = BTreeSet::from(["t1".to_string()]);
        let merge = plan_scheduled_merge(root, "A", "o", &Synced::new(), &deleted).unwrap();
        let value: Value = serde_json::from_slice(&merge.bytes).unwrap();
        assert_eq!(merge.added, 0);
        assert_eq!(value["scheduledTasks"], json!([]));
    }

    #[test]
    fn scheduled_merge_propagates_single_edit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let old = json!({"id":"t1","prompt":"old"});
        let new = json!({"id":"t1","prompt":"new"});
        write(
            &root.join("A/o").join(SCHEDULED_TASKS),
            &json!({"scheduledTasks":[old.clone()]}),
        );
        write(
            &root.join("B/o").join(SCHEDULED_TASKS),
            &json!({"scheduledTasks":[new.clone()]}),
        );
        let mut synced = Synced::new();
        for account in ["A", "B"] {
            let row = synced.entry(account.into()).or_default();
            row.routines.insert("t1".into());
            row.routine_definitions.insert("t1".into(), old.clone());
            row.canonical_routines.insert("t1".into(), old.clone());
        }
        let merge = plan_scheduled_merge(root, "A", "o", &synced, &BTreeSet::new()).unwrap();
        let value: Value = serde_json::from_slice(&merge.bytes).unwrap();
        assert_eq!(merge.updated, 1);
        assert_eq!(value["scheduledTasks"], json!([new]));
    }

    #[test]
    fn preset_merge_adds_missing_folders_once() {
        let doc = json!({
            "byAccount": {
                "A": {"presets":[{"id":"p1","target":{"kind":"local","cwd":"/x"}}], "builtIns":{"k":1}},
                "B": {"presets":[
                    {"id":"p2","target":{"kind":"local","cwd":"/x"}},
                    {"id":"p3","target":{"kind":"local","cwd":"/y"},"lastUsedAt":5},
                    {"id":"gone","target":{"kind":"local","cwd":"/z"}}
                ]},
                "C": {"presets":[{"id":"p4","target":{"kind":"local","cwd":"/y"},"lastUsedAt":1}]}
            },
            "other": true
        });
        let deleted = BTreeSet::from(["gone".to_string()]);
        let merge = plan_preset_merge(&doc, "A", &deleted).expect("changed");
        assert_eq!(merge.added, 1);
        let ids: Vec<&str> = merge.document["byAccount"]["A"]["presets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["p1", "p3"]);
        assert_eq!(merge.document["byAccount"]["A"]["builtIns"], json!({"k":1}));
        assert_eq!(merge.document["other"], json!(true));
        assert!(plan_preset_merge(&merge.document, "A", &deleted).is_none());
    }

    #[test]
    fn preset_merge_creates_missing_account_entry() {
        let doc = json!({"byAccount": {"B": {"presets":[{"id":"p","target":{"kind":"local","cwd":"/x"}}]}}});
        let merge = plan_preset_merge(&doc, "A", &BTreeSet::new()).expect("changed");
        assert_eq!(
            merge.document["byAccount"]["A"]["presets"][0]["id"],
            json!("p")
        );
    }

    #[test]
    fn config_token_swap_and_clear() {
        let config = br#"{"oauth:tokenCache":"old","oauth:tokenCacheV2":"old2","lastKnownAccountUuid":"x","keep":1}"#;
        let swapped: Value =
            serde_json::from_slice(&swap_config_tokens(config, None, Some("new2"), "y").unwrap())
                .unwrap();
        assert_eq!(
            swapped,
            json!({"oauth:tokenCacheV2":"new2","lastKnownAccountUuid":"y","keep":1})
        );
        assert_eq!(
            logged_in_account(&serde_json::to_vec(&swapped).unwrap()),
            Some("y".into())
        );
        let cleared: Value = serde_json::from_slice(&clear_config_tokens(config).unwrap()).unwrap();
        assert_eq!(cleared, json!({"keep":1}));
        assert_eq!(
            logged_in_account(&serde_json::to_vec(&cleared).unwrap()),
            None
        );
    }

    #[test]
    fn synced_ledger_round_trips_with_preset_field() {
        let mut synced = Synced::new();
        synced
            .entry("A".into())
            .or_default()
            .presets
            .insert("p".into());
        let bytes = serde_json::to_vec(&synced).unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("ccSwitchPresets"));
        assert_eq!(parse_synced(&bytes), synced);
        // claude-acc 早期格式
        let old = parse_synced(br#"{"A":["t1"]}"#);
        assert!(old["A"].routines.contains("t1"));
    }
}
