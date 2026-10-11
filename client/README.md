# client

The Windows client, built in PLAN.md Phases 3–5. How it fits together:
docs/decisions/007-client-architecture.md; building and testing it:
docs/client-development.md.

- `core/` (`pingpath-client-core`): the engine (measuring, path selection,
  the fail-open watchdog, failover and fallback), the split-tunnel routes
  and packet filter, and the platform backends: Wintun and IP Helper on
  Windows, a TUN device and `ip` on Linux (development and tests).
- `service/` (`pingpath-service`): runs the engine as a Windows service and
  serves the app over a named pipe (a Unix socket on Linux).
- `ui/`: the Tauri app, in Arabic and English (choose a game, direct vs.
  optimized ping, one "Optimize" toggle, live graph). Its own cargo
  workspace, so Linux builds of the root workspace skip it.
