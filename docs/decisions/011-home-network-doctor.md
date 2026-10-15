# 011. Home network doctor

- **Status:** Accepted
- **Date:** 2026-10-14
- **Builds on:** [004](004-relay-wire-protocol.md), [006](006-control-plane.md), [007](007-client-architecture.md)

## Context

PLAN.md Phase 5: diagnostics inside the client, each with a plain-language fix in Arabic and English:

- Wi-Fi or Ethernet, signal strength, and 2.4 or 5 GHz band;
- a **bufferbloat test**: latency at idle and under load, graded A to F, with the load coming from project-run or self-hostable endpoints (rule 8), whose egress the cost model counts;
- processes using bandwidth in the background (per-process usage through Windows APIs, no hooking);
- DNS response time;
- router guidance: step-by-step instructions for SQM/QoS on common router brands, with links. Instructions only; no automatic router changes in v1.

Done when each check produces a result and a fix suggestion, and the bufferbloat grade matches a reference tool within one letter grade.

What Phase 3 built that this rests on: the engine runs in a LocalSystem service, and the window is an unprivileged app that talks to it over a named pipe (ADR 007). Relays answer PROBE packets on their UDP port, which the client already uses to measure each relay's round trip (ADR 004). The control plane hands out relay lists and short-lived tokens signed with its Ed25519 key, which relays check offline, and it caps what one network (/24 or /64) may ask for (ADR 006).

## Decision

### Where the checks run

In the **service**, behind one pipe request. Two of the checks need rights the window doesn't have: per-process network usage comes from a kernel event trace, which needs an administrator; and on Windows 11 24H2 and later the Wi-Fi details a desktop app reads need location permission, which a service doesn't. Running all of them in one place also keeps the window a display, as ADR 007 has it.

`{"method": "doctor"}` runs the checks one after another and answers with the whole result; while it runs, the service sends a `doctor` event as each check starts and ends, so the window can fill in as it goes. One run at a time; a second request while one runs is refused (`doctor_busy`). The checks' order, and why:

1. **Connection** (a second): what the rest depends on.
2. **Background traffic** (10 s): before the bufferbloat test, whose own traffic would swamp it.
3. **DNS** (a few seconds).
4. **Bufferbloat** (about 25 s): last, as it fills the line.

### The result

Every check returns the same shape: a **status** (`good`, `warning`, `problem`, `unavailable` when this computer can't run it, `failed` when it went wrong), its **figures** (numbers and names, never sentences), and **findings**, each a stable code with parameters. The window turns a code into a sentence and its fix in the player's language, as it does for the service's notices and errors, so the service has no text to translate and `tools/check-i18n.py` checks that every code the service can send has a sentence in both languages.

Every check also logs its figures and findings (PLAN.md section 9), so the diagnostic report carries the last run.

### Connection

The interface that carries traffic to the internet is the one Windows would use for a public address (`GetBestInterfaceEx`). Its row (`GetIfEntry2`) says Wi-Fi or Ethernet (the physical medium) and the negotiated link speed. If it isn't a physical adapter (another product's VPN), that is a finding of its own: everything else then measures that VPN.

On Wi-Fi, the WLAN API (`WlanQueryInterface`) gives the signal (RSSI in dBm, and the 0 to 100 quality), the Wi-Fi generation and the channel; the band comes from the access point's centre frequency (`WlanGetNetworkBssList`), or from the channel number when that is refused (1 to 14: 2.4 GHz; 32 to 177: 5 GHz).

Findings: Wi-Fi at all (a warning: Wi-Fi adds jitter and short bursts of loss that a cable doesn't; plug in if you can); a weak signal (warning below −67 dBm, problem below −75 dBm); the 2.4 GHz band (warning: crowded and slower; use the router's 5 GHz network if it has one); Ethernet negotiated below 100 Mbps (warning: often a damaged cable or a port set to 10 Mbps).

Linux, for development and the tests, reads the same from `/proc/net/route`, `/sys/class/net` and `/proc/net/wireless`, without the band.

### Background traffic

The Microsoft-Windows-Kernel-Network event provider reports every TCP and UDP send and receive with the process ID and size. The service runs a real-time trace session of it for 10 s and adds up bytes per process; the process's image name comes from its ID. This is how Resource Monitor shows network use per process: it reads events Windows already produces, and hooks or diverts nothing (rules 4 and 5). The trace session has a fixed name, so one left behind by a crash (it would run until Windows restarts) is stopped when the service starts, at uninstall, and before a new one starts. A trace that fails to deliver events is a failed check, not a quiet network.

Processes moving more than 100 kbit/s on average are listed with their rates, the service's own traffic excluded. A finding per process, with fixes for the common ones by image name (Steam, Epic, Battle.net and the like downloading; OneDrive, Google Drive and Dropbox syncing; Windows Update's delivery service; browsers streaming video) and a general one otherwise ("close it or pause it while you play"). Below 100 kbit/s is left out: a game in a match sends well under that, and so do idle apps. The check doesn't single out games; the player's own game can appear in the list, which is fine.

Not on Linux (`unavailable`).

### DNS

The resolvers are the ones the internet interface uses (`GetAdaptersAddresses`; `/etc/resolv.conf` on Linux). Each is asked for the address of five widely used names, twice: the first answer may come from the resolver's cache or not, the second does. The figures are the median of each, and how many went unanswered within 2 s.

Findings: no answer at all, or more than one in five unanswered (problem); a cached answer slower than 50 ms (warning: the resolver is far away or overloaded); a first answer slower than 150 ms (warning). For comparison the same names go to three public resolvers (1.1.1.1, 8.8.8.8, 9.9.9.9), and when the fastest of them is both twice as fast and 30 ms faster than the player's, the fix names it and how to set it in Windows. The check doesn't need them (rule 8): unreachable, they are left out. Every finding says that DNS doesn't change ping in a match; it changes how fast games and launchers connect.

### Bufferbloat

**Method.** The client measures latency to a load server for 5 s at idle, then while the server sends to it over eight TCP connections (download), then while it sends to the server over eight (upload), each loaded phase about 8 s. Latency is a small UDP ping every 100 ms. The figures per phase are the median latency, the ping loss, and the throughput the TCP connections reached; the first second of each loaded phase, while TCP is still speeding up, isn't counted. The **increase** is a loaded median minus the idle median.

**Grade**, on the larger of the two increases, with the thresholds of the widely used Waveform bufferbloat test, so that a player comparing the two sees the same scale: A+ under 5 ms, A under 30 ms, B under 60 ms, C under 200 ms, D under 400 ms, F at 400 ms or more. A loaded phase that lost more than half of its pings is graded F: the queue was so long the pings timed out.

Findings: the grade (C or worse is a problem, B a warning), with the fix of turning on SQM (or "Smart QoS") on the router, set to about 90% of the measured speeds, which the window's router guidance explains step by step; and, from the background check, when background processes used more than a quarter of the measured download or upload speed, that the test and the game both share the line with them.

**The load server** is part of the relay: an optional `[doctor]` listener (`listen`, a TCP and UDP port of its own) that any relay may enable, so it is self-hostable like the rest. Protocol, on TCP: the client sends a 2-byte length and a **doctor ticket**, then one byte, down or up; for down, the relay writes bytes until the ticket's time or byte budget is used; for up, it reads and drops them, and every 250 ms writes back how many it has received on the connection. The client counts upload speed from those reports: what it hands its own socket includes what sits in its send buffers, which grow through the phase (8% over the line's rate with a 600 ms queue in the test below). On UDP, the same port echoes a ping back, but only to an address that has a load connection open with a valid ticket: TCP's handshake proves the address, so the echo can't be turned on someone else (rule 10).

**Doctor tickets** are signed by the control plane with the key that signs session tokens, under a different signing context so neither can pass for the other, and checked offline by the relay: version, a random ticket ID, the relay's ID, issued and expiry times (60 s), and a byte budget per direction (250 MB). A relay counts each ticket's bytes and connections (at most 16 at once) in memory until it expires, and keeps no client address beyond the open connections (rule 9). A load connection ends when its ticket does (2 s after its expiry at the latest), and as soon as a write makes no progress for 10 s, so a client that stops reading can't keep its place. A relay runs at most 8 tests at once ("relay busy" beyond that). Before a connection has shown a ticket it costs a place among 512 pending ones and a token from its source network's budget (a /24 or /64: 4 a second, bursts of 40), so a connection flood is closed before any signature check. The client asks for one with `POST /v1/doctor/tickets` and the relay's ID; the control plane issues at most 4 per network per hour and 10 per day, counting a /24 for IPv4 and a /48 for IPv6 (a home holds a /56 or /48, so a /64 would give it hundreds of allowances), under keyed tags, and at most 120 per relay per hour whoever asks (`doctor_busy` beyond that, which the client shows as no test server for now). The relay list carries each relay's doctor address when it has one.

**Which relay**: the one with a doctor listener and the lowest round trip from the PROBE measurement the client already makes, so the test measures the player's own line and as little else as possible.

**Not while optimized.** The test fills the line for about 20 seconds, which would make a game in a match lag, so the bufferbloat check is skipped while the service routes a game (`unavailable`, with the finding `doctor_stop_optimizing`), and stops at once, with the same finding, if Optimize is turned on while it runs. The other checks run.

**Egress.** A test downloads at most 250 MB (a 250 Mbit/s line saturated for 8 s), and most far less: about 100 MB on a 100 Mbit/s line. Run weekly that is about 0.4 GB a month per player, against about 10 GB of game traffic (PLAN.md section 10), and the per-network limits cap what a misbehaving client can draw. The relay counts the bytes (`doctor_bytes_sent`, `doctor_bytes_received`) and the tickets refused, so operators can watch it.

### Router guidance

Content in the window, in both languages: for each common router brand (TP-Link, ASUS, Netgear, Huawei, ZTE, D-Link, Linksys, MikroTik, Ubiquiti, OpenWrt), where the SQM or QoS setting is, what to set it to, and a link to the maker's own instructions. Shown with the bufferbloat result and from the doctor page. Nothing is changed on the router.

### The test

- Each check's findings are unit-tested on fixed figures, and every code has a sentence in both languages (`check-i18n.py`).
- The bufferbloat grade, in the network-namespace test (CI job `client-netns`): `tools/netsim/client-netsim.sh shape <rate> <queue ms>` puts a token-bucket bottleneck with a queue of that many milliseconds on the player's line, both ways. With 20 Mbit/s and queues of 15, 120 and 600 ms, the grade must be within one letter of the one the queue implies (A, C, F), and of a reference measured on the same line: `ping` at 10 per second during `iperf3` with eight streams each way, graded on the same thresholds. Where `iperf3` isn't installed the reference is skipped, except in CI.
- The Windows-only pieces (the WLAN API, the event trace, interfaces and resolvers) have tests in the `windows-backend` job, on the runner's own adapters: the trace must count most of the bytes the test itself sends over UDP, and about half as much received (what it sent itself), so send and receive events can't be confused.
- On a real line, a tester runs the doctor and the Waveform test back to back and records both grades in `docs/benchmarks/bufferbloat.md`.

First runs: with 20 Mbit/s and queues of 15, 120 and 600 ms the doctor graded A, C and F (download and upload added 22 and 25 ms, 119 and 116 ms, 148 and 590 ms), the same as the queue implies and as the reference (+22, +122 and +541 ms). On the Windows runner the trace counted 1,502,000 of the 1,504,000 bytes the test process sent.

## Consequences

- The player gets the usual fixes for lag at home without a second tool, in their language, and a report that shows them.
- Relays that enable `[doctor]` carry bursts of TCP egress besides game traffic; the byte budget, the per-network limits and the metrics bound and show it.
- A second kind of signed credential; the separate signing context keeps a doctor ticket from passing as a session token.
- Rule 8: the bufferbloat test uses only our relays; the public resolvers are a comparison the DNS check can do without.

## Known limits

- The bufferbloat test measures the line to the nearest relay; a bottleneck past it (the ISP's international links) isn't separated from the home line.
- A line faster than the relay's own link, or than 250 Mbit/s for 8 s, can't be filled, and its grade can look better than the line is. The figures show the speed reached.
- Per-process usage is Windows-only, and so is the band.
- Waveform's thresholds are a convention; the grade is meant for comparing a line before and after a fix, not for exact numbers.

## Alternatives considered

- **Running the checks in the window**: no event trace without an administrator, and Wi-Fi details behind location permission on recent Windows.
- **A UDP flood at a fixed rate for the load**: needs the line's speed up front, and doesn't fill a queue the way TCP does; Waveform, Ookla's loaded latency and flent all use parallel TCP connections.
- **A third-party speed test server** for the load: rule 8.
- **Per-connection TCP statistics** (`GetPerTcpConnectionEStats`) for background traffic: TCP only, which misses QUIC video streams and other UDP.
- **A filtering driver** to see or limit per-process traffic: rule 4.
