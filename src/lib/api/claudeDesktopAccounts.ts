import { invoke } from "@tauri-apps/api/core";

/** 一个保存过的 Claude Desktop 登录（`~/.claude-acc/profiles/<label>`，与 claude-acc 共用） */
export interface ClaudeDesktopProfile {
  label: string;
  email: string | null;
  accountUuid: string;
  orgUuid: string | null;
  /** 秒 */
  savedAt: number | null;
  /** 这个账号在 Code 页的会话数 */
  sessionCount: number;
  isActive: boolean;
  /** 凭据和浏览器状态都在，能切过去 */
  usable: boolean;
}

export type ClaudeDesktopCapturePhase =
  | "idle"
  | "preparing"
  | "waitingLogin"
  | "waitingOrg"
  | "saving"
  | "done"
  | "refreshed"
  | "cancelled"
  | "timedOut"
  | "failed";

export interface ClaudeDesktopCaptureStatus {
  phase: ClaudeDesktopCapturePhase;
  label: string | null;
  message: string | null;
  startedAt: number | null;
}

export interface ClaudeDesktopAccountsStatus {
  /** 只有 macOS 支持 */
  supported: boolean;
  installed: boolean;
  running: boolean;
  activeAccountUuid: string | null;
  activeLabel: string | null;
  /** Desktop 已登录，但这个账号还没保存 */
  activeUnmanaged: boolean;
  profilesDir: string;
  profiles: ClaudeDesktopProfile[];
  capture: ClaudeDesktopCaptureStatus;
  busy: boolean;
}

export interface ClaudeDesktopWorkspaceReport {
  sessionsCopied: number;
  sessionsUpdated: number;
  sessionsSkippedDeleted: number;
  routinesAdded: number;
  routinesUpdated: number;
  routineConflicts: number;
  presetsAdded: number;
  notes: string[];
}

export interface ClaudeDesktopSwitchReport {
  label: string;
  previousLabel: string | null;
  workspace: ClaudeDesktopWorkspaceReport;
  archive: string | null;
  notes: string[];
}

export const claudeDesktopAccountsApi = {
  status: () =>
    invoke<ClaudeDesktopAccountsStatus>("claude_desktop_accounts_status"),
  /** 退出 Desktop → 存回当前账号 → 同步工作区 → 装上目标账号 → 重新打开 */
  switchTo: (label: string) =>
    invoke<ClaudeDesktopSwitchReport>("claude_desktop_account_switch", {
      label,
    }),
  /** 把当前登录存成档案（不用退出登录；Desktop 会短暂重启） */
  saveCurrent: (label: string, email?: string) =>
    invoke<string>("claude_desktop_account_save_current", {
      label,
      email: email || null,
    }),
  remove: (label: string) =>
    invoke<void>("claude_desktop_account_remove", { label }),
  /** 只同步工作区到当前账号（Desktop 在运行时会短暂重启） */
  syncWorkspace: () =>
    invoke<ClaudeDesktopWorkspaceReport>("claude_desktop_workspace_sync"),
  /** 退出登录 → 等用户在 Desktop 里登录新账号 → 存成档案；进度看 status().capture */
  captureStart: (label: string, email?: string) =>
    invoke<void>("claude_desktop_account_capture_start", {
      label,
      email: email || null,
    }),
  captureCancel: () => invoke<void>("claude_desktop_account_capture_cancel"),
  captureDismiss: () => invoke<void>("claude_desktop_account_capture_dismiss"),
};
