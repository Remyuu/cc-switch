import type { ReactElement } from "react";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { ClaudeDesktopAccountSection } from "@/components/settings/auth/ClaudeDesktopAccountSection";

const mocks = vi.hoisted(() => ({
  useManagedAuth: vi.fn(),
  liveState: vi.fn(),
  importCurrent: vi.fn(),
  captureStart: vi.fn(),
  captureCancel: vi.fn(),
  captureDismiss: vi.fn(),
  syncWorkspace: vi.fn(),
  quota: vi.fn(),
  toastSuccess: vi.fn(),
  toastError: vi.fn(),
  toastInfo: vi.fn(),
}));

vi.mock("@/lib/platform", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@/lib/platform")>()),
  isMac: () => true,
}));
vi.mock("@/lib/toast", () => ({
  toast: {
    success: mocks.toastSuccess,
    error: mocks.toastError,
    warning: vi.fn(),
    info: mocks.toastInfo,
  },
}));
vi.mock("@/components/providers/forms/hooks/useManagedAuth", () => ({
  useManagedAuth: mocks.useManagedAuth,
}));
vi.mock("@/lib/api/claudeDesktopAccounts", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@/lib/api/claudeDesktopAccounts")>()),
  claudeDesktopAccountsApi: {
    liveState: mocks.liveState,
    importCurrent: mocks.importCurrent,
    captureStart: mocks.captureStart,
    captureCancel: mocks.captureCancel,
    captureDismiss: mocks.captureDismiss,
    syncWorkspace: mocks.syncWorkspace,
  },
}));
vi.mock("@/lib/api/subscription", async (importOriginal) => {
  const actual =
    await importOriginal<typeof import("@/lib/api/subscription")>();
  return {
    ...actual,
    subscriptionApi: {
      ...actual.subscriptionApi,
      getClaudeDesktopAccountQuota: mocks.quota,
    },
  };
});

const renderWithClient = (ui: ReactElement) =>
  render(
    <QueryClientProvider
      client={
        new QueryClient({
          defaultOptions: {
            queries: { retry: false },
            mutations: { retry: false },
          },
        })
      }
    >
      {ui}
    </QueryClientProvider>,
  );

const account = (id: string, login: string) => ({
  id,
  provider: "claude_desktop",
  login,
  avatar_url: null,
  authenticated_at: 0,
  is_default: false,
  github_domain: "claude.ai",
  requires_reauth: false,
});

const auth = (
  accounts: ReturnType<typeof account>[],
  active: string | null,
) => ({
  accounts,
  defaultAccountId: active,
  isStatusSuccess: true,
  isStatusError: false,
  isRemovingAccount: false,
  removeAccount: vi.fn(),
  logout: vi.fn(),
  refetchStatus: vi.fn().mockResolvedValue(undefined),
});

const live = (patch: Record<string, unknown> = {}) => ({
  supported: true,
  installed: true,
  running: true,
  activeAccountId: "a",
  activeUnmanaged: false,
  capture: {
    phase: "idle",
    accountId: null,
    label: null,
    message: null,
    startedAt: null,
  },
  busy: false,
  ...patch,
});

const addButton = () =>
  screen.findByRole("button", {
    name: /authCenter\.addAccount|添加账号|Add account/i,
  });

describe("ClaudeDesktopAccountSection", () => {
  beforeEach(() => {
    for (const mock of Object.values(mocks)) mock.mockReset();
    mocks.quota.mockResolvedValue({
      tool: "claude",
      credentialStatus: "valid",
      success: true,
      tiers: [],
      queriedAt: 0,
    });
    mocks.captureStart.mockResolvedValue(undefined);
    mocks.captureDismiss.mockResolvedValue(undefined);
  });

  it("asks before signing Desktop out when the current account is saved", async () => {
    mocks.useManagedAuth.mockReturnValue(auth([account("a", "a@x.com")], "a"));
    mocks.liveState.mockResolvedValue(live());
    renderWithClient(<ClaudeDesktopAccountSection />);

    await userEvent.click(await addButton());
    const start = await screen.findByRole("button", {
      name: /claudeDesktopAccounts\.startLogin|开始|Start/,
    });
    await userEvent.click(start);
    await waitFor(() => expect(mocks.captureStart).toHaveBeenCalledTimes(1));
  });

  it("saves the current sign-in first when it is not saved yet", async () => {
    mocks.useManagedAuth.mockReturnValue(auth([], null));
    mocks.liveState.mockResolvedValue(
      live({ activeAccountId: "a", activeUnmanaged: true }),
    );
    mocks.importCurrent.mockResolvedValue({
      account: { id: "a", label: "a@x.com", email: "a@x.com" },
      created: true,
    });
    renderWithClient(<ClaudeDesktopAccountSection />);

    // live state 还没回来时点了也要能用（等它回来再判断）
    await waitFor(() => expect(mocks.liveState).toHaveBeenCalled());
    await userEvent.click(await addButton());
    await waitFor(() => expect(mocks.importCurrent).toHaveBeenCalledTimes(1));
    expect(mocks.captureStart).not.toHaveBeenCalled();
  });

  it("goes straight to signing in when Desktop is signed out", async () => {
    mocks.useManagedAuth.mockReturnValue(auth([], null));
    mocks.liveState.mockResolvedValue(
      live({ activeAccountId: null, activeUnmanaged: false }),
    );
    renderWithClient(<ClaudeDesktopAccountSection />);

    await userEvent.click(await addButton());
    expect(
      await screen.findByRole("button", {
        name: /claudeDesktopAccounts\.startLogin|开始|Start/,
      }),
    ).toBeInTheDocument();
  });

  it("reports why when the live state cannot be read", async () => {
    mocks.useManagedAuth.mockReturnValue(auth([], null));
    mocks.liveState.mockRejectedValue(new Error("command not found"));
    renderWithClient(<ClaudeDesktopAccountSection />);

    await userEvent.click(await addButton());
    await waitFor(() => expect(mocks.toastError).toHaveBeenCalled());
  });

  it("says so instead of ignoring the click while another action runs", async () => {
    mocks.useManagedAuth.mockReturnValue(auth([account("a", "a@x.com")], "a"));
    mocks.liveState.mockResolvedValue(live({ busy: true }));
    renderWithClient(<ClaudeDesktopAccountSection />);

    await waitFor(() => expect(mocks.liveState).toHaveBeenCalled());
    await userEvent.click(await addButton());
    await waitFor(() => expect(mocks.toastInfo).toHaveBeenCalledTimes(1));
    expect(mocks.captureStart).not.toHaveBeenCalled();
  });
});
