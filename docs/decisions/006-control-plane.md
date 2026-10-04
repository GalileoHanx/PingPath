# 006. Control plane: session issuance, renewal and the relay registry

- **Status:** Accepted
- **Date:** 2026-10-03
- **Builds on:** [004](004-relay-wire-protocol.md) (session tokens)

## Context

Relays only carry traffic for clients holding a short-lived token signed by the control plane (PLAN.md rule 10). Phase 2 needs the service that signs those tokens, tells clients which relays exist, and collects the relays' latency measurements of each other. Crowd data, path recommendations and a database arrive in Phase 6; this ADR covers what Phase 2 builds (`control/`, `pingpath-control`).

## Decision

### State

The control plane keeps sessions in memory. A restart forgets them; relays keep forwarding until each token expires, and clients ask for a new session when a renewal comes back `unknown_session`. The relay registry is the `[[relays]]` list in the config file. Nothing is written to disk except logs.

### API

JSON over HTTP, behind a TLS-terminating reverse proxy. Errors are `{"error": "<code>", "message": "..."}` with a stable code; rate limits add `Retry-After`.

| Request | Answer | Refusals |
|---|---|---|
| `POST /v1/sessions` `{client_public_key, game, region, exit_id?}` | `201` with `session_id`, `token`, `expires_at`, `renew_secret`, `renew_until`, the exit (address, WireGuard key) and every entry relay | `400 bad_client_key`, `404 unknown_game`, `404 unknown_region`, `403 game_not_routable`, `404 unknown_exit`, `409 exit_wrong_region`, `503 no_exit`, `409 client_key_in_use`, `429 rate_limited`, `429 network_limit`, `503 at_capacity` |
| `POST /v1/sessions/renew` `{session_id, renew_secret}` | `200` with a new token for the same session | `404 unknown_session`, `429 renewal_too_soon`, `410 session_ended`, `403 game_not_routable` |
| `GET /v1/relays` | every relay: ID, name, address, roles, regions, exit key, provider, location | |
| `GET /v1/latency` | the latest relay-to-relay round trip and loss (`latency`), and each exit's round trip to each routable game's region it serves (`regions`, by game and region), at most 2 minutes old | |
| `POST /v1/relay-latency` `{relay_id, samples: [{peer_id, rtt_ms, loss_pct}], regions: [{game, region, rtt_ms, loss_pct}]}` with `Authorization: Bearer <relay API key>` | `204` | `401 unauthorized`, `403 wrong_relay`, `400 bad_report` (including a region the relay doesn't serve as an exit, or a game that is never routed) |
| `GET /healthz` | `{"status": "ok"}` | |

Health details and Prometheus `/metrics` are on a separate admin address that stays private.

### Issuance rules

- The game must exist and its status must allow routing: `untested` and `unsupported` games get `game_not_routable` (rule 7), the same check every exit makes.
- The region must be one of the game's regions, and the exit must serve it (rule 3). Without `exit_id`, the control plane picks the exit serving the region with the fewest sessions it has issued.
- One session per client key. The exit refuses a key that already has a session (ADR 004), so clients make a new WireGuard key for each session.
- Per source network (/24 for IPv4, /64 for IPv6; IPv4-mapped IPv6 counts as IPv4): at most 10 new sessions a minute and 32 at once. At most 100,000 sessions in total. All configurable.
- Tokens last 15 minutes. The answer includes a 32-byte `renew_secret` (stored only as its SHA-256); renewal returns a token with the same session ID, client key, game, region and exit, so relays keep the session's flows and the game server sees no change. Renewals are at least a minute apart and stop 12 hours after the session started (`renew_until`); the last token expires exactly then.
- A session is forgotten once its last token has expired (plus the 60-second clock skew relays allow), which frees its client key and its place in the caps.

### Relay API keys

Each relay gets a random key (`pingpath-control api-key`); the control plane's config holds only its SHA-256, so the config file can't be used to impersonate a relay. A key can only report for its own relay ID, about relays in the registry, with sane values.

### Privacy

The requester's address is used for one thing: counting sessions per network. The network (never the address) is stored as an HMAC under a random per-process key, so even the in-memory table doesn't hold it. No log line holds the requester's address; issuance logs the session ID, game, region and exit, and refusals log only the reason, game and region. Behind a reverse proxy, `client_ip_header` names the header carrying the client's address, and its last entry (the one the proxy added) is used; without the setting the header is ignored, so clients can't choose the network they are counted against.

## Consequences

- A control plane restart doesn't interrupt anyone in a match: relays check tokens offline. Clients only notice at the next renewal and get a new session, which means a new handshake and possibly a new exit address at the server. Phase 6's database can make sessions survive restarts if that matters in practice.
- The control plane's view of exit load counts only the sessions it issued, not traffic; an exit that is full answers `RelayFull` and the client asks for another exit.
- Deferred, with the relay-side items in ADR 004: signed relay registry and game bundles that relays and clients fetch and reload; token revocation; proof of work or account checks before issuance if the per-network caps prove too weak; issuer and exit key rotation driven from the control plane.

## Alternatives considered

- **Long-lived tokens instead of renewal.** Simpler, but a leaked token would work for hours and a game marked unsupported would stay routed until every token ran out.
- **Renewal by the old token alone.** Anyone who saw a token (every entry relay does) could renew it; the renewal secret is only ever sent to the client over TLS.
- **Counting by full client address.** Stricter, but it would mean holding addresses; per-network caps are coarser and hold nothing identifying.
