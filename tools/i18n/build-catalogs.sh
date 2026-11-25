#!/usr/bin/env bash
# Compiles the gettext catalogs in po/ for every language in po/LINGUAS into
# <locale-dir>/<language>/LC_MESSAGES/typewhisper.mo. The app looks for them in
# share/locale next to the bin directory of its executable.
#
#   tools/i18n/build-catalogs.sh <locale-dir>
set -euo pipefail

repo="$(cd "$(dirname "$0")/../.." && pwd)"
out=${1:?usage: build-catalogs.sh <locale-dir>}
while read -r language; do
  [[ -z $language || $language == \#* ]] && continue
  mkdir -p "$out/$language/LC_MESSAGES"
  msgfmt --check --output-file="$out/$language/LC_MESSAGES/typewhisper.mo" "$repo/po/$language.po"
done < "$repo/po/LINGUAS"
