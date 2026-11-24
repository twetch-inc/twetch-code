#!/usr/bin/env node
/**
 * Write a TwetchCode copy of this repo to twetch/.stage, ready to build.
 *
 * Upstream files are never edited for the brand: names, ids, icons and links
 * are changed on the way into the copy, so merges stay clean. Every anchored
 * change must match, or this stops and names it: upstream moved the text.
 */
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const stage = path.join(root, "twetch/.stage");

/** Copied as they are: our own words, and other people's notices. */
const VERBATIM = [
  /^apps\/(desktop|web)\/src\/twetch\//,
  /^LICENSE$/,
  /(^|\/)third-party-licenses/,
  /^pnpm-lock\.yaml$/,
  /\.(png|ico|icns|jpg|jpeg|gif|webp|woff2?|ttf|otf|wasm|mp3|mp4|wav|ndjson|patch)$/,
  /\.test\.[cm]?[jt]sx?$/,
];
const SKIP = [/^\.repos\//, /^twetch\//, /^docs\//, /^evals\/(?!package\.json)/];
const CODE = /^(apps\/(desktop|server|web)|packages|scripts|native)\//;

/** Applied to every file of the code, in order. */
const EVERYWHERE = [
  [/T3 Code \((Beta|Alpha|Dev|Nightly)\)/g, "TwetchCode ($1)"],
  [/T3[ -]Code/g, "TwetchCode"],
  [/com\.t3tools\.T3Code/g, "com.twetch.TwetchCode"],
  [/com\.t3tools\.t3code/g, "com.twetch.code"],
  // A T3 Code session on this machine exports these; none may reach us.
  [/T3CODE_/g, "TWETCHCODE_", /^packages\/ssh\//],
  [/\bt3code(-dev)?:\/\//g, "twetchcode$1://"],
  [/https:\/\/t3\.codes\/privacy-policy/g, "https://twetch.com/privacy"],
  [/https:\/\/t3\.codes\/(download|mobile)/g, "https://twetch.com"],
  [/\]\(https:\/\/t3\.codes\)/g, "](https://twetch.com)"],
  // There is no hosted web app: a name that can never resolve trusts nobody's origin.
  [/https:\/\/(nightly\.|latest\.)?app\.t3\.codes/g, "https://hosted-app.invalid"],
  [/\bapp\.t3\.codes/g, "the hosted web app"],
  [/https:\/\/ingest\.t3\.codes/g, "http://127.0.0.1:9"],
  [/phc_[A-Za-z0-9]{30,}/g, "off"],
];

/** file -> [from, to] pairs. Each must be found. A string replaces every match. */
const ANCHORED = {
  "apps/desktop/src/twetch/brand.ts": [["REBRANDED: boolean = false", "REBRANDED: boolean = true"]],
  "apps/desktop/package.json": [['"productName": "TwetchCode (Beta)"', '"productName": "TwetchCode"']],
  "apps/desktop/src/app/DesktopEnvironment.ts": [
    ["displayName: `${APP_BASE_NAME} (${stageLabel})`", "displayName: APP_BASE_NAME"],
  ],
  "apps/desktop/src/app/DesktopAppIdentity.ts": [
    ["`${environment.branding.baseName} ${environment.branding.stageLabel}`", "environment.branding.baseName"],
  ],
  // Our own data folders. The V1 and legacy names would read T3 Code's own profile.
  "apps/desktop/src/app/DesktopStatePaths.ts": [['".t3"', '".twetch-code"']],
  "apps/desktop/src/app/DesktopUserData.ts": [
    ['"t3code-dev"', '"twetchcode-dev"'],
    ['"t3code-v2"', '"twetchcode"'],
    ['"t3code", "Local State"', '"twetchcode-legacy", "Local State"'],
  ],
  "apps/desktop/src/app/DesktopLegacyLocalStorage.ts": [['"t3code"]', '"twetchcode-legacy"]']],
  "apps/desktop/src/compileCache.ts": [['"t3code"', '"twetchcode"']],
  "apps/server/src/cliCompileCache.ts": [['"t3code"', '"twetchcode"']],
  "apps/server/src/os-jank.ts": [['".t3"', '".twetch-code"']],
  "apps/server/src/cli/browser.ts": [['".t3"', '".twetch-code"']],
  "apps/server/src/project/AgentSessionScanner.ts": [["/.t3/worktrees/", "/.twetch-code/worktrees/"]],
  "apps/desktop/src/wsl/DesktopWslEnvironment.ts": [["$HOME/.t3/", "$HOME/.twetch-code/"]],
  "apps/server/src/cloud/bootService.ts": [['"t3code"', '"twetchcode"'], ["t3code.service", "twetchcode.service"]],
  "apps/server/src/cli/update.ts": [["/t3code.service", "/twetchcode.service"]],
  "apps/server/src/serviceLauncher.ts": [['"t3code"', '"twetchcode"']],
  // The command this app may put on PATH must not be T3 Code's.
  "apps/desktop/src/app/DesktopCliCommand.ts": [
    ['path.join(directory, "t3")', 'path.join(directory, "twetchcode")'],
    ['["t3.cmd", "t3.exe"] : ["t3"]', '["twetchcode.cmd", "twetchcode.exe"] : ["twetchcode"]'],
  ],
  // Scheme and ports.
  "apps/desktop/src/electron/ElectronProtocol.ts": [['"t3code"', '"twetchcode"'], ['"t3code-dev"', '"twetchcode-dev"']],
  "packages/shared/src/codexAuthHandoff.ts": [['"t3code-dev" : "t3code"', '"twetchcode-dev" : "twetchcode"']],
  "packages/shared/src/promptLinks.ts": [['scheme = "t3code"', 'scheme = "twetchcode"']],
  "apps/desktop/src/app/DesktopApp.ts": [["DEFAULT_DESKTOP_BACKEND_PORT = 3773", "DEFAULT_DESKTOP_BACKEND_PORT = 3873"]],
  "apps/server/src/config.ts": [["DEFAULT_PORT = 3773", "DEFAULT_PORT = 3873"]],
  // No analytics, no reports filed on the upstream project, no update feed.
  "apps/server/src/telemetry/AnalyticsService.ts": [
    [
      'Config.Boolean("TWETCHCODE_TELEMETRY_ENABLED").pipe(Config.withDefault(true))',
      'Config.Boolean("TWETCHCODE_TELEMETRY_ENABLED").pipe(Config.withDefault(false))',
    ],
  ],
  "apps/server/src/environment/ServerEnvironment.ts": [["triage: true", "triage: false"]],
  "apps/desktop/src/app/DesktopStartupRecovery.ts": [["https://github.com/pingdotgg/t3code/releases", "https://twetch.com"]],
  "apps/web/src/lib/bootError.ts": [["https://github.com/pingdotgg/t3code/issues/new", "https://twetch.com"]],
  "apps/desktop/src/updates/DesktopUpdatePolicy.ts": [
    [
      'mode: "auto",\n  feedUrl: undefined,\n  allowNightly: true,\n  message: null,',
      'mode: "off",\n  feedUrl: undefined,\n  allowNightly: false,\n  message: "TwetchCode does not update itself. Build a newer version with twetch/update.sh.",',
    ],
  ],
  "scripts/build-desktop-artifact.ts": [
    ['["t3code", "t3code-dev"]', '["twetchcode", "twetchcode-dev"]'],
    ['executableName: "t3code"', 'executableName: "twetchcode"'],
    ['homepage: "https://t3.codes"', 'homepage: "https://twetch.com"'],
    ['author: "T3 Tools"', 'author: "Twetch"'],
    ["T3 Tools <hello@t3.codes>", "TwetchCode <unofficial@twetchcode.invalid>"],
    ['name: "t3code",', 'name: "twetchcode",'],
  ],
  // Words and places that name the upstream product's own services.
  "apps/web/src/components/sidebar/SidebarChrome.tsx": [
    ['<T3Wordmark aria-label="T3"', '<T3Wordmark aria-label="Twetch"'],
    ["items-baseline gap-1 text-sm font-medium tracking-tight", "items-baseline text-sm font-medium tracking-tight"],
  ],
  "apps/web/src/components/ui/standalone-page.tsx": [['"flex items-baseline gap-1.5"', '"flex items-baseline"']],
  "apps/web/src/components/onboarding/WelcomePage.tsx": [
    ["TwetchCode collects anonymous usage data to improve the app. To opt out, see the", "TwetchCode sends no usage data. See the"],
    ["{showPhone ? <PhoneSection environmentId={environmentId} /> : null}", "{showPhone ? null : null}"],
  ],
  "apps/web/src/components/settings/SettingsPanels.tsx": [
    ["including the anonymous usage data TwetchCode collects.", "TwetchCode sends no usage data."],
  ],
  "apps/web/src/components/InstallT3Commands.tsx": [
    ['<CommandBlock command="curl -fsSL https://t3.codes/install.sh | sh" />', '<CommandBlock command="npm install -g t3" />'],
    ['<CommandBlock command="irm https://t3.codes/install.ps1 | iex" />', '<CommandBlock command="npm install -g t3" />'],
  ],
};

/** published path -> our file. */
const ASSETS = {
  "assets/prod/black-macos-1024.png": "mac-1024.png",
  "assets/prod/black-universal-1024.png": "full-1024.png",
  "assets/prod/black-ios-1024.png": "full-1024.png",
  "assets/prod/t3-black-windows.ico": "icon.ico",
  "assets/prod/t3-black-web-favicon.ico": "icon.ico",
  "assets/prod/t3-black-web-favicon-16x16.png": "full-16.png",
  "assets/prod/t3-black-web-favicon-32x32.png": "full-32.png",
  "assets/prod/t3-black-web-apple-touch-180.png": "full-180.png",
  "assets/prod/t3-black-web-icon-192x192.png": "full-192.png",
  "assets/prod/t3-black-web-icon-512x512.png": "full-512.png",
  "apps/web/public/favicon.ico": "icon.ico",
  "apps/web/public/favicon-16x16.png": "full-16.png",
  "apps/web/public/favicon-32x32.png": "full-32.png",
  "apps/web/public/apple-touch-icon.png": "full-180.png",
  "apps/web/public/icon-192x192.png": "full-192.png",
  "apps/web/public/icon-512x512.png": "full-512.png",
  "apps/desktop/resources/dmg/dmg-background-latest.svg": "dmg-background.svg",
  "apps/desktop/resources/dmg/dmg-background-nightly.svg": "dmg-background.svg",
};

const problems = [];
const wanted = new Set();

/** Written only when it differs, so a second build is incremental. */
function put(rel, bytes, mode) {
  const to = path.join(stage, rel);
  wanted.add(rel);
  if (fs.existsSync(to) && fs.lstatSync(to).isFile() && fs.readFileSync(to).equals(bytes)) return;
  fs.mkdirSync(path.dirname(to), { recursive: true });
  fs.rmSync(to, { force: true });
  fs.writeFileSync(to, bytes, { mode });
}

function rebrand(rel, text) {
  for (const [from, to, except] of EVERYWHERE) {
    if (except?.test(rel)) continue;
    text = text.replace(from, to);
  }
  return text;
}

function anchored(rel, text) {
  for (const [from, to] of ANCHORED[rel] ?? []) {
    if (!text.includes(from)) problems.push(`${rel}: no longer contains ${JSON.stringify(from)}`);
    text = text.replaceAll(from, () => to);
  }
  return text;
}

const overlayRoot = path.join(root, "twetch/overlay");
const overlays = new Set(
  fs
    .readdirSync(overlayRoot, { recursive: true, withFileTypes: true })
    .filter((e) => e.isFile())
    .map((e) => path.relative(overlayRoot, path.join(e.parentPath, e.name))),
);

const files = execFileSync("git", ["ls-files", "-z", "--cached", "--others", "--exclude-standard"], {
  cwd: root,
  maxBuffer: 1 << 28,
})
  .toString()
  .split("\0")
  .filter((f) => f !== "" && !SKIP.some((s) => s.test(f)) && fs.existsSync(path.join(root, f)));

const seen = new Set(files);
for (const rel of [...Object.keys(ANCHORED), ...Object.keys(ASSETS), ...overlays]) {
  if (!seen.has(rel)) problems.push(`${rel}: upstream no longer has this file`);
}

for (const rel of files) {
  const from = path.join(root, rel);
  const stat = fs.lstatSync(from);
  if (stat.isSymbolicLink()) {
    const to = path.join(stage, rel);
    wanted.add(rel);
    fs.mkdirSync(path.dirname(to), { recursive: true });
    fs.rmSync(to, { force: true, recursive: true });
    fs.symlinkSync(fs.readlinkSync(from), to);
    continue;
  }
  if (!stat.isFile()) continue;
  if (ASSETS[rel]) {
    put(rel, fs.readFileSync(path.join(root, "twetch/brand", ASSETS[rel])), stat.mode);
    continue;
  }
  if (overlays.has(rel)) {
    put(rel, fs.readFileSync(path.join(overlayRoot, rel)), stat.mode);
    continue;
  }
  let bytes = fs.readFileSync(from);
  const code = CODE.test(rel) && !VERBATIM.some((v) => v.test(rel));
  if (code || ANCHORED[rel]) {
    let text = bytes.toString("utf8");
    if (code) text = rebrand(rel, text);
    bytes = Buffer.from(anchored(rel, text));
  }
  put(rel.replace("com.t3tools.t3code.metainfo.xml", "com.twetch.code.metainfo.xml"), bytes, stat.mode);
}

// The wallet binary, where the desktop build picks up its resources.
const wallet = process.platform === "win32" ? "twetch-wallet.exe" : "twetch-wallet";
const built = path.join(root, "twetch/wallet/target/release", wallet);
if (fs.existsSync(built)) put(`apps/desktop/resources/twetch-wallet/${wallet}`, fs.readFileSync(built), 0o755);
else problems.push("twetch-wallet is not built: cargo build --release in twetch/wallet");

// Files that upstream removed since the last copy.
const manifest = path.join(stage, ".twetch-files.json");
if (fs.existsSync(manifest)) {
  for (const old of JSON.parse(fs.readFileSync(manifest, "utf8"))) {
    if (!wanted.has(old)) fs.rmSync(path.join(stage, old), { force: true });
  }
}
fs.writeFileSync(manifest, JSON.stringify([...wanted]));

// What a user of the built app can read must not carry the upstream brand.
const LEFT = /T3 Code|t3\.codes\/(download|mobile)|app\.t3\.codes|ingest\.t3\.codes/;
for (const rel of wanted) {
  if (!/^apps\/(desktop|web|server)\/(src|index\.html|public)/.test(rel) || VERBATIM.some((v) => v.test(rel))) continue;
  const text = fs.readFileSync(path.join(stage, rel), "utf8");
  if (LEFT.test(text) && !text.includes("not affiliated with T3 Tools, Inc.")) problems.push(`${rel}: the upstream brand is still in it`);
}

if (problems.length > 0) {
  console.error(`The rebrand needs attention (edit twetch/rebrand.mjs):\n  ${problems.join("\n  ")}`);
  process.exit(1);
}
console.log(`TwetchCode source: ${stage} (${wanted.size} files)`);
