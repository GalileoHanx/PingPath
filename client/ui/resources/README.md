# Installer resources

`tauri.conf.json` (`bundle > resources`) ships two files from this folder
next to `PingPath.exe` in the installer. They are build outputs, not
sources, so they are not committed (see `../.gitignore`); CI puts them here
before it runs `tauri build`:

| File | Where it comes from |
|---|---|
| `pingpath-service.exe` | `cargo build --release -p pingpath-service` in the root workspace (`client/service`), copied from `target/release/` |
| `wintun.dll` | The amd64 `wintun.dll` from the signed Wintun release zip on wintun.net, checked against its pinned SHA-256 before use (docs/decisions/007-client-architecture.md) |

The installer's hooks (`../installer-hooks.nsh`) run
`pingpath-service.exe stop` before replacing the files (an upgrade over a
running service), `pingpath-service.exe install` after installing (the
installation fails if that does) and `pingpath-service.exe uninstall`
before uninstalling. The service loads `wintun.dll` by full path from the
folder it is installed in, which only administrators can change.

`cargo check`, `cargo clippy` and development runs work without these
files: `../build.rs` then leaves them out of tauri-build's view of the
configuration and prints a warning. The bundler reads `tauri.conf.json`
itself, so an installer can't be built without them.
