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
3. **Entry relay.** Optional. It sits in a Gulf cloud region close to the player and forwards each encrypted packet to the exit relay for its session. It checks the session token, not the payload, and cannot decrypt. When the player's ISP already reaches the exit region well, the client talks to the exit directly.
4. **Exit relay.** Sits in the same region as the game server (rule 3). It decrypts, drops anything whose destination and port are not in the allowlist, NATs the packet to its own address, and sends it on unchanged (rule 2). Replies come back the same way.
5. **Return path.** The exit encrypts replies and sends them back through the entry relay (or relays) the client used.

### Multipath

In Stability mode the client sends the same encrypted packet through two entry relays to the same exit. Both copies carry the same tunnel counter, so the exit's anti-replay window accepts whichever arrives first and drops the other. Return traffic is duplicated the same way, and the client drops the second copy. No separate sequence-number layer is needed. See [ADR 001](decisions/001-end-to-end-tunnel.md).

### Failure handling

The client sends a tunnel keepalive every ~250 ms. If no reply arrives for 1 s:

1. switch to another healthy entry relay with the **same exit**: the game server sees the same address, so the game session survives;
2. if there is none, remove the routes and go direct. The game server now sees the player's own address; some games will need to reconnect, and the client says so.

The client also adds host routes for the relays' addresses via the physical gateway, so tunnel traffic can never loop back into the tunnel.

## Components

| Component | Path | Role | Phase |
|---|---|---|---|
| Game definitions | `games/`, `crates/games` | Which games exist, their server ranges, ports, regions and anti-cheat status. Every other component loads them through the `pingpath-games` crate | 0 |
| Probe CLI | `tools/probe` | Measures latency, jitter, loss and detours; tells the player where the problem is | 1 |
| Relay | `relay` | Entry and exit roles, allowlist, rate limits, relay-to-relay probing | 2 |
| Control plane | `control` | Relay registry, session tokens, path recommendations, measurement API | 2, 6 |
| Client core + UI | `client` | Wintun adapter, routes, path selection, watchdog, multipath, home-network doctor, UI | 3–5 |
| Dashboard | `dashboard` | Public route map, ISP comparison, relay status | 6 |
| Infrastructure | `infra` | Terraform and Docker for relays and services | 2+ |

## Game definitions as the single source of truth

`games/<id>.yaml` drives three things:

- **Client:** which prefixes to route, and whether routing is allowed at all (`untested` and `unsupported`: never; `testing`: with a warning; `supported`: yes).
- **Exit relay:** the allowlist of destination prefixes and ports it will forward to.
- **Probe and dashboard:** which regions to measure, and the `ping_targets` to use when game servers drop ICMP.

CI validates every definition against `games/schema.json` and against the rules a schema can't express: canonical CIDRs, no private or reserved space, no prefix broader than /16 (IPv4) or /32 (IPv6), no overlaps, placeholder (documentation) ranges only in games that are never routed, a report file for `supported`. See [ADR 002](decisions/002-game-definitions.md).

## Who sees what

| Party | Sees | Never sees |
|---|---|---|
| Entry relay | The player's address, which exit they use, packet sizes and timing | Destinations, payloads |
| Exit relay | Destinations and payloads (game traffic, often encrypted by the game itself) | The player's address when traffic comes through an entry relay; it sees the entry relay's instead |
| Control plane | Session token requests, opt-in telemetry with truncated addresses | Game traffic |
| Dashboard | Aggregates over buckets of at least 20 users | Anything per user |

Relays keep client addresses in memory only for the life of a session and never log them (rule 9).
