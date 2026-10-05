# tools

- `probe/` (`pingpath-probe`): measures latency, jitter, loss and routing detours to a game's server regions, names the ISP and gives a verdict per region. Its JSON report format is published in [`probe/report.schema.json`](probe/report.schema.json). See the main README for usage.
- `netsim/`: simulated networks built from Linux network namespaces. `three-hop.sh` makes client → router → router → server, optionally with `--delay`/`--loss` (netem). CI traces a route through it. `client-netsim.sh` makes the player's PC, the relays with the control plane, and the game servers; CI runs the client engine there on a real kernel (`client/core/tests/netns/`), checking the Phase 3 done-when criteria.
- `windows/`: files shipped with the Windows test build.
- `check-anti-ban-rules.sh`: CI check that `docs/anti-ban-rules.md` matches PLAN.md section 2.
