# TypeWhisper for Linux

[![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)](LICENSE)

A native Linux app for TypeWhisper: dictate into any app with a global shortcut,
with local or cloud speech models, and transform the text with workflows. It is
written in Rust with GTK 4 and libadwaita and uses the XDG desktop portals, so it
runs on GNOME and KDE Plasma under Wayland and inside Flatpak.

**Status: early development.** The repository holds the groundwork that a
[platform spike](docs/spike-2026-11.md) verified on GNOME 51 and Plasma 6.8. The app
cannot dictate yet.

## Layout

| Path | Contents |
| --- | --- |
| `crates/typewhisper` | The GTK 4 and libadwaita app |
| `crates/tw-platform` | Desktop services: microphone capture now, portals and secrets next |
| `crates/tw-formats` | Formats shared with the macOS, Windows and iOS apps, with conformance tests against fixtures written by the Mac app |
| `crates/tw-plugin-protocol` | Wire protocol between the app and plugin processes |
| `plugins/parakeet` | NVIDIA Parakeet through sherpa-onnx, as a plugin process |
| `gnome-shell-extension` | Indicator overlay and focused-app reporting on GNOME |
| `packaging` | Flatpak manifest, desktop file, AppStream metadata |
| `tools` | Headless VM sessions and probes for portal behaviour |

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

Flatpak:

```sh
flatpak-builder --user --install --force-clean build-dir packaging/flatpak/com.typewhisper.TypeWhisper.json
```

The manifest lets cargo fetch crates during the build. A Flathub build will need
vendored crates instead.

## Related

- [TypeWhisper for macOS](https://github.com/TypeWhisper/typewhisper-mac)
- [TypeWhisper for Windows](https://github.com/TypeWhisper/typewhisper-win)

## License

GPLv3, see [LICENSE](LICENSE). Commercial licensing: [LICENSE-COMMERCIAL.md](LICENSE-COMMERCIAL.md).
Trademark policy: [TRADEMARK.md](TRADEMARK.md).
