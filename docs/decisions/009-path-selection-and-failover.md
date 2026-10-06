# 009. Path selection, the watchdog and failover

- **Status:** Proposed
- **Date:** 2026-10-04
- **Builds on:** [004](004-relay-wire-protocol.md), [006](006-control-plane.md), [007](007-client-architecture.md), [008](008-split-tunnel-routing.md)

## Context

PLAN.md Phase 3: pick the best entry and exit pair by measured latency, enable the tunnel only if it beats the direct route (default: at least 10% or 10 ms better), and fail open: a keepalive every 250 ms, no reply for 1 s → another entry with the same exit, and if there is none, remove the routes and go direct, all within 2 s (rule 6).

## Decision

### Measuring

When the player picks a game and region, the service measures, in parallel and within about 3 seconds:

| Leg | How |
|---|---|
| Direct: player → game region | The region's `ping_targets` (ICMP, or TCP connect time), the same code as `pingpath-probe` |
| Player → each relay | PROBE frames (ADR 004) to every entry and every exit serving the region: 5 each, median and loss |
| Entry → exit | `GET /v1/latency` from the control plane (relays measure each other) |
| Exit → game region | **New:** exits measure their regions' `ping_targets` the same way and report them with their relay latency; `GET /v1/latency` returns them as region rows |

### Choosing

Candidates are every exit serving the region, reached straight from the player or through any entry. Each gets an estimate:

```
through an entry:  player→entry + entry→exit + exit→region
straight to exit:  player→exit + exit→region
```

A candidate with an unknown leg, or more than 2% loss on a leg the player measured, is skipped (and logged). The best estimate wins; ties go to the path with fewer hops. The UI shows the direct ping next to the best estimate.

**Turning it on.** The service asks the control plane for a session on the chosen exit (a fresh WireGuard key, ADR 006), connects through the chosen entry, and measures the real path with the exit's in-tunnel echo for half a second. The optimized ping is then `echo RTT + exit→region`. Only if that beats direct by the threshold (default: at least 10 ms or 10%, whichever is larger in the player's favour, configurable) does it add the game routes. Otherwise it closes the session and tells the player the direct route is already as good, with both numbers.

**Standbys.** The other candidates with the same exit, ranked by estimate, are kept as standbys, including "straight to the exit". They keep the exit, so the game server sees no change if traffic moves to one.

### The watchdog

While optimized, the service sends an in-tunnel echo every 250 ms. Each reply proves the whole path works both ways. The path is **failed** when no echo reply has arrived for 1 s.

**Failover** (budget 700 ms in total): take the next standby; through an entry, send HELLO (and the cookie the entry asks for), then a WireGuard handshake, then an echo; straight to the exit, the handshake and an echo. The first echo reply means success: traffic carries on through the new path, and the exit moves its return path after the handshake (ADR 004). If a standby doesn't answer within its share of the budget, try the next.

**Fallback:** if no standby answers within the budget, remove the game routes (traffic goes direct), close the session, and tell the player in plain words: "PingPath lost its connection, so your game now connects directly. Some games need to reconnect." Detection (1 s) plus failover (at most 0.7 s) plus removing routes keeps the total under 2 s. The service does not turn optimization back on by itself, which would change the address the game server sees again; the player chooses.

Every watchdog event is logged with timings: echo lost, failover started, each standby tried and the result, fallback (PLAN.md section 9).

### Keeping the session

- **Renewal** at two thirds of the token's lifetime (`POST /v1/sessions/renew`), then HELLO with the new token on the current path; retried every 30 s on failure. If the token is about to expire with no renewal, the service falls back as above.
- **The 12-hour limit** (ADR 006): at `renew_until` the session ends with a fallback and a notice; the player can optimize again.
- **Network changes** (Wi-Fi to cable, a new gateway): every 2 s the service checks the route to the active relay; if the gateway or interface changed, it moves the relay host routes, opens a new socket, and sends HELLO and a handshake on it, which the exit accepts as the same session.

### What the graph shows

Once a second: the mode (direct or optimized), the ping (optimized: echo RTT + exit→region; direct: the ping target's RTT), jitter (mean difference between consecutive samples) and loss over the last 10 seconds.

## Consequences

- The choice uses real measurements from the player's connection and the relays' own measurements; the control plane only relays numbers, so it can recommend paths later (Phase 6) without changing the client.
- The tunnel is only switched on when it is measurably better, and the player sees both numbers.
- A dead entry costs at most about 1.7 s of lost packets and no address change at the server; losing everything costs the same time and an address change, which the player is told about.
- Exits now probe the game regions they serve; that traffic goes to the definitions' ping targets, a few packets every 10 seconds.

## Alternatives considered

- **Estimating exit → region as zero** (the exit is in the region): simpler, but optimistic by a few milliseconds per region and blind to a badly placed exit.
- **Keeping a standby entry warm** with its own HELLO and keepalives: faster failover, but entries drop sessions that get no traffic from the exit, and it doubles relay state; Phase 4's multipath sends over two entries anyway.
- **Turning optimization back on automatically after a fallback:** would change the server-visible address repeatedly during a match.
