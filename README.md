# PingPath

An open-source network tool that lowers ping, packet loss and jitter for players far from their game's servers, starting with the Gulf and the Middle East. It needs nothing from game developers or ISPs, and it never touches the game itself.

> **Status: early development (Phase 3 built).** The probe, which measures your connection, is ready to try. The relay servers, the control plane and the Windows client are built and tested on Linux, but the client hasn't run on Windows yet, no relays are deployed and there is no installer to download, so nothing changes your connection yet. See [PLAN.md](PLAN.md) for the roadmap.

## Why

Players in the Gulf commonly see 100–150+ ms to the European servers they play on. Distance accounts for only part of that: the speed of light in fiber puts Riyadh → Frankfurt at roughly 43 ms round trip. The rest comes from:

1. **Bad ISP routing:** traffic takes long detours.
2. **Congested paths:** packet loss and jitter.
3. **The home network:** Wi-Fi, bufferbloat, background downloads.

PingPath works on all three with a desktop client, a network of relay servers and a measurement system. The target, for affected players: 20–40% lower latency to their usual game region, packet loss under 0.5% and jitter under 5 ms.

## How it works

```
Game ──► PingPath client ──► entry relay (Gulf) ──► exit relay (e.g. Frankfurt) ──► game server
         routes only the       forwards encrypted      decrypts and forwards
         game's server         packets                 to allowlisted game
         addresses                                     servers only
```

- **Split tunnel.** Only traffic to the selected game's server ranges goes through PingPath; everything else is untouched.
- **Better path.** Relays on cloud backbones skip bad ISP routes. The client turns the tunnel on only when it measures faster than going direct.
- **Stability mode.** Optionally sends each packet over two paths and keeps whichever copy arrives first.
- **Home network doctor.** Spots Wi-Fi, bufferbloat and background-traffic problems and explains how to fix them.
- **Arabic and English.** The desktop app and the public dashboard come in both, with a right-to-left layout for Arabic.
- **Crowd map.** Opt-in, anonymous measurements show which ISPs and routes work best for each game and city.

<details>
<summary><b>What PingPath will never do</b></summary>

To keep players' accounts safe, PingPath works purely at the network level. It never reads or modifies game memory, files or packets, never injects into processes, uses only the signed Wintun adapter driver, never changes your matchmaking region, and fails open to your normal connection if anything breaks. A game is routed only once it has been tested against its anti-cheat. The full list is in [docs/anti-ban-rules.md](docs/anti-ban-rules.md).

</details>

## Try the probe

`pingpath-probe` measures latency, jitter and loss from your PC to each game's server regions, traces the route, names your ISP, and says in plain words whether the problem is your home network, your ISP's route, or nothing at all.

**Windows, no install:** open the latest successful run under [Actions → CI](https://github.com/GalileoHanx/PingPath/actions/workflows/ci.yml), download the `pingpath-windows-x64` artifact and unzip it. Then:

1. Double-click `pingpath-probe.exe` and wait about half a minute per game.
2. Double-click `pingpath-report.exe`, read the report it writes, and send it (see [Reporting a problem or a test result](#reporting-a-problem-or-a-test-result)).

From a terminal you get more control:

```sh
pingpath-probe list                                   # games and regions it knows
pingpath-probe run counter-strike-2 --from Riyadh     # one game; --from sets your city for the distance floor
pingpath-probe run dota-2 --region eu-central --count 50
pingpath-probe cities                                 # cities --from understands
pingpath-probe sdr-import counter-strike-2            # Valve's relay ranges for the game definition
pingpath-probe echo-server --bind 0.0.0.0:7777        # on a server: target for --udp-echo HOST:7777
```

From source, use `cargo run -p pingpath-probe -- <command>`. On Linux, ICMP needs ping sockets: `sudo sysctl -w net.ipv4.ping_group_range="0 2147483647"`.

## The desktop client

The Windows client is built (PLAN.md Phase 3) but not ready for players yet: it hasn't run on Windows, and there are no public relays for it to use. It has two parts:

- **The PingPath service** runs in the background with the rights that changing routes needs. It starts with Windows, and if anything fails it sends your game back to your normal connection by itself.
- **The PingPath app**, in Arabic and English, runs without administrator rights: pick a game and region, measure, compare your direct ping with PingPath's, and turn Optimize on. Optimize goes on only when PingPath is measurably faster. The app shows a live graph of ping, jitter and loss, and tells you in plain words when something changes. Turn Optimize on before joining a match: Windows keeps a match's connection on the route it started with.

To try it now you need a development setup:

- **On Linux**, the engine's tests run against real relays and a real control plane, and a network-namespace test checks the Phase 3 criteria on a real kernel with a real TUN device. The service and the app also run on Linux for development.
- **On Windows**, you can build the service and the app yourself and point them at a control plane and relays of your own ([docs/relay-operator.md](docs/relay-operator.md)).

[docs/client-development.md](docs/client-development.md) has the commands for both. How the client works is in [docs/architecture.md](docs/architecture.md#the-client) and ADRs [007](docs/decisions/007-client-architecture.md), [008](docs/decisions/008-split-tunnel-routing.md) and [009](docs/decisions/009-path-selection-and-failover.md).

<details>
<summary><b>Repository layout</b></summary>

| Path | Contents |
|---|---|
| [`PLAN.md`](PLAN.md) | Goals, hard rules, phases and acceptance criteria |
| [`games/`](games/) | Game definitions (YAML) and their schema |
| [`crates/games/`](crates/games/) | `pingpath-games`: loads and validates game definitions |
| [`crates/diagnostics/`](crates/diagnostics/) | `pingpath-diagnostics`: shared logging and the `pingpath-report` diagnostic report |
| [`crates/tunnel/`](crates/tunnel/) | `pingpath-tunnel`: the relay wire protocol, session tokens and the client end of a session |
| [`tools/probe/`](tools/) | `pingpath-probe`: measures the problem (Phase 1) |
| [`tools/netsim/`](tools/netsim/) | Simulated networks (Linux namespaces) for tests |
| [`relay/`](relay/) | `pingpath-relay`: entry and exit relay server (Phase 2) |
| [`control/`](control/) | `pingpath-control`: session tokens, relay registry and relay latency (Phase 2); crowd data later (Phase 6) |
| [`client/`](client/) | Windows client (Phases 3–5): the engine (`core/`), the Windows service (`service/`) and the Tauri app (`ui/`) |
| [`dashboard/`](dashboard/) | Public route map and ISP comparison (Phase 6) |
| [`infra/`](infra/) | Docker images, systemd units, Terraform for AWS and Google Cloud |
| [`docs/`](docs/) | [Architecture](docs/architecture.md), [decisions](docs/decisions/), [running servers](docs/relay-operator.md), anti-cheat reports |
| [`logs/`](logs/) | Diagnostic reports and the tester profile sent back from testing |

</details>

<details>
<summary><b>Development</b></summary>

You need stable Rust (1.85 or newer).

```sh
cargo build --workspace
cargo test --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings

# Validate game definitions (CI runs this too)
cargo run -p pingpath-games -- validate games

# Bundle recent logs into a shareable diagnostic report
cargo run -p pingpath-diagnostics --bin pingpath-report
```

Every component logs to the console and to daily files in the PingPath log folder (`%LOCALAPPDATA%\PingPath\logs` on Windows; the client's service, which runs as LocalSystem, uses `%ProgramData%\PingPath\logs`). Set `PINGPATH_LOG=debug` for more detail and `PINGPATH_LOG_DIR` to log somewhere else.

CI runs formatting and clippy on Linux and the tests on Linux and Windows, traces a route through a simulated three-hop network, runs the client engine against real relays in Linux network namespaces, checks in a release build that relays add under 1 ms, fails if any game definition is invalid, validates the Terraform, builds the Windows test binaries, the Docker images and the Linux server binaries, and for the client on Windows runs the backend tests as administrator, lints and tests the desktop app, builds the installer and checks that it installs and uninstalls cleanly. It also checks the app's translations and loads its page in both languages.

The desktop app (`client/ui`) is a cargo workspace of its own, left out of the commands above (CI checks it in jobs of its own); [docs/client-development.md](docs/client-development.md) covers it, the client's other tests and running the client.

To run relays and the control plane, see [docs/relay-operator.md](docs/relay-operator.md).

To add a game, see [games/README.md](games/README.md).

</details>

## Reporting a problem or a test result

1. Reproduce it with `PINGPATH_LOG=debug` if you can.
2. Run `pingpath-report`. It writes one Markdown file with versions, OS, warnings, errors and recent logs, with public IP addresses shortened. Read it before sharing.
3. Add a few lines on what you did, what you expected and what happened.
4. Send it: upload it in the chat, attach it to a GitHub issue (*Problem or test report* form), or commit it to [`logs/`](logs/).

Fill in [`logs/tester-profile.md`](logs/tester-profile.md) once so reports don't have to repeat your setup. Details are in [logs/README.md](logs/README.md).

## Roadmap

| Phase | Scope | Status |
|---|---|---|
| 0 | Foundations: workspace, CI, game definition schema and validator, logging and diagnostic report, docs | Done |
| 1 | Probe CLI: measure latency, loss, jitter and routing detours | Built; waiting on a real-connection test run |
| 2 | Relay server (entry/exit), control plane and session tokens, Docker and Terraform | Done; first cloud deployment pending |
| 3 | Windows client MVP: Wintun, split tunnel, path selection, fail-open; desktop app in Arabic and English | Built and tested on Linux; first runs on Windows pending |
| 4 | Multipath Stability mode | Planned |
| 5 | Home network doctor | Planned |
| 6 | Crowd map and public dashboard | Planned |
| 7 | Anti-cheat compatibility testing (ongoing) | Planned |
| 8 | Hardening and public beta | Planned |

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option. Unless you say otherwise, any contribution you submit is licensed the same way.
