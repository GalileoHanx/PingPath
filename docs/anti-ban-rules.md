# Anti-ban rules

Copy of [PLAN.md section 2](../PLAN.md#2-hard-rules-anti-ban-and-scope). Keep the two identical; change PLAN.md first.

The coding agent must follow these in every phase. They exist so users never risk an account ban.

1. **Network level only.** Never read or write game memory, never modify or inject into game files or processes, never hook game APIs.
2. **Never modify game packets.** Game traffic is only *encapsulated* (wrapped in a tunnel) and unwrapped unchanged at the exit relay. Payload bytes are never altered, reordered by intent, or inspected beyond headers needed for routing.
3. **No region spoofing.** Only route to the game-server region the player would normally use. The exit relay must sit in the same region as the target game server. Never use the tool to get into another region's matchmaking.
4. **Minimal drivers.** Use only the signed, widely used **Wintun** driver (from the WireGuard project) for the virtual network adapter. Do not use packet-diverting or filtering drivers such as WinDivert, and do not write custom kernel drivers.
5. **Route by destination, not by process hooking.** Decide what goes through the tunnel using destination IP ranges of game servers (split tunneling via the routing table). No process injection to detect games.
6. **Fail open.** If a relay or the tunnel fails, traffic must fall back to a working route within 1–2 seconds so the player's connection is never left broken by our tool. Fail over between entry relays first: that keeps the same exit relay, so the game server sees no change. Falling back to the direct route changes the address the game server sees, and games that tie a session to the client's address may need to reconnect. The client must say so in plain words rather than promise a seamless switch.
7. **Per-game compatibility list.** Each game has a status: `untested` (the default for new definitions, never routed), `testing` (routed with a warning while compatibility tests run), `supported` (tested against its anti-cheat, with a published report) or `unsupported` (never routed). Games whose rules forbid VPN/tunneling tools are marked `unsupported`.
8. **No external dependencies on companies.** No features that require contacting ISPs, game developers, or telecoms. Everything must work on the project's own infrastructure.
9. **Privacy by default.** Telemetry is opt-in, never stores full IP addresses, and only publishes aggregated data. Relays hold client addresses in memory only for the life of a session and never log them.
10. **No open relays.** Relays carry traffic only for clients holding a valid, short-lived session token, only to allowlisted game destinations, and within per-session rate limits. A relay that could be used to flood a game server would get every user's traffic blocklisted.
