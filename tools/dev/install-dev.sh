#!/usr/bin/env bash
# Installs a development build for the current user, so it runs like an
# installed app:
# - a .desktop file whose Exec points at the build, and the icon. The portals
#   accept the app ID only when a matching .desktop file names a program that exists.
# - the Parakeet and Whisper plugin packages in the user plugin folder.
# - the translations in <target-dir>/share/locale, where the build looks for them.
#
#   tools/dev/install-dev.sh [target-dir]   (default: target, release profile)
set -euo pipefail

repo="$(cd "$(dirname "$0")/../.." && pwd)"
target_dir="${1:-${CARGO_TARGET_DIR:-$repo/target}}"
# The .desktop file needs an absolute path.
mkdir -p "$target_dir"
target_dir="$(cd "$target_dir" && pwd)"
target="$target_dir/release"
cargo build --release --locked --target-dir "$target_dir" \
  -p typewhisper -p tw-plugin-parakeet -p tw-plugin-whisper-cpp --manifest-path "$repo/Cargo.toml"
"$repo/tools/i18n/build-catalogs.sh" "$target_dir/share/locale"
"$repo/tools/i18n/translate-packaging.sh" "$target_dir/packaging"

# Quotes a path as one argument of an Exec key: the quoting rule escapes
# " ` $ and \, the string rule then doubles every backslash, and % becomes %%.
desktop_quote() {
  # shellcheck disable=SC1003  # a lone backslash, not an escaped quote
  local s=$1 bs='\'
  s=${s//"$bs"/"$bs$bs"}
  s=${s//'"'/"$bs\""}
  s=${s//'`'/"$bs\`"}
  s=${s//'$'/"$bs\$"}
  s=${s//"$bs"/"$bs$bs"}
  s=${s//%/%%}
  printf '"%s"' "$s"
}

data="${XDG_DATA_HOME:-}"
[[ $data == /* ]] || data="$HOME/.local/share"
apps="$data/applications"
mkdir -p "$apps"
exec_line="Exec=$(desktop_quote "$target/typewhisper")"
while IFS= read -r line; do
  if [[ $line == Exec=* ]]; then
    printf '%s\n' "$exec_line"
  else
    printf '%s\n' "$line"
  fi
done < "$target_dir/packaging/com.typewhisper.TypeWhisper.desktop" > "$apps/com.typewhisper.TypeWhisper.desktop"
install -Dm644 "$repo/packaging/com.typewhisper.TypeWhisper.svg" \
  "$data/icons/hicolor/scalable/apps/com.typewhisper.TypeWhisper.svg"

for entry in parakeet:com.typewhisper.parakeet whisper-cpp:com.typewhisper.whisper-cpp; do
  name=${entry%%:*}
  plugin="$data/typewhisper/plugins/${entry##*:}"
  mkdir -p "$plugin/bin"
  cp "$repo/plugins/$name/manifest.json" "$plugin/"
  install -m 755 "$target/tw-plugin-$name" "$plugin/bin/tw-plugin-$name"
done

echo "Installed. Start with: $target/typewhisper"
