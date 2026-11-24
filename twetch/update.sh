#!/usr/bin/env bash
# Bring in a newer upstream release, then check our own parts.
#
#   twetch/update.sh            newest release tag (vX.Y.Z)
#   twetch/update.sh v0.1.12    that tag, branch or commit
#
# Merges into the branch `twetch`. Nothing is pushed. On a conflict it stops and
# lists the files; resolve them, `git commit`, and run it again with --check.
set -euo pipefail
cd "$(dirname "$0")/.."

say() { printf '\n== %s\n' "$*"; }
die() { printf '\nstopped: %s\n' "$*" >&2; exit 1; }

check() {
  . twetch/tools.sh
  say "wallet: format, lints, tests"
  (cd twetch/wallet && cargo fmt --all --check && cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked)
  (cd twetch/wallet && cargo build --release --locked)
  say "rebrand: every anchored text is still there"
  node twetch/rebrand.mjs
  say "types of the files we touch"
  pnpm install --frozen-lockfile --prefer-offline
  (cd apps/desktop && ../../node_modules/.bin/effect-tsgo tsrs --noEmit)
  (cd apps/web && ../../node_modules/.bin/effect-tsgo tsrs --noEmit)
  PATH=$PWD/node_modules/.bin:$PATH vp test run scripts/build-desktop-artifact.test.ts apps/desktop/src/backend/DesktopBackendConfiguration.test.ts apps/desktop/src/window/DesktopApplicationMenu.test.ts
  say "our edits to upstream files"
  git diff --stat "$(cat twetch/UPSTREAM)" -- . ':!twetch' ':!apps/*/src/twetch'
  say "done. Build the app with twetch/build.sh"
}

if [ "${1:-}" = --check ]; then
  check
  exit 0
fi

[ "$(git branch --show-current)" = twetch ] || die "check out the branch twetch first"
git rev-parse -q --verify MERGE_HEAD >/dev/null && die "a merge is in progress: resolve it and commit, or git merge --abort"
[ -z "$(git status --porcelain --untracked-files=no)" ] || die "commit or stash your changes first"

say "fetching upstream"
git fetch upstream --tags --prune
# main mirrors upstream; it is never checked out here.
git branch --force main upstream/main

REF=${1:-$(git tag --list 'v[0-9]*' --sort=-v:refname | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | head -1)}
git rev-parse -q --verify "$REF^{commit}" >/dev/null || die "no such ref: $REF"
if git merge-base --is-ancestor "$REF" HEAD; then
  say "$REF is already in"
else
  say "merging $REF ($(git rev-list --count "HEAD..$REF") commits)"
  if ! git merge --no-ff --no-edit -m "Merge upstream $REF" "$REF"; then
    printf '\nConflicts in:\n'
    git diff --name-only --diff-filter=U | sed 's/^/  /'
    printf '\nOurs are the lines marked TwetchCode. Resolve, git add, git commit, then: twetch/update.sh --check\nTo give up: git merge --abort\n'
    exit 2
  fi
  echo "$REF" > twetch/UPSTREAM
  git add twetch/UPSTREAM
  git commit -q -m "twetch: base on upstream $REF"
fi
check
