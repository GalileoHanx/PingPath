# Logs: sending test results and problems back

When you try PingPath and something goes wrong, or you just want to share how a test went, this is how to get me the information I need.

## 0. Run the probe (Phase 1 testing)

Double-click `pingpath-probe.exe` from the Windows test build (see the main README), or run `pingpath-probe run <game> --from <your city>`. It saves its results in the log folder, so step 2 picks them up automatically.

## 1. Reproduce with detailed logs (optional, but helps)

Run the component again with debug logging, then do whatever triggered the problem.

```powershell
# Windows (PowerShell)
$env:PINGPATH_LOG = "debug"
pingpath-probe ...        # from a source checkout: cargo run -p pingpath-probe -- ...
```

```sh
# Linux / macOS
PINGPATH_LOG=debug pingpath-probe ...
```

From Phase 3 on, the client has a "Verbose logging" switch in its settings instead.

## 2. Create a diagnostic report

```sh
pingpath-report
# from a source checkout:
cargo run -p pingpath-diagnostics --bin pingpath-report
```

It writes `pingpath-report-<date>-<time>.md` in the current folder and prints the path. From Phase 3 on, the client also has a "Copy diagnostic report" button.

The report contains: PingPath versions, your OS, PingPath settings, every warning and error, your latest probe results, and the latest lines from each component's log. Public IP addresses are shortened (e.g. `203.0.113.x`) and your home folder shows as `~`. **Read it before sharing**, and delete anything you'd rather keep private.

Logs themselves live in:

| OS | Folder |
|---|---|
| Windows | `%LOCALAPPDATA%\PingPath\logs` |
| macOS | `~/Library/Logs/PingPath` |
| Linux | `~/.local/state/pingpath/logs` |

## 3. Add a few lines of context

At the top of the report (or in your message), write:

- **What you did:** commands, game, region, Optimize on or off
- **What you expected**
- **What happened:** error text, in-game ping before and after, when it started
- **Anything that changed:** Wi-Fi vs. cable, new router, VPN, time of day

## 4. Send it, any one of these

1. **In the chat:** upload the report file in your Claude session (quickest).
2. **GitHub issue:** open an issue with the *Problem or test report* form and attach the file.
3. **This folder:** copy the report here, named `YYYY-MM-DD-short-topic.md`, and commit it. I'll read it in the next session.

Only `.md` files are committed from this folder; raw `.log` files are ignored by git on purpose, because they aren't redacted.

> If the repository is public, anything committed here is public too. Reports shorten IP addresses, but your tester profile includes your city and ISP. Leave out anything you don't want public, or send it in the chat instead.

## Tester profile

Fill in [`tester-profile.md`](tester-profile.md) once, and update it when something changes. Then reports only need to describe the test itself.
