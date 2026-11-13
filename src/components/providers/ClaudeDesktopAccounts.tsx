import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import type { TFunction } from "i18next";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  ArrowLeftRight,
  Loader2,
  Plus,
  RefreshCw,
  Save,
  Trash2,
  UserRound,
} from "lucide-react";
import { toast } from "@/lib/toast";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Notice } from "@/components/ui/notice";
import { HelpTip } from "@/components/ui/help-tip";
import { ConfirmDialog } from "@/components/ConfirmDialog";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { cn } from "@/lib/utils";
import { isMac } from "@/lib/platform";
import { extractErrorMessage } from "@/utils/errorUtils";
import {
  claudeDesktopAccountsApi,
  type ClaudeDesktopCapturePhase,
  type ClaudeDesktopProfile,
  type ClaudeDesktopWorkspaceReport,
} from "@/lib/api/claudeDesktopAccounts";

export const CLAUDE_DESKTOP_ACCOUNTS_KEY = ["claude-desktop-accounts"] as const;

const ACTIVE_PHASES: ClaudeDesktopCapturePhase[] = [
  "preparing",
  "waitingLogin",
  "waitingOrg",
  "saving",
];

/** 和后端 `validate_label` 一致 */
const LABEL_PATTERN = /^[^./\\:][^/\\:]{0,63}$/u;

function useClaudeDesktopAccounts() {
  return useQuery({
    queryKey: CLAUDE_DESKTOP_ACCOUNTS_KEY,
    queryFn: claudeDesktopAccountsApi.status,
    enabled: isMac(),
    staleTime: 10_000,
    // 添加账号时轮询进度
    refetchInterval: (query) => {
      const data = query.state.data;
      return data && (ACTIVE_PHASES.includes(data.capture.phase) || data.busy)
        ? 1_500
        : false;
    },
    refetchOnWindowFocus: true,
  });
}

function workspaceSummary(
  t: TFunction,
  report: ClaudeDesktopWorkspaceReport,
): string {
  return t("claudeDesktopAccounts.workspaceSummary", {
    sessions: report.sessionsCopied + report.sessionsUpdated,
    routines: report.routinesAdded + report.routinesUpdated,
    presets: report.presetsAdded,
  });
}

type NameDialogMode = "capture" | "save";

/**
 * Claude Desktop 供应商页顶部：Desktop 官方登录的多账号切换（macOS）。
 * 切换时 Desktop 会退出、换上目标账号的登录、重新打开，并把 Code 页的会话、定时任务、
 * 工作区预设同步过去。档案目录与 claude-acc / ai-usagebar 共用。
 */
export function ClaudeDesktopAccounts() {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const status = useClaudeDesktopAccounts();
  const [nameDialog, setNameDialog] = useState<NameDialogMode | null>(null);
  const [confirmSwitch, setConfirmSwitch] =
    useState<ClaudeDesktopProfile | null>(null);
  const [confirmRemove, setConfirmRemove] =
    useState<ClaudeDesktopProfile | null>(null);
  const [confirmSync, setConfirmSync] = useState(false);

  const refresh = () =>
    queryClient.invalidateQueries({ queryKey: CLAUDE_DESKTOP_ACCOUNTS_KEY });
  const onError = (titleKey: string) => (error: unknown) =>
    toast.error(t(titleKey), {
      description: extractErrorMessage(error) || undefined,
    });

  const switchMutation = useMutation({
    mutationFn: (label: string) => claudeDesktopAccountsApi.switchTo(label),
    onSuccess: (report) => {
      setConfirmSwitch(null);
      toast.success(
        t("claudeDesktopAccounts.switched", { label: report.label }),
        { description: workspaceSummary(t, report.workspace) },
      );
      [...report.notes, ...report.workspace.notes].forEach((note) =>
        toast.warning(note),
      );
    },
    onError: onError("claudeDesktopAccounts.switchFailed"),
    onSettled: () => void refresh(),
  });

  const removeMutation = useMutation({
    mutationFn: (label: string) => claudeDesktopAccountsApi.remove(label),
    onSuccess: () => setConfirmRemove(null),
    onError: onError("claudeDesktopAccounts.removeFailed"),
    onSettled: () => void refresh(),
  });

  const syncMutation = useMutation({
    mutationFn: claudeDesktopAccountsApi.syncWorkspace,
    onSuccess: (report) => {
      setConfirmSync(false);
      toast.success(t("claudeDesktopAccounts.synced"), {
        description: workspaceSummary(t, report),
      });
      report.notes.forEach((note) => toast.warning(note));
    },
    onError: onError("claudeDesktopAccounts.syncFailed"),
    onSettled: () => void refresh(),
  });

  const nameMutation = useMutation({
    mutationFn: ({
      mode,
      label,
      email,
    }: {
      mode: NameDialogMode;
      label: string;
      email: string;
    }) =>
      mode === "save"
        ? claudeDesktopAccountsApi.saveCurrent(label, email).then(() => mode)
        : claudeDesktopAccountsApi.captureStart(label, email).then(() => mode),
    onSuccess: (mode) => {
      setNameDialog(null);
      if (mode === "save") toast.success(t("claudeDesktopAccounts.saved"));
    },
    onError: onError("claudeDesktopAccounts.actionFailed"),
    onSettled: () => void refresh(),
  });

  // 添加账号结束时提示一次，然后让后端回到空闲
  const capture = status.data?.capture;
  const announced = useRef<string | null>(null);
  useEffect(() => {
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
        toast.success(
          t("claudeDesktopAccounts.captureRefreshed", {
            label: capture.message ?? label,
          }),
        );
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

  if (!isMac()) return null;
  const data = status.data;
  if (!data || !data.supported || !data.installed) return null;

  const capturing = ACTIVE_PHASES.includes(data.capture.phase);
  const working =
    data.busy ||
    capturing ||
    switchMutation.isPending ||
    syncMutation.isPending ||
    removeMutation.isPending ||
    nameMutation.isPending;

  return (
    <section className="space-y-2">
      <div className="flex min-h-8 items-center gap-2">
        <div className="flex min-w-0 flex-1 items-center gap-1 text-caption font-semibold text-fg-2">
          <h2 className="m-0 text-caption font-semibold">
            {t("claudeDesktopAccounts.title")}
          </h2>
          <HelpTip title={t("claudeDesktopAccounts.title")}>
            {t("claudeDesktopAccounts.help", { dir: data.profilesDir })}
          </HelpTip>
        </div>
        <Button
          variant="quiet"
          size="compact"
          disabled={working || !data.activeAccountUuid}
          onClick={() => setConfirmSync(true)}
          title={t("claudeDesktopAccounts.syncHint")}
        >
          <RefreshCw
            className={cn(
              "me-1 h-3.5 w-3.5",
              syncMutation.isPending && "animate-spin",
            )}
          />
          {t("claudeDesktopAccounts.sync")}
        </Button>
        <Button
          variant="neutral"
          size="compact"
          disabled={working || data.activeUnmanaged}
          onClick={() => setNameDialog("capture")}
          title={
            data.activeUnmanaged
              ? t("claudeDesktopAccounts.saveFirst")
              : t("claudeDesktopAccounts.addHint")
          }
        >
          <Plus className="me-1 h-3.5 w-3.5" />
          {t("claudeDesktopAccounts.add")}
        </Button>
      </div>

      {data.activeUnmanaged && (
        <Notice
          tone="warning"
          title={t("claudeDesktopAccounts.unmanagedTitle")}
          actions={
            <Button
              size="compact"
              variant="neutral"
              disabled={working}
              onClick={() => setNameDialog("save")}
            >
              <Save className="me-1 h-3.5 w-3.5" />
              {t("claudeDesktopAccounts.saveCurrent")}
            </Button>
          }
        >
          {t("claudeDesktopAccounts.unmanagedBody")}
        </Notice>
      )}

      {capturing && (
        <Notice
          tone="neutral"
          icon={Loader2}
          title={t(`claudeDesktopAccounts.phase.${data.capture.phase}`, {
            label: data.capture.label ?? "",
          })}
          actions={
            data.capture.phase !== "saving" && (
              <Button
                size="compact"
                variant="neutral"
                onClick={() =>
                  void claudeDesktopAccountsApi.captureCancel().then(refresh)
                }
              >
                {t("common.cancel")}
              </Button>
            )
          }
        >
          {t(`claudeDesktopAccounts.phaseHint.${data.capture.phase}`)}
        </Notice>
      )}

      {data.profiles.length === 0 && !data.activeUnmanaged ? (
        <div className="rounded-panel border border-dashed border-border px-4 py-3 text-caption text-fg-3">
          {t("claudeDesktopAccounts.empty")}
        </div>
      ) : (
        <div className="space-y-2">
          {data.profiles.map((profile) => (
            <ProfileRow
              key={profile.label}
              profile={profile}
              disabled={working}
              switching={
                switchMutation.isPending &&
                switchMutation.variables === profile.label
              }
              canSwitch={!data.activeUnmanaged}
              onSwitch={() => setConfirmSwitch(profile)}
              onRemove={() => setConfirmRemove(profile)}
            />
          ))}
        </div>
      )}

      <AccountNameDialog
        mode={nameDialog}
        pending={nameMutation.isPending}
        existing={data.profiles.map((p) => p.label)}
        onCancel={() => setNameDialog(null)}
        onConfirm={(label, email) =>
          nameDialog && nameMutation.mutate({ mode: nameDialog, label, email })
        }
      />
      <ConfirmDialog
        isOpen={confirmSwitch !== null}
        variant="info"
        title={t("claudeDesktopAccounts.switchTitle", {
          label: confirmSwitch?.label ?? "",
        })}
        message={t("claudeDesktopAccounts.switchConfirm")}
        confirmText={t("claudeDesktopAccounts.switch")}
        pending={switchMutation.isPending}
        onConfirm={() =>
          confirmSwitch && switchMutation.mutate(confirmSwitch.label)
        }
        onCancel={() => !switchMutation.isPending && setConfirmSwitch(null)}
      />
      <ConfirmDialog
        isOpen={confirmRemove !== null}
        title={t("claudeDesktopAccounts.removeTitle")}
        message={t("claudeDesktopAccounts.removeConfirm", {
          label: confirmRemove?.label ?? "",
        })}
        confirmText={t("common.delete")}
        pending={removeMutation.isPending}
        onConfirm={() =>
          confirmRemove && removeMutation.mutate(confirmRemove.label)
        }
        onCancel={() => !removeMutation.isPending && setConfirmRemove(null)}
      />
      <ConfirmDialog
        isOpen={confirmSync}
        variant="info"
        title={t("claudeDesktopAccounts.sync")}
        message={t(
          data.running
            ? "claudeDesktopAccounts.syncConfirmRunning"
            : "claudeDesktopAccounts.syncConfirm",
        )}
        confirmText={t("claudeDesktopAccounts.sync")}
        pending={syncMutation.isPending}
        onConfirm={() => syncMutation.mutate()}
        onCancel={() => !syncMutation.isPending && setConfirmSync(false)}
      />
    </section>
  );
}

function ProfileRow({
  profile,
  disabled,
  switching,
  canSwitch,
  onSwitch,
  onRemove,
}: {
  profile: ClaudeDesktopProfile;
  disabled: boolean;
  switching: boolean;
  canSwitch: boolean;
  onSwitch: () => void;
  onRemove: () => void;
}) {
  const { t } = useTranslation();
  return (
    <div
      className={cn(
        "group relative rounded-panel border bg-surface transition-[border-color,box-shadow] duration-150 hover:shadow-v7-sm",
        profile.isActive
          ? "border-border-strong"
          : "border-border hover:border-border-strong",
      )}
    >
      <div className="flex min-h-[52px] items-center gap-3 py-2 pe-3 ps-4">
        <span className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-subtle text-fg-2">
          <UserRound className="h-4 w-4" />
        </span>
        <div className="min-w-0 flex-1">
          <div className="flex min-w-0 items-center gap-1.5">
            <h3
              className="m-0 min-w-0 truncate text-strong text-fg-1"
              title={profile.label}
            >
              {profile.label}
            </h3>
            {profile.isActive && (
              <span className="shrink-0 rounded-full bg-direct-soft px-1.5 py-px text-caption text-direct-text">
                {t("claudeDesktopAccounts.active")}
              </span>
            )}
            {!profile.usable && (
              <span
                className="shrink-0 rounded-full bg-warning-soft px-1.5 py-px text-caption text-warning-text"
                title={t("claudeDesktopAccounts.incompleteHint")}
              >
                {t("claudeDesktopAccounts.incomplete")}
              </span>
            )}
          </div>
          <div className="mt-0.5 truncate text-caption text-fg-2">
            {[
              profile.email,
              t("claudeDesktopAccounts.sessionCount", {
                count: profile.sessionCount,
              }),
            ]
              .filter(Boolean)
              .join(" · ")}
          </div>
        </div>
        <div className="flex shrink-0 items-center gap-1">
          {!profile.isActive && (
            <Button
              variant="neutral"
              size="compact"
              disabled={disabled || !profile.usable || !canSwitch}
              onClick={onSwitch}
              title={
                canSwitch ? undefined : t("claudeDesktopAccounts.saveFirst")
              }
            >
              {switching ? (
                <Loader2 className="me-1 h-3.5 w-3.5 animate-spin" />
              ) : (
                <ArrowLeftRight className="me-1 h-3.5 w-3.5" />
              )}
              {t("claudeDesktopAccounts.switch")}
            </Button>
          )}
          <Button
            variant="quiet"
            size="icon-compact"
            disabled={disabled || profile.isActive}
            onClick={onRemove}
            aria-label={t("common.delete")}
            title={
              profile.isActive
                ? t("claudeDesktopAccounts.removeActiveHint")
                : t("common.delete")
            }
          >
            <Trash2 className="h-3.5 w-3.5" />
          </Button>
        </div>
      </div>
    </div>
  );
}

function AccountNameDialog({
  mode,
  pending,
  existing,
  onCancel,
  onConfirm,
}: {
  mode: NameDialogMode | null;
  pending: boolean;
  existing: string[];
  onCancel: () => void;
  onConfirm: (label: string, email: string) => void;
}) {
  const { t } = useTranslation();
  const [label, setLabel] = useState("");
  const [email, setEmail] = useState("");
  useEffect(() => {
    if (mode) {
      setLabel("");
      setEmail("");
    }
  }, [mode]);

  const trimmed = label.trim();
  const duplicate = existing.includes(trimmed);
  const invalid = trimmed.length > 0 && !LABEL_PATTERN.test(trimmed);
  const canSubmit = trimmed.length > 0 && !duplicate && !invalid && !pending;

  return (
    <Dialog
      open={mode !== null}
      onOpenChange={(open) => {
        if (!open && !pending) onCancel();
      }}
    >
      <DialogContent className="max-w-md">
        <form
          onSubmit={(e) => {
            e.preventDefault();
            e.stopPropagation();
            if (canSubmit) onConfirm(trimmed, email.trim());
          }}
        >
          <DialogHeader>
            <DialogTitle>
              {mode === "save"
                ? t("claudeDesktopAccounts.saveCurrent")
                : t("claudeDesktopAccounts.add")}
            </DialogTitle>
            <DialogDescription>
              {mode === "save"
                ? t("claudeDesktopAccounts.saveDescription")
                : t("claudeDesktopAccounts.addDescription")}
            </DialogDescription>
          </DialogHeader>
          <div className="space-y-3 px-6 py-3">
            <div className="space-y-1.5">
              <Label htmlFor="claude-desktop-account-label">
                {t("claudeDesktopAccounts.labelField")}
              </Label>
              <Input
                id="claude-desktop-account-label"
                autoFocus
                data-unsaved-ignore
                value={label}
                maxLength={64}
                placeholder={t("claudeDesktopAccounts.labelPlaceholder")}
                onChange={(e) => setLabel(e.target.value)}
              />
              {(duplicate || invalid) && (
                <p className="text-caption text-danger-text">
                  {duplicate
                    ? t("claudeDesktopAccounts.labelDuplicate")
                    : t("claudeDesktopAccounts.labelInvalid")}
                </p>
              )}
            </div>
            <div className="space-y-1.5">
              <Label htmlFor="claude-desktop-account-email">
                {t("claudeDesktopAccounts.emailField")}
              </Label>
              <Input
                id="claude-desktop-account-email"
                data-unsaved-ignore
                type="email"
                value={email}
                placeholder="name@example.com"
                onChange={(e) => setEmail(e.target.value)}
              />
            </div>
          </div>
          <DialogFooter>
            <Button
              type="button"
              variant="outline"
              disabled={pending}
              onClick={onCancel}
            >
              {t("common.cancel")}
            </Button>
            <Button type="submit" disabled={!canSubmit}>
              {pending && <Loader2 className="me-1 h-3.5 w-3.5 animate-spin" />}
              {mode === "save"
                ? t("common.save")
                : t("claudeDesktopAccounts.startLogin")}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
