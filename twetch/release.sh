#!/usr/bin/env bash
# Build, sign, tag, and publish a TwetchCode release for auto-updates.
#
#   twetch/release.sh              # bumps patch in twetch/VERSION (e.g. 0.2.0 -> 0.2.1)
#   twetch/release.sh 0.2.0        # ships exact version
#   twetch/release.sh --dry-run    # builds with update feed without uploading
set -euo pipefail
cd "$(dirname "$0")/.."

REPO=${TWETCH_RELEASE_REPO:-twetch-inc/twetch-code}
DRY_RUN=0
ARG=""
for a in "$@"; do
  case "$a" in
    --dry-run) DRY_RUN=1 ;;
    *) ARG="$a" ;;
  esac
done

CUR=$(tr -d '[:space:]' < twetch/VERSION)
if [ -z "$ARG" ] || [ "$ARG" = "patch" ]; then
  IFS=. read -r MAJ MIN PAT <<< "$CUR"
  VERSION="$MAJ.$MIN.$((PAT + 1))"
elif [ "$ARG" = "minor" ]; then
  IFS=. read -r MAJ MIN _ <<< "$CUR"
  VERSION="$MAJ.$((MIN + 1)).0"
else
  VERSION="$ARG"
fi

# Squirrel.Mac auto-updates require a stable Apple signing identity (not ad-hoc).
if [ "$(uname)" = Darwin ] && [ -z "${TWETCH_SIGN_IDENTITY:-}" ]; then
  DEV_ID=$(security find-identity -v -p codesigning | sed -n 's/.*"\(Developer ID Application: [^"]*\)".*/\1/p' | head -1 || true)
  APPLE_DEV=$(security find-identity -v -p codesigning | sed -n 's/.*"\(Apple Development: [^"]*\)".*/\1/p' | head -1 || true)
  if [ -n "$DEV_ID" ]; then
    export TWETCH_SIGN_IDENTITY="$DEV_ID"
  elif [ -n "$APPLE_DEV" ]; then
    export TWETCH_SIGN_IDENTITY="$APPLE_DEV"
  fi
fi

echo "== TwetchCode release v$VERSION -> $REPO"
echo "== Signing identity: ${TWETCH_SIGN_IDENTITY:-ad-hoc (auto-update install on macOS requires a stable cert)}"

if [ "$VERSION" != "$CUR" ] && [ "$DRY_RUN" -eq 0 ]; then
  printf '%s\n' "$VERSION" > twetch/VERSION
  git add twetch/VERSION
  git commit --no-verify -m "twetch: release v$VERSION"
fi

TWETCH_VERSION="$VERSION" TWETCH_RELEASE_REPO="$REPO" twetch/build.sh

ASSETS=()
for f in \
  "twetch/dist/latest-mac.yml" \
  "twetch/dist/TwetchCode-${VERSION}-arm64.zip" \
  "twetch/dist/TwetchCode-${VERSION}-arm64.zip.blockmap" \
  "twetch/dist/TwetchCode-${VERSION}-arm64.dmg" \
  "twetch/dist/TwetchCode-${VERSION}-arm64.dmg.blockmap"; do
  [ -f "$f" ] && ASSETS+=("$f")
done

echo "== Release assets (${#ASSETS[@]}):"
printf '  %s\n' "${ASSETS[@]}"

if [ "$DRY_RUN" -eq 1 ]; then
  echo "== Dry run complete (not uploaded)."
  exit 0
fi

if ! gh repo view "$REPO" >/dev/null 2>&1; then
  echo "== Creating public GitHub release repo $REPO..."
  gh repo create "$REPO" --public --description "TwetchCode — BSV pay-per-request coding desktop app"
fi

if ! git remote get-url origin >/dev/null 2>&1; then
  git remote add origin "git@github.com:${REPO}.git"
fi

git tag -f "v$VERSION"
git push origin HEAD:twetch "v$VERSION"

if gh release view "v$VERSION" --repo "$REPO" >/dev/null 2>&1; then
  gh release upload "v$VERSION" "${ASSETS[@]}" --repo "$REPO" --clobber
else
  gh release create "v$VERSION" "${ASSETS[@]}" \
    --repo "$REPO" \
    --title "TwetchCode v$VERSION" \
    --notes "TwetchCode v$VERSION (macOS arm64). Auto-updates via Settings > About."
fi

echo "== Published: https://github.com/$REPO/releases/tag/v$VERSION"
