import React from "react";
import { useTranslation } from "react-i18next";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { RefreshCw } from "lucide-react";
import { toast } from "@/lib/toast";
import { Button } from "@/components/ui/button";
import { Notice } from "@/components/ui/notice";
import { ConfirmDialog } from "@/components/ConfirmDialog";
import { useManagedAuth } from "@/components/providers/forms/hooks/useManagedAuth";
import {
  ManagedAccountRemoveDialog,
  type ManagedAccountRemoveTarget,
} from "@/components/providers/forms/ManagedAccountRemoveDialog";
import {
  ManagedAccountsGroup,
  type GroupAccountRow,
} from "@/components/settings/auth/ManagedAccountsGroup";
import { signedInDate } from "@/components/settings/auth/accountDetails";
import { ClaudeDesktopAccountQuota } from "@/components/settings/auth/AccountQuota";
import { useManagedAccountUsers } from "@/components/providers/forms/hooks/useManagedAccountUsers";
import { CLAUDE_DESKTOP_STATUS_KEY } from "@/components/providers/ClaudeDesktopOfficial";
import {
  CLAUDE_DESKTOP_AUTH_PROVIDER,
  claudeDesktopAccountsApi,
  type ClaudeDesktopCapturePhase,
  type ClaudeDesktopWorkspaceReport,
} from "@/lib/api/claudeDesktopAccounts";
import { extractErrorMessage } from "@/utils/errorUtils";
import { isMac } from "@/lib/platform";

const PROVIDER = CLAUDE_DESKTOP_AUTH_PROVIDER;
export const CLAUDE_DESKTOP_LIVE_KEY = ["claude-desktop-account-live"] as const;

const ACTIVE_PHASES: ClaudeDesktopCapturePhase[] = [
  "preparing",
  "waitingLogin",
  "waitingOrg",
  "saving",
];

const message = (error: unknown) => extractErrorMessage(error) || String(error);

/**
 * 授权中心的 Claude Desktop 账号组（macOS）。Desktop 只有一个登录槽位，CC Switch 不能替它登录，
 * 只能收录它当前登着的账号（凭据和浏览器状态的快照），或让它退出登录、等用户登录另一个账号后收录。
 * 用哪个号由启用的、绑定了它的 Claude Desktop 官方卡决定（和 Claude Code 官方卡一样）：启用时
 * Desktop 会重启到那个号，Code 页的会话、定时任务、工作区预设一并同步过去。
 */
export function ClaudeDesktopAccountSection({
  helpSide,
}: {
  helpSide?: "top" | "bottom";
}) {
  const { t, i18n } = useTranslation();
  const queryClient = useQueryClient();
  const {
    accounts,
    defaultAccountId,
    isStatusSuccess,
    isStatusError,
    isRemovingAccount,
    removeAccount,
    refetchStatus,
  } = useManagedAuth(PROVIDER);
  const accountUsers = useManagedAccountUsers(PROVIDER, defaultAccountId);
  const [removeTarget, setRemoveTarget] =
    React.useState<ManagedAccountRemoveTarget | null>(null);
  const [confirmSync, setConfirmSync] = React.useState(false);
  const [confirmCapture, setConfirmCapture] = React.useState(false);

  const liveQuery = useQuery({
    queryKey: CLAUDE_DESKTOP_LIVE_KEY,
    queryFn: claudeDesktopAccountsApi.liveState,
    // 添加账号时轮询进度
    refetchInterval: (query) =>
      query.state.data &&
      (ACTIVE_PHASES.includes(query.state.data.capture.phase) ||
        query.state.data.busy)
        ? 1500
        : 5000,
  });
  const live = liveQuery.data;

  const refresh = async () => {
    await refetchStatus();
    await queryClient.invalidateQueries({ queryKey: CLAUDE_DESKTOP_LIVE_KEY });
    await queryClient.invalidateQueries({
      queryKey: CLAUDE_DESKTOP_STATUS_KEY,
    });
  };

  const importMutation = useMutation({
    mutationFn: claudeDesktopAccountsApi.importCurrent,
    onSuccess: async (result) => {
      toast.success(
        result.created
          ? t("authCenter.toast.added", {
              defaultValue: "已添加 {{service}} 账号。",
              service: "Claude Desktop",
            })
          : t("claudeDesktopAccounts.alreadyImported", {
              name: result.account.email ?? result.account.label,
            }),
      );
      await refresh();
    },
    onError: (error) => toast.error(message(error)),
  });

  const captureMutation = useMutation({
    mutationFn: claudeDesktopAccountsApi.captureStart,
    onSuccess: () => setConfirmCapture(false),
    onError: (error) => toast.error(message(error)),
    onSettled: () => void refresh(),
  });

  const syncMutation = useMutation({
    mutationFn: claudeDesktopAccountsApi.syncWorkspace,
    onSuccess: (report: ClaudeDesktopWorkspaceReport) => {
      setConfirmSync(false);
      toast.success(t("claudeDesktopAccounts.synced"), {
        description: t("claudeDesktopAccounts.workspaceSummary", {
          sessions: report.sessionsCopied + report.sessionsUpdated,
          routines: report.routinesAdded + report.routinesUpdated,
          presets: report.presetsAdded,
        }),
      });
      report.notes.forEach((note) => toast.warning(note));
    },
    onError: (error) =>
      toast.error(t("claudeDesktopAccounts.syncFailed"), {
        description: message(error),
      }),
    onSettled: () => void refresh(),
  });

  // 添加账号结束时提示一次，然后让后端回到空闲
  const capture = live?.capture;
  const announced = React.useRef<string | null>(null);
  React.useEffect(() => {
    if (!capture || ACTIVE_PHASES.includes(capture.phase)) return;
    if (capture.phase === "idle") {
      announced.current = null;
      return;
    }
    const key = `${capture.phase}:${capture.startedAt}`;
    if (announced.current === key) return;
    announced.current = key;
    const label = capture.label ?? "";
    switch (capture.phase) {
      case "done":
        toast.success(t("claudeDesktopAccounts.captureDone", { label }), {
          description: capture.message ?? undefined,
        });
        break;
      case "refreshed":
        toast.success(t("claudeDesktopAccounts.captureRefreshed", { label }));
        break;
      case "cancelled":
        toast.info(t("claudeDesktopAccounts.captureCancelled"));
        break;
      case "timedOut":
        toast.warning(t("claudeDesktopAccounts.captureTimedOut"));
        break;
      case "failed":
        toast.error(t("claudeDesktopAccounts.captureFailed"), {
          description: capture.message ?? undefined,
        });
        break;
    }
    void claudeDesktopAccountsApi
      .captureDismiss()
      .catch(() => undefined)
      .then(refresh);
  }, [capture, t]); // eslint-disable-line react-hooks/exhaustive-deps

  if (!isMac() || (live && (!live.supported || !live.installed))) return null;

  const capturing = Boolean(capture && ACTIVE_PHASES.includes(capture.phase));
  const working =
    Boolean(live?.busy) ||
    capturing ||
    importMutation.isPending ||
    captureMutation.isPending ||
    syncMutation.isPending;

  const rows: GroupAccountRow[] = accounts.map((account) => {
    const date = signedInDate(account.authenticated_at, i18n.language);
    return {
      id: account.id,
      login: account.login,
      details: account.requires_reauth
        ? [t("claudeDesktopAccounts.incompleteHint")]
        : date
          ? [t("claudeDesktopAccounts.addedOn", { date })]
          : [],
      isDefault: defaultAccountId === account.id,
      needsReauth: account.requires_reauth,
      users: accountUsers([account.id]),
      quota: account.requires_reauth ? undefined : (
        <ClaudeDesktopAccountQuota
          accountId={account.id}
          login={account.login}
        />
      ),
      removeDisabledReason:
        defaultAccountId === account.id
          ? t("claudeDesktopAccounts.removeActiveHint")
          : undefined,
    };
  });

  const notice = capturing ? (
    <Notice
      tone="neutral"
      title={t(`claudeDesktopAccounts.phase.${capture!.phase}`)}
      actions={
        capture!.phase !== "saving" && (
          <Button
            type="button"
            variant="neutral"
            size="compact"
            onClick={() =>
              void claudeDesktopAccountsApi.captureCancel().then(refresh)
            }
          >
            {t("common.cancel")}
          </Button>
        )
      }
    >
      {t(`claudeDesktopAccounts.phaseHint.${capture!.phase}`)}
    </Notice>
  ) : live?.activeUnmanaged ? (
    <Notice
      tone="warning"
      title={t("claudeDesktopAccounts.unmanagedTitle")}
      actions={
        <Button
          type="button"
          variant="neutral"
          size="compact"
          disabled={working}
          onClick={() => importMutation.mutate()}
        >
          {t("claudeDesktopAccounts.importNow")}
        </Button>
      }
    >
      {t("claudeDesktopAccounts.unmanagedBody")}
    </Notice>
  ) : null;

  // 添加：Desktop 当前的号还没收录时先收录它；已收录时登录另一个账号
  const onAdd = () => {
    // 读不到 Desktop 的状态（后端没更新、出错）时说清楚，别让按钮看起来没反应
    if (liveQuery.isError) {
      toast.error(t("claudeDesktopAccounts.statusFailed"), {
        description: message(liveQuery.error),
      });
      return;
    }
    if (working) {
      toast.info(t("claudeDesktopAccounts.busyHint"));
      return;
    }
    // Desktop 登着还没收录的号：先收录它；Desktop 没登录、或当前号已收录：去登录另一个账号
    if (live?.activeUnmanaged) {
      importMutation.mutate();
    } else {
      setConfirmCapture(true);
    }
  };

  return (
    <ManagedAccountsGroup
      slug="claude-desktop"
      name="Claude Desktop"
      iconName="claude"
      help={t("claudeDesktopAccounts.help")}
      helpSide={helpSide}
      accounts={rows}
      status={isStatusError ? "error" : isStatusSuccess ? "ready" : "loading"}
      statusErrorText={t("claudeDesktopAccounts.statusFailed")}
      onRetryStatus={() => void refresh()}
      notice={notice}
      emptyText={t("claudeDesktopAccounts.empty")}
      onAdd={onAdd}
      defaultLabel={t("authCenter.inUseBy", {
        defaultValue: "{{app}} 在用",
        app: "Claude Desktop",
      })}
      canReauth={false}
      onReauth={() => undefined}
      settingDefault={false}
      onRemove={(accountId, login) =>
        setRemoveTarget({ kind: "one", accountId, login })
      }
      removing={isRemovingAccount}
      login={{
        starting: false,
        polling: false,
        pollingState: "idle",
        deviceCode: null,
        error: null,
        onCancel: () => {},
        onRetry: onAdd,
      }}
    >
      <div className="flex items-center gap-2 border-t border-border py-2 pe-3 ps-4">
        <p className="m-0 min-w-0 flex-1 text-caption text-fg-2">
          {t("claudeDesktopAccounts.syncHint")}
        </p>
        <Button
          type="button"
          variant="quiet"
          size="compact"
          className="shrink-0 gap-1"
          disabled={working || !live?.activeAccountId}
          onClick={() => setConfirmSync(true)}
        >
          <RefreshCw
            className={
              syncMutation.isPending
                ? "h-3.5 w-3.5 animate-spin"
                : "h-3.5 w-3.5"
            }
          />
          {t("claudeDesktopAccounts.sync")}
        </Button>
      </div>
      <ManagedAccountRemoveDialog
        target={removeTarget}
        serviceName="Claude Desktop"
        users={
          removeTarget?.kind === "one"
            ? accountUsers([removeTarget.accountId])
            : []
        }
        othersRemain={accounts.length > 1}
        note={t("claudeDesktopAccounts.removeNote")}
        pending={isRemovingAccount}
        onConfirm={() => {
          const target = removeTarget;
          setRemoveTarget(null);
          if (target?.kind === "one") removeAccount(target.accountId);
        }}
        onCancel={() => setRemoveTarget(null)}
      />
      <ConfirmDialog
        isOpen={confirmCapture}
        variant="info"
        title={t("claudeDesktopAccounts.add")}
        message={t("claudeDesktopAccounts.addDescription")}
        confirmText={t("claudeDesktopAccounts.startLogin")}
        pending={captureMutation.isPending}
        onConfirm={() => captureMutation.mutate()}
        onCancel={() => !captureMutation.isPending && setConfirmCapture(false)}
      />
      <ConfirmDialog
        isOpen={confirmSync}
        variant="info"
        title={t("claudeDesktopAccounts.sync")}
        message={t(
          live?.running
            ? "claudeDesktopAccounts.syncConfirmRunning"
            : "claudeDesktopAccounts.syncConfirm",
        )}
        confirmText={t("claudeDesktopAccounts.sync")}
        pending={syncMutation.isPending}
        onConfirm={() => syncMutation.mutate()}
        onCancel={() => !syncMutation.isPending && setConfirmSync(false)}
      />
    </ManagedAccountsGroup>
  );
}

export default ClaudeDesktopAccountSection;
