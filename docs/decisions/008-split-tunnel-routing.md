# 008. Split-tunnel routing on the client

- **Status:** Proposed
- **Date:** 2026-10-04
- **Builds on:** [002](002-game-definitions.md), [005](005-exit-egress.md), [007](007-client-architecture.md)

## Context

Rule 5: what goes through the tunnel is decided by destination, through the routing table. Rule 7: games whose status refuses routing are never routed. PLAN.md Phase 3 also asks for host routes that keep tunnel traffic out of the tunnel, for IPv6 never to bypass an optimized session silently, and for a decision on packets that reach the adapter because they match a game's address range but not its ports.

## Decision

### What gets routed

When the player turns Optimize on, and only after the tunnel is up and answering:

1. **Relay host routes first.** For every relay address the session may use (the exit and the standby entries), the service looks up the current route to it (`GetBestRoute2`) and adds a /32 (or /128) host route through that same physical gateway and interface. Tunnel traffic can then never loop into the adapter, even if a relay address falls inside a game's range.
2. **Game routes.** For the selected game's selected region, one route per `server_ranges` prefix, IPv4 and IPv6, through the PingPath adapter. Nothing else: no default route, no DNS change, no other region.

Turning Optimize off removes the game routes first (traffic goes direct at once), then closes the session, then removes the host routes and the adapter.

**Routing policy is checked four times:** the UI greys out games that can't be routed, the service refuses to plan routes for them, the control plane refuses to issue a token, and the exit refuses the session (rule 7). A test covers the client side.

### The adapter

Wintun adapter `PingPath`, address `10.200.0.2/32` (configurable), MTU 1380 (the tunnel MTU from ADR 004), no gateway, no DNS servers, a high interface metric so nothing prefers it. Longest-prefix match sends game prefixes to it whatever the metric.

### Packets that reach the adapter

The service forwards a packet into the tunnel only if it is IPv4 UDP to an address in the region's ranges and a port in the game's UDP ports, the same check the exit makes. Anything else is dropped and counted by reason (`not_udp`, `not_game_port`, `ipv6_not_carried`, `malformed`), logged once a minute with counts, and shown in the UI as a warning when it isn't negligible ("Some traffic to the game's servers isn't carried: ..."). This settles the open question from PLAN.md Phase 3: **game definitions must keep their ranges narrow** (servers that only carry game traffic), and stray traffic is treated as a definition problem to report, not something the client works around. Forwarding strays directly from the client would need sockets pinned to the physical interface and a user-space NAT for UDP and a TCP stack for TCP, which isn't worth it while definitions can be fixed instead.

### IPv6

Exits don't forward IPv6 yet (ADR 005). If a region lists IPv6 ranges, the client still routes them to the adapter and drops what arrives (counted as `ipv6_not_carried`), so a dual-stack game falls back to IPv4 rather than sending its IPv6 traffic around an "optimized" session unnoticed; the UI says IPv6 isn't carried yet. None of today's definitions list IPv6 ranges.

### What the game server sees

While optimized, the exit's address. Games that bind their connection to the address they first used may need to reconnect after Optimize is switched on or off mid-match; the UI says so next to the toggle (rule 6). Programs that fixed their route when they connected keep it until they reconnect, so the UI suggests turning Optimize on before joining a match.

### Leftovers

Routes the service adds carry a fixed protocol value and the adapter's interface, so `cleanup` (run at service start, on uninstall, and by hand) finds and deletes exactly its own routes and nothing else. Host routes are also kept in a list in `%ProgramData%\PingPath\routes.json`, written before each change, as a second record for cleanup after a crash.

## Consequences

- Only the selected game's server traffic is affected; a wrong definition can at worst drop traffic to those few addresses, never affect anything else.
- Narrow definitions become a hard requirement, visible to testers through the stray-traffic warning and counted in the diagnostic report.
- IPv6-only game servers can't be optimized until exits forward IPv6.

## Alternatives considered

- **A default route through the adapter with exclusions.** Breaks rule 5's spirit (everything enters the tunnel) and turns any client bug into a broken connection.
- **Filtering by port with WFP or a packet-diverting driver.** Ruled out by rule 4.
- **Forwarding stray packets directly from user space.** Possible for UDP, not for TCP without a TCP stack; deferred until a real game needs it.
