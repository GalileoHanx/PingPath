# 007. Windows client architecture: privileged service, unprivileged UI

- **Status:** Proposed
- **Date:** 2026-10-04
- **Builds on:** [001](001-end-to-end-tunnel.md), [004](004-relay-wire-protocol.md), [006](006-control-plane.md)

## Context

PLAN.md Phase 3 builds the Windows client: a Wintun adapter, split-tunnel routes for the selected game, path selection, a fail-open watchdog, and a simple Arabic and English UI. Creating adapters and changing routes needs administrator rights; the UI does not, and a UI that runs elevated means a UAC prompt at every start and a larger attack surface. The tunnel must keep working (and fail open) whether or not the UI is open.

## Decision

### Processes

| Process | Runs as | Does |
|---|---|---|
| `pingpath-service.exe` (Windows service `PingPath`) | LocalSystem, started on demand | The engine: Wintun adapter, routes, tunnel, measurements, watchdog. Also `install`, `uninstall` and `cleanup` subcommands used by the installer |
| `PingPath.exe` (Tauri v2) | The signed-in user | The window: game list, direct vs optimized ping, the Optimize toggle, live graph, settings, diagnostic report |

The UI talks to the service over a named pipe, `\\.\pipe\pingpath`, whose security descriptor gives SYSTEM and Administrators full control and interactive users read and write, so only someone signed in at the machine can control it. The protocol is JSON lines: requests with an `id` get one response; the service also pushes events (state changes, one stats sample a second). The first message is a version handshake, so a mismatched UI and service say so instead of misbehaving.

On Linux, the same service binary runs in the foreground with a Unix socket (`/run/pingpath/service.sock`). That is for development and for the network-namespace tests; Linux is not a supported client platform.

### Crates

| Crate | Path | Contents |
|---|---|---|
| `pingpath-client-core` | `client/core` | The engine (platform-independent), the platform backends (`windows`: Wintun and IP Helper; `linux`: TUN and `ip`), the IPC types, settings |
| `pingpath-client-service` | `client/service` | The `pingpath-service` binary: Windows service and console mode, the IPC server, install/uninstall/cleanup |
| `pingpath-client-ui` | `client/ui` | The Tauri app and its static web frontend (HTML, CSS and JavaScript; no npm build step) |

The UI crate is a workspace member but is built and linted only on Windows in CI (on Linux, Tauri needs WebKitGTK, which the client never uses).

### Fail-open by construction

The Wintun adapter belongs to the service process: if the service exits or crashes, the adapter is removed and every route through it goes with it, so game traffic falls back to the normal route without anything having to run. The relay host routes the service adds sit on the physical interface and send relay addresses the way they would go anyway, so a leftover one is harmless; the service removes leftovers (and any stale adapter) when it starts and when `cleanup` runs. Within a running service the watchdog ([ADR 009](009-path-selection-and-failover.md)) removes routes when the tunnel stops answering.

### Settings and logs

- Service settings (control plane URL, improvement thresholds, verbose logging) live in `%ProgramData%\PingPath\client.toml`, changed through the UI over IPC and validated by the service.
- UI preferences (language) live in the user's app data folder.
- The service logs to `%ProgramData%\PingPath\logs`, the UI to `%LOCALAPPDATA%\PingPath\logs` (PLAN.md section 9). The "Verbose logging" switch changes the service's log level at once, without a restart. "Copy diagnostic report" builds one report from both folders, copies it to the clipboard and saves it as a file.
- An optional extra game definitions folder (`games_dir`) lets a tester add a test definition, such as the echo test target below, without rebuilding.

### Languages

Every user-facing string comes from `client/ui/frontend/i18n/{en,ar}.json`; a test fails if a key exists in one file and not the other. Arabic switches the page to `dir="rtl"`, so the layout mirrors. The app starts in Arabic when the Windows display language is Arabic and in English otherwise; Settings has a switch. Numbers use each language's own formatting through `Intl.NumberFormat`.

### Installer

The Tauri NSIS bundler builds a per-machine installer that ships `PingPath.exe`, `pingpath-service.exe` and `wintun.dll` (from wintun.net, checked against a pinned SHA-256). Its hooks run `pingpath-service install` after installing and `pingpath-service uninstall` before uninstalling, which stops the service, removes routes and the adapter, and deletes the service. CI builds the installer and runs a silent install and uninstall on a Windows runner, then checks that no service, adapter or route is left. The installer is unsigned until the project has a code-signing certificate (PLAN.md section 6 notes its lead time); the build has the signing hook ready.

### Testing the done-when criteria

- **Engine tests** run on every OS in CI without privileges: real relays and a real control plane on loopback, and a fake network that sends each test packet into the tunnel or directly according to the routes the engine installed. They cover toggling, entry failover with the same exit address at the server, fallback to direct within 2 s, routing only the game's ranges, and refusing untested and unsupported games.
- **A network-namespace test** on Linux runs the same engine with a real TUN device and real kernel routes against relays and an echo server in other namespaces.
- **A Windows test** on a CI runner creates the real Wintun adapter and routes, checks that only packets for the test prefix reach the adapter, and checks cleanup.
- **A test target for testers:** `pingpath-probe echo-server` gains a mode that reports the address it sees, and a `pingpath-probe harness` command sends a steady stream to it and reports gaps, loss and address changes, for checking toggling and failover on a real connection.

## Consequences

- The UI never runs elevated, and closing it doesn't stop an optimized session.
- Two binaries and an IPC protocol to keep in step; the version handshake makes a mismatch visible.
- The service is the security boundary: it validates every request (game IDs against the definitions, settings ranges) and never runs anything a request names.
- Linux backends exist only to test the engine against a real kernel; they are not shipped.

## Alternatives considered

- **One elevated Tauri app.** Simpler to build, but a UAC prompt at every start, the whole web UI running as administrator, and the tunnel dying with the window.
- **wireguard-nt or WinTun via WireGuard for Windows.** Ruled out by rule 4 and by needing per-path control for Phase 4 multipath (PLAN.md section 4).
