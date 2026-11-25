## Pull requests

Start every pull request title with its category in parentheses:

- `(feature)` new or changed functionality that users notice
- `(bug)` fixes for wrong behaviour
- `(perf)` measured speed, CPU, memory or energy improvements without a behaviour change
- `(chore)` everything else: build, CI, documentation, dependencies, refactoring

For visible UI changes, attach screenshots of the final interface in English (dark mode) to the PR description; never commit them.

## Shared formats

`crates/tw-formats` must read and write exactly what the macOS, Windows and iOS apps do, because sync and backups cross platforms. Never change a format here alone. Fixtures come from the Mac app; regenerate the identity vectors on a Mac with `swift tools/gen_vectors.swift` from `crates/tw-formats`.

## Testing without Linux hardware

The maintainer has no Linux machine. Desktop behaviour is tested in headless Lima VMs on the Mac mini (Fedora aarch64): `tw-gnome` (GNOME) and `tw-kde` (Plasma). Start a session with `tools/vm/start-gnome-session-headless.sh` or `tools/vm/start-plasma-headless.sh` inside the VM; the VMs mount only the folders they need. Drive dialogs through AT-SPI and keys through `tools/desktop-probes`. Stop the VMs (`limactl stop`) when done. x86_64 builds run in WSL2 on the maintainer's Windows machine.
