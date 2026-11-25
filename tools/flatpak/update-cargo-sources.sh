#!/usr/bin/env bash
# Writes packaging/flatpak/cargo-sources.json from Cargo.lock. The Flatpak manifests
# build without network access and take every crate from that file, so run this after
# each change to Cargo.lock. It also checks that the Parakeet add-on builds the
# sherpa-onnx C library whose version Cargo.lock has for the sherpa-onnx-sys bindings.
#
#   tools/flatpak/update-cargo-sources.sh           regenerate the file
#   tools/flatpak/update-cargo-sources.sh --check   fail if the file does not match Cargo.lock
#
# Needs curl and either uv or pipx; they install the generator's Python modules.
set -euo pipefail

# flatpak-cargo-generator.py from flatpak/flatpak-builder-tools, pinned by commit and hash.
generator_commit=41c20aa10819cdb2a4f3ca171758a96d1955c018
generator_sha256=0a2db6be87d75910facef28ab46d4d6460802e8419ab850d0caa6a364d26b380
generator_url="https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/$generator_commit/cargo/flatpak-cargo-generator.py"

check=false
case "${1:-}" in
  "") ;;
  --check) check=true ;;
  *) echo "usage: $0 [--check]" >&2; exit 2 ;;
esac

repo="$(cd "$(dirname "$0")/../.." && pwd)"
output="$repo/packaging/flatpak/cargo-sources.json"
addon="packaging/flatpak/com.typewhisper.TypeWhisper.Plugin.Parakeet.json"

# The bindings must match the C library they are linked with.
sherpa_version="$(awk '$0 == "name = \"sherpa-onnx-sys\"" { getline; gsub(/"/, "", $3); print $3 }' "$repo/Cargo.lock")"
if ! grep -qF "k2-fsa/sherpa-onnx/archive/refs/tags/v$sherpa_version.tar.gz" "$repo/$addon"; then
  echo "Cargo.lock has sherpa-onnx-sys $sherpa_version, but $addon builds another sherpa-onnx." >&2
  echo "Update the sherpa-onnx module and its dependencies there." >&2
  exit 1
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

curl -fsSL --retry 3 -o "$work/flatpak-cargo-generator.py" "$generator_url"
if command -v sha256sum > /dev/null; then
  actual="$(sha256sum "$work/flatpak-cargo-generator.py")"
else
  actual="$(shasum -a 256 "$work/flatpak-cargo-generator.py")"
fi
if [ "${actual%% *}" != "$generator_sha256" ]; then
  echo "flatpak-cargo-generator.py does not match the pinned hash" >&2
  exit 1
fi

# The script declares its Python modules inline (PEP 723).
if command -v uv > /dev/null; then
  run=(uv run --quiet --script)
elif command -v pipx > /dev/null; then
  run=(pipx run)
else
  echo "Install uv or pipx to run flatpak-cargo-generator.py" >&2
  exit 1
fi
"${run[@]}" "$work/flatpak-cargo-generator.py" "$repo/Cargo.lock" -o "$work/cargo-sources.json"
# The generator writes no newline at the end of the file.
echo >> "$work/cargo-sources.json"

if "$check"; then
  if ! diff -u "$output" "$work/cargo-sources.json" > "$work/diff"; then
    head -n 40 "$work/diff" >&2
    echo "packaging/flatpak/cargo-sources.json does not match Cargo.lock." >&2
    echo "Run tools/flatpak/update-cargo-sources.sh and commit the result." >&2
    exit 1
  fi
  echo "packaging/flatpak/cargo-sources.json matches Cargo.lock"
else
  mv "$work/cargo-sources.json" "$output"
  echo "Wrote packaging/flatpak/cargo-sources.json"
fi
