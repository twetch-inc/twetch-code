import { createFileRoute } from "@tanstack/react-router";

import { TwetchWalletSettingsPanel } from "../twetch/TwetchWalletSettingsPanel";

function SettingsWalletRoute() {
  return <TwetchWalletSettingsPanel />;
}

export const Route = createFileRoute("/_chat/settings/wallet")({
  component: SettingsWalletRoute,
});
