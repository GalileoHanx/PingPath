# 001. Encrypt end to end from client to exit; entry relays only forward

- **Status:** Proposed (to be confirmed when Phase 2 starts)
- **Date:** 2026-10-03

## Context

PLAN.md v1 described two encrypted hops, client ↔ entry and entry ↔ exit, both WireGuard via `boringtun`, plus a separate packet sequence number layer so the exit relay can deduplicate packets sent over two paths (Phase 4).

That design has three problems:

1. **Dedup needs a header that WireGuard won't carry.** `boringtun` encapsulates IP packets and checks that what it decrypts is an IP packet, so a custom sequence header can't ride inside the tunnel without a second framing layer.
2. **Two crypto hops cost latency.** Every packet is decrypted and re-encrypted at the entry relay, which works against the Phase 2 target of under 1 ms added latency at p99.
3. **The entry relay sees too much.** It would see both the player's address and every destination.

## Decision

- The client and the **exit** relay share one WireGuard-protocol session (user-space, `boringtun`).
- **Entry** relays forward the client's encrypted packets to the exit for that session and back. A small outer header carries a session ID and a short-lived token signed by the control plane, which the entry checks so it can't be used as an open relay (PLAN.md rule 10). Entry relays hold no tunnel keys.
- **Dedup comes from the tunnel.** The client encrypts each packet once and may send the same ciphertext through two entry relays. Both copies carry the same WireGuard counter, and the exit's anti-replay window accepts the first and drops the second. The client does the same for return traffic.
- The **entry relay is optional.** The client may connect straight to the exit when that path measures better.
- **Failover prefers keeping the exit.** Switching entry relays doesn't change the address the game server sees; switching exits does.

## Consequences

- Entry relays are simple, fast and stateless apart from a session → exit table.
- Multipath needs no extra protocol layer. The client and exit must both track every path a session uses, rather than WireGuard's usual single "latest endpoint".
- The entry relay sees who but not where; the exit sees where but, behind an entry, not who.
- The exit relay is the single point that decrypts, so the allowlist (rule 10) and NAT live there.
- To revisit: whether the outer header should be authenticated per packet (a MAC) or only when a session is set up, given the CPU budget on small instances.
