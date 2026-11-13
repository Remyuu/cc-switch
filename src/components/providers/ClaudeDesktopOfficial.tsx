import React from "react";
import { useTranslation } from "react-i18next";
import { useQuery } from "@tanstack/react-query";
import { AlertTriangle } from "lucide-react";
import { authApi } from "@/lib/api";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectSeparator,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { CLAUDE_DESKTOP_AUTH_PROVIDER } from "@/lib/api/claudeDesktopAccounts";
import { resolveManagedAccountId } from "@/lib/authBinding";
import { useClaudeDesktopAccountQuota } from "@/lib/query/subscription";
import { SubscriptionQuotaView } from "@/components/SubscriptionQuotaFooter";
import { useQuotaDisplay } from "@/components/quota/useQuotaDisplay";
import { claudePlanLabel } from "@/lib/claudePlan";
import { isMac } from "@/lib/platform";
import type { Provider } from "@/types";
import type { ClaudeAccountQuota } from "@/types/subscription";

/** 授权中心 Claude Desktop 分组和卡片共用的账号列表查询 */
export const CLAUDE_DESKTOP_STATUS_KEY = [
  "managed-auth-status",
  CLAUDE_DESKTOP_AUTH_PROVIDER,
] as const;

export function useClaudeDesktopAccounts(enabled = true) {
  return useQuery({
    queryKey: CLAUDE_DESKTOP_STATUS_KEY,
    queryFn: () => authApi.authGetStatus(CLAUDE_DESKTOP_AUTH_PROVIDER),
    staleTime: 30_000,
    enabled: enabled && isMac(),
  });
}

/**
 * Claude Desktop 官方卡绑定的 Desktop 账号 UUID；没绑（沿用 Desktop 现在的登录）时为 null。
 * 账号切换只在 macOS 上做，其他平台忽略绑定。
 */
export function boundClaudeDesktopAccountId(
  provider: Pick<Provider, "meta" | "category">,
): string | null {
  if (!isMac() || provider.category !== "official") return null;
  return (
    resolveManagedAccountId(
      provider.meta,
      CLAUDE_DESKTOP_AUTH_PROVIDER,
    )?.trim() || null
  );
}

const DEFAULT_LOGIN = "__current__";
const MANAGE_ACCOUNTS = "__manage__";

/**
 * Claude Desktop 官方卡的「登录方式」：沿用 Desktop 现在的登录，或绑一个收录过的账号。
 * 绑了账号的卡启用时，CC Switch 会把 Desktop 切到这个号（Desktop 会重启）。
 */
export function ClaudeDesktopAccountSelect({
  value,
  onChange,
  onManageAccounts,
}: {
  value: string | null;
  onChange: (accountId: string | null) => void;
  onManageAccounts?: () => void;
}) {
  const { t } = useTranslation();
  const { data } = useClaudeDesktopAccounts();
  if (!isMac()) return null;
  const accounts = data?.accounts ?? [];
  const selected = accounts.find((account) => account.id === value);
  const missing = value !== null && !selected;
  const followLabel = t("claudeDesktopAccounts.followCurrent");
  const missingLabel = t("claudeDesktopAccounts.missing");
  const label = t("claudeDesktopAccounts.loginMethod");
  const itemClass =
    "min-w-0 overflow-hidden py-2 pl-6 [&>span:last-child]:min-w-0 [&>span:last-child]:flex-1 [&>span:last-child]:overflow-hidden";

  return (
    <div className="space-y-2.5">
      <Label className="text-sm font-medium text-fg-1">{label}</Label>
      <Select
        value={value ?? DEFAULT_LOGIN}
        onValueChange={(next) => {
          if (!next) return;
          if (next === MANAGE_ACCOUNTS) {
            onManageAccounts?.();
            return;
          }
          onChange(next === DEFAULT_LOGIN ? null : next);
        }}
      >
        <SelectTrigger
          className="h-10 min-w-0 rounded-lg bg-surface px-3 shadow-sm"
          aria-label={label}
        >
          <span
            className="min-w-0 flex-1 truncate text-left text-sm font-medium tracking-tight"
            title={selected?.login}
          >
            <SelectValue>
              {value === null
                ? followLabel
                : (selected?.login ?? (data ? missingLabel : undefined))}
            </SelectValue>
          </span>
        </SelectTrigger>
        <SelectContent className="w-[var(--radix-select-trigger-width)] max-w-[var(--radix-select-content-available-width)]">
          <SelectItem value={DEFAULT_LOGIN} className={itemClass}>
            {followLabel}
          </SelectItem>
          {accounts.map((account) => (
            <React.Fragment key={account.id}>
              <SelectSeparator className="mx-2 my-0 bg-border/60" />
              <SelectItem
                value={account.id}
                disabled={account.requires_reauth}
                className={itemClass}
              >
                <span
                  className="min-w-0 truncate text-sm font-medium leading-5"
                  title={account.login}
                >
                  {account.login}
                </span>
              </SelectItem>
            </React.Fragment>
          ))}
          {missing && value && (
            <SelectItem value={value} className={itemClass}>
              {missingLabel}
            </SelectItem>
          )}
          {onManageAccounts && (
            <>
              <SelectSeparator className="mx-2 my-0 bg-border/60" />
              <SelectItem value={MANAGE_ACCOUNTS} className={itemClass}>
                {t("claudeDesktopAccounts.manageAccounts")}
              </SelectItem>
            </>
          )}
        </SelectContent>
      </Select>
      <p className="m-0 text-caption text-fg-2">
        {t("claudeDesktopAccounts.loginMethodHint")}
      </p>
    </div>
  );
}

/**
 * 绑了 Desktop 账号的官方卡副标题：写账号（卡名里已有账号就写「Claude Desktop 账号」），
 * 开了额度查询时跟套餐；账号不在这台电脑上、或缺少保存的登录时给提示。
 */
export function ClaudeDesktopBoundAccountLine({
  accountId,
  providerName,
  note,
  showPlan = false,
}: {
  accountId: string;
  providerName: string;
  note?: string;
  showPlan?: boolean;
}) {
  const { t } = useTranslation();
  const { data, isSuccess } = useClaudeDesktopAccounts();
  // 只读额度查询的缓存（键和 footer 一致），不另发请求
  const { data: quota } = useQuery<ClaudeAccountQuota>({
    queryKey: ["claude_desktop", "quota", accountId],
    enabled: false,
  });
  if (!isSuccess) return null;
  const account = data?.accounts.find((item) => item.id === accountId);
  if (!account) {
    return (
      <div className="mt-0.5 flex min-w-0 items-center gap-1 text-caption text-warning-text">
        <AlertTriangle className="h-3.5 w-3.5 shrink-0" />
        <span className="min-w-0 truncate">
          {t("claudeDesktopAccounts.missing")}
        </span>
      </div>
    );
  }
  const plan = showPlan
    ? claudePlanLabel(
        t,
        quota?.plan?.subscriptionType,
        quota?.plan?.rateLimitTier,
      )
    : null;
  const name = providerName.trim();
  const label =
    note ??
    (name === account.login || name.includes(account.login)
      ? t("claudeDesktopAccounts.accountWord")
      : account.login);
  return (
    <div className="mt-0.5 flex min-w-0 items-center gap-2 text-caption text-fg-2">
      <span className="min-w-0 truncate" title={note ?? account.login}>
        {label}
      </span>
      {plan && <span className="-ms-1 shrink-0">· {plan}</span>}
      {account.requires_reauth && (
        <span className="inline-flex shrink-0 items-center gap-1 text-warning-text">
          <AlertTriangle className="h-3.5 w-3.5" />
          {t("claudeDesktopAccounts.incomplete")}
        </span>
      )}
    </div>
  );
}

/** 绑了 Desktop 账号的官方卡额度：每张卡按自己的号查，正在用的那张按设置的间隔自动刷新。 */
export function ClaudeDesktopAccountQuotaFooter({
  accountId,
  inline = false,
  isCurrent = false,
  autoQueryInterval = 5,
}: {
  accountId: string;
  inline?: boolean;
  isCurrent?: boolean;
  autoQueryInterval?: number;
}) {
  const display = useQuotaDisplay();
  const {
    data: quota,
    isFetching: loading,
    refetch,
  } = useClaudeDesktopAccountQuota(accountId, {
    autoQuery: isCurrent && autoQueryInterval > 0,
    autoQueryIntervalMinutes: autoQueryInterval,
  });
  return (
    <SubscriptionQuotaView
      quota={quota}
      loading={loading}
      refetch={refetch}
      inline={inline}
      display={display}
    />
  );
}
