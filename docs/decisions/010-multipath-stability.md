# 010. Multipath for stability

- **Status:** Proposed
- **Date:** 2026-10-13
- **Builds on:** [001](001-end-to-end-tunnel.md), [004](004-relay-wire-protocol.md), [009](009-path-selection-and-failover.md)

## Context

PLAN.md Phase 4: an optional Stability mode that sends each encrypted tunnel packet over two different entry relays to the same exit, the exit's anti-replay window dropping whichever copy arrives second, and the return path likewise. An automatic mode turns duplication on only when measured loss is over 0.5% or jitter over 10 ms. The extra data used is shown. Done when, in a simulated network with 3% random loss on one path (`tc netem`), delivered loss with multipath is below 0.3%.

What Phase 3 built that this rests on:

- One WireGuard session, client to exit; entries forward its ciphertext without keys (ADR 001). Copies of one packet carry the same counter, and the receiver's replay window drops every copy after the first (`DuplicateCounter`), at the exit (counted as `duplicate`) and at the client alike. No extra protocol is needed for deduplication.
- The exit sends everything back on **one** return path: an address that completed a WireGuard handshake and then sent data under its keys (ADR 004). The handshake requirement is what stops a stranger from moving a session's traffic to an address of their choosing. ADR 004 deferred "several return paths per session" to this phase.
- The client keeps standbys: other entries in front of the same exit, with cookies fetched ahead of time, for failover (ADR 009). An entry forwards a session's traffic only once a HELLO with a valid cookie has recorded the client's address there.

## Decision

### The second path

A session in Stability mode uses two hops in front of the same exit: the **primary** (the current hop, as in ADR 009) and a **secondary**, one of the standbys through another entry. The secondary is picked from the standbys by provider first, then estimate: an entry on a different provider (`provider` in the relay registry) from the primary's, if there is one, since paths through one provider tend to fail together; otherwise the best standby through another entry. A straight-to-exit hop can be the primary but never the secondary (see the exit below), so a session whose only hops are the straight one and a single entry duplicates only with that entry as the secondary.

The client brings the secondary up the way a standby becomes current today, without a handshake: a HELLO through it with its cookie, so the entry records the session. The session's keys don't change.

### Upstream: duplicate every packet

While duplicating, every DATA frame the client sends (game packets, keepalives, echoes) goes to both hops, encrypted once, so both copies carry the same counter. The exit takes whichever arrives first and drops the other as a duplicate, as it already does.

Two kinds of frames go on one path only, so that each path proves itself on its own:

- **Per-path echoes.** Each hop gets its own in-tunnel echo every `echo_interval` (250 ms), sent on that hop alone. They keep each path authenticated at the exit (below), and their answers give each path's own round trip and loss.
- The handshakes of rekeys go on the primary, as today.

### At the exit: a second return path

The exit keeps, besides the handshake-confirmed return path, at most one **secondary return path**: an address that is

- the address of a relay in the exit's configuration with the entry role (`[[relays]]`), and
- not the primary path's address, and
- has sent data that decrypted under the session's current keys within the last `stale_path_secs` (5 s), which the per-path echoes ensure while the client duplicates.

A copy that arrives second proves nothing (the replay check runs before decryption, so anyone could send one), and doesn't count. While the secondary is fresh, the exit sends every frame for the client to both paths, encrypted once; when it goes stale (the client stopped duplicating, or that entry died), the exit sends on the primary alone again. If the primary goes stale while the secondary is fresh, the secondary carries the traffic alone until the client hands over to it (below), so a dying primary costs nothing.

Why a configured entry and no handshake: an entry forwards a session's frames only to the client address it recorded from a HELLO with a valid cookie, so sending a session's traffic to an entry can only ever reach that session's own client; and the exit already trusts configured entries' addresses for HELLOs. A client's own address as secondary would need a handshake from it, which would replace the keys the primary uses; that, with the extra state, is left for later (Known limits).

The exit counts the copies it sends (`duplicated_frames`, `duplicated_bytes`), and the second copies it drops stay counted as `duplicate`.

### Downstream at the client

The client accepts DATA for the session from either hop; the tunnel core drops the second copy of each packet through the replay window, and the session counts those copies per hop as proof that hop delivers (a duplicate isn't authenticated, but for the client's own health check that is enough).

### Health of each path

Each hop has its own figures over the stats window (10 s): round trip and loss from its own echoes, and when it last delivered anything.

- **The secondary fails** (no answer to its echoes for `failure_after`, 1 s): duplication stops, the hop goes back to the standbys, and another standby becomes the secondary at the next decision, if any. No notice: the game didn't notice either.
- **The primary fails while the secondary works:** the secondary becomes the primary at once, with no failover round (its keys are the session's and the exit already sends to it), and the player is told that traffic moved to another entry, as in ADR 009. Another standby becomes the secondary.
- **Both fail:** the watchdog and failover of ADR 009, unchanged.

### When to duplicate

A setting with three values, `stability_mode` in `client.toml` and changeable by the player over the pipe like the improvement thresholds:

- `off`: never.
- `on`: whenever a secondary is available.
- `auto` (the default): when the **primary's own** figures over the window show loss over 0.5% or jitter over 10 ms (`AUTO_LOSS_PCT`, `AUTO_JITTER_MS`), and until they have stayed under both for 30 s (`AUTO_OFF_AFTER`). The decision uses the primary's per-path echoes, not the delivered figures, which duplication itself improves; otherwise auto mode would turn duplication off as soon as it worked.

Every change is logged with the figures behind it.

### The extra data

The status carries the bytes sent and received as second copies since the session started (`extra_bytes_up`, `extra_bytes_down`: the duplicate frames sent, and the duplicate copies that arrived), and whether duplication is on now. The window shows them while optimized ("Extra data used: …"). Game traffic is small (well under 1 Mbps), so duplication costs at most as much again; the figure makes that visible rather than capping it.

### The test

`tools/netsim` gains a two-entry topology with `tc netem` loss on one entry's links. A network-namespace test (CI job `client-netns`) sends 2,000 datagrams through the tunnel to the echo server with 3% random loss each way on one entry, with Stability mode `on`, and requires fewer than 0.3% lost end to end; with `off` it checks the loss is as expected (about 6%), so the test can tell duplication working from a lucky run.

## Consequences

- With duplication on, a single lossy or dead entry costs the game nothing: the other copy arrives, and a dead primary is replaced without a failover gap.
- Up to twice the game's traffic through the relays and the player's connection while duplicating; usually small, and shown.
- Exits keep a little more state per session (a second address), and send twice while duplicating; their per-session rate limits count each frame once.
- The protection of return paths stays as strong as before: a second return path can only ever be one of our own entries, which forward only to the session's verified client.

## Known limits

- Both copies go through the same exit, so a lossy exit or a lossy link from the exit to the game server isn't helped.
- A straight-to-exit hop can't be the secondary; a player with a single entry in front of their exit and no direct route to it can't duplicate.
- Two entries on the same provider (or the same last mile, the player's own Wi-Fi) often lose packets together; the choice prefers different providers but can't see the rest.

## Alternatives considered

- **Forward error correction** (sending parity packets): less extra data for random loss, but adds delay while a group fills, needs a protocol layer of its own on both ends (ADR 001 chose none), and does nothing for a path that dies.
- **Duplicating towards two exits:** the game server would see each packet from two addresses, and a session's flows would split; one exit keeps the server-visible address the same (ADR 009).
- **A handshake per path** to confirm the second return path the way the first is: each handshake replaces the session's keys, so the paths would fight over them; and the configured-entry rule gives the same guarantee without one.
- **Counting a second copy as proof of a path:** the replay check runs before decryption, so anyone who saw a frame could send a copy from anywhere.
