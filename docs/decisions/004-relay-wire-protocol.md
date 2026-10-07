# 004. Relay wire protocol and session tokens

- **Status:** Accepted
- **Date:** 2026-10-03
- **Builds on:** [001](001-end-to-end-tunnel.md) (end-to-end tunnel, forwarding entry relays)

## Context

ADR 001 settled the shape: one WireGuard session from the client to the exit relay, entry relays that forward ciphertext without keys, and duplicates dropped by WireGuard's replay window. This ADR fixes the bytes on the wire, the session token the control plane signs (PLAN.md rule 10: no open relays), and what each party checks. The implementation lives in `crates/tunnel` (`pingpath-tunnel`), shared by the relay, the control plane and the client. A security review during Phase 2 hardened the first draft; the result is described here, and the items it deferred are listed at the end. The Phase 3 client's failover ([ADR 009](009-path-selection-and-failover.md)) brought six changes, described in place: PROBEs have a budget of their own, HELLOs from senders already proven get a larger one, HELLOs forwarded by relays in the configuration skip the budgets, relays that are both entry and exit may forward HELLOs, the exit's handshake limit grows with its session cap, and the exit keeps two answered handshake initiations pending.

## Decision

### Keys

| Key | Type | Held by | Purpose |
|---|---|---|---|
| Issuer key | Ed25519 | Control plane (private), every relay (public; a list, for rotation) | Signs session tokens |
| Exit key | X25519 | Exit relay (private, created on the machine), published in the registry | WireGuard static key |
| Client key | X25519 | Client (private), sent to the control plane (public); a new one for every session | WireGuard static key |
| Relay API key | Random 32 bytes | Relay (key), control plane (its SHA-256) | Relay reports to the control plane (HTTP bearer) |
| Cookie key | Random 32 bytes | Each entry relay, in memory | Return-routability cookies |

Keys are written as standard base64, like WireGuard keys, except the relay API key and its SHA-256, which are hex (`pingpath-control api-key` prints both).

### Session token

Binary, little-endian, signed with the issuer key:

| Field | Size | Meaning |
|---|---|---|
| version | 1 | `1` |
| session_id | 8 | Random, chosen by the control plane |
| client_key | 32 | The client's WireGuard public key |
| exit_id | 4 | Registry ID of the exit relay this session may use |
| issued_at | 8 | Unix seconds |
| expires_at | 8 | Unix seconds; 15 minutes after issue by default |
| game_len, game | 1 + 1–64 | Game ID; the exit only forwards to this game's servers |
| region_len, region | 1 + 1–64 | Game region; the exit only forwards to that region's servers, and only if it serves the region (rule 3) |
| signature | 64 | Ed25519 over `"pingpath-session-token-v1" ‖ all preceding bytes` |

The domain-separation prefix means a token signature can never be mistaken for any other signature the issuer key makes. A token is valid when the signature verifies against one of the relay's issuer keys and `issued_at − 60 s ≤ now < expires_at + 60 s` (clock skew).

**Renewal** keeps the session: the control plane issues a fresh token with the same `session_id`, `client_key`, `exit_id`, game and region and a later expiry ([ADR 006](006-control-plane.md)), and the client sends HELLO with it. Relays extend the session; anything in the new token that differs from the session it names is refused as `BadToken`. An entry extends its record as soon as it forwards the HELLO, the exit only when the HELLO reaches it, so the client counts a renewal done only when the exit acknowledges it ([ADR 009](009-path-selection-and-failover.md)). A new session would mean new NAT source ports at the exit, which game servers see as a new connection.

### Frames

Every UDP datagram between client, entry relay and exit relay is one frame:

```
offset 0   magic    0x50 ('P')
offset 1   type     u8
offset 2   body     depends on type
```

| Type | Name | Body | Direction |
|---|---|---|---|
| 1 | HELLO | session_id (8) ‖ token ‖ cookie (16, optional) | client → (entry →) exit |
| 2 | HELLO_ACK | session_id (8) ‖ status (1) ‖ cookie (16, only with `Verify`) | exit (or entry) → client |
| 3 | DATA | session_id (8) ‖ WireGuard message | both ways |
| 4 | PROBE | nonce (8) ‖ padding (≤ 56) | anyone → relay, relay → relay |
| 5 | PROBE_REPLY | the PROBE body, unchanged | relay → sender |

The token's length is read from its own game and region length fields, so a trailing cookie is unambiguous.

HELLO_ACK status:

| Code | Name | Meaning |
|---|---|---|
| 0 | Ok | Accepted; the exit already has this session's tunnel, with a confirmed return path |
| 1 | BadToken | The token verified but doesn't match the session it names, or its client key is in use by another session |
| 2 | Expired | Expired, or not valid yet |
| 3 | WrongExit | The token names an exit this relay isn't, or (at an entry) doesn't know |
| 4 | GameNotRoutable | The game's status refuses routing (rule 7), or it has no such region |
| 5 | RelayFull | Session cap reached |
| 6 | RateLimited | Reserved |
| 7 | OkNewSession | Accepted with no confirmed tunnel yet (new session, or the exit restarted): start a WireGuard handshake |
| 8 | WrongRegion | The exit doesn't serve the token's region |
| 9 | Verify | An entry asks for HELLO again with the cookie in this frame |

**Sizes.** The tunnel MTU is 1380 bytes: with the 10-byte frame header, WireGuard's 32 bytes and IPv6 outer headers, a full packet is 1470 bytes and fits a 1492-byte PPPoE link. Relays drop any frame larger than 1422 bytes (header plus the largest WireGuard message) before parsing it.

**No reply is ever larger than the frame that caused it**, so a relay can't amplify a spoofed request: HELLO_ACK (11 or 27 bytes) answers a HELLO (at least 139 bytes), PROBE_REPLY repeats the PROBE, and nothing answers unknown sessions, malformed frames or tokens that don't verify.

### Who checks what

**Every relay, before anything else:** frames over the size limit, malformed frames and unknown types are dropped and counted. HELLOs have a budget per source network (/24 for IPv4, /64 for IPv6; 20 a second by default) and a relay-wide one (2,000 a second); PROBEs have their own per-network budget (50 a second), so players measuring behind one carrier NAT don't use up each other's handshakes. A HELLO whose entry cookie is valid (at an entry), or whose token this relay already verified, draws on a per-network budget ten times larger (200 a second) and not on the relay-wide one: its sender has proved it receives at its address, or answering it costs only a lookup, and the answer is never larger than the HELLO. That covers most of a player's later HELLOs: failovers and network changes, the cookies a client fetches ahead of time from standby entries ([ADR 009](009-path-selection-and-failover.md)), and a renewal through an entry once the entry has answered its first HELLO (a new token with an aged cookie, so not proven) with a fresh cookie. Players behind one carrier NAT therefore don't use up strangers' budget. HELLOs forwarded by relays in the configuration skip these budgets, since one entry carries many players at once, and all of them move together when another entry dies. A relay that is both entry and exit forwards HELLOs like any entry, and an exit accepts them when its configuration gives that relay the entry role; a HELLO from a relay that is only an exit is dropped, since exits never send one. Tokens that verified are cached by hash, so replaying one costs a lookup, not a signature check. A token that doesn't verify is dropped **silently**; an expired one is answered `Expired` so honest clients with a stale token learn why. The HELLO's session ID must equal the token's. A relay with both roles then acts as the exit for tokens that name it and as an entry for the rest. Budgets are token buckets that allow a burst of one second's worth (`relay/src/relay.rs`; the rates in `relay/src/config.rs`).

**Entry relay** (keeps `session_id → client address, exit, client key, expiry, last exit traffic, rate limiters`):
- HELLO: the token's exit must be one it knows (else `WrongExit`). The client must then prove it receives traffic at its address: without a valid cookie the entry answers `Verify` with a 16-byte cookie, an HMAC-SHA256 of session ID, client address and port, and a 30-second window (the current and previous windows are accepted). With the cookie, it records the session, or updates the client address of an existing one whose exit and client key match (else `BadToken`) and keeps the later of the two expiries, and forwards the HELLO to the exit. A client whose address changes sends HELLO again.
- DATA from a client: forwarded to the exit only if the session exists, hasn't expired, comes from the recorded client address, and is within its rate limit.
- Frames from the session's exit (and only that exit): forwarded to the client within the session's downstream rate limit.
- A session ends when its token expires or its exit has been silent for 15 seconds (the client's keepalives would have been answered).
- It never decrypts anything and holds no WireGuard keys. Client addresses are held in a type that prints as `<client>`, so they can't reach a log line or a metric by accident.

**Exit relay** (keeps `session_id → WireGuard state, game, region, client key, expiry, return path, flows`):
- HELLO (from an entry or straight from a client): the exit must serve the token's region (`WrongRegion`), and the game must be routable with that region (`GameNotRoutable`). A new session must not reuse a client key held by another session (`BadToken`): one key per session, so a handshake recorded in one session can't be replayed into another. It answers `OkNewSession`, or for a renewal `Ok` once the tunnel is up.
- A session that gets no handshake initiation within 10 seconds is dropped, so HELLOs alone can't fill the session table. Once an initiation has arrived (it creates the tunnel), the session lasts until its token expires, whether or not the handshake completes.
- DATA: the tunnel is created on the first handshake initiation, with the token's client key as its only peer and a relay-wide handshake rate limit: above 1,000 handshakes a second (a tenth of the session cap, at least 500), `boringtun`'s cookie mechanism takes over. The limit is that high because a dead entry moves all its sessions to the same exits at once. The exit only ever answers handshakes; it never starts one. An initiation it has already answered, arriving again through another relay, is refused as a replay (its timestamp isn't newer), so only the first copy gets a response.
- **Return path.** The handshake response goes to the address the initiation came from, and that address becomes the session's return path only when it then sends data that authenticates under the new keys with the receiver index from that response. Other authenticated data from the current path keeps it fresh; nothing else moves it. A spoofed or replayed message can't redirect replies, and a recorded handshake replayed from another address leads nowhere. The exit stops sending on a path that hasn't authenticated for 5 seconds.
- **The last two answered initiations stay pending**, newest first, each with its address and receiver index (`relay/src/exit.rs`, `TunnelState`). The client accepts a response to its current or its previous initiation, and during a failover it sends one initiation every round through several relays at once, so answers to successive initiations can come back through different relays, and the client follows whichever handshake completes (ADR 009). Data confirming either pending initiation therefore moves the return path. Confirming the newest clears both, so a straggler from the older can't move the path back; confirming the older clears only it, leaving the newest free to move the path when the client follows it. With only the newest pending, the client could complete the older handshake (its answer arrived, the newer one's didn't) and confirm it to an exit that had forgotten it: the client would send through one relay while the exit kept replying on the old path.
- Decrypted traffic is forwarded only per [ADR 005](005-exit-egress.md).
- **In-tunnel echo.** UDP to `100.64.0.1:7` inside the tunnel is answered by the exit itself and never leaves it: an authenticated round trip over the whole path, for the Phase 3 watchdog.

**Duplicates** (Phase 4 multipath): both copies of a packet carry the same WireGuard counter; the replay window drops the second as `DuplicateCounter`, counted as `duplicate`. The replay check comes before decryption, so a forged copy is counted the same way.

### Limits (relay configuration, defaults)

| Limit | Default |
|---|---|
| Sessions per relay (each role) | 10,000 |
| Per session, each way | 1,000 packets/s and 2 Mbit/s |
| UDP flows per session / per relay | 8 / 20,000 |
| New flows per session | 2 a second (bursts up to the flow limit) |
| Flow idle timeout / unanswered flow timeout | 120 s / 10 s |
| HELLO per source network / HELLO per relay / PROBE per source network (`handshakes_per_second`, `hellos_per_second`, `probes_per_second`) | 20/s / 2,000/s / 50/s |
| HELLO with a valid entry cookie or an already verified token, per source network | 200/s (ten times `handshakes_per_second`) |
| WireGuard handshakes per exit before `boringtun` asks for cookies | 1,000/s (a tenth of `max_sessions`, at least 500) |
| To one game server address and port, all sessions | 20,000 packets/s and 100 Mbit/s |
| Exit session without a handshake initiation | 10 s |
| Entry session without exit traffic | 15 s |
| Stale return path | 5 s |

Session rate limits are charged only after a packet authenticates at the exit, so junk sent with a copied session ID can't spend a player's budget there. Game traffic is small; legitimate players stay far below these.

### What gets logged (PLAN.md section 9)

Relays log session start, renewal and end by session ID, never the client's address (rule 9; `relay/tests/privacy.rs` checks every log line at every level). Dropped frames are counted by reason as Prometheus metrics and summarised in the log every minute. The control plane's logging is in ADR 006.

## Consequences

- A relay can't be used without a token, and a token only reaches one exit, one game and one region's servers.
- **Tokens travel in clear.** Entry relays and anyone on the client's path can see the game, region and exit a session is for. Someone off the path never sees a token. Someone on the path who copies one can, through an entry, move the session's client address to their own (the cookie only proves they receive traffic there): the real client is cut off until it sends HELLO again, and the attacker receives ciphertext they can't read. That is no worse than dropping the client's packets, which they could already do. At the exit they get nowhere: the return path only moves after a handshake, which needs the client's private key.
- An exit can serve any client the control plane authorises without being configured per client.
- Failover at scale fits the budgets: HELLOs that entries forward skip them, a player's own repeat HELLOs draw on the larger proven budget, and the exit's handshake limit grows with its session cap. HELLOs from senders that have proven nothing are still held to 20 a second per network and 2,000 a second in all.
- The exit moves its return path only after a completed handshake through the new relay, and accepts either of the client's last two. ADR 009 describes the one order of answers in which client and exit still settle on different relays until the next failover.
- The control plane is needed to start or renew a session, not to keep one running: relays verify tokens offline.
- Version byte, frame types and status codes leave room for change; a new token version or frame type is added alongside the old one, not in place of it.

### Deferred

Found in review and left for later phases, each with what limits the risk today:

- **Proof of possession in HELLO** (a per-session signing key named in the token, so only the client can say HELLO). Today an on-path attacker with a copied token can move an entry's client address, as described above; the cookie keeps off-path attackers out.
- **Authenticating the entry ↔ exit leg** (a per-pair key tagging forwarded frames), so an exit could refuse junk earlier and attribute traffic to an entry. Today an exit treats forwarded frames and frames straight from clients alike; WireGuard authenticates everything that matters inside them.
- **A signed relay registry and game definition bundle** that relays and clients fetch and reload without a restart; today both are config files and the definitions built into the binary.
- **Token revocation**; today tokens live 15 minutes, and the control plane refuses to renew a session whose game is no longer routable.
- **Exit key IDs and rotation**, so an exit can change its WireGuard key without dropping sessions.
- **Several return paths per session** for Phase 4 multipath; today the exit returns traffic on one path and moves it as described above.
