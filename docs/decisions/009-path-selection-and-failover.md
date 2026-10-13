# 009. Path selection, the watchdog and failover

- **Status:** Accepted
- **Date:** 2026-10-06
- **Builds on:** [004](004-relay-wire-protocol.md), [006](006-control-plane.md), [007](007-client-architecture.md), [008](008-split-tunnel-routing.md)

## Context

PLAN.md Phase 3: pick the best entry and exit pair by measured latency, turn the tunnel on only if it beats the direct route (by default at least 10% or 10 ms better), and fail open (rule 6): a keepalive every 250 ms, no reply for 1 s → another entry with the same exit, and if there is none, remove the routes and go direct, all within 2 s. Rule 3 limits PingPath to the game region the player would normally use, through an exit in that region.

The proposal written before building tried standbys one after another, each with a share of the failover budget, counted only echo replies as proof of life, and closed sessions without any way to tell the control plane. A design review showed that sequential failover can't fit the budget at Gulf-to-Europe round trips, that one lost probe decided path choice, and more. This ADR describes the design as built, in `client/core/src/` (`measure.rs`, `select.rs`, `control.rs` and `engine/`), `relay/src` and `control/src`, and why.

## Decision

### Measuring

When the player picks a game and region, the engine measures (`engine/mod.rs`, `Engine::measure`):

| Leg | How | Code |
|---|---|---|
| Direct: player → each of the game's regions | The region's `ping_targets` (ICMP, or TCP connect time): 5 pings 200 ms apart with a 1 s timeout, and the fastest answering target's average, picked the way `pingpath-probe` and the exits pick it. Every region, in parallel, for rule 3 | `measure.rs` `PingTargets` |
| Player → each relay | 10 PROBE frames (ADR 004) to every entry and to every exit serving the region, 100 ms apart, then 1 s for the last replies: the median round trip and the share lost | `measure.rs` `ProbePlan`, `probe_relays` |
| Entry → exit | `GET /v1/latency` (ADR 006). Every 10 s each relay sends 5 PROBEs, 200 ms apart, to each relay in its config and reports the round's average round trip with the probes sent and answered. The control plane keeps each measurement's last six rounds (about a minute, 30 probes) and serves the median of their round trips and the loss over all their probes | `relay/src/probe.rs`, `control/src/state.rs` |
| Exit → game region | `GET /v1/latency` region rows. With each relay round, unless the last one is still running, an exit pings the targets of every routable game in each region it serves (3 pings per target, the fastest answering target), on a thread of its own; the results go with the first report after they are in (usually the same round's), so slow targets never hold back the relay rows. Pooled the same way: up to 18 probes | the same |

Every row carries `samples`, the probes behind its loss figure, so the client can tell one lost probe from real loss; a row whose rounds lack counts has 0 and is judged by its percentage alone. A row has no round trip when its latest round got no answer, so a relay that stops answering drops out at its next report, and a row goes once its newest round is more than 2 minutes old. Relay addresses from the control plane must be public unicast before anything is sent to them (`allow_local_relays`, for development and tests, lifts this). On Windows a PROBE to a relay whose port is closed makes the next receive fail with `WSAECONNRESET`; receives skip such errors, so one restarting relay can't spoil the others' figures. A measurement is kept for 5 minutes (`MEASUREMENT_TTL`); turning Optimize on with an older one measures again.

The exit's leg needs ping targets: the game definition validator requires at least one per region of a routable game, outside its server ranges, so they are never routed into the tunnel (`crates/games/src/validate.rs`).

### Choosing

Candidates are every exit that serves the region and has a WireGuard key in `/v1/relays`, reached straight from the player or through any entry (`select.rs`, `candidates`):

```
through an entry:  player→entry + entry→exit + exit→region
straight to exit:  player→exit + exit→region
```

A candidate is left out when:

- **a leg has no figure:** unmeasured, or the relay's latest round got no answer;
- **a leg is lossy:** more than 2% loss **and** at least two probes lost (`MAX_LOSS_PCT`, `MIN_LOST`). One lost probe is noise: among 10 it reads as 10%, and used to make path choice random on lossy links;
- **the entry doesn't report the exit.** A relay probes exactly the peers in its own config and forwards only to the exits there, so the entry's own row for the exit is what shows it can carry traffic there; the exit's row for the entry alone proves nothing (that entry would answer `WrongExit`). The entry's row must show the exit answering in its latest round, and its loss is the one judged; the exit's row for the entry may only give a faster round trip, since a stale row from an exit that has died must not stand in for the entry's;
- **the exit is more than 30 ms from the region** (`MAX_EXIT_TO_REGION_MS`): it isn't in the game server's region (rule 3). The figure is the exit's own for this game and this region; region IDs are free-form per game, and another game's targets could hide a misplaced exit.

The best estimate wins; ties go to the path with fewer hops. The engine logs each region's direct ping, every usable path with its legs and estimate, best first, the best path and how many candidates there were, and every exit or path it left out, by relay name, with the leg that ruled it out (the exit's to the region, the player's, or the entry's to the exit) and why: not measured, lossy (the loss and how many probes it rests on), or too far from the region (PLAN.md section 9). An exit listed without a key is never a candidate and isn't logged here; the control plane logs its key mismatch (ADR 006). The player sees the direct ping next to the best estimate.

**Only regions the player normally uses** (rule 3): a region can be optimized only if its direct ping is within 20 ms or 20%, whichever is larger, of the player's best region for that game (`USUAL_MARGIN_MS`, `USUAL_MARGIN_PCT` in `engine/mod.rs`). The engine refuses others as `not_usual_region`, so the rule holds whatever the UI does.

### Turning it on

Games that are never routed were refused before measuring (ADR 008), and so were games whose server ranges overlap those of a game that is never routed (`shares_ranges_with_refused_game`), since its traffic would be carried too. The engine refuses, with a stable reason the UI translates (`engine/types.rs`, `EngineError`), when the game is `testing` and its warning hasn't been acknowledged (rule 7), when the direct route couldn't be measured (there would be nothing to compare with), when the region isn't a usual one, when there is no candidate, or when the best estimate doesn't beat direct by the threshold. Otherwise it tries the best candidate's exit (`Engine::attempt`):

1. **A session.** A fresh WireGuard key and `POST /v1/sessions` naming the exit (ADR 006).
2. **The tunnel**, through the best candidate's hop, else the other hops with that exit in order of estimate, up to 3 s each (`CONNECT_TIMEOUT`). Through an entry: HELLO, which the entry answers with a cookie (Verify), then HELLO with the cookie and the handshake initiation together, since the entry records the session before the initiation reaches it. Straight to the exit, HELLO and the initiation go together from the start. Steps are resent every 200 ms (`engine/link.rs`, `RETRY`), but a new handshake initiation is made at most once a second (`REINITIATE`): each replaces the one before and an answer counts only for the last two, so a hop with a round trip of up to about 2 s still connects; a refusal from the relay (`RelayFull`, `WrongExit` and the like) moves on to the next hop at once. The path is up when the handshake completes, which proves it works both ways. One unconnected UDP socket per address family serves every hop, so the source address follows the routing table and a cookie fetched for a standby is still valid when traffic moves there (`engine/link.rs`).
3. **The real path**, measured with 4 in-tunnel echoes 100 ms apart within 1 s (`VERIFY_ECHOES`, `verify`). The optimized ping is their median round trip plus the exit's figure for the region.
4. **The threshold.** Only if the optimized ping beats direct by at least 10 ms **or** by at least 10% of the direct ping, either one being enough (PLAN.md: "≥10% or ≥10 ms"; `select::worthwhile`), are routes added: the relays' host routes first, then the game's ranges (ADR 008). A host route that can't be added is logged and skipped; a game route that can't be added undoes them all. Players can change both numbers in Settings (`min_improvement_ms`, `min_improvement_pct`).
5. **Otherwise** the player is told the direct route is as good, with both numbers, and the session is closed.

If no hop with that exit connects, or the exit doesn't answer the echo (an exit that is full and answers `RelayFull`, one that is restarting, a configuration mismatch), the best candidate on another exit is tried once, with a new key and a new session (`EXITS_TRIED` is 2): every hop of an attempt shares its exit, so another entry wouldn't help. A real path that connects but isn't better ends there, since the other exits' estimates were no better, and so does a refusal from the control plane.

**Every session is closed** when it is no longer used: turned off, fallen back, not better, or an attempt that never connected. A `Closer` (`engine/session.rs`) sends `POST /v1/sessions/close` when it is dropped, best effort and on its own thread, so the session stops counting against the player's network and its exit at once. Abandoned sessions used to count for about 16 minutes against caps that a carrier NAT's players share.

### Standbys and cookies

The other candidates with the same exit, best first, are the session's standbys, "straight to the exit" among them when it qualified. Moving to one keeps the exit, the exit's session and its flows, so the game server sees no change (rule 6 puts entry failover first for this reason).

Every 20 s (`STANDBY_REFRESH`) the session sends each standby entry a HELLO without a cookie. The entry answers with a cookie and records no session (ADR 004; it only caches the verified token); a cookie is valid for 30 to 60 s. With a cookie in hand, a failover through that entry skips the round trip that would ask for one: HELLO and the handshake initiation go out together. Once an entry has verified the session's token, these HELLOs draw on its larger budget for proven senders and not on its relay-wide one, so players behind one carrier NAT don't use up strangers' handshakes (ADR 004).

### The watchdog

While optimized, the session sends an in-tunnel echo every 250 ms (`echo_interval`) and runs WireGuard's timers on the same tick. Any frame from the current hop that authenticates as coming from the exit proves the path works: echo replies, game traffic, keepalives, handshake responses (`Handled::authenticated` in `crates/tunnel/src/core.rs`). The path has **failed** when nothing has authenticated for 1 s (`failure_after`), measured by a deadline that each such frame moves on, not by counting missed echoes. Counting only echo replies would turn a burst of game traffic, trimmed by the entry's per-session rate limit, into a failed path.

A receive error never fails the path. On Windows an ICMP error for an earlier send would surface on the next receive although the socket still works; the Windows backend turns those reports off on the tunnel socket (`SIO_UDP_CONNRESET`), and receives skip any error that still comes. Only the deadline decides.

### Failover

When the path fails (`Session::failover`):

- **One initiation, every hop at once.** The same handshake initiation, each copy with a HELLO carrying that hop's cookie, goes to the top 3 standbys (`PARALLEL_STANDBYS`) **and** to the current hop, in case the failure was a blip. The exit answers the first copy to arrive and rejects the others as replays, so the answer's source says which relay won. Every hop gets a HELLO, the straight-to-exit one too, so an exit that restarted learns of the session again (DATA for a session it doesn't hold gets no answer).
- **Retransmission** every 1.25 times the path's round trip, as measured through the tunnel when Optimize was turned on, and at least every 100 ms (`MIN_RETRANSMIT`), each time with a new initiation. An entry whose cookie has aged answers Verify; the HELLO and the initiation go out again at once with the new cookie.
- **The first completed handshake wins:** traffic moves to that relay, and the confirming keepalive that goes with it moves the exit's return path there (ADR 004); the hop it left becomes the last standby. An authenticated frame from the current hop instead ends the failover with the path unchanged. When the path moved, the player is told that traffic moved to another entry and the game server saw no change.
- Packets that reach the adapter meanwhile are dropped: they would arrive stale.

**Late handshakes.** The client accepts a response to its current or its previous initiation, and successive initiations can be answered through different relays, so a handshake can complete through a standby after the failover settled elsewhere, or after the path recovered. Its confirming keepalive moves the exit's return path to that standby, so the client follows it: the hop that delivers a handshake completed through a standby becomes the current hop. The exit keeps its last two answered initiations pending (ADR 004), so confirming either moves the path, and once the newer is confirmed a straggler from the older can't move it back.

**The budget** is 700 ms (`failover_budget`). Detection 1 s after the last authenticated frame, plus the budget, puts the fallback at about 1.7 s, plus the time to remove the game routes, inside rule 6's 2 s. A standby needs one round trip through it, plus one to the entry when its cookie had aged, so at a Gulf-to-Frankfurt round trip of about 110 ms the budget leaves room for several rounds. In the network-namespace test (`client/core/tests/netns`) moving off a killed entry took about 1 s and falling back after every relay was killed about 1.7 s. The engine tests (`client/core/tests/engine.rs`) run the same cases against real relays behind proxies that add 50 ms each way in front of the entries and 75 ms in front of the exit, with 1% loss on every path when the entry is killed.

**A local outage.** When Wi-Fi drops for a moment, the relays and the direct route go quiet together; falling back then can't bring the game back and would only change the address the server sees. So as the failover starts, the session also pings each of the region's ping targets three times, a third of 80% of the budget apart (the window is 560 ms, `DIRECT_CHECK_SHARE`), each ping waiting until the window ends, so one lost ping doesn't count as an outage and the answer is known at the deadline however fast it came. If none answered, the relays get 1.5 s more, once (`OUTAGE_EXTENSION`); if nothing answers by then either, the session falls back anyway. A dead path with a working direct route still falls back within 2 s, unless all three pings to every target are lost. An engine test drops every relay and the direct route for 2.2 s and checks that the session keeps its path and the server-visible address (`a_short_local_outage_keeps_the_path`).

### Fallback

When no relay answers in time, or for one of the other causes below, the session ends (`Session::end`): the game routes go first, so game traffic goes direct at once (how long that took is logged), then the relays' host routes, and the session is closed at the control plane. The engine raises a notice: optimization stopped, the game now connects directly, and some games need to reconnect (rule 6). It keeps the last 20 notices in its status until the player dismisses them, so the UI can show one even if it wasn't open at the time (ADR 007). The engine never turns optimization back on by itself, which would change the server-visible address again; the player chooses.

The causes (`FallbackCause` in `engine/types.rs`): `relays_unreachable`, `exit_not_forwarding`, `session_expired`, `session_ended`, `renewal_refused`, `engine_stalled`, `adapter_lost` and `suspended`.

The adapter itself outlives a session: it is created on the first Optimize (slow on Windows, where the first one installs the driver), stays without routes for the next one, and is removed when the service stops. After `adapter_lost` (its reader stopped: the adapter was disabled or removed outside PingPath, or failed) the engine removes it at once, so the next Optimize creates a new one instead of ending at once again. A session that stops unexpectedly (its task panicked) is handled the same way: the adapter is removed, and the notice gives the cause as `adapter_lost`.

### The guard

A plain OS thread outside the async runtime (`engine/guard.rs`) watches a heartbeat that the session's loop beats on every turn (at least four times a second, with the default echo interval), inside the failover loop, and between the steps of moving host routes after a network change (on Windows each step writes the route ledger to disk, which a busy disk can make slow). If the beat stops for 1 s (`failure_after`) while game routes are in place, checked every 100 ms, the guard removes the game routes itself: a deadlocked or starved service can't leave game traffic routed into an adapter nobody reads. When the loop runs again it sees what happened and ends the session as `engine_stalled`. A service that exits or crashes normally takes the adapter, and every route through it, with it; an adapter or relay host route still left over is removed when the service starts again (ADR 007).

### Renewal

- **When.** Tokens last 15 minutes. The session renews at two thirds of the token's lifetime (`POST /v1/sessions/renew` with the current token and `renew_until`, ADR 006), then sends HELLO with the new token on the current hop. Through an entry, that HELLO usually carries a cookie that has aged by then, so the entry answers `Verify` and the HELLO goes again at once with the new cookie.
- **Done when the exit says so.** The renewal is complete only when the exit acknowledges that HELLO; until then the HELLO is sent again every second (`RENEW_HELLO_EVERY`). An entry extends its record as soon as it sees the new token but the exit only when the HELLO reaches it, and an exit that missed it would end the session at the old expiry.
- **Failures.** A failed request is tried again after 30 s, or after the `Retry-After` the control plane gives. `unknown_session` and `game_not_routable` are final: the session falls back at once (`renewal_refused`). `session_ended` means no more renewals. A relay that answers the current hop's HELLO with `Expired` makes the session renew at once.
- **A control-plane restart** doesn't touch running sessions: the restarted control plane recognises a token it signed and the renewal secret derived for it, and adopts the session (ADR 006). `unknown_session` means it couldn't, for example because the issuer seed changed.
- **The final expiry.** Renewals stop at `renew_until`, 12 hours after the session started; the last token expires exactly then, and the session falls back as `session_ended` with a notice. The player can optimize again.
- **Expiry.** Once a second the session checks whether the token the exit holds has run out, on a monotonic clock, and falls back if it has (`session_expired`, or `session_ended` after the last renewal). A renewed token counts only once the exit acknowledges it, so while one is unacknowledged (the exit may refuse it, say because its own game definitions no longer route the game) the old token's deadline stands; the check runs whatever renewal is doing, including between retries of a failed one. The relays allow 60 s of clock skew, so the session falls back that long before they stop forwarding.

### Network changes

Once a second the session asks the platform for the physical route to each relay it holds a host route for (`Session::check_network`). `Platform::best_route` sets PingPath's own routes aside. On Windows, when one of them covers the address, it redoes the route choice from the route table without the adapter's routes and our host routes (otherwise `GetBestRoute2` already gives the physical route); on Linux it asks `ip route get fibmatch`, then the main table if the route matched is ours. So a new gateway, or a move from Wi-Fi to cable, shows even while our host route still points where the network was. When the route changed, the session moves the host routes, pins its socket to the new interface again (Windows: `IP_UNICAST_IF`), forgets every entry cookie (they are tied to the old address), and sends HELLO and a handshake initiation on the current hop. Through an entry, the entry asks for a cookie for the new address and then records it; straight to the exit, the handshake moves the exit's return path. The exit and its flows stay the same.

### A game flow the exit stops delivering

The echo can't see an exit that has stopped reaching the game server (its flow limit, a lost route, a server that blocks the exit's address). So the session notes, for each game server address and port, when packets last went out and when an answer last came back. If one that had been answering gets nothing for 2.5 s (`FLOW_SILENCE`) while the game still sends to it and the path itself is alive, the session falls back (`exit_not_forwarding`): another entry with the same exit wouldn't help. Flows unused for 30 s are forgotten (`FLOW_IDLE`). A flow that never answered isn't judged, since nothing shows the server would have.

### The exit lost the session

An exit that restarted has no sessions, and answers HELLO with `OkNewSession`. When that happens to a running session (on a renewal, a failover or a network change), the session starts a new handshake and traffic carries on through the same exit, but the exit opens new flows, so the game server sees new ports (ADR 005). The player is told the address changed and some games need to reconnect (`Report::AddressChanged`, notice `address_changed`).

### Sleep

Before the PC suspends, the Windows service passes the power event to the engine (`client/service/src/windows/scm.rs`, `run.rs`), which ends the session cleanly as `suspended`: game routes removed, session closed, notice raised. Nothing is turned back on at resume, so the watchdog never wakes to a path that went quiet while the PC slept.

### What the graph shows

| While | Every | Ping | Jitter | Loss |
|---|---|---|---|---|
| Optimized | 1 s | The average echo round trip over the last second, plus the exit's figure for the region | Mean difference between consecutive echo round trips over the last 10 s | Echoes unanswered over the last 10 s; one younger than two echo intervals isn't counted yet |
| Measured, not optimized | 2 s, or longer while a series is still running | The direct route (5 pings to each of the region's ping targets, the fastest answering one) | none | The share of that target's pings lost; 100% when none answered |

The status also shows the path (exit, and entry if any), the optimized and direct pings measured when Optimize went on, and how many game packets were sent into the tunnel, so the player can see whether the game's traffic really goes through PingPath, with the game's packets the tunnel couldn't carry by reason (ADR 008). Both counts start again with each session.

### Logs

What happens is logged (PLAN.md section 9): the direct figure for each region and the choice, as described under Choosing; every relay a connect attempt failed through, by name, and the one that connected; the verified ping against direct; each watchdog event with its timings (how long the path was silent, how long the failover took, how many rounds, which relay took over); every fallback with its cause and how long removing the game routes took; the guard removing the game routes; an exit that had lost the session; a late handshake followed to a standby; renewals and refusals; network changes; and, when the player is warned that game traffic is being dropped because the tunnel can't carry it, the counts by reason, at most every 30 s. A session closed at the control plane is logged; a close that fails is logged only at debug level (the session then counts until its token expires, ADR 006).

### Timing constants

| What | Value | Where |
|---|---|---|
| Player's PROBEs per relay | 10, 100 ms apart, 1 s for the last replies | `client/core/src/measure.rs` `ProbePlan` |
| Direct route | 5 pings per target, 200 ms apart, 1 s timeout | `measure.rs` `PingTargets` |
| Relay probe rounds | every 10 s; 5 PROBEs per peer, 200 ms apart, 1 s for late replies | `relay/src/config.rs` `ProbeConfig`, `relay/src/probe.rs` |
| Exit's region pings | 3 per target, 200 ms apart, 1 s timeout | `relay/src/probe.rs` `REGION_SERIES` |
| Rounds pooled per row; oldest round kept | 6; 120 s | `control/src/state.rs` `WINDOW_ROUNDS`, `LATENCY_MAX_AGE_SECS` |
| Lossy leg | more than 2%, and at least 2 probes lost | `client/core/src/select.rs` `MAX_LOSS_PCT`, `MIN_LOST` |
| Exit to region, at most | 30 ms | `select.rs` `MAX_EXIT_TO_REGION_MS` |
| Usual region | within 20 ms or 20% of the best, whichever is larger | `client/core/src/engine/mod.rs` `USUAL_MARGIN_MS`, `USUAL_MARGIN_PCT` |
| Measurement reused for | 5 min | `engine/mod.rs` `MEASUREMENT_TTL` |
| Connecting through a hop; resend | 3 s; 200 ms | `engine/mod.rs` `CONNECT_TIMEOUT`, `engine/link.rs` `RETRY` |
| Verifying the path | 4 echoes, 100 ms apart, within 1 s | `engine/mod.rs` `VERIFY_ECHOES`, `verify` |
| Improvement threshold | 10 ms or 10% | `client/core/src/config.rs` |
| Exits tried when turning on | 2 | `engine/mod.rs` `EXITS_TRIED` |
| Standby cookie refresh | 20 s | `engine/session.rs` `STANDBY_REFRESH` |
| Entry cookie valid | 30 to 60 s | `relay/src/entry.rs` `COOKIE_WINDOW_SECS` |
| Echo interval | 250 ms | `config.rs` `echo_interval` |
| Path failed after | 1 s without an authenticated frame | `config.rs` `failure_after` |
| Failover budget | 700 ms | `config.rs` `failover_budget` |
| Hops tried at once | 3 standbys and the current hop | `session.rs` `PARALLEL_STANDBYS` |
| Failover retransmission | 1.25 × the path's round trip, at least 100 ms | `session.rs` `MIN_RETRANSMIT` |
| Direct check | at most 80% of the budget (560 ms) | `session.rs` `DIRECT_CHECK_SHARE` |
| Local-outage extension | 1.5 s, once | `session.rs` `OUTAGE_EXTENSION` |
| Guard | 1 s without a beat (`failure_after`), checked every 100 ms | `engine/guard.rs` |
| Token lifetime; renewals until | 15 min; 12 h after the session started | `control/src/config.rs` `SessionLimits` |
| Renewal | at 2/3 of the token's lifetime; retry after 30 s or `Retry-After`; HELLO every 1 s until acknowledged | `session.rs` `RENEW_HELLO_EVERY` |
| Game flow silence; flow forgotten | 2.5 s; 30 s | `session.rs` `FLOW_SILENCE`, `FLOW_IDLE` |
| Network check | every 1 s | `session.rs` `every_second` |
| Blocked game traffic | a notice once 20 more packets were dropped than at the last one (checked every second); logged at most every 30 s | `session.rs` `every_second`, `BLOCKED_LOG_EVERY` |
| Graph | 1 s optimized, 2 s direct (only while a window is connected); 10 s window | `session.rs`, `engine/mod.rs`, `client/core/src/stats.rs` `WINDOW` |
| Notices kept until dismissed | the last 20 | `engine/mod.rs` `NOTICES_KEPT` |

The three watchdog timings live in `client.toml`, which only administrators can change, and are validated: the echo interval 50 to 1,000 ms, `failure_after` 200 to 5,000 ms and longer than the echo interval, the budget 100 to 5,000 ms.

## Consequences

- Path choice rests on real measurements, the player's own and the relays', and loss is judged on enough probes: 10 from the player, and at the control plane up to 30 per relay leg and 18 per region. A relay that stops answering still drops out at its next report. The control plane only passes numbers on, so it can recommend paths later (Phase 6) without changing the client.
- The tunnel goes on only when the real path, measured through the tunnel, beats direct, and the player sees both numbers.
- A dead entry costs about a second of lost packets and no address change at the server. Losing every relay costs about 1.7 s, an address change and a notice. A local outage keeps the path for up to about 3.2 s and only then falls back.
- A session the client is done with stops counting against the player's network at once. Closing is best effort: a session whose close is lost still ends with its token.
- Standby cookies cost each standby entry one HELLO and one Verify per player every 20 s, and no session state (a cached token, dropped when it expires).
- Exits ping the game regions they serve, only for games that may be routed: 3 pings per target, at most once per 10-second round.
- **Known limits of what is built:**
  - The retransmission interval comes from the path's round trip when Optimize was turned on, and is the same for every hop. The client accepts answers only to its last two initiations, so a standby whose round trip is more than about twice that interval can't win within the budget. Standbys share the exit, so their round trips are usually close to the path's; an interval per hop is the fix if testers see otherwise.
  - The client follows any handshake that completes through a standby. If answers to two initiations come back in the opposite order through different relays, the client follows the older while the exit has settled on the newer. Game traffic still flows both ways, but the watchdog counts only the hop the client sends on, so one more failover settles it.
  - Network changes are polled once a second, and not while a failover runs; a change the watchdog notices first goes through failover, and a fallback if that fails. Event-driven detection (`NotifyRouteChange2`, netlink) is the next step if testers see it.
  - On sleep, the service queues the end of the session and returns at once; Windows doesn't wait for it. Usually it is done in milliseconds, but one queued behind a failover or a measurement may not finish before the PC sleeps; then, at wake, the session ends through the watchdog or the guard thread (traffic goes direct either way), and the logged cause can be wrong.
  - Failover sends the same initiation to the current hop as to the standbys, and the exit answers only the first copy to arrive. If the current entry still carries traffic towards the exit but not back, its copy, usually the fastest, keeps winning and the answer keeps being lost, so the session falls back rather than move to a standby.
  - If the exit restarts during a failover, each hop's HELLO is answered as a new session: the player can get more than one "address changed" notice, and a later HELLO on the same path can be taken as needing a handshake until the next rekey.
  - Rule 3's usual-region check uses only regions whose direct ping was measured. If the nearest region's ping targets don't answer, a farther region can count as usual and be optimized.
  - Relay addresses must be public, but the client doesn't check them against its own networks: a control plane that is compromised could name an IPv6 address in the player's own prefix (or, through hairpin NAT, the player's public IPv4 address) and have the service probe it. The control plane is reached over HTTPS and trusted for the relay list.
  - A session that can't be renewed (`unknown_session`, or the 12-hour limit) falls back and the server-visible address changes. A new session wouldn't avoid that, even on the same exit: it would get new flows, so new ports at the server (ADR 004). A match that runs across the 12-hour limit may need a reconnect in some games.

## Alternatives considered

- **Trying standbys one after another, each with a share of the budget** (the proposal). Each attempt cost a cookie round trip, a HELLO_ACK, the handshake and an echo, about 350 ms at a Gulf-to-Frankfurt round trip of 110 ms, so with two or three standbys a healthy one was cut off before it could answer, and one lost packet used up an attempt. The straight-to-exit standby sent no HELLO, so it couldn't recover an exit that had restarted, and the current hop was never retried after a blip. In parallel, with cookies fetched ahead of time, the whole failover needs about one round trip.
- **Success on the first echo reply.** A completed handshake already proves the path works both ways and moves the exit's return path; waiting for an echo costs another round trip.
- **Echo replies as the only proof of life.** A burst trimmed by a rate limit, or a few lost echoes on a lossy link, would fail a working path; every authenticated frame is equally good proof.
- **Keeping standbys warm** with their own entry sessions and keepalives. Partly done through the cookie prefetch, which saves the round trip that matters and leaves no session at the entry. Full warm standbys would double entry state, entries drop sessions whose exit has been quiet for 15 s, the exit returns traffic on one path at a time, and Phase 4's multipath sends over two entries anyway.
- **Falling back as soon as the relays fail,** whatever the direct route: a moment of Wi-Fi loss would change the server-visible address for nothing.
- **Judging a leg lossy on any lost probe** (the 2% rule applied to 3 to 5 probes): one lost probe disqualified a relay, so path choice was random on lossy links.
- **Estimating exit → region as zero** (the exit is in the region): optimistic by a few milliseconds and blind to a misplaced exit. **Mixing every game's ping targets for a region ID**, as first built: region IDs are free-form per game, so it could understate the optimized ping by tens of milliseconds, and exits would probe the servers of games that are never routed.
- **Turning optimization back on automatically after a fallback:** it would change the server-visible address again, possibly mid-match.
