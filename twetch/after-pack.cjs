// Copied into the build as scripts/twetch-after-pack.cjs. Upstream's step, then an
// ad-hoc seal of the whole app, so macOS sees one intact bundle without a certificate.
const { execFileSync } = require("node:child_process");
const path = require("node:path");
const upstream = require("./prepare-server-compile-cache.cjs");

module.exports = async (context) => {
  await upstream(context);
  if (context.electronPlatformName !== "darwin" || process.platform !== "darwin") return;
  if (process.env.TWETCHCODE_SIGNED === "1") return;
  const app = path.join(context.appOutDir, `${context.packager.appInfo.productFilename}.app`);
  execFileSync("codesign", ["--force", "--deep", "--sign", "-", app], { stdio: "inherit" });
};
