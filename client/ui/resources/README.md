# Installer resources

`tauri.conf.json` (`bundle > resources`) ships two files from this folder
next to `PingPath.exe` in the installer. They are build outputs, not
sources, so they are not committed (see `../.gitignore`); CI puts them here
before it runs `tauri build`:

| File | Where it comes from |
|---|---|
| `pingpath-service.exe` | `cargo build --release -p pingpath-service` in the root workspace (`client/service`), copied from `target/release/` |
| `wintun.dll` | The amd64 `wintun.dll` (0.14, signed by WireGuard) bundled in the `wintun` 0.5.1 crate's source (`wintun/bin/amd64/wintun.dll` after `cargo fetch`), SHA-256 `e5da8447dc2c320edc0fc52fa01885c103de8c118481f683643cacc3220dafce`; CI checks the hash before copying it here (docs/decisions/007-client-architecture.md) |

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
