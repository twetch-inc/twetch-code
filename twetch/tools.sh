# Sourced: the Node and pnpm this repo asks for, kept in twetch/.tools.
NODE_VERSION=$(node -p 'require("./package.json").engines.node' 2>/dev/null || sed -n 's/.*"node": "\([0-9.]*\)".*/\1/p' package.json | head -1)
PNPM_VERSION=$(sed -n 's/.*"packageManager": "pnpm@\([0-9.]*\).*/\1/p' package.json)
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) NODE_DIST=darwin-arm64 ;;
  Darwin-x86_64) NODE_DIST=darwin-x64 ;;
  Linux-aarch64) NODE_DIST=linux-arm64 ;;
  *) NODE_DIST=linux-x64 ;;
esac
NODE_DIR=$PWD/twetch/.tools/node-v$NODE_VERSION-$NODE_DIST
if [ ! -x "$NODE_DIR/bin/node" ]; then
  mkdir -p twetch/.tools
  FILE=node-v$NODE_VERSION-$NODE_DIST.tar.gz
  curl -fsSL "https://nodejs.org/dist/v$NODE_VERSION/$FILE" -o "twetch/.tools/$FILE"
  WANT=$(curl -fsSL "https://nodejs.org/dist/v$NODE_VERSION/SHASUMS256.txt" | awk -v f="$FILE" '$2 == f { print $1 }')
  GOT=$(shasum -a 256 "twetch/.tools/$FILE" | awk '{ print $1 }')
  [ -n "$WANT" ] && [ "$WANT" = "$GOT" ] || { echo "Node download does not match its checksum" >&2; exit 1; }
  tar xzf "twetch/.tools/$FILE" -C twetch/.tools
  rm "twetch/.tools/$FILE"
fi
export PATH=$NODE_DIR/bin:$PATH
[ "$(pnpm --version 2>/dev/null)" = "$PNPM_VERSION" ] || npm install --silent -g "pnpm@$PNPM_VERSION"
