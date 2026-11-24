/* oxlint-disable t3code/no-global-process-runtime -- Loaded before the Effect runtime. */
// @effect-diagnostics nodeBuiltinImport:off globalConsole:off globalFetch:off globalTimers:off - Plain Node module, loaded before the Effect runtime.
/**
 * Runs the wallet sidecar for the desktop app, and points OpenCode at it.
 *
 * Upstream's main.ts imports this file first. The local server inherits this
 * process's environment, so what OpenCode needs is set here, before it starts.
 */
import * as NodeChildProcess from "node:child_process";
import * as NodeFS from "node:fs";
import * as NodeOS from "node:os";
import * as NodePath from "node:path";
import * as NodeReadline from "node:readline";

import * as Electron from "electron";

import { REBRANDED } from "./brand.ts";
import * as OpenCodeInstall from "./opencode.ts";

const DEFAULT_PORT = 48213;
const RESTART_MS = [500, 2_000, 10_000, 30_000];
const DEFAULT_MODEL = "deepseek/deepseek-v4.1-flash-1017";

interface Ready {
  readonly port: number;
  readonly admin_token: string;
}

export interface WalletStatus {
  readonly ready: boolean;
  readonly balance_sats: number;
  readonly balance_usd: number | null;
}

let home = "";
let child: NodeChildProcess.ChildProcess | undefined;
let ready: Ready | undefined;
let quitting = false;
let restarts = 0;
let walletWindow: Electron.BrowserWindow | undefined;

function binaryPath(): string | undefined {
  const name = process.platform === "win32" ? "twetch-wallet.exe" : "twetch-wallet";
  const candidates = Electron.app.isPackaged
    ? [NodePath.join(process.resourcesPath, "twetch-wallet", name)]
    : ["release", "debug"].map((profile) =>
        NodePath.join(__dirname, "../../../twetch/wallet/target", profile, name),
      );
  return candidates.find((candidate) => NodeFS.existsSync(candidate));
}

/** Only what the sidecar needs: no tokens of other tools, and nothing of T3 Code's. */
function sidecarEnvironment(): NodeJS.ProcessEnv {
  const env: NodeJS.ProcessEnv = { TWETCHCODE_HOME: home };
  const names = [
    "HOME",
    "USER",
    "LOGNAME",
    "PATH",
    "TMPDIR",
    "LANG",
    "TZ",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "SYSTEMROOT",
    "DBUS_SESSION_BUS_ADDRESS",
    "XDG_RUNTIME_DIR",
    "HTTPS_PROXY",
    "NO_PROXY",
    "TWETCH_AI_URL",
    "TWETCH_WALLET_WOC_URL",
    "TWETCH_WALLET_PORT",
  ];
  for (const name of names) {
    if (process.env[name] !== undefined) env[name] = process.env[name];
  }
  return env;
}

function launch(): void {
  const binary = binaryPath();
  if (binary === undefined) {
    console.error("[twetch] the wallet binary is missing from this build");
    return;
  }
  const spawned = NodeChildProcess.spawn(binary, [], {
    cwd: home,
    env: sidecarEnvironment(),
    stdio: ["pipe", "pipe", "pipe"],
    windowsHide: true,
  });
  child = spawned;
  const log = NodeFS.createWriteStream(NodePath.join(home, "wallet", "wallet.log"), {
    flags: "a",
    mode: 0o600,
  });
  log.on("error", () => {});
  spawned.stderr.pipe(log);
  spawned.stdin.on("error", () => {});
  spawned.on("error", (error) => console.error("[twetch] wallet:", error.message));
  NodeReadline.createInterface({ input: spawned.stdout }).on("line", (line) => {
    try {
      const value = JSON.parse(line) as Partial<Ready> & { ready?: boolean };
      if (value.ready !== true || typeof value.port !== "number") return;
      if (typeof value.admin_token !== "string") return;
      ready = { port: value.port, admin_token: value.admin_token };
      restarts = 0;
      writeOpenCodeConfig().catch((error: unknown) =>
        console.error("[twetch] OpenCode config:", error),
      );
    } catch {
      // Not the ready line.
    }
  });
  spawned.on("exit", () => {
    if (child === spawned) {
      child = undefined;
      ready = undefined;
    }
    if (quitting) return;
    const wait = RESTART_MS[Math.min(restarts, RESTART_MS.length - 1)];
    restarts += 1;
    setTimeout(() => {
      if (!quitting && child === undefined) launch();
    }, wait);
  });
}

async function local(path: string, method = "GET"): Promise<unknown> {
  if (ready === undefined) throw new Error("The wallet is not running.");
  const response = await fetch(`http://127.0.0.1:${ready.port}${path}`, {
    method,
    headers: { authorization: `Bearer ${ready.admin_token}` },
    signal: AbortSignal.timeout(10_000),
  });
  if (!response.ok) throw new Error(`The wallet answered ${response.status}.`);
  return response.json();
}

function writeAtomic(path: string, text: string): void {
  NodeFS.mkdirSync(NodePath.dirname(path), { recursive: true, mode: 0o700 });
  const temporary = `${path}.${process.pid}.tmp`;
  NodeFS.writeFileSync(temporary, text, { mode: 0o600 });
  NodeFS.renameSync(temporary, path);
}

const openCodePath = (...parts: string[]): string => NodePath.join(home, "opencode", ...parts);

/** The token stays in its own 0600 file: OpenCode reads it from there, no environment carries it. */
async function writeOpenCodeConfig(): Promise<void> {
  const config = (await local("/local/opencode-config")) as {
    provider: { twetch: { options: Record<string, unknown> } };
  };
  config.provider.twetch.options["apiKey"] =
    `{file:${NodePath.join(home, "wallet", "proxy-token")}}`;
  writeAtomic(openCodePath("twetch.json"), `${JSON.stringify(config, null, 2)}\n`);
}

/** OpenCode finds no file to load until the first wallet start; give it a valid one. */
function ensureOpenCodeConfig(): void {
  if (NodeFS.existsSync(openCodePath("twetch.json"))) return;
  const port = Number(process.env["TWETCH_WALLET_PORT"]) || DEFAULT_PORT;
  const model = (name: string) => ({ name, limit: { context: 1_048_576, output: 65_536 } });
  const config = {
    model: `twetch/${DEFAULT_MODEL}`,
    share: "disabled",
    autoupdate: false,
    provider: {
      twetch: {
        npm: "@ai-sdk/openai-compatible",
        name: "Twetch",
        options: {
          baseURL: `http://127.0.0.1:${port}/v1`,
          apiKey: `{file:${NodePath.join(home, "wallet", "proxy-token")}}`,
        },
        models: {
          [DEFAULT_MODEL]: model("DeepSeek V4.1 Flash"),
          "z-ai/glm-5.3": model("GLM 5.3"),
          "z-ai/glm-5.3-flash": model("GLM 5.3 Flash"),
          "deepseek/deepseek-v4-pro-0813": model("DeepSeek V4 Pro"),
        },
      },
    },
  };
  writeAtomic(openCodePath("twetch.json"), `${JSON.stringify(config, null, 2)}\n`);
}

/** First run only: turn OpenCode on and choose the model. Later the settings are the user's. */
function seedSettings(stateDir: string, openCodeBinary: string): void {
  const path = NodePath.join(stateDir, "settings.json");
  if (NodeFS.existsSync(path)) return;
  const selection = { instanceId: "opencode", model: `twetch/${DEFAULT_MODEL}` };
  const settings = {
    providerInstances: {
      opencode: {
        driver: "opencode",
        enabled: true,
        config: { enabled: true, binaryPath: openCodeBinary },
      },
    },
    defaultModelSelection: selection,
    textGenerationModelSelection: selection,
  };
  writeAtomic(path, `${JSON.stringify(settings, null, 2)}\n`);
}

async function openWallet(): Promise<void> {
  const { path } = (await local("/local/wallet-link", "POST")) as { path: string };
  const origin = `http://127.0.0.1:${ready?.port}`;
  if (walletWindow === undefined || walletWindow.isDestroyed()) {
    walletWindow = new Electron.BrowserWindow({
      width: 980,
      height: 860,
      minWidth: 400,
      minHeight: 480,
      title: "TwetchCode Wallet",
      backgroundColor: Electron.nativeTheme.shouldUseDarkColors ? "#111513" : "#f6f7f5",
      webPreferences: {
        // Not persisted: the session ends with the app.
        partition: "twetch-wallet",
        sandbox: true,
        contextIsolation: true,
        nodeIntegration: false,
      },
    });
    const contents = walletWindow.webContents;
    contents.session.setPermissionRequestHandler((_contents, permission, done) =>
      done(permission === "clipboard-sanitized-write"),
    );
    contents.on("will-navigate", (event, url) => {
      if (new URL(url).origin !== origin) event.preventDefault();
    });
    contents.setWindowOpenHandler(({ url }) => {
      if (url.startsWith("https://whatsonchain.com/tx/")) void Electron.shell.openExternal(url);
      return { action: "deny" };
    });
    walletWindow.on("closed", () => {
      walletWindow = undefined;
    });
  }
  await walletWindow.loadURL(`${origin}${path}`);
  walletWindow.show();
}

/** Only the app's own window may ask: never a page in the preview browser. */
function fromApp(event: Electron.IpcMainInvokeEvent): boolean {
  const url = event.senderFrame?.url ?? "";
  return (
    event.senderFrame === event.sender.mainFrame &&
    /^(twetchcode|t3code)(-dev)?:\/\/app\//.test(url)
  );
}

function registerIpc(): void {
  Electron.ipcMain.handle("twetch:wallet-open", async (event) => {
    if (fromApp(event)) await openWallet();
  });
  Electron.ipcMain.handle("twetch:wallet-status", async (event) =>
    fromApp(event) && ready !== undefined ? local("/local/status") : null,
  );
}

/**
 * OpenCode gets its own config folder and database, so the user's own OpenCode
 * (its settings, sessions and background service) is never opened.
 */
function install(): void {
  // Same rule as DesktopConfig's home, under the name the rebrand gives it.
  home = process.env["TWETCHCODE_HOME"]?.trim() || NodePath.join(NodeOS.homedir(), ".twetch-code");
  NodeFS.mkdirSync(NodePath.join(home, "wallet"), { recursive: true, mode: 0o700 });
  NodeFS.mkdirSync(openCodePath("config"), { recursive: true, mode: 0o700 });
  ensureOpenCodeConfig();
  const bin = openCodePath("bin");
  const found = OpenCodeInstall.find(bin);
  seedSettings(NodePath.join(home, "userdata"), found ?? OpenCodeInstall.managedPath(bin));
  Object.assign(process.env, {
    TWETCHCODE_TELEMETRY_ENABLED: "false",
    OPENCODE_CONFIG: openCodePath("twetch.json"),
    OPENCODE_CONFIG_DIR: openCodePath("config"),
    OPENCODE_DB: openCodePath("opencode.db"),
    OPENCODE_DISABLE_AUTOUPDATE: "1",
  });
  Electron.app.on("will-quit", () => {
    quitting = true;
    // It stops when its stdin closes; the signal is for one that is stuck.
    child?.stdin?.end();
    child?.kill("SIGTERM");
  });
  void Electron.app.whenReady().then(() => {
    registerIpc();
    if (!quitting) launch();
    if (found === undefined) void OpenCodeInstall.offer(bin);
  });
}

if (REBRANDED && !process.argv.includes("--version")) {
  try {
    install();
  } catch (error) {
    console.error("[twetch] wallet:", error);
  }
}
