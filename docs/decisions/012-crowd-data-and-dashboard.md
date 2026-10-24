# 012. Crowd data, the public dashboard and recommended paths

- **Status:** Proposed
- **Date:** 2026-10-24
- **Builds on:** [003](003-probe-data-sources.md), [006](006-control-plane.md), [007](007-client-architecture.md), [009](009-path-selection-and-failover.md)

## Context

PLAN.md Phase 6:

- opt-in telemetry from the client and the probe CLI: game, region, city (coarse), ISP ASN, direct latency, optimized latency, loss, jitter, timestamp;
- client addresses truncated to /24 (IPv4) or /48 (IPv6) at ingestion, before the ASN and city lookup, so no full address is ever written; data aggregated; any city, ISP or game bucket with fewer than 20 users hidden;
- a dashboard in Arabic (right to left) and English: a route map (per game and city, the best relay path and the typical improvement), an ISP comparison (typical ping per ISP to each game region) and live status (relay health and the current improvement per region);
- the control plane recommends a starting relay pair to new users.

Done when the dashboard shows real aggregated data, the privacy threshold is enforced by tests, and new clients receive a recommended path within 1 s of selecting a game.

Rules this touches: 9 (telemetry opt-in, no full addresses stored, only aggregates published), 8 (nothing may depend on another company), 3 (only the region the player would use).

What exists:

- **The control plane** (ADR 006) is one axum binary holding everything in memory: the relay registry from its config, the relays' latency reports (relay to relay, exit to game region) for the last 2 minutes, sessions, and per-network counters behind keyed hashes it never writes down. It learns the client's address from the socket or a configured proxy header. It has no database and no address lookup, and nothing reaches the client but relay lists, latency figures and tokens; the client chooses its path itself (ADR 009).
- **The client** measures every region of a game directly (5 pings each), then fetches `/v1/relays` and `/v1/latency`, probes the relays (about 2 s) and ranks paths: 3–4 s plus two HTTP round trips after the player presses Measure. Selecting a game starts nothing. While optimized it measures the path every second over a 10 s window (round trip, jitter, loss), but keeps no totals for a session, and its direct measurement drops the jitter the probe library computes.
- **The probe CLI** measures direct latency only (it never uses the relays) and finds the player's ISP by sending the full public address to Team Cymru's DNS service; its reports stay on the machine.
- **The dashboard** directory holds a README. The window's frontend (plain JavaScript, Arabic and English, a Playwright smoke test, `check-i18n.py`) is the model for a static page.

## Decision

### What is sent

Three kinds of record, each a few numbers and identifiers, never free text:

| Kind | From | When | Contents |
|---|---|---|---|
| `measurement` | client | after each measurement | the game; for **each** region measured, the direct round trip, loss and jitter; for the region it chose, the best path's entry and exit relay IDs and its estimate |
| `session` | client | when an optimized session ends, if it lasted at least 60 s | the game and region, the entry and exit relay IDs, the direct round trip measured before it, and over the session the median and 95th percentile of the optimized ping, the loss and the jitter; its length in minutes, rounded; whether Stability mode duplicated traffic |
| `probe` | probe CLI | after a run, with `--share` | the game; for each region, the best target's round trip, loss and jitter and how it was measured (ICMP or TCP) |

Each report also carries the client or probe version and a **contributor ID** (below). Times are the server's, rounded down to the hour; the client's clock isn't used. Not sent: the player's address (the server sees it, see below), the city, the ISP, coordinates, adapter names, game server addresses, hop addresses, or anything typed.

The client gains the numbers it lacks: direct measurements keep their jitter (`Leg.jitter_ms`), and a session keeps a running summary of its per-second samples that it returns when it ends.

### Consent

Off by default, everywhere:

- **Client:** a Settings switch, *Share anonymous measurements*, stored in `client.toml` as `share_measurements = false`. Next to it, the window lists what is sent and shows the last report sent, exactly as sent. A one-time card on the main page asks once; closing it or saying no leaves sharing off. The service sends reports only while the switch is on, and they never block or slow measuring or optimizing.
- **Probe CLI:** `--share` for one run. It prints the records and then sends them, and saves them next to the report (`.shared.json`). Without it nothing is sent.
- Turning sharing off deletes the contributor ID on the machine and asks the server to delete that contributor's records (`DELETE /v1/crowd/contributor`).

### The contributor ID

A random 128-bit number, made when sharing is turned on, kept in its own file in the data folder (not in `client.toml`, which is logged whole, and not in the diagnostic report), and replaced every 30 days. The probe keeps its own in its log folder. It exists only so the server can count **people**, not reports, per bucket, and give each person one voice in a median. The server stores a keyed hash of it, never the ID itself.

### Ingestion

`POST /v1/crowd/reports`: `{contributor, client, records: [...]}`, at most 20 records and 16 KiB. In order:

1. **Truncate.** The address (socket or proxy header, as for sessions) becomes a `NetworkPrefix`: the /24 for IPv4 (IPv4-mapped addresses as IPv4), the /48 for IPv6. Nothing past this step sees the address; the lookup functions take a `NetworkPrefix` and nothing else, so the type system holds the rule.
2. **Look up** the prefix's ASN, ISP name, country and city in two local database files (below).
3. **Tag** the prefix with a keyed hash, for rate limits and for the network-diversity rule below. Then the prefix is dropped: what is stored is the ASN, the country, the city and the tag, never the prefix.
4. **Check** each record: known game and region, known relay IDs, numbers in range (round trips 0–2000 ms, loss 0–100 %, jitter 0–1000 ms). A report with any bad record is refused whole (`invalid_report`).
5. **Limit**, per network tag: 60 reports an hour, and at most 10 new contributor IDs a day. Per contributor: 20 reports an hour. Over a limit the report is refused (`crowd_limit`, with `Retry-After`).
6. **Store** one row per record, with the contributor's keyed hash, in the crowd database.

Addresses and contributor IDs are never logged; the log says how many records were stored or why a report was refused, as the session log does.

### City and ISP: offline databases

**DB-IP's free "IP to City Lite" and "IP to ASN Lite"** databases: MMDB files, updated monthly, licensed CC BY 4.0, downloaded without an account. The control plane reads them with the `maxminddb` crate at start, and again when the files change. `infra/update-geo.sh` fetches the month's files. The dashboard credits DB-IP, as the licence requires. Nothing is asked of another company at run time (rule 8). Where a database has no city for a prefix, the country stands in for it.

The probe CLI keeps its own Team Cymru lookup for its local report, but `--share` doesn't send its result: the server finds the ISP itself from the truncated address.

### Storage

**SQLite**, one file (`crowd.sqlite`) in the control plane's state directory, through `rusqlite` with its bundled SQLite, written from a blocking task. PLAN.md suggests PostgreSQL with TimescaleDB, but the volume doesn't need a database server: 10,000 contributors sending 10 records a day for 30 days is 3 million rows, a few hundred MB. A single file keeps the control plane a single binary that a community can host. If it outgrows SQLite, the store is behind one module.

Rows older than **30 days** are deleted every hour. The crowd store is optional: without a `[crowd]` section the control plane runs as it does now, and the crowd and public routes answer 404.

### Aggregation and the privacy threshold

Every 10 minutes the control plane recomputes the published tables from the last 30 days, in memory:

- **One voice each.** For each bucket, each contributor's records are reduced to that contributor's median. The bucket's figures are the median and 90th percentile of those per-contributor values, so one contributor sending many reports moves a figure no more than one sending few.
- **The threshold.** A bucket is published only with **at least 20 contributors**, from **at least 5 networks** (distinct tags). The second condition makes it expensive to fill a bucket with invented contributors from one place, which otherwise would let someone read one real player's figure from the median.
- **Coarse output.** Figures are rounded to the millisecond, and counts are given as bands (20+, 50+, 100+, 500+, 1000+), never exact, so one more contributor doesn't show.

The buckets:

| Table | Bucket | Figures |
|---|---|---|
| Routes | game, region, country, city | direct ping; optimized ping; improvement (each contributor's direct minus optimized); the best path: among relay pairs used by at least 5 of the bucket's contributors, the one with the lowest optimized ping |
| ISPs | game, region, country, ASN (with its name) | direct ping (median and 90th percentile), loss, jitter |
| Status | relay, and game region | relays: up if a latency report arrived in the last 60 s, with their round trips to peers and regions. Regions: the improvement from sessions in the last 24 h, under the same threshold |

The tables are published read-only at `GET /v1/public/routes`, `/v1/public/isps` and `/v1/public/status`, with `generated_at`, `Cache-Control: max-age=300` and `Access-Control-Allow-Origin: *`, since the dashboard is served from another origin.

### Recommended paths

`GET /v1/recommendation?game=<id>`, answered from tables already in memory:

1. the requester's address becomes a `NetworkPrefix` and is looked up, as at ingestion, and then dropped; nothing is stored or logged;
2. for each region of the game, the best path from the routes table for the most specific bucket that is published: ASN within country, then city, then country;
3. failing all of those, a path from the relays alone: the exit with the lowest round trip to the region, and the entry in the requester's country on the same provider (PLAN.md section 7).

The answer gives, per region, the entry and exit relay IDs, the typical optimized and direct ping when they come from the crowd, and where it came from (`crowd` or `relays`). Asking needs no consent: it sends nothing the server keeps, as `/v1/relays` doesn't.

**In the client.** Selecting a game asks the service for the recommendation at once (`recommend`, a 1 s timeout). The window shows it before any measurement: "Players on your network usually get 48 ms through Bahrain → Frankfurt". When the player measures, the recommended pair is probed first and is always among the candidates. But ranking, the usual-region rule and the tunnel's check before routes go in decide as before (ADR 009): a recommendation never routes anything by itself. The client also keeps the relay list and latency figures it fetched for a minute, so measuring right after selecting a game doesn't fetch them again.

### The dashboard

A static site in `dashboard/`, written like the window: plain JavaScript, no build step, `en.json` and `ar.json`, `dir="rtl"` for Arabic, checked by `check-i18n.py` and smoke-tested with Playwright against fixture data. Three pages:

- **Routes:** choose a game. Per city, the usual region, direct and optimized ping, the improvement, and the best path.
- **ISPs:** choose a game, region and country. Each ISP with its typical ping, loss and jitter.
- **Status:** each relay, up or not, and each region's current improvement.

It reads one setting, the control plane's address (`config.json`), and is served as files, for example by the same Caddy that fronts the control plane. Buckets below the threshold are absent from the API, so the page has nothing to hide. Where a city or ISP is missing, it says the data isn't there yet.

### The tests

- **Threshold:** unit tests on the aggregation, for 19 and 20 contributors, 20 contributors from 4 networks, one contributor flooding a bucket, and the banding.
- **Nothing identifying is written:** an end-to-end test sends reports from known addresses through the proxy header. It then searches the database file and the logs for each address, its /24 or /48, and each contributor ID, and finds none.
- **Real aggregated data:** the end-to-end test sends records from 25 contributors on 6 networks to a real control plane. It reads the public routes back, and the dashboard's smoke test renders a copy of them.
- **1 s:** a test times the recommendation from the service's `recommend` request to its answer, against a real control plane, and requires under 1 s.
- **Consent:** the service sends nothing while `share_measurements` is off; a test counts the requests a fake control plane receives.

### Order of work

1. **P6-2:** the crowd store, ingestion with truncation and lookups, limits, and the contributor delete.
2. **P6-3:** aggregation, the threshold, and the public routes.
3. **P6-4:** recommendations: the endpoint, the client's `recommend`, and the first probe.
4. **P6-5:** reporting from the client and the probe, the consent switch and card, and the last report shown.
5. **P6-6:** the dashboard.
6. **P6-7:** the done-when tests, docs, CI, and the merge.

The server comes first so the client has something to report to and be recommended by.

## Consequences

- The control plane writes to disk for the first time: a state directory in its systemd unit and Docker volume, a backup note in the operator guide, and two data files to refresh monthly.
- Opting in is the player's choice, so the data will be thin at first, and most buckets stay hidden until enough players share. The recommendation falls back to relay figures until then.
- One person can't move a published figure much: they get one voice per bucket, and new contributor IDs are capped per network. A group with many networks still could. That is the limit of what a pseudonymous, opt-in design can resist without accounts.
- The client sends nothing unless asked. Even so, sharing reveals to our server which games a network plays and how often. That is why only the server's own aggregates leave it.
- Rule 8 holds: the address databases are files on our own machines. The probe's own ISP lookup still asks Team Cymru for its local report, as ADR 003 records.

## Known limits

- City accuracy is the database's: often the capital for mobile networks and CGNAT. The pages say "city as the address suggests".
- A VPN in front of the client makes its records look like the VPN's ISP. The connection check already warns about a virtual adapter. Records aren't filtered by it.
- Records can't be deleted for a contributor ID that was already replaced. Rotation means the oldest are gone within 30 days anyway.

## Alternatives considered

- **PostgreSQL with TimescaleDB** (PLAN.md's suggestion): more than the volume needs, and a second service for every self-hoster. Left for later behind the store module.
- **Counting networks instead of contributors:** needs no ID, but undercounts players behind CGNAT, where many share an address. Both are used: contributors for the count, networks for diversity.
- **MaxMind GeoLite2:** comparable data, but needs an account and a licence key per installation; DB-IP Lite needs neither.
- **The client looking up its own city and ISP:** would send the full address to a third party, as the probe does now. The server already sees the address, and can look up the truncated one itself.
- **A recommendation pushed in `/v1/relays` or `/v1/latency`:** those are the same for everyone. The recommendation depends on the requester's network, and selecting a game must answer before any measurement, so it gets its own route.
- **Publishing exact counts and means:** a mean moves with one outlier, and an exact count shows when one person joins. Medians and bands do neither.
