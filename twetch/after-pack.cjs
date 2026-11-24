// Copied into the build as scripts/twetch-after-pack.cjs. Upstream's step, then an
// seal of the whole app: ad-hoc, or with TWETCH_SIGN_IDENTITY when one is set.
const { execFileSync } = require("node:child_process");
const path = require("node:path");
const upstream = require("./prepare-server-compile-cache.cjs");

module.exports = async (context) => {
  await upstream(context);
  if (context.electronPlatformName !== "darwin" || process.platform !== "darwin") return;
  const app = path.join(context.appOutDir, `${context.packager.appInfo.productFilename}.app`);
  execFileSync("codesign", ["--force", "--deep", "--sign", process.env.TWETCH_SIGN_IDENTITY || "-", app], { stdio: "inherit" });
};
