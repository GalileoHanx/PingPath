# 005. Exit relay egress: user-space UDP forwarding

- **Status:** Accepted
- **Date:** 2026-10-03
- **Builds on:** [004](004-relay-wire-protocol.md) (sessions and tokens)

## Context

The exit relay receives the client's IP packets out of the WireGuard tunnel and must deliver them to the game server and carry the replies back, without altering game payloads (PLAN.md rule 2) and only to allowlisted destinations (rule 10). The usual WireGuard server setup (a TUN device plus kernel NAT) needs root, is Linux-specific, and is hard to test without a machine to break.

## Decision

The exit forwards **UDP in user space**. For each packet that comes out of a session's tunnel, in order:

1. **Parse.** IPv4, UDP, no fragments, lengths consistent. Checksums aren't checked: the packet already authenticated, and the exit rebuilds the headers anyway. Anything else is dropped and counted by reason: `not_ipv4`, `not_udp`, `fragment`, `malformed_packet`.
2. **Session rate limit**, charged here, after the packet authenticated (1,000 packets/s and 2 Mbit/s each way by default).
3. **In-tunnel echo.** A packet to `100.64.0.1:7` is answered by the exit itself, never forwarded (ADR 004).
4. **Never to our own infrastructure.** Destinations that are this relay's own address or any relay in its config are refused (`denied_infrastructure`), so the tunnel can't be used to reach relays from inside.
5. **Allowlist.** The destination address must be within the `server_ranges` of the token's **region** of the token's game, the port within the game's UDP `ports`, and the game's status `testing` or `supported` (rule 7). Otherwise `not_allowlisted`. Game definitions can't list well-known reflection ports (DNS, NTP, SSDP, memcached and others) or more than 2,048 UDP ports, so even a bad definition can't turn exits into an amplifier.
6. **Per-destination cap.** Traffic to one server address and port, from all sessions together, is limited (20,000 packets/s and 100 Mbit/s by default), so many sessions can't add up to a flood (`destination_limited`).
7. **Flow.** Each flow, keyed by the client's inner source address and port and the destination address and port, gets its own UDP socket **connected** to the destination. A session may hold 8 flows and open 2 new ones a second (bursts up to 8); the relay holds at most 20,000 (`flow_limit`, `flow_rate`).
8. **Send** the UDP payload unchanged through the flow's socket. The operating system builds the outer headers with the exit's address: that is the NAT.

**Replies.** Because the flow's socket is connected, the kernel only delivers datagrams from that server address and port. Each reply is checked against the session's downstream rate limit, wrapped in an IPv4/UDP packet addressed back to the client's inner address and port, encrypted in the session's tunnel and sent on its current return path (ADR 004). A reply too big for the tunnel MTU (1352 bytes of UDP payload) is dropped (`reply_too_big`) rather than fragmented.

**Closing flows.** A flow closes after 120 s without traffic, or after 10 s if the server has never answered, so probes at silent addresses don't hold sockets. Flows close with their session.

## Consequences

- No root, no TUN device, no iptables: the relay runs as an unprivileged user on any Linux VPS, and the whole data path runs in tests on one machine.
- Payload bytes are never touched; only headers are rebuilt, as any NAT does.
- The OS picks each flow's source port, so the game server sees one stable `exit address:port` per flow for the life of the session; renewal keeps the session and so the ports.
- One socket per flow costs a file descriptor and an ephemeral port. The relay-wide flow cap keeps both bounded; the systemd unit raises the file limit, and `infra/systemd/99-pingpath-relay.conf` widens the ephemeral port range and reserves the relay's own ports.
- **TCP isn't forwarded yet.** Game traffic is UDP for the games in scope; TCP (if a game turns out to need it) would need a user-space TCP stack and gets its own ADR.
- **IPv6 isn't forwarded yet**; inner IPv6 packets are dropped and counted. The relay's own sockets can still be reached over IPv6.
