# TypeWhisper for Linux

[![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)](LICENSE)

A native Linux app for TypeWhisper: dictate into any app with a global shortcut,
with local or cloud speech models, and transform the text with workflows. It is
written in Rust with GTK 4 and libadwaita and uses the XDG desktop portals, so it
runs on GNOME and KDE Plasma under Wayland and inside Flatpak.

**Status: early development.** The app dictates: hold or tap a global shortcut,
speak, and NVIDIA Parakeet transcribes on the CPU and pastes the text into the
focused app. There are no settings, workflows or other engines yet. The groundwork
comes from a [platform spike](docs/spike-2026-11.md) on GNOME 51 and Plasma 6.8.

## Layout

| Path | Contents |
| --- | --- |
| `crates/typewhisper` | The GTK 4 and libadwaita app |
| `crates/tw-platform` | Desktop services: microphone capture, global shortcut, pasting through the portals |
| `crates/tw-formats` | Formats shared with the macOS, Windows and iOS apps, with conformance tests against fixtures written by the Mac app |
| `crates/tw-plugin-protocol` | Wire protocol between the app and plugin processes |
| `crates/tw-plugin-host` | Finds installed plugins, starts them and calls them |
| `plugins/parakeet` | NVIDIA Parakeet through sherpa-onnx, as a plugin process |
| `gnome-shell-extension` | Indicator overlay and focused-app reporting on GNOME |
| `packaging` | Flatpak manifest, desktop file, AppStream metadata |
| `tools` | Headless VM sessions, probes for portal behaviour, Flatpak helpers |

## Plugins

Every plugin is its own executable with a `manifest.json`. The app starts it and
talks to it over stdin and stdout (see `crates/tw-plugin-protocol`). A plugin
crash cannot take the app down. Plugins will come from the in-app catalog, as
Flatpak extensions (`com.typewhisper.TypeWhisper.Plugin.*`) and as `.deb`/`.rpm`
packages.

## Build

Fedora:

```sh
sudo dnf install gcc gcc-c++ cargo rust gtk4-devel libadwaita-devel pulseaudio-libs-devel
cargo build --workspace
cargo test --workspace
cargo run -p typewhisper
```

Ubuntu 26.04: install `build-essential libgtk-4-dev libadwaita-1-dev libpulse-dev`
and Rust through rustup.

To dictate with a development build, install it for your user first:

```sh
tools/dev/install-dev.sh
target/release/typewhisper
```

The script builds the release, installs a `.desktop` file that points at it and
copies the Parakeet plugin to `~/.local/share/typewhisper/plugins`. The desktop
portals refuse an app ID without a matching `.desktop` file. In the window, set up
the shortcut, allow typing (on GNOME, switch on "Allow Remote Interaction" in the
dialog) and download the speech model.

Flatpak:

```sh
flatpak-builder --user --install --force-clean build-dir packaging/flatpak/com.typewhisper.TypeWhisper.json
flatpak-builder --user --install --force-clean build-dir packaging/flatpak/com.typewhisper.TypeWhisper.Plugin.Parakeet.json
flatpak run com.typewhisper.TypeWhisper
```

The app defines the extension point `com.typewhisper.TypeWhisper.Plugin`, and each
plugin is its own Flatpak mounted under `/app/plugins/<name>`. The second command
builds Parakeet that way; it needs the app installed first.

The builds have no network access, as on Flathub. flatpak-builder downloads every
source beforehand and checks its hash. Cargo takes the crates from
`packaging/flatpak/cargo-sources.json`, so after every change to `Cargo.lock` run
`tools/flatpak/update-cargo-sources.sh` and commit the result; CI fails while the
file is out of date. The add-on builds sherpa-onnx and its C++ libraries from
source. ONNX Runtime is still the prebuilt static library that sherpa-onnx links
by default, so a Flathub submission of the add-on still needs ONNX Runtime built
from source.

## Related

- [TypeWhisper for macOS](https://github.com/TypeWhisper/typewhisper-mac)
- [TypeWhisper for Windows](https://github.com/TypeWhisper/typewhisper-win)

## License

GPLv3, see [LICENSE](LICENSE). Commercial licensing: [LICENSE-COMMERCIAL.md](LICENSE-COMMERCIAL.md).
Trademark policy: [TRADEMARK.md](TRADEMARK.md).
