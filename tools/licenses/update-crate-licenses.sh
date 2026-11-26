#!/usr/bin/env bash
# Writes the license notices of the Rust crates that the app and the Parakeet
# plugin compile in to packaging/licenses/, with cargo-about. Run it after every
# change to Cargo.lock. With --check it changes nothing and fails while a file
# is out of date.
#
#   tools/licenses/update-crate-licenses.sh [--check]
set -euo pipefail

version=0.9.3
repo="$(cd "$(dirname "$0")/../.." && pwd)"
check=false
[[ ${1:-} == --check ]] && check=true

if ! cargo about --version 2>/dev/null | grep -qx "cargo-about $version"; then
  echo "This needs cargo-about $version:" >&2
  echo "  cargo install --locked --features cli --version $version cargo-about" >&2
  exit 1
fi

cd "$repo"
# cargo-about reads the crate sources offline, so every crate in Cargo.lock has
# to be downloaded, also those only other platforms build.
cargo fetch --locked --quiet

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
stale=0
for entry in crates/typewhisper:typewhisper plugins/parakeet:parakeet; do
  crate=${entry%%:*}
  out="packaging/licenses/${entry##*:}-crates.txt"
  cargo about generate --frozen --manifest-path "$crate/Cargo.toml" \
    -c tools/licenses/about.toml tools/licenses/notices.hbs -o "$work/raw.txt"
  # Some license files come with Windows line ends, spaces at line ends and
  # blank lines at the end. They mean nothing, and git diff --check rejects them.
  sed -e 's/\r$//' -e 's/[[:space:]]*$//' "$work/raw.txt" |
    awk '{ line[NR] = $0 } END { n = NR; while (n > 0 && line[n] == "") n--; for (i = 1; i <= n; i++) print line[i] }' \
      > "$work/notices.txt"
  if $check; then
    if ! cmp -s "$work/notices.txt" "$out"; then
      echo "out of date: $out (run tools/licenses/update-crate-licenses.sh)" >&2
      stale=1
    fi
  else
    mkdir -p "$(dirname "$out")"
    cp "$work/notices.txt" "$out"
    echo "Wrote $out"
  fi
done
exit "$stale"
