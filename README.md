# PingPath

An open-source network tool that lowers ping, packet loss and jitter for players far from their game's servers, starting with the Gulf and the Middle East. It needs nothing from game developers or ISPs, and it never touches the game itself.

> **Status: early development (Phase 1).** The probe, which measures your connection, is ready to try. It doesn't change anything yet: no routing, no relays. See [PLAN.md](PLAN.md) for the roadmap.

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

<details>
<summary><b>Repository layout</b></summary>

| Path | Contents |
|---|---|
| [`PLAN.md`](PLAN.md) | Goals, hard rules, phases and acceptance criteria |
| [`games/`](games/) | Game definitions (YAML) and their schema |
| [`crates/games/`](crates/games/) | `pingpath-games`: loads and validates game definitions |
| [`crates/diagnostics/`](crates/diagnostics/) | `pingpath-diagnostics`: shared logging and the `pingpath-report` diagnostic report |
| [`tools/probe/`](tools/) | `pingpath-probe`: measures the problem (Phase 1) |
| [`tools/netsim/`](tools/netsim/) | Simulated networks (Linux namespaces) for tests |
| [`relay/`](relay/) | Entry/exit relay server (Phase 2) |
| [`control/`](control/) | Control plane and measurement API (Phases 2, 6) |
| [`client/`](client/) | Windows client: core and Tauri UI (Phases 3–5) |
| [`dashboard/`](dashboard/) | Public route map and ISP comparison (Phase 6) |
| [`infra/`](infra/) | Terraform and Docker (Phase 2+) |
| [`docs/`](docs/) | [Architecture](docs/architecture.md), [decisions](docs/decisions/), anti-cheat reports |
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

Every component logs to the console and to daily files in the PingPath log folder (`%LOCALAPPDATA%\PingPath\logs` on Windows). Set `PINGPATH_LOG=debug` for more detail and `PINGPATH_LOG_DIR` to log somewhere else.

CI runs formatting, clippy and tests on Linux and Windows, traces a route through a simulated three-hop network, fails if any game definition is invalid, and builds the Windows test binaries.

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
| 2 | Relay server (entry/exit), session tokens, Terraform | Next |
| 3 | Windows client MVP: Wintun, split tunnel, path selection, fail-open | Planned |
| 4 | Multipath Stability mode | Planned |
| 5 | Home network doctor | Planned |
| 6 | Crowd map and public dashboard | Planned |
| 7 | Anti-cheat compatibility testing (ongoing) | Planned |
| 8 | Hardening and public beta | Planned |

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option. Unless you say otherwise, any contribution you submit is licensed the same way.
