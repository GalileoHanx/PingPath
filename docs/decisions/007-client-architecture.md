# 007. Windows client architecture: privileged service, unprivileged app

- **Status:** Accepted
- **Date:** 2026-10-06
- **Builds on:** [001](001-end-to-end-tunnel.md), [004](004-relay-wire-protocol.md), [006](006-control-plane.md). Routing is in [008](008-split-tunnel-routing.md), path selection and failover in [009](009-path-selection-and-failover.md).

## Context

PLAN.md Phase 3 builds the Windows client: a Wintun adapter, routes for the selected game only, path selection, a fail-open watchdog, and a simple app in Arabic and English. Creating an adapter and changing routes needs administrator rights. The app doesn't, and an app that runs elevated means a UAC prompt at every start and a web page running as administrator. The tunnel has to keep working, and fail open, whether or not the window is open.

The proposal written before building (in this file's history) had a service started on demand, settings any signed-in user could change over the pipe, service logs in `%LOCALAPPDATA%`, and fail-open resting on the service process exiting. The Phase 3 design review found that:

- an app that isn't elevated can't start a stopped LocalSystem service;
- any signed-in user could point the SYSTEM service at another control plane or another source of game definitions;
- a pipe that grants `GENERIC_WRITE` lets any user create a rogue instance of it;
- a service that hangs instead of exiting leaves game traffic routed into an adapter nobody reads;
- a LocalSystem service's `%LOCALAPPDATA%` is the system profile, which the user can't read;
- a player with the window closed would never hear about a fallback.

This ADR describes what was built, in `client/`, and why.

## Decision

### Processes

| Process | Runs as | Does |
|---|---|---|
| `pingpath-service.exe`, the Windows service `PingPath` | LocalSystem. Starts with Windows and is restarted if it fails | The engine: adapter, routes, tunnel, measurements, watchdog. Serves the app over a named pipe. Its subcommands install, stop, uninstall and clean up |
| `PingPath.exe` (Tauri 2) | The signed-in user, never elevated | The window: games, direct against optimized ping, the Optimize switch, the live graph, notices, settings, the diagnostic report; a tray icon while Optimize is on |

Closing the window doesn't stop an optimized session: the service owns it.

### Crates

| Crate | Path | Contents |
|---|---|---|
| `pingpath-client-core` | `client/core` | The engine (`engine/`) and what it decides with (`plan.rs`, `policy.rs`, `filter.rs`, `select.rs`, `measure.rs`, `control.rs`, `stats.rs`); the settings (`config.rs`); the IPC messages (`ipc.rs`); the platform backends (`net/`): `windows` (Wintun and IP Helper), `linux` (TUN and `ip`), `fake` (in memory, for tests) and `route_table` (the Windows backend's route decisions without system calls, so every OS tests them) |
| `pingpath-service` | `client/service` | The `pingpath-service` binary: the Windows service and its subcommands, the pipe server, the data folder's permissions; on Linux, a console mode with a Unix socket |
| `pingpath-client-ui` | `client/ui` | The Tauri app and its static frontend (HTML, CSS and JavaScript; no npm, no bundler) |

`client/ui` is a cargo workspace of its own, excluded from the root one: Tauri needs WebKitGTK on Linux, which the Linux CI runners don't have and the shipped client never uses.

### The engine

The engine (`client/core/src/engine/`) is platform-independent and runs as one tokio task. The service drives it through an `EngineHandle`: measure a game and region, turn Optimize on (with the player's acknowledgement for games still being tested) or off, dismiss a notice, change settings, tell it the PC is going to sleep, shut it down. It publishes its status (mode, measurement, path, game packets carried and blocked, notices) and the live graph's samples.

At start it removes what an earlier run left behind (`Platform::cleanup`). It creates the adapter the first time Optimize goes on and keeps it while the service runs; an adapter that stopped delivering packets (disabled or removed outside PingPath) is removed, and the next Optimize creates a new one. Creating and removing the adapter block for seconds on Windows, so they run on the blocking pool.

Each optimized session is a task of its own (`engine/session.rs`) that moves packets between the adapter and the tunnel and runs the watchdog. The **guard** (`engine/guard.rs`), a plain OS thread outside the async runtime, removes the game routes if that task stops beating for a second (`failure_after`): a service that hangs, rather than exits, can't leave game traffic routed into an adapter nobody reads. The task beats on every turn of its loop, inside the failover loop, and between the steps of moving host routes after a network change (on Windows each writes the route ledger to disk), so slow but working steps aren't taken for a hang. Measuring, choosing, failover and the guard are in ADR 009.

The engine keeps the last 20 notices (fell back, failed over, address changed, not better, direct route unknown, refused, game traffic blocked, errors) until the player dismisses them, so the app shows them even if it wasn't open when they happened.

### The service

**Registration** (`client/service/src/windows/scm.rs`). The service `PingPath` runs as LocalSystem in its own process and starts automatically with Windows, because the app never elevates and a user who isn't an administrator can't start a stopped service. If it fails, Windows restarts it after 1 s, 5 s and then 30 s (the count resets after a day), also when it stops with an error rather than crashing. On Stop or Shutdown it turns Optimize off first (game routes before the relays' host routes), then removes the adapter.

**Subcommands** (`client/service/src/main.rs`). All but `service` need an elevated prompt. `--data-dir` points `run` and `cleanup` at another data folder, for development. The service that `install` registers is started with `service` alone, so it always uses `%ProgramData%\PingPath`; `install`, `uninstall` and `service` therefore refuse `--data-dir` rather than secure or delete a folder the service won't use.

| Command | Does |
|---|---|
| `install` | Secures `%ProgramData%\PingPath`, registers the service (or updates an existing registration), sets its restart actions and starts it. Waits up to 15 s for an earlier registration still "marked for deletion" by an uninstall |
| `stop` | Stops the service if it is registered, waiting up to 20 s. The installer runs it before replacing files |
| `uninstall` | Stops and deletes the service, runs `cleanup`, removes the Wintun driver (Wintun's `WintunDeleteDriver`, which keeps it while another product's Wintun adapter uses it), then deletes `%ProgramData%\PingPath`. If cleanup fails the folder is kept: it holds the route ledger a later `cleanup` needs |
| `cleanup` | Removes the relay host routes and the adapter an earlier run left behind (ADR 008). Refuses while the service runs: its live host routes are in the same ledger, and its adapter has the same GUID |
| `run` | Runs the service in the console until Ctrl+C, for development. Refuses while the service runs |
| `service` | Hidden: how the Service Control Manager starts it |

**The pipe** (`client/service/src/windows/pipe.rs`, `security.rs`). The app connects to `\\.\pipe\pingpath`, a byte-mode pipe that refuses remote clients, with this security descriptor:

```
D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x12019b;;;IU)
```

SYSTEM and Administrators have full control. Interactive users (anyone signed in at the PC, Remote Desktop included) get `0x12019b`: read, and write data and attributes, but not `FILE_CREATE_PIPE_INSTANCE`, which `GENERIC_WRITE` includes, so no user can create an instance of the pipe and answer the app in the service's place. Every instance is created with this descriptor (one created without it would get the default permissions). The first is created with `first_pipe_instance`, so the service fails loudly if another process already holds the name, and the next instance is created before an accepted connection is served, so one always listens and the name can't be taken between connections.

The app opens the pipe with `CreateFileW`, asking for exactly `FILE_GENERIC_READ | FILE_WRITE_DATA | FILE_WRITE_ATTRIBUTES`; tokio's default open asks for `GENERIC_WRITE` and is refused. It also asks for `SECURITY_IDENTIFICATION`: if the service isn't running and someone else created the pipe, they can learn who connected but never act as them.

**The protocol** (`client/core/src/ipc.rs`) is JSON lines. Requests carry an `id` and get one answer each; the service also pushes `status` and `sample` events. The first request must be `hello` with the protocol version (1 today), within 10 s. A different version gets `version_mismatch` and the connection closes, so a mismatched app and service say so instead of misbehaving. The requests are `hello`, `games`, `status`, `measure`, `enable`, `disable`, `dismiss`, `settings`, `set_settings` and `log_paths`. Every error has a stable code that the app translates: the service's own (`hello_required`, `version_mismatch`, `owned_by_another_user`, `rate_limited`, `too_many_requests`, `too_many_connections`, `invalid_settings`, `cannot_save`, `bad_request`) and the engine's (`EngineError` in `engine/types.rs`).

**Users of one PC** (`client/service/src/server.rs`). The pipe is open to every signed-in user, so the service keeps them from each other. It tells callers apart by the Windows session of the pipe's client (`GetNamedPipeClientSessionId`; the user ID on Linux).

- An optimized session belongs to the user who turned it on. While it is connecting, optimized or failing over, only they may measure or turn Optimize on or off; and while a session, current or last, has an owner, only they may dismiss notices. Anyone else gets `owned_by_another_user`.
- Every status carries `owner`: `"you"`, `"another_user"` or `null`, so the app can say why its switch is disabled.
- When the owner signs out, their session is turned off and belongs to nobody.

**Limits**, so one user can't starve the service: 16 connections in all and 4 per user; 4 slow requests (measure, enable, disable) in flight per connection; 20 requests a second per connection on average with bursts of 40, and a connection that keeps ignoring that is closed; request lines over 64 KiB are refused before they are read whole. At most 256 answers and events wait for one connection; beyond that its live events are dropped. A connection that falls more than 256 events behind the engine gets the current status in place of those it missed. A connection refused for the connection limits gets one line saying why (`too_many_connections`), and its pipe instance stays open until the app has read it and hung up, for at most a second: closing at once would throw the line away unread.

**Settings** (`client/core/src/config.rs`) live in `%ProgramData%\PingPath\client.toml`, which only administrators can change. They are validated when loaded and written atomically (a temporary file, then a rename). Over the pipe a player may change only what concerns them: verbose logging and the two improvement thresholds. The rest is set in the file by an administrator and read when the service starts; the app shows the control plane URL read-only. The SYSTEM service trusts what these settings name, so a signed-in user must not be able to point it at another control plane or another source of game definitions.

| Setting | Default | |
|---|---|---|
| `control_url` | empty | HTTPS. Plain HTTP only to `127.0.0.1`, `localhost` or `[::1]`, decided on the parsed host, and never with a user name or password: the renewal secret travels in the request |
| `games_dir` | none | Extra game definitions (a tester's echo target, a game being tested). Must resolve to a folder inside the data folder. They can add games but never replace a built-in one |
| `adapter_address` | `192.0.0.8` | ADR 008 |
| `min_improvement_ms`, `min_improvement_pct` | 10 ms, 10% | Optimize only when better by either (ADR 009). Players may change them, within 0 to 1,000 ms and 0 to 100% |
| `verbose_logging` | off | Players may change it; it applies at once, without a restart |
| `echo_interval`, `failure_after`, `failover_budget` | 250 ms, 1 s, 700 ms | The watchdog's timings (ADR 009), within validated ranges |
| `allow_local_relays` | off | Accept relays on private or loopback addresses: development and tests only |

A settings file that can't be used is logged and the defaults used, so the app can say the service isn't set up instead of the service restarting in a loop. A change that was applied but couldn't be saved answers `cannot_save`.

**The data folder** (`client/service/src/paths.rs`, `windows/security.rs`):

```
%ProgramData%\PingPath\     SYSTEM and Administrators only
    client.toml             settings
    state\routes.json       the relay host route ledger (ADR 008)
    logs\                   the service's logs; Users may read them too
```

plus the `games_dir` folder, if one is set. The folder's permissions are `D:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)`, inherited by everything inside and nothing inherited from `C:\ProgramData`; `logs\` adds read for Users (`(A;OICI;FR;;;BU)`) so the app's diagnostic report can include the service's logs. Any user may create folders in `C:\ProgramData`, so a `PingPath` folder that is already there and isn't owned by SYSTEM, Administrators or TrustedInstaller, or is a link, is moved aside (`PingPath.untrusted-<time>`) and a new one is created with the right permissions from the start. `install` and every start of the service do this, before logging starts, so log files inherit the logs' permissions.

**Logs.** The service logs to `%ProgramData%\PingPath\logs` (`service.<date>.log`). `PINGPATH_LOG_DIR` still overrides it, for development. The app logs to the user's own `%LOCALAPPDATA%\PingPath\logs` (`ui.<date>.log`). "Verbose logging" changes the service's level at once, through the level handle `pingpath-diagnostics` gives each component, and is saved in `client.toml`.

**Sleep.** Windows tells the service before the PC suspends, and the engine falls back cleanly (game routes removed, the session closed, a notice) rather than leave routes to relays the PC won't reach when it wakes. Nothing is turned back on at resume (ADR 009).

### The Windows backend

`client/core/src/net/windows/` implements the engine's `Platform` trait with Wintun and IP Helper. The routes it adds, and how it recognises them, are in ADR 008.

**Wintun bindings: the `wintun` crate, 0.5.1.** Of the two maintained bindings, it builds without warnings on the MSRV (1.85) and on current stable, and cargo reports no future incompatibility for it. `wintun-bindings` 0.7.33 and later depend on libloading 0.9, which needs Rust 1.88; 0.7.32 and earlier declare the same `extern "stdcall"` functions as `wintun` and bring a third version of windows-sys. The catch: rustc is phasing out `stdcall` on x86-64 (rust-lang/rust#137018). When that becomes an error, or the MSRV reaches 1.88, the backend moves to `wintun-bindings`, whose API is close. The crate's helpers that run `netsh` are not used; IP Helper sets the address, MTU and metric.

**Loading wintun.dll** (`dll.rs`). The installer puts the WireGuard project's signed `wintun.dll` (Wintun 0.14, amd64) next to `pingpath-service.exe` in the install folder, which only administrators can change. The service runs as LocalSystem, so a DLL found through the standard search order (the current folder, `PATH`) would run with full rights. So `wintun.dll` is loaded only by its absolute path, after `SetDefaultDllDirectories` has narrowed the process's DLL search to System32 and the application's folder. A missing DLL and one built for another processor fail with a message that says to reinstall; one that lacks a Wintun 0.14 function fails with a message that says it isn't Wintun 0.14.

**The adapter** (`adapter.rs`):

- Named `PingPath` and created by the service (never opened) with a fixed GUID, `{066C8B67-BABF-4B6D-8AC5-666510895AAB}`: Windows keeps one network profile for it instead of "Network 2, 3, ..." on every run, and the GUID is how crash cleanup finds it. After creating it, the backend checks that the adapter it got has that GUID.
- IPv4: the adapter's address (ADR 008) as a /32, the tunnel MTU, a fixed interface metric of 9000, and duplicate address detection off, set before the address goes on so the address is usable at once. Creating the adapter waits until Windows says the address is usable, for at most 5 s (then it logs why the first packets may be lost), and fails if another interface already holds the address.
- IPv6: router discovery off, default routes refused, and the routes Windows gives every interface removed, so no IPv6 traffic has a way in.
- Its session has a 4 MiB ring each way. One blocking thread reads packets, frees each one's slot in the ring at once and hands a copy to the engine, dropping it if the engine is behind (a late game packet is worse than a lost one); an async task writes packets to the ring.
- Dropping it wakes and joins the reader, ends the session and closes the adapter, which removes it.

**Fail-open when the process dies.** The adapter belongs to the service process. On Windows 10 and later, Wintun's device ends with the process that created it, and Windows removes every route through the adapter with it, so a crashed or killed service leaves game traffic going direct without anything having to run. Windows may still list an adapter left that way; Wintun treats it as orphaned and removes it when an adapter with the same GUID is created, which both `cleanup` and the next Optimize do. The relays' host routes stay behind, which is harmless: they send each relay the way it would go anyway, and the next start removes them (ADR 008). A Windows test kills a process holding the adapter and a game route and checks the route is gone within 2 s.

**Sockets** (`socket.rs`). The engine hands its tunnel sockets to the backend, which pins them to the physical interface (`IP_UNICAST_IF`) and turns off `SIO_UDP_CONNRESET`, so an ICMP error from a relay that restarted doesn't surface as a receive error (ADR 008 and 009).

Every adapter creation and removal and every route change is logged (PLAN.md section 9). Relay and game server addresses are logged as the route prefixes they are, the player's gateway goes through `Redacted`, and the player's own addresses are never logged.

### The Linux backend

`client/core/src/net/linux/` gives the same engine a TUN device, opened with one `TUNSETIFF` ioctl, and kernel routes through iproute2's `ip`, marked with protocol 80. As with Wintun, the device and every route through it go with the process, and dropping the adapter also takes its link down, so its routes are gone when the drop returns.

Linux runs the client for development and for the network-namespace test (`client/core/tests/netns/`), which checks the engine's routing against a real kernel. It is not a platform PingPath supports for players. There the service runs in a console with `run`, as root; listens on `/run/pingpath/service.sock`, which any local user may connect to (told apart by user ID, as any signed-in user may connect on Windows); keeps its files in `/var/lib/pingpath`; and removes its socket however it stops. [client-development.md](../client-development.md) shows how to run it.

### The app

`client/ui` is a Tauri 2 app (`PingPath.exe`, identifier `net.pingpath.client`). It runs as the signed-in user and never elevates: everything that needs rights happens in the service. There is one window per signed-in user; starting PingPath again brings it back.

- **The page** is static HTML, CSS and JavaScript (`client/ui/frontend/`). Its content security policy starts from `default-src 'none'` and allows only the page's own files and Tauri's IPC. The page may call the app's own commands and listen to its events, nothing else; the clipboard, notifications and the tray are used from Rust.
- **The service connection** (`src/service.rs`, `src/transport.rs`) is one connection, reopened with backoff (250 ms, growing to 5 s, and longer after an access-denied or version answer) while the service can't be reached, and the page then says why (not running, access denied, another version, too many connections, ...) and shows the Optimize switch's state as unknown rather than off. The backoff starts over only after a connection that lasted 10 s, so a service that answers and then hangs up isn't reconnected four times a second. Its I/O is blocking, on a thread that never waits inside a read: a Windows pipe handle opened without overlapped I/O serialises its reads and writes, so a waiting read would hold back the next request.
- **What the player sees:** the games, with those that can't be routed greyed out and the reason; a game's regions, with those the player doesn't normally use marked after measuring (rule 3); direct and estimated optimized ping side by side; the Optimize switch with its note, "turn it on before joining a match" (ADR 008); a one-time warning for games still being tested; a live graph of ping, jitter and loss; notices in plain words; the number of game packets carried, with a hint to rejoin the match while it stays at zero; and Settings: language, verbose logging, the improvement thresholds, the control plane URL (read-only), "Copy diagnostic report" and the versions. While another user's session runs, the switch is disabled and says why.
- **Arabic and English.** Every user-facing string comes from `frontend/i18n/en.json` and `ar.json`, the tray's and the notifications' included. Arabic sets `dir="rtl"` and `lang="ar"`, and the layout mirrors through logical CSS properties; numbers and times are formatted for the language with `Intl`. The app follows the Windows display language (Arabic when it starts with `ar`, English otherwise), with a switch in Settings. The language, and the games whose warning the player accepted, are kept per user in `%APPDATA%\net.pingpath.client\preferences.json`.
- **The tray.** While the player's own session is on, closing the window hides it to the notification area instead; a notification says so the first time. The icon's menu opens the window, turns Optimize off or quits. Quitting closes only the window: the session belongs to the service and keeps running. While the window is hidden, a fallback or an address change raises a Windows notification, and so does losing the service during the player's own session: a service that stopped or crashed took the session with it and can't raise a notice about that itself. Notices are remembered by ID and time, so a reconnect doesn't notify the same one twice. A player who quit the window sees the notices the next time it opens.
- **"Copy diagnostic report"** asks the service where its logs are (`%ProgramData%\PingPath\logs` if it can't be reached, which is often when a report is needed), builds one report with `pingpath-diagnostics`, with a part for the service's logs and one for the app's, copies it to the clipboard and saves it in `Documents\PingPath`.

`tools/check-i18n.py` fails if the two languages' keys differ, if a key the page or the Rust side uses is missing, if words are written into the page or `app.js` instead of coming from the translation files, or if a code the service or the engine can send (read from the Rust source) has no words or isn't shown by the page. `client/ui/tests/smoke.mjs` loads the page headless, with the real content security policy and a stand-in for the service, in both languages; the stand-in accepts only the commands, argument names and events `src/main.rs` has, so a rename on either side fails it.

### The installer

Tauri's NSIS bundler builds a per-machine installer, in English and Arabic, that ships `PingPath.exe`, `pingpath-service.exe` and `wintun.dll`, and embeds Microsoft's WebView2 bootstrapper for PCs that lack the WebView2 runtime. Its hooks (`client/ui/installer-hooks.nsh`):

| When | Does |
|---|---|
| Before installing over an existing copy | Asks the player to close the PingPath window (Tauri does it only after the hooks, by which time the service would already be stopped), then runs `pingpath-service stop` so the files can be replaced. If the service won't stop: Retry or Cancel, rather than failing later on a locked file |
| After installing | `pingpath-service install`. If it fails, the installation fails visibly, with the service's own output in the details: PingPath can't work without its service |
| Before uninstalling | Closes the window, then `pingpath-service uninstall`: the service, leftover routes and the adapter, and `%ProgramData%\PingPath`. If that fails, the player is asked whether to remove the rest anyway. An uninstall run by Tauri's updater only stops the service |
| After uninstalling, with "Delete the application data" ticked | Also deletes the app's logs in `%LOCALAPPDATA%\PingPath`, which Tauri's own clean-up misses |

A silent install or uninstall that hits one of these failures exits with code 2. On an upgrade, Tauri's installer offers to uninstall the old version first (the default), which removes the service and `%ProgramData%\PingPath`, `client.toml` included, and installs afresh; declining keeps them, and `install` updates the registration. The hooks were checked by compiling them into tauri-bundler 2.9's and 2.10's NSIS templates with makensis.

The installer is **unsigned until the project has a code-signing certificate** (PLAN.md section 6 notes its lead time), so Windows may warn that it is unrecognised.

### Testing

| Test | Runs | Checks |
|---|---|---|
| Engine tests, `client/core/tests/engine.rs` | Any OS, no privileges; CI on Linux and Windows | Real relays and a real control plane on loopback, each relay behind a proxy adding 50 ms (entries) or 75 ms (exit) each way, and 1% loss on every path in the test that kills the entry, with the in-memory network as the player's PC: toggling keeps the echo harness connected, killing the entry moves traffic to the other entry with the same server-side address, killing every relay falls back within 2 s, only the game's range is tunnelled, games that must not be routed never are, a slower path isn't turned on, the guard, a short local outage keeping the path, sessions closed, a dead exit passed over, a crash taking the routes with the adapter |
| Service tests, `client/service/tests/service.rs` | Any OS; CI | The hello handshake, what players may change, two users kept from each other's sessions, the limits, the Unix socket |
| Network-namespace test, `client/core/tests/netns/` | Linux, as root; CI job `client-netns` | The engine with the Linux backend against real relays, a control plane and an echo server, each in a namespace from `tools/netsim/client-netsim.sh`: Phase 3's done-when criteria on a real kernel |
| Windows backend tests, `client/core/tests/windows_backend.rs` | Windows, as administrator, with `PINGPATH_WINTUN_DLL`; ignored by default; CI job `windows-backend` | The adapter receives only routed traffic, sent from its address, and a reply written to it reaches the game's socket; it has no IPv6 routes beyond link-local and multicast; game routes go on no other interface; host routes follow the ledger, `best_route` sets ours aside, and cleanup leaves other routes alone; routes vanish within 2 s of the process being killed |
| `tools/check-i18n.py`, `client/ui/tests/smoke.mjs` | Python 3; Node.js with Playwright; CI job `ui-page` | The translations; the page in both languages |

What CI runs (`.github/workflows/ci.yml`): formatting for both cargo workspaces and clippy for the root one on Linux; the root workspace's tests on Linux and on windows-latest; the network-namespace test (`client-netns`); `check-i18n.py` and the smoke test (`ui-page`); clippy on Windows for the engine, the service and the app, and the app's unit tests (`windows-lint`); the Windows backend tests as administrator, with the amd64 `wintun.dll` (0.14, signed by WireGuard) bundled in the `wintun` 0.5.1 crate's source (`wintun/bin/amd64/wintun.dll` after `cargo fetch`), SHA-256 `e5da8447dc2c320edc0fc52fa01885c103de8c118481f683643cacc3220dafce` checked before use (`windows-backend`); a release build of the service and the NSIS installer (`installer`); and on a fresh runner `tools/check-installer.ps1` (`installer-check`), which installs, leaves a relay host route recorded in the ledger as a crash would, installs again over the running service (which must restart and remove that route), leaves another, uninstalls, and fails unless the service, the adapter, PingPath's routes, `%ProgramData%\PingPath`, the install folder and its registry entries are gone and the route table matches a snapshot taken before. It never creates the Wintun adapter, which needs a session through real relays; the adapter's removal on a crash (the game route was gone 126 ms after the kill on the first run) and the driver's removal at uninstall are covered by `windows-backend`. [client-development.md](../client-development.md) has the commands.

## Consequences

- The app never runs elevated, and closing it doesn't stop an optimized session; the tray keeps the session within reach, and notices wait until the player dismisses them.
- The service is the security boundary. It validates every request (game IDs against the definitions, settings against their ranges), never runs anything a request names, and takes the settings that say where it connects and what it routes only from a file administrators control. Changing the control plane or adding a test definition means editing `client.toml` as an administrator and restarting the service.
- A crash takes the adapter and the game routes with the process, and a hang is covered by the guard thread; neither needs the app.
- Two binaries and a protocol to keep in step. The hello handshake makes a mismatch visible, and `check-i18n.py` makes sure every code either side sends has words in both languages.
- The Linux backend exists only to test the engine against a real kernel; it is not shipped.
- **Known limits of what is built:**
  - The app doesn't start by itself at sign-in. A player who quit the window learns of a fallback only when they open it again; the fallback itself has already sent the game direct.
  - An upgrade that uninstalls the old version first (the installer's default) loses `client.toml`, so an administrator's settings must be set again.
  - If `wintun` 0.5.1 creates the adapter but then fails to look it up by name, it returns the error without closing the new adapter's handle, so that adapter, with no routes, stays until the service stops.
  - Until the Windows CI jobs above have passed, nothing here has run on Windows: the backend, the service, the app and the installer's hooks were compiled and linted (the app by cross-compiling, the hooks with makensis).

## Alternatives considered

- **One elevated Tauri app.** Simpler, but a UAC prompt at every start, the whole web page running as administrator, and the tunnel dying with the window.
- **A service started on demand** (the proposal). An app that isn't elevated can't start a stopped LocalSystem service, so the first click would fail.
- **Settings changed over the pipe** (the proposal). Any signed-in user could have pointed the SYSTEM service at their own control plane or game definitions.
- **Service logs in `%LOCALAPPDATA%`** (the proposal, and PLAN.md section 9's default). For LocalSystem that is the system profile, which the app can't read for its report.
- **Fail-open only through the process exiting** (the proposal). A service that hangs keeps its routes; hence the guard thread.
- **wireguard-nt, or Wintun through WireGuard for Windows.** Ruled out by rule 4, and they give no per-path control for Phase 4's multipath (PLAN.md section 4).
- **`wintun-bindings`.** See above: it can't build on the MSRV yet.
