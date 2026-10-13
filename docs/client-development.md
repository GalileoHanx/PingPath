# Developing the client

How to build, run and test the PingPath client. What it is and why it is built this way: [architecture.md](architecture.md#the-client) and ADRs [007](decisions/007-client-architecture.md) (the service, the app, the backends, the installer), [008](decisions/008-split-tunnel-routing.md) (routing) and [009](decisions/009-path-selection-and-failover.md) (path selection and failover).

| Path | Crate | What |
|---|---|---|
| `client/core` | `pingpath-client-core` | The engine and the platform backends: Windows (Wintun and IP Helper), Linux (TUN and `ip`), and an in-memory network for tests |
| `client/service` | `pingpath-service` | The `pingpath-service` binary: the Windows service, the named pipe, install and uninstall; a console mode with a Unix socket on Linux |
| `client/ui` | `pingpath-client-ui` | The Tauri app. A cargo workspace of its own, so the root workspace's `cargo build`, `cargo test` and `cargo clippy` leave it out |

Players use Windows. Linux runs the same engine and service for development and for the network-namespace test, which checks the engine's routing against a real kernel.

## What you need

- Rust 1.85 or newer.
- **Linux backend and namespace test:** root, iproute2 (`ip`) and `/dev/net/tun`.
- **Windows:** an administrator account, and the amd64 `wintun.dll` (0.14, signed by WireGuard) bundled in the `wintun` 0.5.1 crate's source (`wintun/bin/amd64/wintun.dll` after `cargo fetch`), SHA-256 `e5da8447dc2c320edc0fc52fa01885c103de8c118481f683643cacc3220dafce`, which is what CI uses; the Wintun 0.14 release zip on [wintun.net](https://www.wintun.net/) has the same signed DLL in a later build.
- **The app:** Tauri 2's prerequisites. On Windows, the Visual Studio C++ build tools and the WebView2 runtime; on Linux, the WebKitGTK 4.1 development files.
- **The app's checks:** Python 3 for the translation check; Node.js with Playwright and its Chromium for the smoke test.

## Tests that need no privileges

```sh
cargo test -p pingpath-client-core    # unit tests and tests/engine.rs
cargo test -p pingpath-service        # tests/service.rs
```

Both run on any OS, and CI runs them on Linux and Windows as part of `cargo test --workspace`. The tests that need root or Windows are marked `#[ignore]`, so these commands skip them.

- **Unit tests** cover the route plan, the packet filter, path selection, the Windows backend's route decisions and its ledger (`net/route_table.rs`, kept free of system calls for this), and the Linux backend's parsing of `ip -j` output.
- **The engine tests** (`client/core/tests/engine.rs`) run real relays and a real control plane on loopback, each relay behind a proxy that adds 50 ms (entries) or 75 ms (exit) each way, so failover is tested at round trips like Gulf to Frankfurt; the test that kills the entry also loses 1% on every path. The in-memory network (`net/fake.rs`) stands in for the player's PC: a game's packets go to the adapter or out directly according to the routes the engine installed, and dropping the adapter removes its routes, as on Windows. They check Phase 3's done-when criteria: toggling keeps the echo harness connected, killing the entry moves traffic to the other entry with the same server-side address, killing every relay falls back within 2 s, only the game's range is tunnelled, and games that must not be routed never are. Also: a slower path isn't turned on, the guard removes routes from a hung engine, a short local outage keeps the path, sessions are closed, a dead exit is passed over, and a crash takes the routes with the adapter.
- **The service tests** (`client/service/tests/service.rs`) talk to the service the way the app does, over in-process streams and a Unix socket: the hello handshake, the settings players may change, two users kept from each other's sessions, and the connection and request limits.

The engine and the relays talk to the control plane over HTTP with `ureq`, which uses `ALL_PROXY`, `HTTPS_PROXY` or `HTTP_PROXY` (in either case) when one is set. If your shell sets a proxy, `NO_PROXY` must list `127.0.0.1` for the engine tests, or unset the proxy variables. `ureq` matches `NO_PROXY` entries as host names, not address ranges, so an entry like `10.0.0.0/8` doesn't help.

## The Linux backend

`client/core/src/net/linux/` gives the engine a TUN device (one `TUNSETIFF` ioctl) and kernel routes through `ip`. Every route it adds carries `proto 80`, so `ip route show proto 80` lists PingPath's routes and cleanup removes exactly those, with the adapter. The device and its routes go with the process; dropping the adapter also takes its link down, so its routes are gone at once. The adapter is called `PingPath` unless the caller picks another name (the namespace test uses `pptun0`).

It needs `CAP_NET_ADMIN`, which in practice means root. It is for development and tests only: players use Windows.

## The network-namespace test

`tools/netsim/client-netsim.sh` builds three network namespaces: the player's PC (`ppgame`), the relays with the control plane and "the internet" (`pprelays`), and the game servers (`ppserver`). The comment at the top of the script has the addresses. `client/core/tests/netns/` runs the engine there with the Linux backend, a real TUN device and kernel routes, against real relays, a real control plane and a UDP echo server. Which way a reply went is read from the address the echo server saw: the exit's (the tunnel) or the player's (direct).

```sh
# Build the test and note the binary's path ("Executable tests/netns/main.rs (target/debug/deps/netns-...)")
cargo test -p pingpath-client-core --test netns --no-run

sudo tools/netsim/client-netsim.sh up --no-direct-exit
sudo env -u ALL_PROXY -u all_proxy -u HTTPS_PROXY -u https_proxy -u HTTP_PROXY -u http_proxy \
    ip netns exec ppgame target/debug/deps/netns-<hash> --ignored --test-threads=1 --nocapture
sudo tools/netsim/client-netsim.sh down
```

- `--no-direct-exit` drops the player's packets to the exit, so the client always goes through an entry and killing that entry tests failover to the other one with the same exit.
- **Unset the proxy variables.** Inside the namespaces the relays report to the control plane at `10.2.1.30` and the engine fetches from it, all over plain HTTP through `ureq`, in the test's own process. With a proxy variable set, those requests go to a proxy the namespaces can't reach. `NO_PROXY` would have to list `10.2.1.30` itself, since a range such as `10.0.0.0/8` doesn't match it (above); unsetting the variables is simpler. CI runners set no proxy.
- The test refuses to run outside `ppgame`, so it can't change your own routing table.

It checks, in one run: a game that must never be routed is refused and nothing is routed; Optimize on, off and on again never leaves the echo harness without a reply for 2 s; host routes keep the entries on the physical network; only the game's range goes to the adapter, and another server outside it stays direct; killing the entry in use moves traffic to the other entry with the same exit and the same address at the server; a hung engine loses its game route to the guard within 2 s; killing every relay sends the game direct within 2 s; and stopping leaves no routes or adapter behind. With `--nocapture` it prints the timings. CI runs it in the `client-netns` job.

To look around while the namespaces are up, run commands in them, for example `sudo ip netns exec ppgame ip route`.

## Running the service and the app on Linux

```sh
cargo build -p pingpath-service
sudo target/debug/pingpath-service run      # until Ctrl+C
```

It keeps its files in `/var/lib/pingpath` (`--data-dir` picks another folder): settings in `client.toml`, logs in `logs/` (`PINGPATH_LOG_DIR` and `PINGPATH_LOG=debug` work as everywhere). It listens for the app on `/run/pingpath/service.sock`. Ctrl+C turns Optimize off and removes the adapter; after a crash, `sudo target/debug/pingpath-service cleanup` removes what was left.

It needs a control plane and relays ([relay-operator.md](relay-operator.md)). For ones on this machine or a private network, `client.toml` would say:

```toml
control_url = "http://127.0.0.1:8080"   # plain HTTP only to this machine; HTTPS otherwise
allow_local_relays = true               # accept relays on private or loopback addresses
```

The direct ping to a region uses the game definition's `ping_targets`; ICMP targets need ping sockets (`sudo sysctl -w net.ipv4.ping_group_range="0 2147483647"`).

The app runs on Linux too, where WebKitGTK is installed, and connects to the service's socket:

```sh
cd client/ui && cargo run
```

It logs to `~/.local/state/pingpath/logs`. `cargo run` doesn't need the installer's resources: `client/ui/build.rs` leaves them out and says so.

## Running on Windows

There is no installer to download yet, so build it yourself:

```powershell
cargo build --release -p pingpath-service
```

Put `wintun.dll` next to `pingpath-service.exe`: the service loads it by full path from its own folder and nowhere else. Then, in a terminal run as administrator:

- `pingpath-service run` runs it in the console until Ctrl+C.
- `pingpath-service install` registers and starts the Windows service at the executable's current path. The service runs as SYSTEM and loads `wintun.dll` from that folder, so first copy both files to a folder inside Program Files (for example `C:\Program Files\PingPath`), which only administrators can change; `install` refuses an executable anywhere else, and so does the installer. `pingpath-service uninstall` removes the service with its routes, its adapter and `%ProgramData%\PingPath`.

The settings are in `%ProgramData%\PingPath\client.toml`, which only administrators can change; the first run creates the folder. Set `control_url` there, then restart the service (`Restart-Service PingPath`) or `run` again. The service logs to `%ProgramData%\PingPath\logs`.

The app, from a normal terminal (it never needs elevation):

```powershell
cd client/ui
cargo run
```

It connects to `\\.\pipe\pingpath` and logs to `%LOCALAPPDATA%\PingPath\logs`.

To build the installer, put a release `pingpath-service.exe` and `wintun.dll` in `client/ui/resources/` ([its README](../client/ui/resources/README.md) says where each comes from; CI's `installer` job does the same) and run `cargo tauri build` in `client/ui` with the Tauri CLI (`cargo install tauri-cli --version "^2"`). The installer is unsigned.

## The Windows backend tests

`client/core/tests/windows_backend.rs` tests the Wintun adapter, the route table and fail-open against a real Windows. They change the machine's adapters and routes, so they are ignored by default. Run them as administrator, with `PINGPATH_WINTUN_DLL` set to the absolute path of `wintun.dll`:

```powershell
$env:PINGPATH_WINTUN_DLL = "C:\full\path\to\wintun.dll"
cargo test -p pingpath-client-core --test windows_backend -- --ignored
```

- `the_adapter_receives_only_routed_traffic`: the adapter gets no IPv6 routes beyond link-local and multicast; a packet to a routed test prefix arrives on the adapter, sent from the adapter's address, and one to an address outside it doesn't (packets are tagged, so Windows' own traffic on the adapter can't confuse the test); a reply written to the adapter, from the server to the adapter's address, reaches the sending socket, which is the way the exit's replies take; the route comes off again, and dropping the adapter removes it from Windows.
- `host_routes_follow_the_ledger`: a game route on any interface but the adapter is refused; the tunnel socket can be pinned to the physical interface and still send; a relay host route goes through the gateway `best_route` finds, with the marker metric and a ledger record; `best_route` still finds the physical route while our host route covers it, including one of ours that points elsewhere (on-link); a host route another program already provides is used and left alone; cleanup after a "crash" removes our route and leaves the default route and another program's host route alone, even when a ledger record matches that route in everything but the metric.
- `routes_vanish_when_the_service_is_killed`: a child process (`crashing_child`, which does nothing when run on its own) creates the adapter and a game route and is killed with `TerminateProcess`, as in a crash; the route must be gone within 2 s of the kill. The child is killed however the test ends, and cleanup afterwards removes its stale adapter.

They route only the benchmarking range `198.18.0.0/15` to the adapter, and add host routes for `1.1.1.1` and `1.0.0.1` through the machine's own gateway, so they need a connection to the internet. They take turns, since they share the adapter's GUID with each other and with the PingPath service: stop the service first if it is installed (`pingpath-service stop`).

CI runs them on windows-latest as administrator (job `windows-backend`), and lints the Windows-only code there (`windows-lint`). From Linux, lint it with the mingw-w64 toolchain:

```sh
rustup target add x86_64-pc-windows-gnu
cargo clippy -p pingpath-client-core -p pingpath-service --all-targets --target x86_64-pc-windows-gnu -- -D warnings
```


## The app's checks

```sh
python3 tools/check-i18n.py
NODE_PATH=/path/to/node_modules node client/ui/tests/smoke.mjs
```

- **`tools/check-i18n.py`** (standard library only) fails if `client/ui/frontend/i18n/en.json` and `ar.json` don't have the same keys, if a value is empty or a placeholder differs, if a key the page (`index.html`, `app.js`) or the Rust side (`client/ui/src`, for the tray and notifications) uses is missing, if words are written into the page instead of coming from the translation files (text or a spoken attribute in `index.html` that no `data-i18n` replaces, a string literal `app.js` puts on screen, a `t()` call whose key it can't read), or if a code the engine or the service can send has no words or isn't shown by the page. It reads those codes from the Rust source, so a new error or notice fails the check until it is translated.
- **`client/ui/tests/smoke.mjs`** loads the page in headless Chromium, served with the content security policy from `tauri.conf.json`, with a stand-in for Tauri and the service. The stand-in reads the commands, their argument names and the events from `src/main.rs` and refuses anything else, so a renamed command or event, or a camelCase argument, fails the run. It switches between English and Arabic, checks `dir="rtl"`, that no raw translation key or unfilled placeholder shows and that nothing is wider than the window, and walks the main flows: Optimize on and off, the warning for a game still being tested, another user's session, the banner when the service can't be reached. `NODE_PATH` must name the `node_modules` folder that holds `playwright`. Screenshots go to `client/ui/tests/screenshots/` (not committed).

The app's Rust code, from `client/ui`:

```sh
cargo fmt --check                     # the root workspace's fmt check doesn't reach client/ui
cargo clippy --all-targets -- -D warnings      # on Windows, or Linux with WebKitGTK
cargo clippy --all-targets --target x86_64-pc-windows-gnu -- -D warnings   # from Linux, with the mingw-w64 toolchain
cargo test                            # where Tauri builds
```

CI runs `check-i18n.py` and the smoke test on Linux (`ui-page`), `cargo fmt --check` for `client/ui` (`lint`), and clippy and the tests on Windows (`windows-lint`); run them before sending a change to the app.
