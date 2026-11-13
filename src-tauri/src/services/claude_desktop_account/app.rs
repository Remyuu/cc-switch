//! 切换里唯一和外部世界打交道的部分：退出 / 重启 Claude Desktop、写回滚归档。
//!
//! 放在 trait 后面，切换流程的「步骤顺序」可以用记录器单测：必须先快照旧账号再改写
//! `config.json`，并且无论前面哪步失败都要把 Desktop 重新打开。

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use crate::error::AppError;

pub trait AppControl: Send + Sync {
    /// Desktop 是否在运行。
    fn is_running(&self) -> Result<bool, AppError>;
    /// 停掉 Desktop，真正退出后才返回（SQLite / LevelDB 在写时不能复制）。
    fn quit(&self) -> Result<(), AppError>;
    fn relaunch(&self) -> Result<(), AppError>;
    /// 把 `members`（相对 `root`）打包成 gzip tar。
    fn archive(&self, archive: &Path, root: &Path, members: &[String]) -> Result<(), AppError>;
    /// 删掉 `cleanup` 列出的路径后解开归档，回滚一次失败的切换。
    fn restore(&self, archive: &Path, root: &Path, cleanup: &[&str]) -> Result<(), AppError>;
}

/// 真实的 macOS 实现。只调用固定路径的系统程序，不从 `PATH` 解析。
pub struct DesktopApp;

const QUIT_GRACE: Duration = Duration::from_secs(2);
const QUIT_POLL: Duration = Duration::from_millis(100);
const QUIT_POLLS: usize = 30;
/// 主进程可执行文件路径的结尾；助手进程路径不同，杀主进程会一起带走。
const MAIN_EXECUTABLE: &str = "/Claude.app/Contents/MacOS/Claude";

fn wait_until_stopped(app: &DesktopApp) -> Result<bool, AppError> {
    for _ in 0..QUIT_POLLS {
        if !app.is_running()? {
            return Ok(true);
        }
        std::thread::sleep(QUIT_POLL);
    }
    Ok(!app.is_running()?)
}

/// `pkill -x Claude` / `pgrep` 找不到 Desktop 主进程，只能从 `ps` 按可执行文件路径匹配。
fn main_pid() -> Option<String> {
    let output = Command::new("/bin/ps")
        .args(["-Ao", "pid=,comm="])
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| {
            let (pid, command) = line.trim().split_once(char::is_whitespace)?;
            command
                .trim()
                .ends_with(MAIN_EXECUTABLE)
                .then(|| pid.to_string())
        })
}

fn signal_main(signal: &str) {
    if let Some(pid) = main_pid() {
        let _ = Command::new("/bin/kill").args([signal, &pid]).output();
    }
}

#[cfg(unix)]
fn set_private_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn set_private_mode(_path: &Path, _mode: u32) {}

impl AppControl for DesktopApp {
    fn is_running(&self) -> Result<bool, AppError> {
        // 问 AppleScript「是否在运行」不会把它启动
        let output = Command::new("/usr/bin/osascript")
            .args(["-e", "application \"Claude\" is running"])
            .output()
            .map_err(|e| AppError::Message(format!("无法判断 Claude Desktop 是否在运行: {e}")))?;
        if !output.status.success() {
            return Err(AppError::Message(format!(
                "无法判断 Claude Desktop 是否在运行（osascript 退出码 {}）",
                output.status.code().unwrap_or(-1)
            )));
        }
        match String::from_utf8_lossy(&output.stdout).trim() {
            "true" => Ok(true),
            "false" => Ok(false),
            other => Err(AppError::Message(format!(
                "无法判断 Claude Desktop 是否在运行（意外的返回 {other:?}）"
            ))),
        }
    }

    fn quit(&self) -> Result<(), AppError> {
        if !self.is_running()? {
            return Ok(());
        }
        // 先优雅退出，让它自己收拾子进程；信号只是兜底
        let _ = Command::new("/usr/bin/osascript")
            .args(["-e", "tell application \"Claude\" to quit"])
            .output();
        std::thread::sleep(QUIT_GRACE);
        if wait_until_stopped(self)? {
            return Ok(());
        }
        signal_main("-TERM");
        if wait_until_stopped(self)? {
            return Ok(());
        }
        signal_main("-KILL");
        if wait_until_stopped(self)? {
            Ok(())
        } else {
            Err(AppError::Message(
                "Claude Desktop 没有退出，账号数据没有改动".into(),
            ))
        }
    }

    fn relaunch(&self) -> Result<(), AppError> {
        let output = Command::new("/usr/bin/open")
            .args(["-a", "Claude"])
            .output()
            .map_err(|e| AppError::Message(format!("无法重新打开 Claude Desktop: {e}")))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(AppError::Message(format!(
                "无法重新打开 Claude Desktop（open 退出码 {}）",
                output.status.code().unwrap_or(-1)
            )))
        }
    }

    fn archive(&self, archive: &Path, root: &Path, members: &[String]) -> Result<(), AppError> {
        if let Some(parent) = archive.parent() {
            std::fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;
            // 归档里有 Cookie、凭据和浏览器状态
            set_private_mode(parent, 0o700);
        }
        let output = Command::new("/usr/bin/tar")
            .arg("-czf")
            .arg(archive)
            .arg("-C")
            .arg(root)
            .arg("--")
            .args(members)
            .output()
            .map_err(|e| AppError::Message(format!("无法运行 tar: {e}")))?;
        if output.status.success() {
            set_private_mode(archive, 0o600);
            return Ok(());
        }
        Err(AppError::Message(format!(
            "无法写入回滚归档 {}（tar 退出码 {}）: {}",
            archive.display(),
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }

    fn restore(&self, archive: &Path, root: &Path, cleanup: &[&str]) -> Result<(), AppError> {
        for member in cleanup {
            super::remove_if_present(&root.join(member))?;
        }
        let output = Command::new("/usr/bin/tar")
            .arg("-xzf")
            .arg(archive)
            .arg("-C")
            .arg(root)
            .output()
            .map_err(|e| AppError::Message(format!("无法运行 tar 回滚: {e}")))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(AppError::Message(format!(
                "无法从 {} 回滚（tar 退出码 {}）",
                archive.display(),
                output.status.code().unwrap_or(-1)
            )))
        }
    }
}

/// 只记录调用顺序、不真正动手的实现，供单测断言步骤顺序。
#[cfg(test)]
#[derive(Default)]
pub struct Recorder {
    pub calls: std::sync::Mutex<Vec<String>>,
    pub running: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
impl Recorder {
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    fn push(&self, call: impl Into<String>) {
        self.calls.lock().unwrap().push(call.into());
    }
}

#[cfg(test)]
impl AppControl for Recorder {
    fn is_running(&self) -> Result<bool, AppError> {
        Ok(self.running.load(std::sync::atomic::Ordering::SeqCst))
    }
    fn quit(&self) -> Result<(), AppError> {
        self.push("quit");
        self.running
            .store(false, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    fn relaunch(&self) -> Result<(), AppError> {
        self.push("relaunch");
        self.running
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    fn archive(&self, archive: &Path, _root: &Path, _members: &[String]) -> Result<(), AppError> {
        self.push("archive");
        std::fs::create_dir_all(archive.parent().unwrap()).unwrap();
        std::fs::write(archive, b"").unwrap();
        Ok(())
    }
    fn restore(&self, _archive: &Path, _root: &Path, _cleanup: &[&str]) -> Result<(), AppError> {
        self.push("restore");
        Ok(())
    }
}
