# 002. Game definition format and validation

- **Status:** Accepted
- **Date:** 2026-10-03

## Context

Game definitions decide what the client routes and what the exit relay will forward to. A bad definition can route a player's LAN into the tunnel, pull unrelated traffic through relays, or route a game that was never tested against its anti-cheat. PLAN.md asks for YAML files validated by a JSON Schema in CI.

## Decision

- One YAML file per game, `games/<id>.yaml`, whose `id` must match the file name.
- `games/schema.json` (JSON Schema 2020-12) defines the structure.
- The `pingpath-games` Rust crate loads definitions for every component and enforces, on top of the schema:
  - CIDRs parse and have no host bits set;
  - no private, loopback, link-local, multicast, CGNAT or reserved space, and IPv6 only from global unicast (2000::/3);
  - no prefix broader than /16 (IPv4) or /32 (IPv6);
  - no overlapping ranges within a game, and unique region IDs;
  - documentation ranges (RFC 5737, RFC 3849, RFC 9637) are allowed as placeholders only in games the client never routes;
  - `supported` needs `report: docs/anticheat/<id>.md`, and the file must exist; `unsupported` needs a `status_reason`.
- Four statuses instead of three: **`untested`** is added as the default for new definitions. Like `unsupported` it is never routed, but it doesn't claim the game forbids tunneling. `testing` is routed with a warning.
- CI runs `pingpath-games validate games`. Tests run every fixture in `crates/games/tests/fixtures/invalid/`, each of which must fail with the problem named in its `# expect:` line.

## Consequences

- Community contributors can add a game without verified addresses; it simply can't be routed until someone verifies them and testing starts.
- The client, relay and CI can't disagree about what a valid definition is, because they share one crate.
- The prefix-breadth limits may need raising for a game hosted in large cloud blocks. Raise them deliberately in this crate, with a note here, rather than per game.
