# Bufferbloat grade: PingPath against a reference

PLAN.md Phase 5's done-when: the network doctor's bufferbloat grade matches a reference tool within one letter grade ([ADR 011](../decisions/011-home-network-doctor.md)). The grade uses the thresholds of the Waveform bufferbloat test: A+ under 5 ms added under load, A under 30, B under 60, C under 200, D under 400, F from 400 ms.

## On test queues (CI, every push)

`client/core/tests/netns/bufferbloat.rs` in the `client-netns` job: a 20 Mbit/s line with a token-bucket queue of a known length, both ways (`tools/netsim/client-netsim.sh shape`), measured by the doctor and by `ping` (10 a second) during `iperf3` with eight streams each way.

| Queue | PingPath (added down / up) | Implied by the queue | ping + iperf3 |
|---|---|---|---|
| 15 ms | A (+22 / +25 ms) | A | A (+22 ms) |
| 120 ms | C (+119 / +116 ms) | C | C (+122 ms) |
| 600 ms | F (+148 / +590 ms) | F | F (+541 ms) |

First run, 2026-10-14, Linux 6.18 network namespaces.

## On real lines (testers)

Not run yet: it needs PingPath on Windows and a relay with a doctor listener near the tester. To add a row:

1. Close other programs that use the network, and use the same PC and connection for both tests.
2. In PingPath, open **Network check** and run it; note the bufferbloat grade and the added ping while downloading and uploading.
3. Straight after, run the Waveform test (`https://www.waveform.com/tools/bufferbloat`) in a browser; note its grade and its download and upload "active" latency increases.
4. Run both again in the other order, so a change in the line between them shows.
5. Add a row below, and attach PingPath's diagnostic report (it logs the figures).

| Date | City, ISP, connection | PingPath grade (down / up added) | Waveform grade (down / up added) | Within one letter |
|---|---|---|---|---|
