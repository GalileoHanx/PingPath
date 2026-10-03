# Architecture

This is how PingPath fits together. [PLAN.md](../PLAN.md) has the goals, phases and acceptance criteria, [anti-ban-rules.md](anti-ban-rules.md) has the rules every component follows, and [decisions/](decisions/) records why things are the way they are.

## Data path

```
Game ──► Wintun adapter ──► client core ──UDP──► entry relay ──► exit relay ──► game server
         (routes only for     encrypts         forwards          decrypts,
          the game's server   once per         ciphertext,       checks allowlist,
          ranges)             packet           holds no keys     NATs to its own address
```

1. **Split tunnel.** The client adds routes to the Wintun adapter for the selected game's `server_ranges` and nothing else. All other traffic, including the game's login and matchmaking traffic outside those ranges, takes the normal route. The client never inspects processes; routing is by destination only.
2. **Encryption.** The client and the exit relay share a WireGuard-protocol session (user-space, `boringtun`). Each IP packet from the adapter is encrypted once.
3. **Entry relay.** Optional. It sits in a Gulf cloud region close to the player and forwards each encrypted packet to the exit relay for its session. It checks the session token and that the player really receives traffic at their address (a cookie), not the payload, and cannot decrypt. When the player's ISP already reaches the exit region well, the client talks to the exit directly.
4. **Exit relay.** Sits in the same region as the game server (rule 3). It decrypts, drops anything whose destination and port are not in the allowlist for the session's game and region, and sends the payload on unchanged (rule 2) from a socket of its own: a NAT in user space, with no root and no kernel routing ([ADR 005](decisions/005-exit-egress.md)). Replies come back the same way.
5. **Return path.** The exit encrypts replies and sends them to the address that last completed a WireGuard handshake and then sent data under the new keys: the entry relay the client uses, or the client itself. A copied or spoofed packet can't move it ([ADR 004](decisions/004-relay-wire-protocol.md)).

### Starting a session

```
client ── POST /v1/sessions {key, game, region} ──► control plane      (HTTPS)
       ◄── token, renewal secret, exit address and key, entry relays ──
client ── HELLO(token) ──► entry ── HELLO_ACK Verify(cookie) ──► client
client ── HELLO(token, cookie) ──► entry ──► exit ── HELLO_ACK ──► client
client ── WireGuard handshake ──► entry ──► exit ──► client
client ── game traffic ──► entry ──► exit ──► game server
```

The control plane signs a token for one game, one region and one exit, valid for 15 minutes; relays check it offline, so the control plane isn't on the data path ([ADR 006](decisions/006-control-plane.md)). Before the token runs out the client renews it for the same session and sends HELLO again; the session, its flows and the address the game server sees stay the same. The client makes a new WireGuard key for every session.

### Multipath

In Stability mode the client sends the same encrypted packet through two entry relays to the same exit. Both copies carry the same tunnel counter, so the exit's anti-replay window accepts whichever arrives first and drops the other. Return traffic is duplicated the same way, and the client drops the second copy. No separate sequence-number layer is needed. See [ADR 001](decisions/001-end-to-end-tunnel.md).

### Failure handling

The client sends a keepalive every ~250 ms: a UDP datagram to the exit's in-tunnel echo address (`100.64.0.1:7`), which the exit answers itself, so each reply proves the whole path works in both directions. If no reply arrives for 1 s:

1. switch to another healthy entry relay with the **same exit**: the game server sees the same address, so the game session survives;
2. if there is none, remove the routes and go direct. The game server now sees the player's own address; some games will need to reconnect, and the client says so.

The client also adds host routes for the relays' addresses via the physical gateway, so tunnel traffic can never loop back into the tunnel.

## Components

| Component | Path | Role | Phase |
|---|---|---|---|
| Game definitions | `games/`, `crates/games` | Which games exist, their server ranges, ports, regions and anti-cheat status. Every other component loads them through the `pingpath-games` crate | 0 |
| Diagnostics | `crates/diagnostics` | Logging setup every binary starts with, address redaction, and the `pingpath-report` diagnostic report | 0 |
| Probe CLI | `tools/probe` | Measures latency, jitter, loss and detours; tells the player where the problem is. ICMP without admin rights, TCP fallback, Valve relay lists, DNS-based ISP lookup ([ADR 003](decisions/003-probe-data-sources.md)) | 1 |
| Tunnel protocol | `crates/tunnel` | What clients and relays agree on: frames, session tokens, keys, inner IPv4/UDP packets, and the client end of a session ([ADR 004](decisions/004-relay-wire-protocol.md)) | 2 |
| Relay | `relay` | Entry and exit roles, allowlist, rate limits, relay-to-relay probing ([ADR 005](decisions/005-exit-egress.md)) | 2 |
| Control plane | `control` | Session tokens and renewal, relay registry, relay latency ([ADR 006](decisions/006-control-plane.md)); path recommendations and crowd data later | 2, 6 |
| Client core + UI | `client` | Wintun adapter, routes, path selection, watchdog, multipath, home-network doctor, UI | 3–5 |
| Dashboard | `dashboard` | Public route map, ISP comparison, relay status | 6 |
| Infrastructure | `infra` | Docker images, systemd units, Terraform for AWS and Google Cloud ([relay-operator.md](relay-operator.md)) | 2+ |

## Game definitions as the single source of truth

`games/<id>.yaml` drives three things:

- **Client:** which prefixes to route, and whether routing is allowed at all (`untested` and `unsupported`: never; `testing`: with a warning; `supported`: yes).
- **Exit relay:** the allowlist of destination prefixes and ports it will forward to.
- **Probe and dashboard:** which regions to measure, and the `ping_targets` to use when game servers drop ICMP.

CI validates every definition against `games/schema.json` and against the rules a schema can't express: canonical CIDRs, no private or reserved space, no prefix broader than /16 (IPv4) or /32 (IPv6), no overlaps, placeholder (documentation) ranges only in games that are never routed, a report file for `supported`. See [ADR 002](decisions/002-game-definitions.md).

## Logs

Every binary calls `pingpath_diagnostics::init("<component>", version)` first. Logs go to stderr and to `<log dir>/<component>.<date>.log` as JSON lines, kept 7 days; `PINGPATH_LOG` sets the level. Log addresses that could identify a person through `Redacted(addr)`. `pingpath-report` turns the log folder into one Markdown report that testers send back (PLAN.md section 9, `logs/README.md`).

Relays and the control plane never log a player's address. Relays hold it in a type that prints as `<client>` and identify sessions by ID; a test runs a session through its whole life with every log level on and fails if the address appears. The control plane uses the requester's address only to count sessions per network, kept as a keyed hash.

## Who sees what

| Party | Sees | Never sees |
|---|---|---|
| Anyone on the player's path (ISP, Wi-Fi) | The relay addresses, and the game, region and exit named in the session token, which travels in clear in HELLO; then packet sizes and timing | Destinations, payloads |
| Entry relay | The player's address, the token (so the game, region and exit), packet sizes and timing | Destinations, payloads |
| Exit relay | Destinations and payloads (game traffic, often encrypted by the game itself) | The player's address when traffic comes through an entry relay; it sees the entry relay's instead |
| Control plane | The requester's address when a session is requested (used for per-network caps, not stored or logged), the game, region and exit chosen; opt-in telemetry with truncated addresses from Phase 6 | Game traffic |
| Dashboard | Aggregates over buckets of at least 20 users | Anything per user |

Relays keep client addresses in memory only for the life of a session and never log them (rule 9).
