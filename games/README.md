# Game definitions

One YAML file per game, `<id>.yaml`, describing its server ranges, ports, regions and anti-cheat status. The client routes only these ranges, and the exit relay forwards only to them. The format is defined by [`schema.json`](schema.json); the full rules are in [ADR 002](../docs/decisions/002-game-definitions.md).

## Adding a game

1. Copy an existing file to `<id>.yaml` (kebab-case, e.g. `rocket-league.yaml`) and set `id` to match.
2. Leave `status: untested`. A new game is never routed until compatibility testing starts (PLAN.md Phase 7).
3. Fill in `server_ranges` per region with the narrowest prefixes you can verify, and say in `notes` where they came from. Until you have verified ranges, use documentation placeholders (`192.0.2.0/24`, `198.51.100.0/24`, `203.0.113.0/24`).
4. Validate:

   ```sh
   cargo run -p pingpath-games -- validate games
   ```

Statuses:

| Status | Routed? | Requires |
|---|---|---|
| `untested` | Never | Nothing; the default |
| `testing` | Yes, with a warning | Verified IPv4 ranges, UDP ports only, and at least one `ping_targets` entry per region outside the server ranges |
| `supported` | Yes | The same, and `report: docs/anticheat/<id>.md` |
| `unsupported` | Never | `status_reason` |

Ping targets are how the client measures the direct route and how exits measure their distance to the region (docs/decisions/009-path-selection-and-failover.md), so a region without one can never be optimized. Pick addresses near the game's servers that answer ICMP or TCP, outside the routed ranges.
