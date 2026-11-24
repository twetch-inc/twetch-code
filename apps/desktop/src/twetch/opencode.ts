/* oxlint-disable t3code/no-global-process-runtime -- Loaded before the Effect runtime. */
// @effect-diagnostics nodeBuiltinImport:off globalConsole:off globalFetch:off globalTimers:off - Plain Node module, loaded before the Effect runtime.
/** Finds OpenCode, and offers to download a pinned one when there is none. */
import * as NodeChildProcess from "node:child_process";
import * as NodeCrypto from "node:crypto";
import * as NodeFS from "node:fs";
import * as NodeOS from "node:os";
import * as NodePath from "node:path";
import * as NodeStreamPromises from "node:stream/promises";

import * as Electron from "electron";

const VERSION = "2.4.1";
/** The npm packages' own sha512. */
const PACKAGES: Record<string, string> = {
  "darwin-arm64":
    "r6PIZl5wqFxVYfVFjm2JK4uf1a3YT7FMEVTODJF/1hKe6wBdK7THzbAM1SSyuRuMnSkDb9+dd8xNOk9zQK6trg==",
  "darwin-x64":
    "J/khLpe0pisTGDD5jpddQjqb5e8qUsWYpiRkqIBN3a/P4S5asfP2ErLKSevmpGzhg9fC1TpLJnGVXwgN+JJgKg==",
  "linux-arm64":
    "olNKfoAuHaDtHrWrlqxh2RPSonSfAZPGJlru7tMNeNTlHyLSrTbDe/+HJ6vdYRp7g4yKLSQ8SG264U/IC7Pgtw==",
  "linux-x64":
    "FYQAnneVlLmqy862674vvRVkj8yHuZAFHJGV3UCGQxo8aKG58IHPKrspbxruVajnIEpb0t8PeO63H/oW4vJG5A==",
  "win32-arm64":
    "Ab4dLYTSawLB1cDJsDVCcy3V5DtjHZA95PEPu04+YT1s2EeH/VsjULbmqdRWzwGR/aPWs0+DA4i9IPqDYfUl4Q==",
  "win32-x64":
    "zYbwH8ab3Lv0d+uyvw0pxqSjf/m+oROxHO0kjBBDFWu6ZY31aTnUcijaOxEc7VTssiwkk/0HIapAADFuPqZiNQ==",
};

const NAME = process.platform === "win32" ? "opencode.exe" : "opencode";

/** Where our own copy goes, whether or not it is there yet. */
export const managedPath = (binDir: string): string => NodePath.join(binDir, VERSION, NAME);

/** An installed OpenCode. A GUI app's PATH is short, so the usual folders are looked at too. */
export function find(binDir: string): string | undefined {
  const home = NodeOS.homedir();
  const dirs = [
    ...(process.env["PATH"] ?? "").split(NodePath.delimiter),
    NodePath.join(home, ".local/bin"),
    NodePath.join(home, ".opencode/bin"),
    NodePath.join(home, ".bun/bin"),
    "/opt/homebrew/bin",
    "/usr/local/bin",
  ];
  const managed = managedPath(binDir);
  if (NodeFS.existsSync(managed)) return managed;
  return dirs
    .filter((dir) => dir !== "")
    .map((dir) => NodePath.join(dir, NAME))
    .find((path) => NodeFS.existsSync(path));
}

async function download(binDir: string, expected: string): Promise<void> {
  const platform = process.platform === "win32" ? "windows" : process.platform;
  const name = `cli-${platform}-${process.arch}`;
  const work = NodePath.join(binDir, `.download-${process.pid}`);
  NodeFS.mkdirSync(work, { recursive: true, mode: 0o700 });
  try {
    const archive = NodePath.join(work, "opencode.tgz");
    const response = await fetch(
      `https://registry.npmjs.org/@opencode/${name}/-/${name}-${VERSION}.tgz`,
    );
    if (!response.ok || response.body === null) {
      throw new Error(`The download answered ${response.status}.`);
    }
    const hash = NodeCrypto.createHash("sha512");
    await NodeStreamPromises.pipeline(
      response.body,
      async function* (source: AsyncIterable<Uint8Array>) {
        for await (const chunk of source) {
          hash.update(chunk);
          yield chunk;
        }
      },
      NodeFS.createWriteStream(archive, { mode: 0o600 }),
    );
    if (hash.digest("base64") !== expected) throw new Error("The download was damaged.");
    await new Promise<void>((resolve, reject) => {
      const tar = NodeChildProcess.spawn("tar", ["-xzf", archive, "-C", work], { stdio: "ignore" });
      tar.on("error", reject);
      tar.on("exit", (code) =>
        code === 0 ? resolve() : reject(new Error("The download could not be unpacked.")),
      );
    });
    const target = managedPath(binDir);
    NodeFS.mkdirSync(NodePath.dirname(target), { recursive: true });
    NodeFS.renameSync(NodePath.join(work, "package/bin", NAME), target);
    NodeFS.chmodSync(target, 0o755);
  } finally {
    NodeFS.rmSync(work, { recursive: true, force: true });
  }
}

/** Ask once per start while OpenCode is missing. The settings already name where it will be. */
export async function offer(binDir: string): Promise<void> {
  const expected = PACKAGES[`${process.platform}-${process.arch}`];
  const manual = "Install it with: npm install -g @opencode/cli";
  if (expected === undefined) {
    await Electron.dialog.showMessageBox({
      type: "info",
      message: "TwetchCode needs OpenCode",
      detail: `${manual}\nThen choose Refresh provider status in Settings > Providers.`,
    });
    return;
  }
  const { response } = await Electron.dialog.showMessageBox({
    type: "question",
    message: "TwetchCode needs OpenCode",
    detail:
      "OpenCode is the open-source coding agent that runs the Twetch models. It is about 240 MB unpacked and is kept in TwetchCode's own folder.",
    buttons: ["Download", "Not Now"],
    defaultId: 0,
    cancelId: 1,
  });
  if (response !== 0) return;
  try {
    await download(binDir, expected);
    await Electron.dialog.showMessageBox({
      type: "info",
      message: "OpenCode is ready",
      detail: "Choose Refresh provider status in Settings > Providers to use it now.",
    });
  } catch (error) {
    await Electron.dialog.showMessageBox({
      type: "error",
      message: "OpenCode could not be downloaded",
      detail: `${error instanceof Error ? error.message : String(error)}\n${manual}`,
    });
  }
}
