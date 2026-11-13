use super::app::Recorder;
use super::*;
use serde_json::json;
use std::sync::atomic::Ordering;

fn write(path: &Path, contents: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn write_json(path: &Path, value: &Value) {
    write(path, &serde_json::to_vec(value).unwrap());
}

const A: &str = "aaaaaaaa-0000-0000-0000-000000000001";
const B: &str = "bbbbbbbb-0000-0000-0000-000000000002";
const ORG: &str = "00000000-0000-0000-0000-00000000000f";

/// Desktop 登录在 A；档案里存着 A（旧快照）和 B。
fn fixture() -> (tempfile::TempDir, Paths) {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("Claude");
    write_json(
        &data.join(CONFIG_JSON),
        &json!({"lastKnownAccountUuid": A, "oauth:tokenCacheV2": "live-a", "keep": true}),
    );
    write(&data.join("Cookies"), b"cookies-a");
    write(&data.join("Local Storage/leveldb/CURRENT"), b"ldb-a");
    write(&data.join(BRIDGE_FILE), b"{}");
    write_json(&data.join(DEVICE_REGISTRY), &json!({ A: {"d": 1} }));
    let sessions = data.join(SESSIONS_DIR);
    write_json(
        &sessions.join(A).join(ORG).join("local_s1.json"),
        &json!({"lastActivityAt": 10, "isArchived": false}),
    );
    write_json(
        &sessions.join(B).join(ORG).join("local_s2.json"),
        &json!({"lastActivityAt": 20, "isArchived": true}),
    );
    write_json(
        &data.join(PRESETS_JSON),
        &json!({"byAccount": {A: {"presets": [{"id": "p1", "target": {"kind": "local", "cwd": "/proj"}}]}}}),
    );

    let paths = Paths::at(data, root.path());
    for (label, uuid, token) in [("a", A, "old-a"), ("b", B, "saved-b")] {
        let dir = paths.profile_dir(label);
        write_json(
            &dir.join(META_JSON),
            &json!({"label": label, "accountUuid": uuid, "orgUuid": ORG}),
        );
        write(&dir.join(TOKEN_CACHE_V2), token.as_bytes());
        write(
            &dir.join(DESKTOP_STATE).join("Cookies"),
            format!("cookies-{label}").as_bytes(),
        );
        write(
            &dir.join(DESKTOP_STATE)
                .join("Local Storage/leveldb/CURRENT"),
            format!("ldb-{label}").as_bytes(),
        );
    }
    (root, paths)
}

fn manager(paths: &Paths) -> (ClaudeDesktopAccountManager, Arc<Recorder>) {
    let recorder = Arc::new(Recorder::default());
    recorder.running.store(true, Ordering::SeqCst);
    (
        ClaudeDesktopAccountManager::new(paths.clone(), recorder.clone()),
        recorder,
    )
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

#[test]
fn lists_accounts_by_uuid_with_active_flag() {
    let (_root, paths) = fixture();
    let (manager, _) = manager(&paths);
    let state = manager.live_state();
    assert_eq!(state.active_account_id.as_deref(), Some(A));
    assert!(!state.active_unmanaged);
    let accounts = manager.accounts();
    assert_eq!(accounts.len(), 2);
    assert_eq!(accounts[0].id, A);
    assert!(accounts[0].is_active && accounts[0].usable);
    assert_eq!(accounts[1].session_count, 1);
    assert_eq!(accounts[1].display_name(), "b");
}

#[test]
fn switch_snapshots_outgoing_merges_workspace_and_installs_target() {
    let (_root, paths) = fixture();
    let (manager, recorder) = manager(&paths);

    let report = manager.switch_to(B, None).unwrap();
    assert_eq!(report.previous_label.as_deref(), Some("a"));
    assert_eq!(recorder.calls(), vec!["quit", "archive", "relaunch"]);

    // 旧账号 A 的实时登录存回了它的档案
    assert_eq!(read(&paths.profile_dir("a").join(TOKEN_CACHE_V2)), "live-a");
    assert_eq!(
        read(&paths.profile_dir("a").join(DESKTOP_STATE).join("Cookies")),
        "cookies-a"
    );
    // 装上了 B
    let config: Value = serde_json::from_str(&read(&paths.config_json())).unwrap();
    assert_eq!(config["lastKnownAccountUuid"], json!(B));
    assert_eq!(config["oauth:tokenCacheV2"], json!("saved-b"));
    assert_eq!(config["keep"], json!(true));
    assert_eq!(read(&paths.data_dir.join("Cookies")), "cookies-b");
    assert_eq!(
        read(&paths.data_dir.join("Local Storage/leveldb/CURRENT")),
        "ldb-b"
    );
    assert!(!paths.data_dir.join(BRIDGE_FILE).exists());

    // 工作区：A 的会话、预设到了 B；B 的归档索引按会话标记重建
    let b_dir = paths.sessions_root().join(B).join(ORG);
    assert!(b_dir.join("local_s1.json").is_file());
    assert_eq!(report.workspace.sessions_copied, 1);
    assert_eq!(report.workspace.presets_added, 1);
    let archived: Value = serde_json::from_str(&read(&b_dir.join(merge::ARCHIVED_INDEX))).unwrap();
    assert_eq!(archived["archived"], json!(["local_s2"]));
    let presets: Value = serde_json::from_str(&read(&paths.presets_path())).unwrap();
    assert_eq!(presets["byAccount"][B]["presets"][0]["id"], json!("p1"));
    // 账本记下了现状
    let synced = load_synced(&paths.synced_path());
    assert!(synced[B].sessions.contains("local_s1.json"));
    assert!(synced[B].presets.contains("p1"));
}

#[test]
fn switch_refuses_when_live_login_is_unsaved() {
    let (_root, paths) = fixture();
    remove_if_present(&paths.profile_dir("a")).unwrap();
    let (manager, recorder) = manager(&paths);
    let error = manager.switch_to(B, None).unwrap_err().to_string();
    assert!(error.contains("保存当前登录"), "{error}");
    assert!(recorder.calls().is_empty());
}

#[test]
fn switch_to_active_only_runs_hook_and_unknown_fails() {
    let (_root, paths) = fixture();
    let (manager, recorder) = manager(&paths);
    let mut ran = 0;
    let mut hook = || -> Result<(), AppError> {
        ran += 1;
        Ok(())
    };
    manager.switch_to(A, Some(&mut hook)).unwrap();
    assert_eq!(ran, 1);
    assert!(manager.switch_to("nope", None).is_err());
    assert!(recorder.calls().is_empty());
}

#[test]
fn switch_hook_runs_before_relaunch_and_failure_rolls_back() {
    let (_root, paths) = fixture();
    let (manager, recorder) = manager(&paths);
    let config_seen = std::cell::RefCell::new(None);
    let mut hook = || -> Result<(), AppError> {
        // 钩子跑的时候新账号已经装好、Desktop 还没重新打开
        *config_seen.borrow_mut() = Some(read(&paths.config_json()));
        Err(AppError::Message("write failed".into()))
    };
    assert!(manager.switch_to(B, Some(&mut hook)).is_err());
    assert!(config_seen.borrow().as_deref().unwrap().contains(B));
    assert_eq!(
        recorder.calls(),
        vec!["quit", "archive", "restore", "relaunch"]
    );
}

#[test]
fn deleted_session_is_not_brought_back_on_switch_back() {
    let (_root, paths) = fixture();
    let (manager, _) = manager(&paths);
    manager.switch_to(B, None).unwrap();
    // 在 B 里删掉 s1，再切回 A，再切回 B：s1 不应回到 B
    std::fs::remove_file(
        paths
            .sessions_root()
            .join(B)
            .join(ORG)
            .join("local_s1.json"),
    )
    .unwrap();
    manager.switch_to(A, None).unwrap();
    manager.switch_to(B, None).unwrap();
    assert!(!paths
        .sessions_root()
        .join(B)
        .join(ORG)
        .join("local_s1.json")
        .exists());
    // A 自己那份不动
    assert!(paths
        .sessions_root()
        .join(A)
        .join(ORG)
        .join("local_s1.json")
        .exists());
}

#[test]
fn save_current_adopts_unmanaged_login() {
    let (_root, paths) = fixture();
    remove_if_present(&paths.profile_dir("a")).unwrap();
    let (manager, recorder) = manager(&paths);
    assert!(manager.live_state().active_unmanaged);

    save_current_profile(
        &paths,
        recorder.as_ref(),
        "main",
        Some("me@example.com".into()),
    )
    .unwrap();
    assert_eq!(recorder.calls(), vec!["quit", "relaunch"]);
    assert!(!manager.live_state().active_unmanaged);
    let main = manager.account(A).unwrap();
    assert_eq!(main.label, "main");
    assert!(main.is_active);
    assert_eq!(main.org_uuid.as_deref(), Some(ORG));
    assert_eq!(main.display_name(), "me@example.com");
    assert!(main.usable);
    // 再存一次同一个账号会被拒绝
    assert!(save_current_profile(&paths, recorder.as_ref(), "again", None).is_err());
}

#[test]
fn sync_workspace_merges_into_active_without_switching() {
    let (_root, paths) = fixture();
    let (manager, recorder) = manager(&paths);
    let report = manager.sync_workspace().unwrap();
    assert_eq!(report.sessions_copied, 1);
    assert_eq!(recorder.calls(), vec!["quit", "relaunch"]);
    assert!(paths
        .sessions_root()
        .join(A)
        .join(ORG)
        .join("local_s2.json")
        .is_file());
    let config: Value = serde_json::from_str(&read(&paths.config_json())).unwrap();
    assert_eq!(config["lastKnownAccountUuid"], json!(A));
}

#[test]
fn remove_refuses_active_profile() {
    let (_root, paths) = fixture();
    let (manager, _) = manager(&paths);
    assert!(manager.remove(A).is_err());
    manager.remove(B).unwrap();
    assert!(!paths.profile_dir("b").exists());
}

#[test]
fn labels_are_validated() {
    for bad in ["", ".hidden", "a/b", "a\\b", "x:y", "a\nb"] {
        assert!(validate_label(bad).is_err(), "{bad:?}");
    }
    for good in ["work", "个人", "me@example.com", "team-1_x"] {
        assert!(validate_label(good).is_ok(), "{good:?}");
    }
}

#[test]
fn unique_label_prefers_email_and_avoids_collisions() {
    let (_root, paths) = fixture();
    let info = token::AccountInfo {
        email: Some("b".into()),
        ..token::AccountInfo::default()
    };
    // 「b」已经有了
    assert_eq!(unique_label(&paths, Some(&info), B), "b-2");
    assert_eq!(
        unique_label(&paths, None, "12345678-abcd"),
        "claude-12345678"
    );
    let bad = token::AccountInfo {
        email: Some(".weird/name".into()),
        ..token::AccountInfo::default()
    };
    assert_eq!(
        unique_label(&paths, Some(&bad), "abcdefgh-1"),
        "claude-abcdefgh"
    );
}
