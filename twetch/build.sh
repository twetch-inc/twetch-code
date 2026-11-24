#!/usr/bin/env bash
# Build TwetchCode: the wallet, the rebranded copy, the desktop app.
#
#   twetch/build.sh                  macOS arm64 .dmg and .app, ad-hoc signed
#   twetch/build.sh --platform linux --target AppImage --arch x64
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
. twetch/tools.sh

VERSION=$(git describe --tags --abbrev=0 --match 'v[0-9]*' --exclude '*-*')
VERSION=${VERSION#v}
[ $# -gt 0 ] || set -- --platform mac --target dmg --arch arm64

(cd twetch/wallet && cargo build --release --locked)
node twetch/rebrand.mjs

cd twetch/.stage
# A T3 Code session exports its own settings; none belong in this build. Neither
# does anyone's update feed, telemetry key or cloud account.
for name in $(env | sed -n 's/^\(T3CODE_[A-Z0-9_]*\|TWETCHCODE_[A-Z0-9_]*\|VITE_[A-Z0-9_]*\|GITHUB_REPOSITORY\|GH_TOKEN\|GITHUB_TOKEN\)=.*/\1/p'); do unset "$name"; done
export CSC_IDENTITY_AUTO_DISCOVERY=false
# Never let the copy's install script touch the real repo's git hooks.
export VITE_GIT_HOOKS=0
pnpm install --frozen-lockfile --prefer-offline
export PATH=$PWD/node_modules/.bin:$PATH
rm -rf "$ROOT/twetch/dist"
node scripts/build-desktop-artifact.ts "$@" --build-version "$VERSION" --output-dir "$ROOT/twetch/dist"
cd "$ROOT"

if [ "$(uname)" = Darwin ]; then
  DMG=$(ls twetch/dist/*.dmg 2>/dev/null | head -1 || true)
  if [ -n "$DMG" ]; then
    MOUNT=$(mktemp -d)
    hdiutil attach -quiet -nobrowse -readonly -mountpoint "$MOUNT" "$DMG"
    rm -rf twetch/dist/TwetchCode.app
    ditto "$MOUNT/TwetchCode.app" twetch/dist/TwetchCode.app
    hdiutil detach -quiet "$MOUNT"
    codesign --verify --deep --strict twetch/dist/TwetchCode.app
  fi
fi
ls -la twetch/dist
