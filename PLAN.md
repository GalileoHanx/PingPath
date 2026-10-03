# PingPath — Project Plan

> A self-reliant, open-source network tool that lowers ping, packet loss and jitter for gamers who have no nearby game servers, **without game-developer or ISP involvement and without touching the game itself**.

---

## 1. Goal

Players in regions without local game servers (starting with the Gulf / Middle East) often see 100–150+ ms to the nearest servers. Distance sets a floor (e.g. Riyadh → Frankfurt is roughly 40–45 ms round trip at the speed of light in fiber), but most of the extra lag comes from:

1. Bad ISP routing (traffic takes long detours)
2. Packet loss and jitter on congested paths
3. Problems inside the player's home network (Wi-Fi, bufferbloat, background traffic)

PingPath attacks all three using only things the project controls: a desktop client, a network of relay servers, and a measurement system.

How much there is to gain depends on the game. Titles that already carry traffic over their own private backbone (e.g. Valve's Steam Datagram Relay for Counter-Strike 2 and Dota 2, Riot Direct for Riot's games) leave less to win than games whose traffic crosses the public internet straight to a dedicated server. Phase 1 measures this per game before any routing work targets it.

**Success target:** for affected users, cut average latency to their usual game region by 20–40%, and bring packet loss below 0.5% and jitter below 5 ms, compared with a direct connection.

---

## 2. Hard rules (anti-ban and scope)

The coding agent must follow these in every phase. They exist so users never risk an account ban.

1. **Network level only.** Never read or write game memory, never modify or inject into game files or processes, never hook game APIs.
2. **Never modify game packets.** Game traffic is only *encapsulated* (wrapped in a tunnel) and unwrapped unchanged at the exit relay. Payload bytes are never altered, reordered by intent, or inspected beyond headers needed for routing.
3. **No region spoofing.** Only route to the game-server region the player would normally use. The exit relay must sit in the same region as the target game server. Never use the tool to get into another region's matchmaking.
4. **Minimal drivers.** Use only the signed, widely used **Wintun** driver (from the WireGuard project) for the virtual network adapter. Do not use packet-diverting or filtering drivers such as WinDivert, and do not write custom kernel drivers.
5. **Route by destination, not by process hooking.** Decide what goes through the tunnel using destination IP ranges of game servers (split tunneling via the routing table). No process injection to detect games.
6. **Fail open.** If a relay or the tunnel fails, traffic must fall back to a working route within 1–2 seconds so the player's connection is never left broken by our tool. Fail over between entry relays first: that keeps the same exit relay, so the game server sees no change. Falling back to the direct route changes the address the game server sees, and games that tie a session to the client's address may need to reconnect. The client must say so in plain words rather than promise a seamless switch.
7. **Per-game compatibility list.** Each game has a status: `untested` (the default for new definitions, never routed), `testing` (routed with a warning while compatibility tests run), `supported` (tested against its anti-cheat, with a published report) or `unsupported` (never routed). Games whose rules forbid VPN/tunneling tools are marked `unsupported`.
8. **No external dependencies on companies.** No features that require contacting ISPs, game developers, or telecoms. Everything must work on the project's own infrastructure.
9. **Privacy by default.** Telemetry is opt-in, never stores full IP addresses, and only publishes aggregated data. Relays hold client addresses in memory only for the life of a session and never log them.
10. **No open relays.** Relays carry traffic only for clients holding a valid, short-lived session token, only to allowlisted game destinations, and within per-session rate limits. A relay that could be used to flood a game server would get every user's traffic blocklisted.

---

## 3. System overview

```
[Game on PC] → [PingPath client: Wintun adapter + route selection]
      → (UDP tunnel, encrypted end to end from client to exit relay;
         optionally duplicated over 2 entry relays)
      → [Entry relay near player, e.g. Gulf cloud region]  forwards ciphertext, holds no keys
      → (same cloud provider's backbone)
      → [Exit relay near game server, e.g. Frankfurt]      decrypts, checks allowlist, NATs
      → [Game server]
```

The entry relay is optional per path: when the player's ISP already reaches the exit region well, the client connects to the exit directly. See `docs/decisions/001-end-to-end-tunnel.md`.

Components:

| Component | Purpose |
|---|---|
| **Client (Windows first)** | Virtual adapter, split tunneling, path selection, multipath, home-network diagnostics, UI |
| **Relay server** | Lightweight Linux UDP relay. Entry role forwards encrypted packets by session without decrypting; exit role terminates the tunnel, enforces the allowlist and drops duplicate packets |
| **Control plane** | Relay registry, game definitions, path recommendations, client config |
| **Measurement service** | Collects opt-in latency data; builds the live route map |
| **Public dashboard** | Shows best routes and ISP comparisons per city and game, so players can choose for themselves |
| **Game definitions repo** | Community-maintained YAML files listing each game's server IP ranges, ports, regions and support status |

---

## 4. Tech stack (recommended)

- **Client core:** Rust (`tokio`, Wintun bindings, user-space WireGuard protocol via `boringtun`). Not `wireguard-nt`: it is a separate kernel driver (rule 4 allows only Wintun) and would not let us steer packets per path for multipath.
- **Client UI:** Tauri (Rust backend + small web UI), keeps install size small
- **Relay:** Rust, single static binary, runs on any cheap Linux VPS; config via TOML
- **Control plane & measurement API:** Rust (`axum`) or Go, PostgreSQL + TimescaleDB for time series
- **Dashboard:** static site (SvelteKit or plain TS) reading a public JSON API
- **Infra:** Docker images, Terraform for relay deployment, GitHub Actions CI
- **Game definitions:** YAML in `/games`, validated in CI by the `pingpath-games` crate: the JSON Schema plus rules a schema cannot express (valid CIDRs, no private or overly broad ranges, no placeholder ranges in routable games). Every component loads games through the same crate.

The agent may substitute equivalents if it justifies the choice in `docs/decisions/`.

---

## 5. Repository structure

```
/client          Windows client (core + Tauri UI)
/relay           Relay server
/control         Control plane + measurement API
/crates          Shared Rust libraries (game definitions, logging and diagnostics, later the tunnel protocol)
/dashboard       Public route map / ISP comparison site
/games           Game definition YAML files + schema
/infra           Terraform, Docker, deployment scripts
/logs            Diagnostic reports and the tester profile sent back from testing
/tools           Probe CLI, test harnesses, network simulators
/docs            Architecture, decisions, anti-cheat test reports
Cargo.toml       Rust workspace covering all Rust components
PLAN.md          This file
README.md        Project overview and developer quick start
```

---

## 6. Phases and milestones

Each milestone has acceptance criteria. Do not start a phase until the criteria of the phases it builds on pass; phases marked *parallel* may overlap. Every phase also meets the logging rule in section 9.

Week numbers show order, not commitments: they assume one developer working the happy path. Two items have outside lead times and should start early: a Windows code-signing certificate (needed for Phase 3's signed installer) and test accounts for anti-cheat testing (Phase 7).

### Phase 0 — Foundations (week 1)
- Monorepo setup, CI (build, lint, tests) for all components
- `games/schema.json` and 2–3 example game definitions (fields: name, server IP ranges, UDP/TCP ports, regions, anti-cheat, support status, notes), plus the `pingpath-games` validator
- `docs/architecture.md`, `docs/anti-ban-rules.md` (copy of section 2) and `docs/decisions/`
- `pingpath-diagnostics`: shared logging for every component and the `pingpath-report` diagnostic report; `logs/` folder, tester profile and issue form for sending reports back (section 9)

**Done when:** CI is green, a schema-invalid game file fails CI, and `pingpath-report` builds a report from the components' logs.

### Phase 1 — Probe CLI: measure the problem (weeks 1–2)
A command-line tool that, for a given game, measures from the user's PC:
- Latency, jitter, and packet loss to each game-server region (ICMP and UDP where allowed)
- Traceroute with hop latency, highlighting detours (e.g. traffic going via a distant country)
- User's ISP (ASN lookup) and approximate location: the city given with `--from`, otherwise the ISP's country. Automatic city detection needs a geo-IP database and moves to the control plane in Phase 6
- A verdict per region: *routing problem* (detour, or latency well above the distance floor), *home/last-mile problem* (loss or jitter already at the first hops), or *already near optimal* (including games on private backbones)
- Whether WireGuard-style UDP is throttled or blocked on the user's ISP, against `pingpath-probe echo-server` on a server we run (relays take over in Phase 2)
- Output as readable text and JSON

Game servers often drop ICMP, so direct-path latency falls back to each region's `ping_targets` from the game definition (an address for ICMP, `address:port` for TCP connection time). For games on Valve's relay network, targets come from Valve's published relay list (`sdr_app_id`, `sdr_pops`). The probe never sends game servers more traffic, or traffic to other ports, than the game itself would. Methods and data sources: `docs/decisions/003-probe-data-sources.md`.

Phase 1 also replaces the placeholder ranges in the example game definitions with verified ones, using `pingpath-probe sdr-import` run by a tester (the build environment can't reach Valve's API).

Testers get ready-to-run Windows builds of `pingpath-probe` and `pingpath-report` from each CI run.

**Done when:** running the probe on a real connection produces a correct report for at least 2 games, and the JSON validates against a published schema.

### Phase 2 — Relay server (weeks 2–4)
- UDP relay with two roles: **entry** (receives from clients) and **exit** (forwards to game servers)
- Tunnel encrypted end to end between client and exit (WireGuard protocol via `boringtun`). Entry relays forward ciphertext by session ID and never hold keys (`docs/decisions/001-end-to-end-tunnel.md`)
- **Deduplication** (needed for multipath in Phase 4): the tunnel's per-packet counter and anti-replay window drop the second copy of a duplicated packet at the exit, and at the client for return traffic
- Session tokens: the control plane issues short-lived signed tokens; relays reject sessions without one (rule 10)
- Health endpoint, metrics (Prometheus), per-session rate and bandwidth limits, and an exit-side allowlist: only destinations and ports from game definitions whose status allows routing
- Relays continuously probe each other and report relay-to-relay latency to the control plane
- Terraform module to deploy a relay to at least two cloud providers

**Done when:** a test client can send UDP through entry → exit to an echo server, with added latency from relay processing under 1 ms at the 99th percentile; the relay refuses destinations not in the allowlist; and it refuses sessions without a valid token.

### Phase 3 — Windows client MVP (weeks 4–7)
- Wintun virtual adapter; split tunneling by adding routes **only** for the selected game's server IP ranges, and only if the game's status allows routing (rule 7)
- Host routes for the relays' own addresses via the physical gateway, so tunnel traffic can never loop into the tunnel
- IPv6: route a game's IPv6 ranges too; never let its IPv6 traffic silently bypass an "optimized" session
- Path selection: pick the best entry/exit relay pair by real measured latency; compare against direct route and **only enable the tunnel if it is actually better** (configurable threshold, default ≥10% or ≥10 ms improvement)
- Fail-open watchdog driven by tunnel keepalives (e.g. every 250 ms), not game traffic, which can pause for seconds in menus and loading screens. No reply for 1 s → switch to another healthy entry relay with the same exit; if none, remove routes and fall back to direct
- Simple UI: choose game → see direct vs. optimized ping → one "Optimize" toggle → live graph of ping, jitter, loss. Warn that switching on or off mid-match changes the address the game server sees and may need a reconnect in some games
- Settings: "Verbose logging" switch and a "Copy diagnostic report" button (section 9)
- Signed installer; clean uninstall that removes all routes and the adapter

Open question to settle in an ADR: packets that reach the adapter because they match a game prefix but not the game's ports. The exit relay will drop them, so either keep prefixes narrow enough that this never matters or forward such packets directly from the client in user space.

**Done when:**
- Toggling on/off never disconnects an active session of the UDP echo test harness for more than 2 s
- Killing an entry relay mid-session moves traffic to another entry with the same exit, with no address change at the server; killing all relays triggers direct fallback within 2 s
- Only traffic to the game's IP ranges goes through the tunnel (verified by test)
- Games with status `untested` or `unsupported` are never routed (verified by test)
- Uninstall leaves no routes, adapters or services behind

### Phase 4 — Multipath for stability (weeks 7–9)
- Optional "Stability mode": send each encrypted tunnel packet over two different entry relays to the same exit; the exit's anti-replay window drops whichever copy arrives second; return path likewise. Duplication only helps where the paths really differ, so prefer entry relays on different providers
- Automatic mode: enable duplication only when measured loss > 0.5% or jitter > 10 ms
- Bandwidth guard: show the extra data used (game traffic is small, typically well under 1 Mbps)

**Done when:** in a simulated network with 3% random loss on one path (use `tc netem` in `/tools`), delivered loss with multipath is below 0.3%.

### Phase 5 — Home network doctor (weeks 8–10, parallel)
Diagnostics inside the client, each with a plain-language fix:
- Wi-Fi vs. Ethernet detection, signal strength, and 2.4 vs 5 GHz band
- **Bufferbloat test:** latency at idle vs. under load, with a grade (A–F). Load comes from project-run or self-hostable test endpoints (rule 8); count their egress in the cost model
- Processes using bandwidth in the background (per-process network usage via Windows APIs, no hooking)
- DNS response time
- Router guidance: step-by-step instructions to enable SQM/QoS, with links for common router brands (instructions only, no automatic router changes in v1)

**Done when:** each check produces a result and a fix suggestion, and the bufferbloat grade matches a reference tool within one letter grade.

### Phase 6 — Crowd map and public dashboard (weeks 10–13)
- Opt-in telemetry from client and probe CLI: game, region, city (coarse), ISP ASN, direct latency, optimized latency, loss, jitter, timestamp
- Privacy: client addresses are truncated (/24 for IPv4, /48 for IPv6) at ingestion, before ASN and city lookup, so no full IP is ever written; data aggregated; any city/ISP/game bucket with fewer than 20 users is hidden
- Dashboard pages:
  - **Route map:** per game and city, best relay path and typical improvement
  - **ISP comparison:** typical ping per ISP to each game region, so players can make their own informed choice of provider
  - **Live status:** relay health and current improvement per region
- Control plane uses crowd data to recommend starting relay pairs to new users (faster first connection)

**Done when:** dashboard shows real aggregated data, the privacy threshold is enforced by tests, and new clients receive a recommended path within 1 s of selecting a game.

### Phase 7 — Anti-cheat compatibility testing (ongoing, required before marking any game "supported")
For each game:
- Play test sessions with the tool enabled across several days on test accounts
- Record anti-cheat used (e.g. Easy Anti-Cheat, BattlEye, Vanguard, FACEIT AC, VAC) and results
- Check the game's terms of service for rules about VPNs or network tools
- Check whether the game, its anti-cheat or its platform flags or limits connections from cloud datacenter addresses (the exit relay's address)
- Check whether the game rejects game traffic from an address different from the one used to log in and matchmake (only game-server traffic is tunneled)
- Record what happens when the tunnel is switched on or off, or falls back, mid-match
- Publish a report in `docs/anticheat/<game>.md`
- Set status in the game's YAML: `untested`, `testing`, `supported`, or `unsupported`. CI requires a report for `supported` and a reason for `unsupported`

The client must refuse to route a game marked `untested` or `unsupported` and show a warning for `testing`.

### Phase 8 — Hardening and release (weeks 13–15)
- Auto-update with signed releases
- Crash reporting: crashes are always logged locally (section 9); sending crash reports automatically is opt-in
- Load test: each relay handles at least 1,000 concurrent players on a small VPS
- Documentation: user guide, relay operator guide, how to add a game definition
- Public beta

---

## 7. Relay placement (starting point)

Verify current cloud-region availability before deploying.

- **Entry relays (near players):** Gulf cloud regions (e.g. Bahrain, UAE, Saudi Arabia/Dammam/Jeddah/Riyadh, depending on provider), deployed on at least two different providers so routing differs
- **Exit relays (near game servers):** the regions Gulf players typically connect to, such as Frankfurt, Paris, Marseille, Stockholm, Istanbul, Mumbai
- Pair each entry relay with an exit relay on the **same provider**, so the middle leg rides that provider's backbone; a cross-provider pair crosses the public internet and is used only when measured faster
- Start with the cheapest instances that meet the latency target; add capacity based on dashboard usage data
- Egress pricing varies widely between providers and regions; include it in placement decisions (see section 10)

---

## 8. Testing strategy

- **Unit tests** for all packet handling, dedup, path selection and privacy aggregation
- **Game definition tests:** every file in `/games` validates, and a fixture for each rule proves an invalid file fails CI
- **Network simulation** in `/tools` using Linux `tc netem` to add delay, loss and jitter, so features are testable without real games
- **Integration test:** client → entry → exit → UDP echo server in Docker, run in CI
- **Real-world benchmark script:** compares direct vs. optimized for each game region and logs results to `docs/benchmarks/`
- **Fail-safety tests:** relay crash, network change (Wi-Fi ↔ Ethernet), sleep/resume, uninstall

---

## 9. Logs and diagnostics

Goal: any problem a tester hits can be understood from one file they send back, without a screen-share or a second round of questions.

- **One logging setup for every component.** Each binary starts logging through the `pingpath-diagnostics` crate (`tracing`): readable lines on the console, and JSON lines in a daily file `<component>.<date>.log`, kept for 7 days
- **Where:** Windows `%LOCALAPPDATA%\PingPath\logs`, macOS `~/Library/Logs/PingPath`, Linux `~/.local/state/pingpath/logs`. `PINGPATH_LOG_DIR` overrides it (relays set it to e.g. `/var/log/pingpath`)
- **How much:** `PINGPATH_LOG` sets the level (default `info`; `debug` when reproducing a problem). The client gets a "Verbose logging" switch in Phase 3
- **Every start is recorded** with component, version, OS and log level, and panics are logged before the process exits
- **What each component must log:**

  | Component | Must log |
  |---|---|
  | Probe | Each target measured and how (ICMP, UDP or `ping_targets`), results, traceroute hops, ASN lookup result, every failure with its cause |
  | Relay | Session start and end by session ID (never the client address), every dropped packet's reason (no token, not allowlisted, rate limit) as counters, health changes |
  | Control plane | Tokens issued or refused and why, relay registry changes, API errors |
  | Client | Adapter created or removed, every route added or removed, each path choice with the measurements behind it, watchdog events (keepalive loss, failover, fallback) with timings, why routing was refused (game status), settings changes |

- **Privacy:** logs stay on the machine and nothing is uploaded automatically. Addresses that could identify a person are logged through `Redacted`, which shortens public addresses to their /24 or /48 network (`203.0.113.x`); relays never log client addresses (rule 9). The report also shortens any public address that slipped through and hides the user's home folder
- **Diagnostic report:** `pingpath-report` (later also a "Copy diagnostic report" button in the client) writes one Markdown file with versions, OS, settings, warnings and errors, and the latest lines of each component's log. The tester reads it, then shares it in one of three ways: upload it in the chat, attach it to a GitHub issue using the *Problem or test report* form, or commit it to `logs/` in the repository (see `logs/README.md`)
- **Tester profile:** `logs/tester-profile.md` holds what doesn't change between tests (city, ISP, connection type, Wi-Fi or Ethernet, router, games and regions, usual in-game ping), so reports don't have to repeat it
- **Rule for every phase:** every failure a tester can hit must leave a log line that explains it, with enough context to diagnose it from the report alone. When a report turns out to lack what was needed, the fix adds the missing logging too

---

## 10. Sustainability (self-funded, no partners needed)

- Fully open source: dual-licensed MIT OR Apache-2.0 (`LICENSE-MIT`, `LICENSE-APACHE`), the Rust ecosystem convention
- Relay costs covered by optional supporter tier or donations; the core features stay free
- Relay software is easy to self-host so communities or groups of friends can run their own private relays
- Bandwidth is the main running cost. Rough model: 64 kbps each way × 3 h/day ≈ 5 GB per active player per month; every byte leaves two relays (entry and exit), so ≈ 10 GB billed egress, more with Stability mode. At typical cloud egress prices that is on the order of $0.50–$2 per active player per month. Re-check against real usage from Phase 6 and prefer providers with bundled bandwidth

---

## 11. Out of scope

- Contacting or partnering with ISPs, telecoms or game developers
- Hosting game servers, private servers or emulated servers
- Anything that modifies game clients, memory or packets
- Changing the player's matchmaking region
- Mobile and console clients (consider after v1; consoles could later use a router/PC-gateway mode)

---

## 12. Risks and open questions

| Risk | Mitigation / next step |
|---|---|
| VPN use is regulated in some target countries, and some ISPs throttle VPN-like traffic | Legal review per launch country before public beta; Phase 1 probe measures throttling |
| Games or anti-cheats flag datacenter addresses (the exit relay's) | Part of every Phase 7 test; affected games marked `unsupported` |
| Games tie a session to the client's address, so fallback or toggling mid-match forces a reconnect | Entry-relay failover keeps the exit; honest UI copy; behavior recorded per game in Phase 7 |
| Games on private backbones gain little | Phase 1 verdicts; don't promise improvements the probe can't show |
| Game prefixes shared with unrelated services (big cloud ranges) pull non-game traffic into the tunnel | Narrow prefixes from measured server addresses; validator caps prefix breadth; Phase 3 ADR on non-game packets |
| Code signing and SmartScreen reputation take time and money | Start certificate procurement in Phase 0–1 |
| Egress cost grows with users | Cost model in section 10; usage data from Phase 6 |

---

## 13. Instructions for the coding agent

1. Work phase by phase; open a GitHub issue per milestone and a PR per feature.
   - **Branches:** `main` holds finished phases. Each phase from Phase 2 on is developed on its own branch named `phase-<n>-<topic>` (e.g. `phase-2-relay`) and merged into `main` only once its done-when criteria pass. Every branch name says what it contains; no generated names.
2. Record significant design choices in `docs/decisions/NNN-title.md`.
3. Never violate section 2. If a feature seems to require breaking a rule, stop and document why instead of implementing it.
4. Prefer simple, measurable improvements: every optimization must show its effect in the benchmark or simulation tools.
5. Keep the client lightweight: idle CPU under 1% and memory under 100 MB.
6. When the user reports a problem or a test result, read their diagnostic report (in `logs/`, the chat or the GitHub issue) and `logs/tester-profile.md` before asking questions. If the report lacks what was needed, add that logging as part of the fix.

---

## Revision history

- **v5 (2026-10-03).** Phase 1 as built: location from `--from` or the ISP's country; the WireGuard throttling test runs against our own echo server; Valve relay lists and `sdr-import`; ADR 003.
- **v4 (2026-10-03).** Dual MIT OR Apache-2.0 license; branch workflow in section 13; CI checks that docs/anti-ban-rules.md matches section 2.
- **v3 (2026-10-03).** Added section 9, Logs and diagnostics: shared logging, the `pingpath-report` diagnostic report, the `logs/` folder, tester profile and issue form, and a logging rule for every phase. Former sections 9–12 are now 10–13.
- **v2 (2026-10-03).** Project named PingPath (repository previously PingLess). Review changes: end-to-end client↔exit encryption with entry relays that forward ciphertext, so multipath dedup comes from the tunnel's anti-replay window (ADR 001); `untested` status, the default for new games and never routed; rule 10 (no open relays) and relay session tokens; keepalive-driven watchdog and entry-first failover; honest wording on what fail-open can promise; Phase 1 verdicts and `ping_targets`; Phase 7 checks for datacenter addresses and address binding; same-provider relay pairs; bandwidth cost model; risks table; removed `wireguard-nt` from the stack (conflicts with rule 4).
- **v1.** Initial plan.
