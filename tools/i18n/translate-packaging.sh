#!/usr/bin/env bash
# Writes the desktop file and the AppStream metadata, with the translations
# from po/, from their templates in packaging/ into <out-dir>.
#
#   tools/i18n/translate-packaging.sh <out-dir>
set -euo pipefail

repo="$(cd "$(dirname "$0")/../.." && pwd)"
out=${1:?usage: translate-packaging.sh <out-dir>}
mkdir -p "$out"
msgfmt --desktop -d "$repo/po" \
  --template="$repo/packaging/com.typewhisper.TypeWhisper.desktop.in" \
  -o "$out/com.typewhisper.TypeWhisper.desktop"
for template in \
  "$repo/packaging/com.typewhisper.TypeWhisper.metainfo.xml.in" \
  "$repo/packaging/flatpak/com.typewhisper.TypeWhisper.Plugin.Parakeet.metainfo.xml.in"; do
  msgfmt --xml -d "$repo/po" --template="$template" -o "$out/$(basename "$template" .in)"
done
