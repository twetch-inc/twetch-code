import { useNavigate } from "@tanstack/react-router";
import { WalletIcon } from "lucide-react";
import { useEffect, useState } from "react";

import { SidebarMenu, SidebarMenuButton, SidebarMenuItem } from "../components/ui/sidebar";

export interface WalletStatus {
  readonly ready: boolean;
  readonly balance_sats: number;
  readonly balance_usd: number | null;
}

declare global {
  interface Window {
    readonly twetchWallet?: {
      readonly status: () => Promise<WalletStatus | null>;
      readonly api: <T = unknown>(endpoint: string, body?: unknown) => Promise<T>;
    };
  }
}

const REFRESH_MS = 15_000;

function chip(status: WalletStatus | null): string {
  if (status === null) return "";
  if (status.balance_usd === null) return `${status.balance_sats.toLocaleString("en-US")} sats`;
  return status.balance_usd.toLocaleString("en-US", { style: "currency", currency: "USD" });
}

/* oxlint-disable t3code/no-untranslated-copy -- TwetchCode copy is English only */
/** The wallet belongs to the desktop app; a browser client has none. */
export function TwetchWalletItem() {
  const wallet = window.twetchWallet;
  const navigate = useNavigate();
  const [status, setStatus] = useState<WalletStatus | null>(null);
  // oxlint-disable-next-line t3code/no-unreasoned-state-sync -- polls the wallet process
  useEffect(() => {
    if (wallet === undefined) return;
    const read = () => {
      if (document.hidden) return;
      wallet.status().then(setStatus, () => setStatus(null));
    };
    read();
    const timer = setInterval(read, REFRESH_MS);
    window.addEventListener("focus", read);
    return () => {
      clearInterval(timer);
      window.removeEventListener("focus", read);
    };
  }, [wallet]);
  if (wallet === undefined) return null;
  return (
    <SidebarMenu>
      <SidebarMenuItem>
        <SidebarMenuButton onClick={() => void navigate({ to: "/settings/wallet" })}>
          <WalletIcon />
          <span className="flex-1">Wallet</span>
          <span className="text-muted-foreground tabular-nums">{chip(status)}</span>
        </SidebarMenuButton>
      </SidebarMenuItem>
    </SidebarMenu>
  );
}
