import { invoke } from "@tauri-apps/api/core";
import type { ManagedAuthAccount } from "./auth";

/** 授权中心里 Claude Desktop 账号的认证供应商标识（卡片绑定存的 authProvider） */
export const CLAUDE_DESKTOP_AUTH_PROVIDER = "claude_desktop" as const;

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
  /** 收录成的账号（完成后才有） */
  accountId: string | null;
  label: string | null;
  message: string | null;
  startedAt: number | null;
}

/** Claude Desktop 现在的样子 */
export interface ClaudeDesktopLiveState {
  /** 只有 macOS 支持 */
  supported: boolean;
  installed: boolean;
  running: boolean;
  /** Desktop 登着的账号 UUID */
  activeAccountId: string | null;
  /** Desktop 已登录，但这个账号还没收录 */
  activeUnmanaged: boolean;
  capture: ClaudeDesktopCaptureStatus;
  busy: boolean;
}

export interface ClaudeDesktopAccountSummary {
  id: string;
  label: string;
  email: string | null;
  orgUuid: string | null;
  savedAt: number | null;
  sessionCount: number;
  isActive: boolean;
  usable: boolean;
}

export interface ClaudeDesktopImportResult {
  account: ClaudeDesktopAccountSummary;
  created: boolean;
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

export const claudeDesktopAccountsApi = {
  liveState: () =>
    invoke<ClaudeDesktopLiveState>("claude_desktop_account_live_state"),
  /** 收录 Desktop 当前登录的账号（名字用邮箱）；Desktop 会短暂重启 */
  importCurrent: () =>
    invoke<ClaudeDesktopImportResult>("claude_desktop_account_import_current"),
  /** Desktop 退出登录后重新打开，用户登录另一个账号后自动收录；进度看 liveState().capture */
  captureStart: () => invoke<void>("claude_desktop_account_capture_start"),
  captureCancel: () => invoke<void>("claude_desktop_account_capture_cancel"),
  captureDismiss: () => invoke<void>("claude_desktop_account_capture_dismiss"),
  /** 把所有账号的会话、定时任务、工作区预设并进当前账号（Desktop 在运行时会重启） */
  syncWorkspace: () =>
    invoke<ClaudeDesktopWorkspaceReport>("claude_desktop_workspace_sync"),
};

export type { ManagedAuthAccount };
