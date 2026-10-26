#!/bin/sh
# newgit fork: build "T3 Code (newgit)" for this Mac and install it into /Applications.
set -eu

cd "$(dirname "$0")/../.."
# T3 Code sets this for its agents; it makes Electron (and so the app) start as plain Node.
unset ELECTRON_RUN_AS_NODE

case "$(uname -m)" in
  arm64) arch=arm64 ;;
  *) arch=x64 ;;
esac
app="/Applications/T3 Code (newgit).app"
out="release-newgit"

corepack pnpm install --frozen-lockfile
rm -rf "$out"
PATH="$PWD/node_modules/.bin:$PATH" node scripts/build-desktop-artifact.ts \
  --platform mac --target zip --arch "$arch" --output-dir "$out"

osascript -e 'quit app "T3 Code (newgit)"' 2>/dev/null || true
stage="$(mktemp -d)"
ditto -x -k "$out"/T3-Code-*-"$arch".zip "$stage"
rm -rf "$app"
ditto "$stage/T3 Code (newgit).app" "$app"
rm -rf "$stage"
echo "Installed $app"
