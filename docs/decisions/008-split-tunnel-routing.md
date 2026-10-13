# 008. Split-tunnel routing on the client

- **Status:** Accepted
- **Date:** 2026-10-06
- **Builds on:** [002](002-game-definitions.md), [005](005-exit-egress.md), [007](007-client-architecture.md)

## Context

Rule 5: what goes through the tunnel is decided by destination, through the routing table. Rule 7: games whose status refuses routing are never routed. PLAN.md Phase 3 also asks for host routes that keep tunnel traffic out of the tunnel, for a game's IPv6 traffic never to bypass an optimized session silently, and for a decision on packets that reach the adapter because they match a game's address range but not its ports. Exits carry only IPv4 UDP ([ADR 005](005-exit-egress.md)).

The proposal written before building (in this file's history) routed a region's IPv6 ranges to the adapter and dropped what arrived, gave the adapter the address `10.200.0.2`, and found PingPath's routes by a fixed route protocol value. The Phase 3 design review found that:

- Windows sends IPv6 around an adapter that has no IPv6 address, so routing IPv6 ranges there would let that traffic bypass the tunnel silently;
- `10.200.0.2` could be a real host on the player's network;
- `route add` and `New-NetRoute` use the same protocol value, so cleanup by protocol could delete the default route or another program's routes;
- routing one game would also carry a game that is never routed if they share server ranges;
- a game whose definition lists TCP ports would have that traffic blocked while optimized;
- on Windows, turning Optimize on doesn't move a game's existing connection;
- packets from the tunnel were written to the adapter without a check matching the outbound one.

This ADR describes what was built (`client/core/src/plan.rs`, `policy.rs`, `filter.rs`, `net/`) and why.

## Decision

### What gets routed

When the player turns Optimize on, routes go in only after the tunnel is up and the real path, measured through it, beats the direct route (ADR 009). The plan (`plan.rs`) has two parts, added in this order:

1. **Relay host routes.** For every relay the session sends to, the hop it connects through and each standby, a host route (/32, or /128) through the physical next hop the system uses for it now (`Platform::best_route`, below). Tunnel traffic can then never loop into the adapter, even if a relay's address falls inside a game's range. Relays on loopback (development and tests) need none. A host route that can't be added is logged and skipped; on Windows the tunnel sockets are pinned to the physical interface as well (below).
2. **Game routes.** One route through the adapter for each IPv4 prefix in the selected region's `server_ranges`. Nothing else: never a default route, never IPv6, no other region, no DNS change. If a game route can't be added, every route added so far is removed and Optimize stays off.

Turning Optimize off, or falling back, removes the game routes first, so game traffic goes direct at once, then the host routes. The adapter stays, without routes, until the service stops, unless it stopped delivering packets, in which case the engine removes it and the next Optimize creates a new one (ADR 007).

**Rule 7 is checked at every step:** the app greys out games that can't be routed; the engine refuses to measure them or plan their routes (`policy.rs`); the control plane refuses to issue a token (`game_not_routable`); the exit refuses the session (ADR 004). A routable game that shares server ranges with a game that is never routed is refused too, since routing it would carry the other game's traffic: the validator reports it (`check_catalog`), and the service and the engine refuse such a game (`overlaps_with_refused`). A game with status `testing` is routed only after the player has accepted a warning, once per game (ADR 007).

### The adapter's address and MTU

The adapter's address is `192.0.0.8`, the IPv4 dummy address (RFC 7600), which can't be a host on the player's network or a game server. An administrator can change it in `client.toml` (`adapter_address`); the app can't. It is a /32, so it brings no subnet route with it. While optimized, game packets leave the PC from this address, and only replies to it are let back in (below).

The MTU is 1,380 bytes, the tunnel MTU ([ADR 004](004-relay-wire-protocol.md)). On Windows the adapter's interface metric is a fixed, high 9000, so the system never prefers it for anything; longest-prefix match sends the game's prefixes to it whatever the metric.

### IPv4 only

The client never routes IPv6: the plan skips IPv6 ranges, the Windows backend refuses an IPv6 route to the adapter, and the adapter's IPv6 side gets no routes (ADR 007). Exits don't carry IPv6, and Windows would send it around an adapter without an IPv6 address anyway.

PLAN.md's requirement that a game's IPv6 traffic never silently bypass an optimized session is met through the game definitions. The validator (`crates/games/src/validate.rs`) refuses, in any game that may be routed (`testing` or `supported`):

- IPv6 server ranges, so every range a routable game lists is IPv4 and is routed whole;
- TCP ports, since PingPath carries only UDP and TCP to the routed ranges would be blocked while optimized;
- regions without a ping target, or with one inside the game's server ranges, which would be routed into the tunnel while optimized.

A game that needs IPv6 or TCP to its servers can't be marked routable until exits carry them.

### The packet filter

Routes decide what reaches the adapter; the filter (`filter.rs`) decides what crosses it, with the same check the exit makes.

- **Out.** A packet from the adapter goes into the tunnel only if it is IPv4 UDP to an address in the region's ranges and a port in the game's UDP ports. The destination is checked first, so the system's own chatter on a new interface (multicast, router solicitations) counts as `not_game_destination` and is never shown as a game problem. Everything else is dropped and counted by reason: `ipv6_not_carried`, `not_udp`, `not_game_port`, `fragment`, `malformed`.
- **In.** A packet from the tunnel is written to the adapter only if it is IPv4 UDP from the region's servers on the game's ports, to the adapter's own address (`inbound_not_game` otherwise). Nothing else from the network is let in.

**Packets to a game prefix but not its ports** (PLAN.md Phase 3's open question) are dropped, as the exit would drop them, and the player hears about it. The status counts game traffic blocked this way (`not_game_port`, `not_udp`, and `ipv6_not_carried`), and once 20 or more packets have been blocked the engine raises a notice, updated as the count grows by 20: some of the game's packets couldn't be carried and were blocked while optimized; please send a diagnostic report. The counts start again with each session, so an earlier session's drops can't raise the notice. Raising it also logs a warning with the count by reason (never an address), at most every 30 s, so the report the notice asks for shows what was blocked (PLAN.md section 9). So the question is settled in favour of narrow definitions: a routable game lists only ranges whose servers it reaches over UDP on its listed ports, and stray traffic is a definition problem to report and fix, not something the client works around. Forwarding strays directly from the client instead would need sockets pinned to the physical interface, a user-space NAT for UDP and a TCP stack for TCP.

### Recognising PingPath's routes

Cleanup runs when the service starts, in `pingpath-service cleanup` and in `uninstall` (ADR 007). It must find PingPath's routes and nothing else.

**On Windows** (`net/windows/`, `net/route_table.rs`), never by the route protocol value: PingPath's rows use `MIB_IPPROTO_NETMGMT`, the value `route add` uses too, so it marks nothing.

- **Game routes** are on-link routes on the adapter (Wintun is a layer-3 link, so there is no gateway), with route metric 0 plus the adapter's interface metric. Being on the adapter is what makes a route ours: the backend refuses to add a game route on any other interface, and removing one never touches another interface's route. Windows removes them with the adapter, including when the service crashes.
- **Relay host routes** sit on the physical interface, so they survive a crash. Each carries route metric **293**: a /32 wins over any shorter prefix whatever its metric, so the value only serves to recognise our rows; it is uncommon and below the 9999 that `route.exe` accepts. Each is written to a ledger, `%ProgramData%\PingPath\state\routes.json` (admin-only), **before** it is added, and forgotten after it is removed; every write replaces the file atomically. A route that already exists for the relay with the same interface and next hop but another metric belongs to someone else: it already sends the relay the right way, so it is used and left alone, and its record is dropped from the ledger.
- **Cleanup** deletes exactly the route table rows that match a ledger record in every field: destination, next hop, interface and metric 293. A route someone else added is never touched, even one to the same place. Records whose route is gone are forgotten; a record whose delete failed is kept for next time. If the ledger can't be read, a new one is started and any host routes an earlier run left stay, which is harmless: they send relays the way they would go anyway.

A Windows test (`client/core/tests/windows_backend.rs`) adds and removes a relay host route through the runner's gateway and checks that cleanup leaves the default route, and another program's host route made beforehand with a different metric, in place. It also checks that a game route on any interface but the adapter is refused, and that `best_route` still finds the physical route while a host route of ours that points elsewhere covers the address. CI runs it on windows-latest as administrator (job `windows-backend`, ADR 007).

**On Linux** (`net/linux/ip.rs`), development and tests only, every route PingPath adds carries `proto 80`, and cleanup deletes the `PingPath` link and flushes the routes with that protocol. That is enough on a development machine; it is not on players' PCs, where other software uses the same value.

### Finding the physical route

`Platform::best_route(dst)` returns how the physical network reaches `dst` with PingPath's own routes set aside, so a network change (a new gateway, Wi-Fi to cable) shows up even while a relay's host route still points where the network was.

- **Windows:** when none of PingPath's routes covers `dst`, the system's own answer (`GetBestRoute2`) is the physical route. When one does (the relay's host route, or a game range that happens to contain the relay), the choice is redone from the route table without PingPath's rows (the adapter's routes and metric-293 host routes), the way the IP stack makes it: the longest prefix, then the lowest route plus interface metric, among connected interfaces.
- **Linux:** `ip route get fibmatch`, and if the route it matched is ours, the best other route in the main table.

The session checks the route to its relays once a second and moves the host routes when it changed (ADR 009).

### Pinning the tunnel socket

On Windows the tunnel's sockets are pinned to the physical interface the first relay's route uses (`IP_UNICAST_IF`), and pinned again after a network change: a backstop to the host routes, so tunnel traffic can't loop into the adapter even while a host route is missing. A socket of the other address family than that route's gateway is left unpinned, since that interface was chosen for the other family. The same call turns off `SIO_UDP_CONNRESET`. On Linux the host routes alone do this.

### What the game server sees, and when to turn it on

While optimized, the game server sees the exit's address. Windows keeps a connected socket on the route and source address it started with: it caches the route on the socket, and its strong host model sends from an address only through that address's interface. So turning Optimize on reaches only connections the game opens afterwards, and a match's connection stays on the route it started with, either way, until the game reconnects. Some games also tie a match to the player's address (rule 6). Hence the note next to the switch in the app: turn it on before joining a match; switching it on or off during a match may need a reconnect. The app also shows how many game packets went through PingPath, with a hint to rejoin the match while it stays at zero.

The engine and network-namespace tests send their test traffic from unconnected sockets, which follow the routing table packet by packet, so they don't show this; it is for testers on Windows to confirm per game (PLAN.md Phase 7 records what happens when the tunnel is switched mid-match).

## Consequences

- Only the selected game's traffic to its listed IPv4 ranges is affected. A wrong definition can at worst block traffic to those few addresses while optimized, and the player is told.
- Narrow, IPv4, UDP-only definitions are a hard requirement for routable games, and the validator enforces it.
- Games whose servers need IPv6 or TCP can't be optimized until exits carry them.
- Cleanup can't remove a route PingPath didn't add, whatever other software does with route protocol values; the price is a ledger file to keep in step.
- Turning Optimize on mid-match doesn't help that match, and the app says so.
- Game traffic blocked by the filter shows in the status, raises a notice and is logged by reason, so a tester's diagnostic report shows which definition needs fixing.

## Alternatives considered

- **A default route through the adapter, with exclusions.** Against rule 5's spirit (everything enters the tunnel), and any client bug would break the whole connection.
- **Filtering by port with WFP or a packet-diverting driver.** Ruled out by rule 4.
- **Forwarding stray packets directly from user space.** Possible for UDP, not for TCP without a TCP stack; deferred until a real game needs it.
- **Routing IPv6 ranges to the adapter and dropping what arrives** (the proposal). Windows would send IPv6 around the adapter anyway, silently, and dropping it wouldn't make a UDP game fall back to IPv4.
- **Finding PingPath's routes by a fixed protocol value** (the proposal). Other software, and `route add` itself, use the same values; cleanup could delete the default route.
- **`10.200.0.2` as the adapter's address** (the proposal). It could be a real host on the player's network, unreachable while optimized.
