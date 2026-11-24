# TwetchCode

TwetchCode is an unofficial fork based on the T3 Code source code, not affiliated with T3 Tools, Inc.
The upstream source is MIT licensed; its `LICENSE` is kept. The upstream name, logo and icons are not used.

It adds one thing: the Twetch models at `https://ai.twetch.com`, paid per request in BSV from a wallet on this computer.
Branch `twetch` is ours, `main` mirrors upstream, `twetch/UPSTREAM` names the release it is based on.

## Build

    twetch/build.sh

Output (macOS arm64, ad-hoc signed): `twetch/dist/TwetchCode.app` and `twetch/dist/TwetchCode-<version>-arm64.dmg`.
Needs Rust, Xcode command line tools, and ssh access to `twetch-inc/twetch-rs` (the pinned `twetch-crypto`).
Node and pnpm are fetched into `twetch/.tools`. The app is not notarized: on another Mac, open it from the right-click menu the first time.

## Update from upstream

    twetch/update.sh            # newest release
    twetch/update.sh v0.1.12    # or any ref

Merges into `twetch`, then runs our checks. A conflict stops it and lists the files; `twetch/update.sh --check` continues after you resolve them.

## How it fits together

| Where | What |
|---|---|
| `twetch/wallet` | The Rust sidecar `twetch-wallet`: proxy, payments, wallet page. `twetch/wallet/stub` is a test gateway and chain, never shipped |
| `apps/desktop/src/twetch` | Starts the sidecar, points OpenCode at it, opens the wallet window |
| `apps/web/src/twetch` | The Wallet entry with the balance, and the notice in Settings > About |
| `twetch/rebrand.mjs` | Writes a renamed copy to `twetch/.stage`; upstream files are not edited for the brand |
| `twetch/overlay`, `twetch/brand` | Files and icons that replace upstream's in that copy |

Upstream files edited by hand: five, seven lines, each marked `TwetchCode` or using a `Twetch…` component
(`main.ts`, `preload.ts`, `SidebarChrome.tsx`, `SettingsPanels.tsx`, `build-desktop-artifact.ts`).

Running from source without the rebrand (`vp run dev`) leaves the wallet off, so it can never touch a T3 Code home.

## Data and settings

- Home: `~/.twetch-code` (`TWETCHCODE_HOME` to change). App data: `~/Library/Application Support/twetchcode`. Ids: `com.twetch.code`, scheme `twetchcode://`. Ports: server from 3873, wallet 48213.
- The recovery phrase and the gateway key are in the system keychain (`com.twetch.code.wallet`), nowhere else. Another home has its own keychain entries and its own wallet.
- OpenCode runs with its own config folder and database under `~/.twetch-code/opencode`. Your own OpenCode settings, sessions and service are not used. `~/.twetch-code/opencode/config/opencode.json` is yours to add to. OpenCode still shares its log, snapshot and cache folders.
- If OpenCode is missing, the app offers to download a pinned, checksummed copy.
- `TWETCH_AI_URL`, `TWETCH_WALLET_WOC_URL`, `TWETCH_WALLET_PORT` override the gateway, the chain provider and the wallet port.
- Product analytics, the upstream update feed, T3 Connect, the phone app and bug reports to the upstream project are off.
- SSH remote environments run the upstream `t3` CLI on that remote machine, in its own `~/.t3` there.

## How it pays

One transaction per request for exactly its `cost_sats`, to Twetch @0 (`1J82Vbt4Uz8kKr2Xb3Kro9WtCh6udoctCJ`, compiled in; any other payee is refused).
Signed bytes are stored before they are sent, and only the same bytes are sent again. A second transaction for a request is
signed only when the chain shows another spender of a coin of the first. If the chain gives no clear answer the payment stops and waits for you.
Limits (per payment $1, per day $10, editable) stop new requests with a 402. A request that cost more than the limit waits for your approval.

## Try it without money

    cargo run --release -p twetch-stub --manifest-path twetch/wallet/Cargo.toml -- 48300
    TWETCHCODE_HOME=/tmp/twetch-try TWETCH_AI_URL=http://127.0.0.1:48300 TWETCH_WALLET_WOC_URL=http://127.0.0.1:48300/woc \
      twetch/dist/TwetchCode.app/Contents/MacOS/TwetchCode
    curl -X POST 'http://127.0.0.1:48300/stub/faucet?address=<deposit address>&sats=20000000'

The real gateway: `TWETCH_GATEWAY_BIN=<inference binary> TWETCH_GATEWAY_CONFIG=<config.json> cargo test` in `twetch/wallet`.
