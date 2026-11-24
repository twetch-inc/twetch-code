/* oxlint-disable t3code/no-untranslated-copy -- TwetchCode copy is English only */
import {
  AlertTriangleIcon,
  CheckIcon,
  CopyIcon,
  ExternalLinkIcon,
  EyeIcon,
  EyeOffIcon,
  KeyRoundIcon,
  RefreshCwIcon,
  SendIcon,
} from "lucide-react";
import { type FormEvent, useCallback, useEffect, useRef, useState } from "react";

import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogClose,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogPopup,
  AlertDialogTitle,
} from "../components/ui/alert-dialog";
import { Badge } from "../components/ui/badge";
import { Button } from "../components/ui/button";
import { Input } from "../components/ui/input";
import { toastManager } from "../components/ui/toast";
import {
  SettingsPageContainer,
  SettingsRow,
  SettingsSection,
} from "../components/settings/settingsLayout";

interface WalletRow {
  readonly id: string;
  readonly kind: "request" | "topup" | "withdraw";
  readonly state: string;
  readonly ts: number;
  readonly model: string;
  readonly sats: number;
  readonly fee: number;
  readonly usd: number | null;
  readonly to: string;
  readonly txid: string;
  readonly note: string;
}

interface GatewayAccountInfo {
  readonly balance_sats: number;
  readonly credit_sats: number;
  readonly spent_sats: number;
}

interface WalletLimits {
  readonly per_payment_usd: number;
  readonly per_day_usd: number;
}

interface WalletSnapshot {
  readonly status: "ready" | "missing" | "locked";
  readonly problem: string;
  readonly address: string;
  readonly pubkey: string;
  readonly pay_to: string;
  readonly ready_sats: number;
  readonly coming_sats: number;
  readonly owed_sats: number;
  readonly bsv_usd: number | null;
  readonly gateway: GatewayAccountInfo | null;
  readonly gateway_error: string;
  readonly limits: WalletLimits;
  readonly spent_today_usd: number;
  readonly waiting: readonly WalletRow[];
  readonly recent: readonly WalletRow[];
  readonly can_restore: boolean;
}

interface ConfirmDialogState {
  readonly title: string;
  readonly description: string;
  readonly confirmLabel: string;
  readonly destructive?: boolean;
  readonly onConfirm: () => Promise<void>;
}

const POLL_MS = 3_000;

const STATE_LABELS: Readonly<Record<string, string>> = {
  open: "Running",
  owed: "To pay",
  held: "Needs approval",
  signed: "Sending",
  pending: "Sent",
  paid: "Paid",
  free: "Free",
  gone: "Not charged",
  failed: "Failed",
  canceled: "Not sent",
};

function stateBadgeVariant(
  state: string,
): "success" | "warning" | "error" | "info" | "secondary" {
  if (state === "paid" || state === "free") return "success";
  if (state === "held" || state === "owed") return "warning";
  if (state === "failed") return "error";
  if (state === "open" || state === "signed" || state === "pending") return "info";
  return "secondary";
}

function formatUsd(value: number | null | undefined): string {
  if (value === null || value === undefined) return "";
  if (value !== 0 && Math.abs(value) < 0.01) {
    return `$${value.toFixed(6).replace(/0+$/, "")}`;
  }
  return value.toLocaleString("en-US", { style: "currency", currency: "USD" });
}

function formatBsv(sats: number): string {
  return `${(sats / 1e8).toFixed(8)} BSV`;
}

function formatSats(n: number): string {
  return `${Number(n).toLocaleString("en-US")} sats`;
}

function rowTitle(row: WalletRow): string {
  if (row.kind === "topup") return "Top-up";
  if (row.kind === "withdraw") return `Withdraw to ${row.to.slice(0, 8)}…`;
  return row.model || "Request";
}

function notifySuccess(title: string) {
  toastManager.add({ type: "success", title });
}

function notifyError(error: unknown) {
  const message = error instanceof Error ? error.message : String(error);
  toastManager.add({ type: "error", title: message || "That did not work." });
}

export function TwetchWalletSettingsPanel() {
  const wallet = window.twetchWallet;
  const [snapshot, setSnapshot] = useState<WalletSnapshot | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [qrSvg, setQrSvg] = useState<string | null>(null);
  const qrAddressRef = useRef<string>("");

  const [perPayment, setPerPayment] = useState("1.00");
  const [perDay, setPerDay] = useState("10.00");
  const limitsDirtyRef = useRef(false);

  const [withdrawTo, setWithdrawTo] = useState("");
  const [withdrawAmount, setWithdrawAmount] = useState("");
  const [restorePhrase, setRestorePhrase] = useState("");

  const [revealedPhrase, setRevealedPhrase] = useState<string | null>(null);
  const [copiedAddress, setCopiedAddress] = useState(false);
  const [copiedPhrase, setCopiedPhrase] = useState(false);
  const [busyKey, setBusyKey] = useState<string | null>(null);
  const [confirmState, setConfirmState] = useState<ConfirmDialogState | null>(null);

  const loadState = useCallback(async () => {
    if (wallet === undefined) return;
    try {
      const next = await wallet.api<WalletSnapshot>("state");
      setSnapshot(next);
      setLoadError(null);
      if (!limitsDirtyRef.current) {
        setPerPayment(next.limits.per_payment_usd.toFixed(2));
        setPerDay(next.limits.per_day_usd.toFixed(2));
      }
      if (next.address && qrAddressRef.current !== next.address) {
        qrAddressRef.current = next.address;
        wallet
          .api<string>("qr")
          .then((svg) => setQrSvg(svg))
          .catch(() => {});
      }
    } catch (error) {
      setLoadError(
        error instanceof Error ? error.message : "TwetchCode's wallet is not running.",
      );
    }
  }, [wallet]);

  // oxlint-disable-next-line t3code/no-unreasoned-state-sync -- polls the local wallet sidecar
  useEffect(() => {
    if (wallet === undefined) return;
    let active = true;
    const tick = () => {
      if (!active || document.hidden) return;
      void loadState();
    };
    void loadState().then(() => wallet.api("refresh", {}).catch(() => {}));
    const timer = setInterval(tick, POLL_MS);
    const onVisibility = () => {
      if (document.hidden) {
        setRevealedPhrase(null);
      } else {
        tick();
      }
    };
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      active = false;
      clearInterval(timer);
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [wallet, loadState]);

  const runAction = useCallback(
    async (key: string, fn: () => Promise<void>) => {
      setBusyKey(key);
      try {
        await fn();
        await loadState();
      } catch (error) {
        notifyError(error);
      } finally {
        setBusyKey(null);
      }
    },
    [loadState],
  );

  if (wallet === undefined) {
    return (
      <SettingsPageContainer>
        <SettingsSection title="Wallet">
          <SettingsRow
            title="Desktop wallet only"
            description="The BSV pay-per-request wallet runs locally inside the TwetchCode desktop app."
          />
        </SettingsSection>
      </SettingsPageContainer>
    );
  }

  const totalSats = snapshot ? snapshot.ready_sats + snapshot.coming_sats : 0;
  const heroBalance = snapshot
    ? snapshot.bsv_usd === null
      ? formatBsv(totalSats)
      : formatUsd((totalSats * snapshot.bsv_usd) / 1e8)
    : "—";
  const subBalance = snapshot
    ? snapshot.bsv_usd === null
      ? "BSV price is unavailable right now"
      : formatBsv(totalSats)
    : "Connecting to wallet…";

  const problemText = loadError || snapshot?.problem || "";

  const handleCopyAddress = async () => {
    if (!snapshot?.address) return;
    try {
      await navigator.clipboard.writeText(snapshot.address);
      setCopiedAddress(true);
      notifySuccess("Deposit address copied");
      setTimeout(() => setCopiedAddress(false), 1800);
    } catch (error) {
      notifyError(error);
    }
  };

  const handleCopyPhrase = async () => {
    if (!revealedPhrase) return;
    try {
      await navigator.clipboard.writeText(revealedPhrase);
      setCopiedPhrase(true);
      notifySuccess("Recovery phrase copied");
      setTimeout(() => setCopiedPhrase(false), 1800);
    } catch (error) {
      notifyError(error);
    }
  };

  const handleSaveLimits = (event: FormEvent) => {
    event.preventDefault();
    const nextPerPayment = Number(perPayment.replace(/[$,\s]/g, ""));
    const nextPerDay = Number(perDay.replace(/[$,\s]/g, ""));
    if (!Number.isFinite(nextPerPayment) || !Number.isFinite(nextPerDay)) {
      notifyError(new Error("Enter valid dollar amounts."));
      return;
    }
    void runAction("limits", async () => {
      await wallet.api("limits", {
        per_payment_usd: nextPerPayment,
        per_day_usd: nextPerDay,
      });
      limitsDirtyRef.current = false;
      notifySuccess("Spending limits saved");
    });
  };

  const handleWithdrawSubmit = (event: FormEvent) => {
    event.preventDefault();
    const to = withdrawTo.trim();
    if (!to) {
      notifyError(new Error("Enter a destination BSV address."));
      return;
    }
    const raw = withdrawAmount.trim();
    const amountSats = raw === "" ? null : Math.round(Number(raw) * 1e8);
    if (amountSats !== null && !(amountSats > 0)) {
      notifyError(new Error("Enter an amount in BSV, or leave it empty to send everything."));
      return;
    }
    const label = amountSats === null ? "everything" : formatBsv(amountSats);
    setConfirmState({
      title: "Withdraw BSV",
      description: `Send ${label} to ${to}? This cannot be undone.`,
      confirmLabel: "Withdraw",
      destructive: true,
      onConfirm: async () => {
        const res = await wallet.api<WalletRow>(
          "withdraw",
          amountSats === null ? { to } : { to, sats: amountSats },
        );
        setWithdrawTo("");
        setWithdrawAmount("");
        notifySuccess(`Sent: ${res.txid.slice(0, 12)}…`);
      },
    });
  };

  const handleRevealToggle = () => {
    if (revealedPhrase !== null) {
      setRevealedPhrase(null);
      return;
    }
    setConfirmState({
      title: "Show recovery phrase",
      description:
        "Make sure nobody can see your screen. Never type these words into a website or chat.",
      confirmLabel: "Show recovery phrase",
      onConfirm: async () => {
        const res = await wallet.api<{ phrase: string }>("reveal", {});
        setRevealedPhrase(res.phrase);
      },
    });
  };

  const handleRotateKey = () => {
    setConfirmState({
      title: "Replace gateway key",
      description:
        "The current gateway API key stops working right away. Your balance and history stay.",
      confirmLabel: "Replace key",
      onConfirm: async () => {
        await wallet.api("rotate", {});
        notifySuccess("Gateway key replaced");
      },
    });
  };

  const handleRestoreSubmit = (event: FormEvent) => {
    event.preventDefault();
    const phrase = restorePhrase.trim();
    if (!phrase) {
      notifyError(new Error("Enter your recovery phrase."));
      return;
    }
    setConfirmState({
      title: "Restore wallet",
      description: "The wallet on this computer will be replaced by the one these words belong to.",
      confirmLabel: "Restore",
      destructive: true,
      onConfirm: async () => {
        await wallet.api("restore", { phrase });
        setRestorePhrase("");
        qrAddressRef.current = "";
        setQrSvg(null);
        notifySuccess("Wallet restored");
      },
    });
  };

  return (
    <SettingsPageContainer>
      {problemText ? (
        <div className="flex items-start gap-2.5 rounded-xl border border-destructive/30 bg-destructive/8 px-4 py-3 text-sm text-destructive-foreground">
          <AlertTriangleIcon className="mt-0.5 size-4 shrink-0 text-destructive" />
          <p className="leading-relaxed">{problemText}</p>
        </div>
      ) : null}

      <SettingsSection
        title="Balance"
        headerAction={
          <Button
            size="xs"
            variant="outline"
            disabled={busyKey !== null}
            onClick={() =>
              void runAction("refresh", async () => {
                await wallet.api("refresh", {});
                notifySuccess("Checked for deposits");
              })
            }
          >
            <RefreshCwIcon
              className={busyKey === "refresh" ? "size-3.5 animate-spin" : "size-3.5"}
            />
            Check for deposits
          </Button>
        }
      >
        <div className="px-3 py-4 sm:px-4">
          <div className="flex flex-wrap items-baseline justify-between gap-2">
            <div>
              <div className="text-3xl font-semibold tracking-tight text-foreground tabular-nums">
                {heroBalance}
              </div>
              <div className="mt-1 text-xs text-muted-foreground tabular-nums">{subBalance}</div>
              {snapshot && snapshot.coming_sats > 0 ? (
                <div className="mt-1 text-xs text-muted-foreground tabular-nums">
                  {formatSats(snapshot.coming_sats)} change on its way
                </div>
              ) : null}
            </div>
          </div>
        </div>

        {snapshot?.status === "ready" && snapshot.address ? (
          <div className="flex flex-col gap-4 px-3 py-4 sm:flex-row sm:items-center sm:px-4">
            {qrSvg ? (
              <div className="w-fit shrink-0 rounded-lg border border-border/60 bg-white p-2">
                <img
                  src={`data:image/svg+xml,${encodeURIComponent(qrSvg)}`}
                  alt="Deposit address QR code"
                  className="size-32"
                />
              </div>
            ) : null}
            <div className="min-w-0 flex-1 space-y-2">
              <div className="text-xs font-medium text-muted-foreground">Deposit address</div>
              <div className="break-all font-mono text-xs text-foreground select-all">
                {snapshot.address}
              </div>
              <div className="flex flex-wrap items-center gap-2 pt-0.5">
                <Button size="xs" variant="outline" onClick={() => void handleCopyAddress()}>
                  {copiedAddress ? (
                    <CheckIcon className="size-3.5" />
                  ) : (
                    <CopyIcon className="size-3.5" />
                  )}
                  {copiedAddress ? "Copied" : "Copy address"}
                </Button>
              </div>
              <p className="text-xs text-muted-foreground">
                Send BSV only. A deposit shows up within seconds.
              </p>
            </div>
          </div>
        ) : null}
      </SettingsSection>

      {snapshot && snapshot.waiting.length > 0 ? (
        <SettingsSection title="Needs attention">
          {snapshot.waiting.map((item) => {
            const price = item.usd === null ? "" : ` (${formatUsd(item.usd)})`;
            return (
              <SettingsRow
                key={item.id}
                title={`${rowTitle(item)}: ${formatSats(item.sats)}${price}`}
                description={item.note || undefined}
                control={
                  <div className="flex items-center gap-2">
                    {item.state === "held" ? (
                      <Button
                        size="xs"
                        disabled={busyKey !== null}
                        onClick={() =>
                          void runAction(`approve:${item.id}`, async () => {
                            await wallet.api("approve", { id: item.id });
                            notifySuccess("Payment approved");
                          })
                        }
                      >
                        Approve payment
                      </Button>
                    ) : (
                      <>
                        {item.txid ? (
                          <Button
                            size="xs"
                            disabled={busyKey !== null}
                            onClick={() =>
                              void runAction(`retry:${item.id}`, async () => {
                                await wallet.api("retry", { id: item.id });
                                notifySuccess("Sending again");
                              })
                            }
                          >
                            Send again
                          </Button>
                        ) : null}
                        <Button
                          size="xs"
                          variant="outline"
                          disabled={busyKey !== null}
                          onClick={() =>
                            void runAction(`cancel:${item.id}`, async () => {
                              await wallet.api("cancel", { id: item.id });
                              notifySuccess("Coins released");
                            })
                          }
                        >
                          Release coins
                        </Button>
                      </>
                    )}
                  </div>
                }
              />
            );
          })}
        </SettingsSection>
      ) : null}

      <form onSubmit={handleSaveLimits}>
        <SettingsSection
          title="Spending limits"
          headerAction={
            <Button size="xs" type="submit" disabled={busyKey !== null}>
              Save limits
            </Button>
          }
        >
          <SettingsRow
            title="Per payment (USD)"
            description="Requests above this amount pause for your approval."
            control={
              <Input
                type="text"
                inputMode="decimal"
                size="sm"
                className="w-28 font-mono tabular-nums"
                value={perPayment}
                onChange={(event) => {
                  limitsDirtyRef.current = true;
                  setPerPayment(event.target.value);
                }}
              />
            }
          />
          <SettingsRow
            title="Per day (USD)"
            description={
              snapshot
                ? `${formatUsd(snapshot.spent_today_usd)} spent today.`
                : "Daily spending cap."
            }
            control={
              <Input
                type="text"
                inputMode="decimal"
                size="sm"
                className="w-28 font-mono tabular-nums"
                value={perDay}
                onChange={(event) => {
                  limitsDirtyRef.current = true;
                  setPerDay(event.target.value);
                }}
              />
            }
          />
        </SettingsSection>
      </form>

      <SettingsSection title="Inference account">
        <div className="grid grid-cols-2 gap-4 px-3 py-4 sm:grid-cols-4 sm:px-4">
          <div>
            <div className="text-xs text-muted-foreground">Balance at Twetch</div>
            <div className="mt-1 text-sm font-medium text-foreground tabular-nums">
              {snapshot?.gateway ? formatSats(snapshot.gateway.balance_sats) : "—"}
            </div>
          </div>
          <div>
            <div className="text-xs text-muted-foreground">Credit</div>
            <div className="mt-1 text-sm font-medium text-foreground tabular-nums">
              {snapshot?.gateway ? formatSats(snapshot.gateway.credit_sats) : "—"}
            </div>
          </div>
          <div>
            <div className="text-xs text-muted-foreground">Used</div>
            <div className="mt-1 text-sm font-medium text-foreground tabular-nums">
              {snapshot?.gateway ? formatSats(snapshot.gateway.spent_sats) : "—"}
            </div>
          </div>
          <div>
            <div className="text-xs text-muted-foreground">Waiting to be paid</div>
            <div className="mt-1 text-sm font-medium text-foreground tabular-nums">
              {snapshot ? formatSats(snapshot.owed_sats) : "—"}
            </div>
          </div>
        </div>
        <div className="px-3 py-2.5 text-xs text-muted-foreground sm:px-4">
          {snapshot?.gateway_error
            ? `Twetch's inference gateway could not be reached: ${snapshot.gateway_error}`
            : "Each request is paid with one BSV transaction for exactly what it cost."}
        </div>
      </SettingsSection>

      <SettingsSection title="Activity">
        {snapshot && snapshot.recent.length > 0 ? (
          <div className="overflow-x-auto">
            <table className="w-full text-left text-xs">
              <thead>
                <tr className="border-b border-border/50 text-muted-foreground">
                  <th className="px-3 py-2 font-medium sm:px-4">When</th>
                  <th className="px-3 py-2 font-medium">Item</th>
                  <th className="px-3 py-2 text-right font-medium">Cost</th>
                  <th className="px-3 py-2 font-medium">Status</th>
                  <th className="px-3 py-2 font-medium sm:px-4">Transaction</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-border/40">
                {snapshot.recent.map((item) => (
                  <tr key={item.id}>
                    <td className="whitespace-nowrap px-3 py-2 text-muted-foreground tabular-nums sm:px-4">
                      {new Date(item.ts).toLocaleString([], {
                        month: "short",
                        day: "numeric",
                        hour: "2-digit",
                        minute: "2-digit",
                      })}
                    </td>
                    <td className="px-3 py-2 text-foreground">{rowTitle(item)}</td>
                    <td
                      className="whitespace-nowrap px-3 py-2 text-right text-foreground tabular-nums"
                      title={item.usd !== null ? formatUsd(item.usd) : undefined}
                    >
                      {item.state === "open" || item.state === "gone"
                        ? "—"
                        : formatSats(item.sats)}
                    </td>
                    <td className="px-3 py-2" title={item.note || undefined}>
                      <Badge size="sm" variant={stateBadgeVariant(item.state)}>
                        {STATE_LABELS[item.state] ?? item.state}
                      </Badge>
                    </td>
                    <td className="whitespace-nowrap px-3 py-2 font-mono sm:px-4">
                      {item.txid ? (
                        <a
                          href={`https://whatsonchain.com/tx/${item.txid}`}
                          target="_blank"
                          rel="noreferrer"
                          title={item.txid}
                          className="inline-flex items-center gap-1 text-muted-foreground hover:text-foreground hover:underline"
                        >
                          {item.txid.slice(0, 10)}…
                          <ExternalLinkIcon className="size-3" />
                        </a>
                      ) : (
                        <span className="text-muted-foreground">—</span>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        ) : (
          <div className="px-3 py-4 text-xs text-muted-foreground sm:px-4">
            No transactions yet.
          </div>
        )}
      </SettingsSection>

      <form onSubmit={handleWithdrawSubmit}>
        <SettingsSection
          title="Withdraw"
          headerAction={
            <Button size="xs" variant="outline" type="submit" disabled={busyKey !== null}>
              <SendIcon className="size-3.5" />
              Withdraw
            </Button>
          }
        >
          <SettingsRow
            title="Destination address"
            description="BSV address to send funds to."
            control={
              <Input
                type="text"
                size="sm"
                placeholder="1..."
                className="w-full font-mono sm:w-72"
                value={withdrawTo}
                onChange={(event) => setWithdrawTo(event.target.value)}
              />
            }
          />
          <SettingsRow
            title="Amount (BSV)"
            description="Leave empty to send your entire available balance."
            control={
              <Input
                type="text"
                inputMode="decimal"
                size="sm"
                placeholder="All"
                className="w-36 font-mono tabular-nums"
                value={withdrawAmount}
                onChange={(event) => setWithdrawAmount(event.target.value)}
              />
            }
          />
        </SettingsSection>
      </form>

      <SettingsSection title="Security & recovery">
        <SettingsRow
          title="Recovery phrase"
          description="Back up these 12 words before funding your wallet. Stored in your system keychain."
          control={
            <Button
              size="xs"
              variant="outline"
              disabled={busyKey !== null}
              onClick={handleRevealToggle}
            >
              {revealedPhrase !== null ? (
                <EyeOffIcon className="size-3.5" />
              ) : (
                <EyeIcon className="size-3.5" />
              )}
              {revealedPhrase !== null ? "Hide recovery phrase" : "Show recovery phrase"}
            </Button>
          }
        >
          {revealedPhrase !== null ? (
            <div className="mt-2 mb-3 rounded-lg border border-border/60 bg-muted/40 p-3">
              <p className="font-mono text-xs leading-relaxed text-foreground select-all">
                {revealedPhrase}
              </p>
              <div className="mt-2 flex items-center justify-end">
                <Button size="xs" variant="ghost" onClick={() => void handleCopyPhrase()}>
                  {copiedPhrase ? (
                    <CheckIcon className="size-3.5" />
                  ) : (
                    <CopyIcon className="size-3.5" />
                  )}
                  {copiedPhrase ? "Copied" : "Copy phrase"}
                </Button>
              </div>
            </div>
          ) : null}
        </SettingsRow>

        <SettingsRow
          title="Gateway API key"
          description="Replace the bearer key used with ai.twetch.com. Your balance stays."
          control={
            <Button
              size="xs"
              variant="outline"
              disabled={busyKey !== null}
              onClick={handleRotateKey}
            >
              <KeyRoundIcon className="size-3.5" />
              Replace key
            </Button>
          }
        />

        {snapshot?.can_restore ? (
          <form onSubmit={handleRestoreSubmit}>
            <SettingsRow
              title="Restore wallet"
              description="Replace this empty wallet with an existing 12-word recovery phrase."
              control={
                <div className="flex w-full flex-col gap-2 sm:w-auto sm:flex-row sm:items-center">
                  <Input
                    type="text"
                    size="sm"
                    placeholder="12 recovery words"
                    className="w-full font-mono sm:w-64"
                    value={restorePhrase}
                    onChange={(event) => setRestorePhrase(event.target.value)}
                  />
                  <Button size="xs" variant="outline" type="submit" disabled={busyKey !== null}>
                    Restore
                  </Button>
                </div>
              }
            />
          </form>
        ) : null}
      </SettingsSection>

      <AlertDialog
        open={confirmState !== null}
        onOpenChange={(open) => {
          if (!open) setConfirmState(null);
        }}
      >
        <AlertDialogPopup>
          <AlertDialogHeader>
            <AlertDialogTitle>{confirmState?.title}</AlertDialogTitle>
            <AlertDialogDescription>{confirmState?.description}</AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogClose render={<Button variant="outline">Cancel</Button>} />
            <AlertDialogAction
              render={
                <Button
                  variant={confirmState?.destructive ? "destructive" : "default"}
                  onClick={() => {
                    const current = confirmState;
                    setConfirmState(null);
                    if (current) {
                      void runAction("confirm", current.onConfirm);
                    }
                  }}
                >
                  {confirmState?.confirmLabel}
                </Button>
              }
            />
          </AlertDialogFooter>
        </AlertDialogPopup>
      </AlertDialog>
    </SettingsPageContainer>
  );
}
