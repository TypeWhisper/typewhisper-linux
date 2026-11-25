#!/usr/bin/env bash
# Installs a development build for the current user, so it runs like an
# installed app:
# - a .desktop file whose Exec points at the build. The portals accept the app
#   ID only when a matching .desktop file names a program that exists.
# - the Parakeet plugin package in the user plugin folder.
#
#   tools/dev/install-dev.sh [target-dir]   (default: target, release profile)
set -euo pipefail

repo="$(cd "$(dirname "$0")/../.." && pwd)"
target_dir="${1:-${CARGO_TARGET_DIR:-$repo/target}}"
target="$target_dir/release"
cargo build --release --locked --target-dir "$target_dir" \
  -p typewhisper -p tw-plugin-parakeet --manifest-path "$repo/Cargo.toml"

data="${XDG_DATA_HOME:-$HOME/.local/share}"
apps="$data/applications"
mkdir -p "$apps"
sed "s|^Exec=.*|Exec=$target/typewhisper|" "$repo/packaging/com.typewhisper.TypeWhisper.desktop" \
  > "$apps/com.typewhisper.TypeWhisper.desktop"

plugin="$data/typewhisper/plugins/com.typewhisper.parakeet"
mkdir -p "$plugin/bin"
cp "$repo/plugins/parakeet/manifest.json" "$plugin/"
install -m 755 "$target/tw-plugin-parakeet" "$plugin/bin/tw-plugin-parakeet"

echo "Installed. Start with: $target/typewhisper"
