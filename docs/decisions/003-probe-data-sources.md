# 003. Probe measurement methods and data sources

- **Status:** Accepted
- **Date:** 2026-10-03

## Context

Phase 1's probe must measure latency, loss and jitter to each game region, trace the route, and name the player's ISP and approximate location, before PingPath runs any servers of its own. It runs on players' Windows PCs, usually without administrator rights. Rule 8 forbids features that need ISPs, game developers or telecoms to cooperate.

## Decision

**Measuring**
- **ICMP without admin rights.** Windows: the ICMP helper API (`IcmpSendEcho2`), which `ping` and `tracert` use. Linux: ping sockets (`SOCK_DGRAM` + `IPPROTO_ICMP`), allowed when `net.ipv4.ping_group_range` includes the user. IPv4 only for now; other systems get a clear "unsupported" error.
- **TCP connection time** for targets that drop ICMP: a `ping_targets` entry written `address:port`. A refused connection still measures a round trip.
- **Traceroute mtr-style:** every TTL probed several times in parallel, so each hop gets loss, latency and jitter rather than a single sample. On Linux, routers' "time exceeded" replies come back through `IP_RECVERR`.
- **UDP and WireGuard throttling** against `pingpath-probe echo-server`, which we host ourselves: plain and WireGuard-shaped packets of the same size are compared. The server only ever echoes, never answers bigger than it was asked, and rate-limits each source.

**Data sources**
- **Valve relays.** For games on Valve's Steam Datagram Relay (`sdr_app_id`), targets come from the relay list Valve publishes for Steam clients (`GetSDRConfig`), read at probe time for the region's `sdr_pops`. This reads public data; it needs no cooperation from Valve. `pingpath-probe sdr-import` prints the ranges for the game definition.
- **Public IP and ISP over DNS.** OpenDNS (`myip.opendns.com`) or Google (`o-o.myaddr.l.google.com`) for the public IP; Team Cymru's IP-to-ASN service for AS number, name and registry country. No HTTP API, account or key.
- **Location:** `--from <city>` or `--from-coords`, otherwise the ISP's registry country mapped to its main city. There is no automatic city detection: that would need a third-party geo-IP service. The control plane can offer it from an offline database in Phase 6.

## Consequences

- The probe works on a stock Windows PC with no install and no admin rights, which is what testers have.
- Valve, OpenDNS, Google and Team Cymru are read-only data sources, not partners. If one disappears, its part of the report says "unavailable" and the rest still works. Phase 2's control plane takes over the public-IP lookup and serves relay lists.
- Reports show route hops unredacted (they are ISP and Valve infrastructure), while the player's own public IP is cut to its /24. `pingpath-report` shortens every public address again before sharing.
- Not covered yet: IPv6 ICMP and traceroute, and traceroute on macOS.
