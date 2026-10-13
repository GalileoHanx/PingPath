# 006. Control plane: session issuance, renewal and the relay registry

- **Status:** Accepted
- **Date:** 2026-10-03
- **Builds on:** [004](004-relay-wire-protocol.md) (session tokens)

## Context

Relays only carry traffic for clients holding a short-lived token signed by the control plane (PLAN.md rule 10). Phase 2 needs the service that signs those tokens, tells clients which relays exist, and collects the relays' latency measurements of each other. Crowd data, path recommendations and a database arrive in Phase 6. This ADR covers what is built (`control/`, `pingpath-control`): the Phase 2 service, and what the Phase 3 client needed from it ([ADR 009](009-path-selection-and-failover.md)): exits' round trips to their game regions, renewals that survive a restart, a way to close a session, latency reports that carry probe counts and are judged sample by sample, and a check of each exit's WireGuard key against the registry. The code is in `control/src/state.rs` (the rules) and `control/src/api.rs` (the HTTP side).

## Decision

### State

Nothing is written to disk except logs. Sessions live in memory, but a restart doesn't strand them: each session's renewal secret is computed from the session under a key derived from the issuer seed, and a renewal carries the current token and `renew_until`, so a restarted control plane can check both and take the session back (Renewal, below). The relay registry is the `[[relays]]` list in the config file. The latency rounds and the list of exits whose key doesn't match are in memory too; they come back with the relays' next reports, within about 10 seconds, and the rows have their full minute of rounds again a minute later.

### API

JSON over HTTP, behind a TLS-terminating reverse proxy. Requests are JSON objects of at most 16 KiB with no unknown fields; anything else is refused as `invalid_request`. Errors are `{"error": "<code>", "message": "..."}` with a stable code; refusals worth retrying later (`rate_limited`, `network_limit`, `at_capacity`, `renewal_too_soon`) add `Retry-After`.

| Request | Answer | Refusals |
|---|---|---|
| `POST /v1/sessions` `{client_public_key, game, region, exit_id?}` | `201` with `session_id`, `token`, `issued_at`, `expires_at`, `renew_until`, `renew_secret`, `game`, `region`, the `exit` (as `/v1/relays` lists it) and every entry relay (`entries`) | `400 bad_client_key`, `404 unknown_game`, `403 game_not_routable`, `404 unknown_region`, `404 unknown_exit` (also an exit whose key doesn't match), `409 exit_wrong_region`, `503 no_exit`, `409 client_key_in_use`, `503 at_capacity`, `429 network_limit`, `429 rate_limited` |
| `POST /v1/sessions/renew` `{session_id, renew_secret, token, renew_until}`: the session's current token and its `renew_until` | `200` with a new token for the same session, in the same shape without `renew_secret` | `404 unknown_session`, `410 session_ended`, `429 renewal_too_soon`, `403 game_not_routable` |
| `POST /v1/sessions/close` `{session_id, renew_secret}` | `204`, also for a session the control plane doesn't hold (it is over already) | `404 unknown_session` (the secret isn't the session's) |
| `GET /v1/relays` | `relays`: every relay with `id`, `name`, `address`, `roles`, `regions`, `public_key` (exits' WireGuard key), `provider`, `location`. An exit whose reported key doesn't match the registry is listed without a key, so clients never use it as an exit (one that is also an entry is still used as an entry) | |
| `GET /v1/latency` | `latency` rows (`relay_id`, `peer_id`) and `regions` rows (`relay_id`, `game`, `region`: an exit's round trip to a routable game's region it serves), each with `rtt_ms`, `loss_pct`, `samples` and `age_secs`, pooled over the measurement's last six rounds (Latency reports, below) | |
| `POST /v1/relay-latency` `{relay_id, public_key?, samples: [{peer_id, rtt_ms, loss_pct, sent, received}], regions: [{game, region, rtt_ms, loss_pct, sent, received}]}` with `Authorization: Bearer <relay API key>` | `200` with `ignored`: the samples not stored, each with its `peer_id`, or `game` and `region`, and a `reason` (a key mismatch has only its `reason`) | `401 unauthorized`, `403 wrong_relay`, `400 bad_report` (more than 1,024 samples or region samples; in practice the 16 KiB request limit, about 160 to 240 samples, comes first as `invalid_request`) |
| `GET /healthz` | `{"status": "ok"}` | |

Health details (`/healthz` there adds the version, uptime, sessions held and relay count) and Prometheus `/metrics` are on a separate admin address that stays private.

### Issuance rules

- The game must exist and its status must allow routing: `untested` and `unsupported` games get `game_not_routable` (rule 7), the same check every exit makes.
- The region must be one of the game's regions, and the exit must serve it (rule 3). Without `exit_id`, the control plane picks the exit serving the region with the fewest sessions it holds. An exit whose reported key doesn't match the registry is never picked, and naming it gets `unknown_exit`: a session on it could never complete a handshake.
- One session per client key. The exit refuses a key that already has a session (ADR 004), so clients make a new WireGuard key for each session.
- Per source network (/24 for IPv4, /64 for IPv6; IPv4-mapped IPv6 counts as IPv4): at most 10 new sessions in each clock minute (`Retry-After` is the rest of that minute) and 32 at once. At most 100,000 sessions in total. All configurable (`[sessions]`).
- Tokens last 15 minutes. The answer to `POST /v1/sessions` includes the 32-byte `renew_secret`, the only time it is sent.
- A session is forgotten when the client closes it, or once its last token has expired (plus the 60-second clock skew relays allow; expired sessions are swept before an issue or renewal, at most every 10 seconds). Either way its client key and its place in the caps are freed.

### Renewal

- **The secret.** `renew_secret` is an HMAC-SHA256 over the session ID, client key, exit ID, `renew_until`, game and region, under a key derived from the issuer seed (the SHA-256 of a fixed label and the seed). It is never stored: the control plane computes it again to check one. Nothing relays see is enough to compute it, and since `renew_until` is part of it, a client can't stretch its session by claiming a later one.
- **A renewal** presents the secret and returns a token with the same session ID, client key, game, region and exit, so relays keep the session's flows and the game server sees no change. Renewals are at least a minute apart (`renewal_too_soon`). Each token lasts 15 minutes but never past `renew_until`, 12 hours after the session started, so the last token expires exactly then; from a minute before it the answer is `session_ended`. A game that is no longer routable gets `game_not_routable`, so a game marked `unsupported` in the control plane's definitions stops being routed within one token lifetime.
- **After a restart** the control plane doesn't hold the session, so it takes it back from the request: the token must be the session's, verify against the control plane's own issuer key and still be valid (with the 60-second skew), the secret must be the one computed from the token's claims and the given `renew_until`, and no other session may hold the client key. Then the session is recorded again and renewed as usual. It counts against the requester's network again but is never refused for that network's limits, or for the overall cap: the player is in a match. Anything else is `unknown_session`, which clients take as final (ADR 009); a changed issuer seed means no session can be taken back.

### Closing

`POST /v1/sessions/close` with the session ID and the renewal secret removes the session at once, freeing its client key and its place in its network's and its exit's counts. The client closes every session it stops using: turned off, fallen back, not better, or an attempt that never connected (ADR 009). Closing a session the control plane doesn't hold succeeds, since it is over already (it expired, was closed before, or a restart forgot it); a wrong secret is `unknown_session`, so nobody else can close it.

### Latency reports

Every probe round (10 s by default), each relay that has an API key reports, per peer in its config, the round's average round trip (none when every probe was lost), the loss, and the probes sent and received. Exits add their round trip to each region they serve, by game and region, for games that may be routed, and the WireGuard public key they actually use.

- **Sample by sample.** A sample the control plane can't use is left out and listed in the answer's `ignored`, and the rest is stored, so a relay running other game definitions (mid-deploy) loses only those rows. The reasons: `invalid_measurement` (a round trip outside 0 to 60 seconds, loss outside 0 to 100%, more answers than probes, or more than 1,000 probes), `unknown_peer` (a relay outside the registry, or itself), `game_not_routable` (a region sample for a game the control plane doesn't know or never routes), `region_not_served` (the game has no such region, or the relay isn't an exit serving it) and `exit_key_mismatch`. The relay logs the list when it changes (a warning while it isn't empty), the control plane logs a warning for each report with ignored samples, and a metric counts them by reason.
- **Exit keys.** When an exit's reported key isn't the registry's, the control plane logs an error, lists the relay in `/v1/relays` without a key, issues no new sessions on it (sessions already on it are still renewed), and answers `exit_key_mismatch`. A report with the right key clears it. Without the check, a wrong key in the registry looked to clients like an exit that never answers.
- **Pooling.** For each measurement (a relay and a peer, or an exit, a game and a region) the control plane keeps the last six rounds (`WINDOW_ROUNDS`), about a minute, and drops rounds older than 2 minutes (`LATENCY_MAX_AGE_SECS`); a measurement with none left is no longer served. A row's `rtt_ms` is the median of the kept rounds' round trips, or none when the latest round got no answer, so a relay that stops answering shows at its next report. `loss_pct` is the loss over every probe kept and `samples` how many probes that is: 30 for relay rows and 18 for region rows once six rounds are in. When a kept round lacks counts (a relay that doesn't send them), the row has the latest round's loss and `samples` 0. `age_secs` is the latest round's age. One round of 3 to 5 probes made a single lost probe read as 20 to 33% loss, and clients judged loss on it (ADR 009).

### Relay API keys

Each relay gets a random key (`pingpath-control api-key`); the control plane's config holds only its SHA-256, so the config file can't be used to impersonate a relay. A key can only report for its own relay ID (`wrong_relay`); samples about relays outside the registry, or with values that make no sense, are ignored.

### Privacy

The requester's address is used for one thing: counting sessions per network, at issuance and when a session is taken back after a restart. The network (never the address) is stored as an HMAC under a random per-process key, so even the in-memory table doesn't hold it. Closing needs no address. No log line holds the requester's address; issuance logs the session ID, game, region and exit, renewal, closing and taking a session back log the session ID, and refusals log only the reason (with the game and region at issuance). Behind a reverse proxy, `client_ip_header` names the header carrying the client's address, and its last entry (the one the proxy added) is used; without the setting the header is ignored, so clients can't choose the network they are counted against.

### Metrics

On the admin address, in the Prometheus text format (`control/src/metrics.rs`):

| Metric | What |
|---|---|
| `pingpath_control_sessions_issued_total` | Sessions granted |
| `pingpath_control_sessions_renewed_total` | Tokens renewed, sessions taken back after a restart included |
| `pingpath_control_sessions_closed_total` | Sessions the client closed before their token expired (Phase 3) |
| `pingpath_control_latency_reports_total` | Relay latency reports accepted |
| `pingpath_control_latency_samples_ignored_total{reason}` | Latency samples not stored, by the reasons above (Phase 3) |
| `pingpath_control_refusals_total{reason}` | Requests refused, by code |
| `pingpath_control_active_sessions` | Sessions held |
| `pingpath_control_uptime_seconds` | Seconds since start |

## Consequences

- A control-plane restart doesn't interrupt anyone: relays check tokens offline, and each session's next renewal takes it back, with no change at the game server. A new issuer seed loses running sessions, and so does taking their exit out of the registry; they fall back at their next renewal (ADR 009). A network, and the control plane as a whole, can briefly hold more sessions than its cap after a restart, since taking a session back is never refused for that.
- The renewal secret needs no storage, and adds no key to guard: whoever holds the issuer seed can sign tokens anyway.
- A session the client is done with stops counting against its network and its exit at once. Closing is best effort; a session whose close is lost counts until its last token expires, at most about 16 minutes.
- The control plane's view of exit load counts the sessions it holds, not traffic. An exit that is full answers `RelayFull`, and the client tries the best path through another exit once (ADR 009).
- A latency report is never refused for one bad sample. A misconfigured relay shows up as ignored samples, in both logs and a metric, instead of as rows that silently disappear.
- Deferred, with the relay-side items in ADR 004: signed relay registry and game bundles that relays and clients fetch and reload; token revocation; proof of work or account checks before issuance if the per-network caps prove too weak; issuer and exit key rotation driven from the control plane, including taking sessions back across an issuer key change.

## Alternatives considered

- **Long-lived tokens instead of renewal.** Simpler, but a leaked token would work for hours and a game marked unsupported would stay routed until every token ran out.
- **Renewal by the old token alone.** Anyone who saw a token (every entry relay does) could renew it; the renewal secret is only ever sent to the client over TLS.
- **A random renewal secret, stored as its SHA-256** (as first built). A restart forgot every secret, so every optimized player's next renewal got `unknown_session` and fell back mid-match.
- **Storing sessions in a file or a database** so they survive a restart. More to run and back up; the token and a derived secret give the same result with nothing stored. Phase 6's database can hold sessions if something else needs them.
- **Refusing a whole latency report for one bad sample** (as first built). One sample for a game or region the control plane doesn't route, during a deploy, dropped the whole report, which could take every exit out of path selection.
- **Serving only the latest round.** Loss over 3 to 5 probes turned one lost probe into 20 to 33%.
- **Counting by full client address.** Stricter, but it would mean holding addresses; per-network caps are coarser and hold nothing identifying.
