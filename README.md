# TypeWhisper for Linux

[![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)](LICENSE)

A native Linux app for TypeWhisper: dictate into any app with a global shortcut,
with local or cloud speech models, and transform the text with workflows. It is
written in Rust with GTK 4 and libadwaita and uses the XDG desktop portals, so it
runs on GNOME and KDE Plasma under Wayland and inside Flatpak.

**Status: early development.** The app dictates: hold or tap a global shortcut,
speak, and a local speech engine transcribes on the CPU and pastes the text into
the focused app. The engines are NVIDIA Parakeet, the default, and OpenAI Whisper
through whisper.cpp, with the model sizes of the Windows app. There are a few
settings, but no workflows yet. The groundwork comes from a [platform spike](docs/spike-2026-11.md) on GNOME 51 and
Plasma 6.8.

Terminals get Ctrl+Shift+V instead of Ctrl+V. The app recognizes them through
AT-SPI and through the app that the desktop reports as focused: the GNOME Shell
extension reports it on GNOME, and on KDE Plasma the app loads a small KWin script
while it runs. The Flatpak may not talk to KWin, so on KDE it recognizes only
terminals that show up in AT-SPI.

## Layout

| Path | Contents |
| --- | --- |
| `crates/typewhisper` | The GTK 4 and libadwaita app, the indicator overlay on KDE Plasma and other compositors with wlr-layer-shell, and the KWin script that reports the focused app |
| `crates/tw-platform` | Desktop services: microphone capture, global shortcut, pasting through the portals, plugin secrets in the desktop's keyring |
| `crates/tw-formats` | Formats shared with the macOS, Windows and iOS apps, with conformance tests against fixtures written by the Mac app |
| `crates/tw-plugin-protocol` | Wire protocol between the app and plugin processes |
| `crates/tw-plugin-host` | Finds installed plugins, starts them and calls them |
| `crates/tw-openai-transcription` | Speech to text over OpenAI-style `/audio/transcriptions` endpoints, shared by the cloud plugins |
| `crates/tw-openai-compatible` | Server profiles of the OpenAI-compatible engine and the rules its requests follow, shared by the app and the plugin |
| `plugins/parakeet` | NVIDIA Parakeet through sherpa-onnx, as a plugin process |
| `plugins/whisper-cpp` | OpenAI Whisper through whisper.cpp, as a plugin process |
| `plugins/groq` | Groq's hosted Whisper models, as a plugin process |
| `plugins/openai-compatible` | Any server with OpenAI's API (LM Studio, Ollama, vLLM, OpenAI, Azure): transcription in batch or over the Realtime WebSocket, and language models |
| `gnome-shell-extension` | Indicator overlay and focused-app reporting on GNOME |
| `packaging` | Flatpak manifest, desktop file, AppStream metadata |
| `tools` | Headless VM sessions, probes for portal behaviour, Flatpak helpers |

## Translations

The app uses the system language. Besides English it speaks German, Spanish,
Japanese and Simplified Chinese, the languages of the macOS app. The texts are
gettext catalogs in `po/`, with the English text as the message ID. After adding
or changing a text in the code, run

```sh
tools/i18n/update-translations.py path/to/typewhisper-mac/TypeWhisper/Resources/Localizable.xcstrings
```

It adds the new texts to every catalog, takes the macOS app's translation where
the English text is the same, and lists the texts that still need one. CI fails
while a translation is missing. The desktop file and the AppStream metadata are
templates in `packaging/` (`*.in`); the script reads their texts with `xgettext`,
which needs the AppStream ITS rules that Linux distributions install with
AppStream. `tools/dev/install-dev.sh` and the Flatpak build compile the catalogs
and write the translated desktop file and metadata with `msgfmt`.

## Plugins

Every plugin is its own executable with a `manifest.json`. The app starts it and
talks to it over stdin and stdout (see `crates/tw-plugin-protocol`). A plugin
crash cannot take the app down. Plugins will come from the in-app catalog, as
Flatpak extensions (`com.typewhisper.TypeWhisper.Plugin.*`) and as `.deb`/`.rpm`
packages.

A plugin that needs settings, such as the API key of a cloud service, gets them
from the app over the same connection and stores nothing itself. The app keeps
the keys in the desktop's keyring: GNOME Keyring or KWallet through the Secret
Service, and inside Flatpak a keyring file that the Secret portal unlocks.

## Build

Fedora:

```sh
sudo dnf install gcc gcc-c++ cargo rust gettext cmake clang-devel gtk4-devel gtk4-layer-shell-devel libadwaita-devel pulseaudio-libs-devel
cargo build --workspace
cargo test --workspace
cargo run -p typewhisper
```

Ubuntu 26.04: install `build-essential gettext cmake libclang-dev libgtk-4-dev libgtk4-layer-shell-dev libadwaita-1-dev libpulse-dev`
and Rust through rustup.

The Whisper plugin compiles whisper.cpp from source with CMake, and bindgen reads
its header with libclang. The build optimizes for the CPU it runs on.

To dictate with a development build, install it for your user first:

```sh
tools/dev/install-dev.sh
target/release/typewhisper
```

The script builds the release, installs a `.desktop` file that points at it and
copies the Parakeet, Whisper, Groq and OpenAI-compatible plugins to
`~/.local/share/typewhisper/plugins`. The desktop portals refuse an app ID
without a matching `.desktop` file. In the window, set up the shortcut, allow
typing (on GNOME, switch on "Allow Remote Interaction" in the dialog) and
download the speech model. Under Engine, pick Whisper and one of its models to
dictate with it instead of Parakeet, or Groq with your Groq API key. For a
server with OpenAI's API, such as LM Studio, Ollama or vLLM, add a profile
under OpenAI Compatible, then pick OpenAI Compatible and the profile under
Engine.

Flatpak:

```sh
flatpak-builder --user --install --force-clean build-dir packaging/flatpak/com.typewhisper.TypeWhisper.json
flatpak-builder --user --install --force-clean build-dir packaging/flatpak/com.typewhisper.TypeWhisper.Plugin.Parakeet.json
flatpak run com.typewhisper.TypeWhisper
```

The app defines the extension point `com.typewhisper.TypeWhisper.Plugin`, and each
plugin is its own Flatpak mounted under `/app/plugins/<name>`. The second command
builds Parakeet that way; it needs the app installed first. Whisper has no add-on
yet.

The builds have no network access, as on Flathub. flatpak-builder downloads every
source beforehand and checks its hash. Cargo takes the crates from
`packaging/flatpak/cargo-sources.json`, so after every change to `Cargo.lock` run
`tools/flatpak/update-cargo-sources.sh` and commit the result; CI fails while the
file is out of date. The same goes for the license notices of the Rust crates in
`packaging/licenses/`, which the Flatpaks install: `tools/licenses/update-crate-licenses.sh`
writes them with cargo-about 0.9.3. The add-on builds ONNX Runtime, sherpa-onnx and their C++
libraries from source. github.com/microsoft/onnxruntime returns 404, so the ONNX
Runtime source comes from the matching release on
[TypeWhisper/onnxruntime](https://github.com/TypeWhisper/onnxruntime), a
`git archive` of the upstream release commit. Its CMake dependencies are sources
under `mirror/`, where ONNX Runtime looks for local copies before it downloads
anything; when ONNX Runtime changes version, take them from its `cmake/deps.txt`.
The build joins ONNX Runtime's static libraries into one `libonnxruntime.a`, the
library the sherpa-onnx crate links.

## Related

- [TypeWhisper for macOS](https://github.com/TypeWhisper/typewhisper-mac)
- [TypeWhisper for Windows](https://github.com/TypeWhisper/typewhisper-win)

## License

GPLv3, see [LICENSE](LICENSE). Commercial licensing: [LICENSE-COMMERCIAL.md](LICENSE-COMMERCIAL.md).
Trademark policy: [TRADEMARK.md](TRADEMARK.md).
