# 012. Crowd data, the public dashboard and recommended paths

- **Status:** Proposed
- **Date:** 2026-10-24
- **Builds on:** [003](003-probe-data-sources.md), [006](006-control-plane.md), [007](007-client-architecture.md), [009](009-path-selection-and-failover.md)

## Context

PLAN.md Phase 6:

- opt-in telemetry from the client and the probe CLI: game, region, city (coarse), ISP ASN, direct latency, optimized latency, loss, jitter, timestamp;
- client addresses truncated to /24 (IPv4) or /48 (IPv6) at ingestion, before the ASN and city lookup, so no full address is ever written; data aggregated; any city, ISP or game bucket with fewer than 20 users hidden;
- a dashboard in Arabic (right to left) and English: a route map (per game and city, the best relay path and the typical improvement), an ISP comparison (typical ping per ISP to each game region) and live status (relay health and the current improvement per region);
- the control plane recommends a starting relay pair to new users (faster first connection).

Done when the dashboard shows real aggregated data, the privacy threshold is enforced by tests, and new clients receive a recommended path within 1 s of selecting a game.

Rules this touches:

- **Rule 9:** telemetry is opt-in, no full addresses are stored, and only aggregates are published.
- **Rule 8:** nothing may depend on another company.
- **Rule 3:** only the region the player would use.
- **Section 9:** every failure leaves a log line that explains it.

What exists:

- **The control plane** (ADR 006):
  - It is one axum binary holding everything in memory: the relay registry from its config, the relays' latency reports for the last 2 minutes, sessions, and per-network counters.
  - The counters sit behind keyed hashes whose key is random per process.
  - It learns the client's address from the socket, or from a configured proxy header.
  - It has no database and no address lookup. Nothing reaches the client but relay lists, latency figures and tokens; the client chooses its path itself (ADR 009).
- **The client:**
  - Selecting a game starts nothing. After the player presses Measure, it measures every region directly (5 pings each), fetches `/v1/relays` and `/v1/latency`, probes every relay (about 2 s) and ranks paths: 3–4 s plus two HTTP round trips.
  - Its direct measurement drops the jitter the probe library computes.
  - While optimized it measures the path every second over a 10 s window, but keeps no totals for a session.
  - One service serves every signed-in Windows user, and its settings file is machine-wide.
- **The probe CLI** measures direct latency only, and never through relays. It finds the player's ISP by sending the full public address to Team Cymru's DNS service. Its reports stay on the machine.
- **The dashboard** directory holds a README. The window's frontend is the model for a static page: plain JavaScript, Arabic and English, a Playwright smoke test and `check-i18n.py`.

## Decision

### Where it lives

PLAN.md's "measurement service" becomes a module of the control plane (`crowd`), not a second server. It needs the relay registry and the address the control plane already sees, and it keeps one binary for self-hosters.

The module is optional. Without a `[crowd]` section:

- reports and the crowd tables are off, and their routes answer 404;
- relay status and recommendations work from the relays alone.

A failure in the crowd store never affects sessions. It runs on its own threads, and its errors stay there.

The record types and their validation live in a small shared crate, `pingpath-crowd`, used by the control plane, the client and the probe, so the two ends can't drift.

### What is sent

The fewest fields that the tables below use, as numbers and identifiers, never free text:

| Kind | From | When | Contents |
|---|---|---|---|
| `measurement` | client | after each measurement | the game. For **each** region measured: the direct round trip, loss and jitter. For the region chosen: the best path's entry and exit relay IDs |
| `session` | client | when an optimized session of at least 60 s ends | the game and region; the exit and the first entry; the direct round trip measured before it; and over the session: the median ping, loss as lost over sent, and the median jitter. The ping is what the client shows: the tunnel's echo round trip plus the exit's measured round trip to the region (ADR 009) |
| `probe` | probe CLI | after a run, with `--share` | the game. For each region: the best target's round trip, loss and jitter, and how it was measured |

Each report also carries the client's or probe's version and the person's **contributor ID** (below).

Not sent:

- the player's address (the server sees it; see Ingestion);
- the city, the ISP or coordinates;
- the session's length;
- adapter names, game server addresses, hop addresses, or anything typed.

The server stamps each record with the hour it arrived. After 24 hours, which only the status page needs, it rounds that down to the day.

The client gains what it lacks:

- **Jitter:** direct measurements keep their jitter, on `RegionMeasurement`. `Leg` also describes relay legs, so it stays as it is.
- **Session summary:** the engine already receives every per-second sample. It keeps a running summary of the session's samples itself, so a session that ends badly still has one.

### Consent

Off by default, and **per Windows user**: one PC is often shared, by a family or in a gaming café.

**In the window:**

- A Settings switch, *Share my measurements*. The window keeps the choice, and the contributor ID, in the user's own profile (`%APPDATA%\PingPath\crowd.json`), which other users can't read.
- On connecting, and whenever the choice changes, the window tells the service with a `set_sharing` request.
- The service sends a record only for a measurement or session that a sharing caller started. It remembers that caller's choice for the length of the session, so a session that outlives its window still counts as theirs.
- Nothing is sent for other users.

**What the Settings page shows next to the switch:**

- what is sent;
- what the server works out from the request: the ISP, the approximate city, which games are played, and the hour;
- that records are kept 30 days;
- that turning sharing off deletes them;
- the last report sent, as sent, with the contributor ID shown as hidden.

It also says that picking a game asks the server for a recommended path, which works without sharing.

**The one-time card on the main page:**

- It asks once, with two buttons of equal weight.
- Closing it leaves sharing off.

Reports never block or slow measuring or optimizing.

**Probe CLI:**

- `--share` shares one run. It needs the control plane's address (`--control URL`, or `PINGPATH_CONTROL_URL`).
- It prints the records, with the ID hidden, then sends them, and saves them beside the report without the ID.
- `--forget` deletes what was shared.
- Without `--share`, nothing is sent.

**Turning sharing off:**

1. The window, or `--forget`, asks the server to delete the contributor's records (`DELETE /v1/crowd/contributor`).
2. The local ID is deleted only once the server confirms. The request is retried while the player is offline.
3. The published figures change at the next daily recompute.

### The contributor ID

A random 128-bit number, made when a person first shares, kept in their own profile, and **never replaced**: the 30-day retention already limits how far records can be linked.

The probe, running as the same Windows user, uses the same file, so a person counts once whichever tool they use. It exists only so the server can count **people**, not reports, per bucket, and give each person one voice.

It works like a password for that person's records, so:

- it is sent only in request bodies;
- it is never shown, printed, saved beside reports or logged;
- the file isn't in the log folder;
- `pingpath-report` strips anything shaped like it.

The server stores a keyed hash of it (below), never the ID.

### Ingestion

`POST /v1/crowd/reports`, with `{contributor, client, records: [...]}`: at most 20 records and 16 KiB, as `application/json` only.

**1. Truncate.**
- The address (the socket's, or the proxy header's, as for sessions) becomes a `NetworkPrefix`: the /24 for IPv4, with IPv4-mapped addresses taken as IPv4, or the /48 for IPv6.
- Nothing past this step sees the address. The lookup functions take a `NetworkPrefix` and nothing else, so the type system holds the rule.

**2. Look up** the prefix's ASN, ISP name, country and city in two local files (below).

**3. Tag** the prefix with a keyed hash, for the limits and for the network rule in Aggregation.
- Then the prefix is dropped. The record keeps the ASN, the country, the city and the tag.
- To whoever holds the key, a tag *is* the /24 or /48, because there are only 2²⁴ of them to try. Rule 9 allows truncated addresses. The tag is never published, and the key is guarded (below).

**4. Check** each record:
- the game and region are known, and so are the relay IDs;
- the numbers are in range: round trips 0–2000 ms, loss 0–100 %, jitter 0–1000 ms;
- an optimized ping is plausible: not below the relays' own reported entry-to-exit plus exit-to-region round trips, less 10 ms.

A record that fails is skipped and counted by reason, as `/v1/relay-latency` does with stale samples. The report as a whole is refused only if it is malformed (`invalid_report`).

**5. Limit.** Over a limit, the report is refused (`crowd_limit`, with `Retry-After`), and the client drops it rather than retrying.

| Scope | Limit |
|---|---|
| Per network tag | 60 reports an hour, and at most 10 new contributors a day |
| Per contributor | 20 reports an hour |
| Per ASN | a daily row budget |
| The whole store | a daily row budget |

**6. Queue** the rows to the store's single writer thread.
- The queue is bounded. When it is full, reports are refused with `crowd_busy`, never held.
- A first-seen table holds each contributor's hash and the days it reported. The new-contributor limit and the two-day rule (below) need it.

Addresses, tags and contributor IDs are never logged.

### Keys

One persistent **crowd key**, 32 random bytes in its own file in the state directory (mode 0600). It is:
- not in the database;
- left out of any backup;
- the key for both the network tags and the contributor hashes.

A per-process key, like the session counters' key, would make every restart give each network and each person a second identity in the 30-day window, and break deletes. Replacing the key deletes the crowd database with it.

### City and ISP: offline databases

**DB-IP's free "IP to City Lite" and "IP to ASN Lite".** They come as MMDB files, are updated monthly, are licensed CC BY 4.0, and are downloaded without an account.

How the control plane uses them:
- It reads them with the `maxminddb` crate into memory. The safe `open_readfile` keeps the city file, about 100 MB, in memory, and the memory-mapping option is unsafe code.
- It reloads them when their modification time changes. `infra/update-geo.sh` downloads each month's files, then renames them into place.
- The dashboard credits DB-IP, as the licence requires.

Nothing is asked of another company at run time, so rule 8 holds.

Gaps and labels:
- Where a file has no city for a prefix, the country stands in.
- Cities are used as the file names them (in English) and labelled "as the address suggests".

The probe keeps its Team Cymru lookup for its local report, but `--share` doesn't send that result: the server finds the ISP itself. When the probe has no `--from`, it may take its city from the recommendation answer below, which returns the requester's own coarse location (ADR 003's "city from an offline database in Phase 6").

### Storage

**SQLite**, one file (`crowd.sqlite`) in the control plane's state directory, through `rusqlite` with its bundled SQLite, at a version within the workspace's Rust 1.85 minimum.

- **Connections:**
  - one writer thread, the only one that writes;
  - WAL mode;
  - a separate read connection for aggregation, which streams rows in index order (SQLite has no median function).
- **Deleting:** `secure_delete` is on, and the WAL is checkpointed after deletes, so deleted rows don't linger in free pages.
- **Backups:** the file is not backed up, so expired and deleted rows can't come back from a backup.
- **Retention:** rows older than **30 days** are deleted hourly.
- **Deployment:**
  - systemd gets `StateDirectory=pingpath`;
  - the Docker image gets a volume owned by its non-root user.

Why not PostgreSQL with TimescaleDB, as PLAN.md suggests:
- The volume doesn't need a database server: 10,000 people with 10 records a day for 30 days is 3 million rows, a few hundred MB.
- One file keeps the control plane one binary that a community can host.

If it outgrows SQLite, the store is behind one module.

### Aggregation and the privacy threshold

The routes and ISP tables are recomputed **once a day**. The status table is recomputed **hourly**. Daily recomputes keep one person's records, joining, ageing out or deleted, from showing up as a visible change between two close snapshots.

**Who counts.** A contributor counts toward a bucket only:
- after reporting on **2 different days**;
- from a network that is not a hosting provider's: a configurable list of ASNs (`excluded_asns`), shipped with the major clouds and the relays' providers, is left out of the routes and ISP tables.

**One voice each.** Each counted contributor's records in a bucket are reduced to that person's median. Figures are the median of those values, so someone sending many reports moves a figure no more than someone sending few.

**The threshold, for every number.** A figure is published, or returned in a recommendation, only if it rests on at least **20 contributors from at least 5 networks**. Each network counts for at most **4** of the 20. This covers:
- a bucket's direct ping;
- its optimized ping;
- its improvement;
- each relay pair offered as its best path.

So a city where 20 people measured but only 6 optimized shows direct pings and no optimized figures. And inventing contributors from one place gets nowhere.

**Coarse output:**
- figures are rounded to **5 ms**;
- the 90th percentile appears only for buckets of 50 or more;
- counts are given as bands (20+, 50+, 100+, 500+, 1000+), never exact.

**The tables:**

| Table | Levels | Figures |
|---|---|---|
| Routes | game and region by: country and city; country; country and ASN | direct ping; optimized ping; improvement (each person's direct minus optimized); best path, the qualifying relay pair with the lowest optimized ping |
| ISPs | game, region, country, ASN (with its name) | direct ping, loss and jitter, from client and probe records alike (a person counts once) |
| Status | relays; game regions | relays: up if a latency report arrived in the last 60 s, and their round trips to peers and regions, live, which involves no players' data. Regions: the improvement from sessions in the last 24 h, under the same threshold |

**Where they are published:**

| Route | Contents | Cache |
|---|---|---|
| `GET /v1/public/routes` | all three route levels | `max-age=3600` |
| `GET /v1/public/isps` | the ISP table | `max-age=3600` |
| `GET /v1/public/status` | relays and regions | `max-age=30` |

Each carries `generated_at`. These are the only routes with `Access-Control-Allow-Origin: *`, because the dashboard is served from another origin. The reports and recommendation routes send no CORS header.

### Recommended paths

`GET /v1/recommendations` is always on, even without `[crowd]`. It answers for **all** games at once, so the server doesn't learn which game was picked, and it is limited per network. It works from tables already in memory:

1. The requester's address becomes a `NetworkPrefix`, is looked up, and is dropped. Nothing is stored or logged.
2. For each game and region, it takes the best path from the most specific published route level: the ASN within the country, then the city, then the country.
3. Failing those, it builds a path from the relays alone, as `select::candidates` would:
   - an exit whose key matches the registry and whose round trip to the region is within 30 ms, the lowest such;
   - an entry on the same provider in the requester's country, with a fresh entry-to-exit round trip;
   - otherwise no entry.

   `RelayEntry` gains a validated ISO `country`, because `location` is free text.

**The answer** holds:
- the requester's own coarse location: country, city, ISP and ASN, with its source;
- for each game and region: the entry and exit (IDs, names and locations), the typical optimized and direct ping when they come from the crowd, and the level they came from (`asn`, `city`, `country` or `relays`).

**In the client.**
- **Who fetches.** The **service**, not the engine, fetches the recommendations. The engine runs one request at a time, and a 3–4 s measurement would delay them. The service keeps one control-plane client for this, with a 1 s timeout. It fetches when a window connects and then every 10 minutes while one is, and answers the window's `recommend` request from what it holds.
- **What selecting a game shows.** It shows the recommendation at once, before any measurement, worded by its source:
  - "players on your ISP usually get 48 ms through Bahrain → Frankfurt";
  - "players in Riyadh…" or "players in Saudi Arabia…" at the city and country levels;
  - the path without a number, when it comes from the relays alone.
- **Rule 3.** The region it names is the one the crowd's figures make usual: the lowest typical direct ping. Until the player measures, it is labelled typical for their location.
- **Faster first connection.** When the crowd recommends a pair for the region that the direct pings make usual, the measurement probes that pair alone first, with a short probe. If that pair is worthwhile, the player can turn Optimize on at once, and the full probe of every relay then runs to find standbys.
  - The direct pings still run first: they decide the usual region (rule 3) and the baseline.
  - The tunnel's check before routes go in still decides, as before (ADR 009).
  - A recommendation never routes anything by itself.

  ADR 009's remark that recommendations would need no change to the client no longer holds. This is the change.

### The dashboard

A static site in `dashboard/`, written like the window:

- plain JavaScript, no build step, and no third-party assets (no map tiles, fonts or analytics, which would break rule 8 and expose visitors);
- `en.json` and `ar.json`, with `dir="rtl"` for Arabic and paths isolated with `<bdi>` so "Bahrain → Frankfurt" reads right;
- country names from `Intl.DisplayNames`, digits as the window formats them, and `?lang=` so a shared link keeps its language.

It reads one setting, the control plane's address (`config.json`), and is served as files, for example by the Caddy that fronts the control plane.

Three pages:

- **Routes:** choose a game. A table per city: the usual region, direct and optimized ping, the improvement, and the best path.
  - PLAN.md's "route map" is a table for now.
  - A drawn map, if added, would use an offline public-domain basemap such as Natural Earth.
- **ISPs:** choose a game, region and country. Each ISP with its typical ping, loss and jitter.
- **Status:** each relay, up or not, and each region's current improvement over the last day.

Common to all three:
- Tables have headers, and status is written in words, not shown by colour alone.
- Where a city or ISP is missing, the page says the data isn't there yet. It explains the 20-person rule, shows `generated_at`, and invites players to share.

### Logging (PLAN.md section 9)

**Control plane:**
- the crowd module on or off at start;
- each address file loaded or reloaded, with its date, a failure to load, and a warning when it is over 60 days old;
- lookup misses, counted;
- each recompute, with how long it took and the buckets published and hidden per table;
- rows deleted by retention, and contributor deletes with their row counts;
- SQLite errors;
- reports refused by code;
- the recommendation's level, at debug.

Each also as a Prometheus counter, beside ADR 006's.

**Client:**
- each report sent or refused, with its code and `Retry-After`;
- each delete request and its result;
- each recommendation fetch, with how long it took or the timeout;
- whether the path the player ended up on matched the recommendation.

**Probe:** the outcome of `--share` and `--forget`.

Never logged anywhere: addresses, tags or contributor IDs. The operator guide also says not to keep proxy access logs for `/v1/crowd/` and `/v1/recommendations`, which would hold full addresses.

### Usage and cost

PLAN.md sections 7 and 10 ask for usage data to place capacity and to check the cost model. That needs no players' consent:
- the relays already count the bytes they forward and duplicate;
- the control plane adds session-minutes per relay to its metrics.

P6-7 checks section 10's model against whatever usage exists by then. The real numbers come after deployment.

### The tests, and the order of work

Each step brings its own done-when test:

1. **P6-2: the crowd store and ingestion.**
   - The `pingpath-crowd` crate and its record schema, with a test the client reuses.
   - Ingestion: truncation, lookups, the plausibility check, the limits, the bounded queue, the keys, and the contributor delete.
   - Address lookups sit behind a trait with an in-memory table for unit tests, plus one small committed MMDB fixture. A checked-in script builds it from documentation ranges, with DB-IP's field names.
   - *Test:* reports sent from known addresses through the proxy header. Then the database, its `-wal` and `-shm` files, and the TRACE-level logs captured by the test are searched for each address, its /24 or /48, and each contributor ID, and none is found.
2. **P6-3: aggregation, the threshold and the public routes.**
   - *Tests:*
     - 19 against 20 contributors;
     - 20 contributors from 4 networks;
     - a network with 10 contributors counting as 4;
     - the two-day rule;
     - excluded ASNs;
     - one person flooding a bucket;
     - one person through the client and the probe counting once;
     - optimized figures withheld when few people optimized;
     - a delete dropping a bucket below 20;
     - the same rules in the status and recommendation answers;
     - rounding and bands.
3. **P6-4: recommendations.**
   - The endpoint, `RelayEntry.country`, the service's fetch and `recommend`, the window showing the path on selection, and the short first probe.
   - *Tests:*
     - with a fresh service and the control plane behind a proxy that adds 150 ms of round trip, a recommendation reaches the window within 1 s of selecting a game, timed in the service test from the window's request;
     - the smoke test shows it without measuring;
     - an engine test compares the time to Ready with and without a recommendation.
4. **P6-5: reporting from the client and the probe.**
   - The engine's jitter, session summary and bounded record channel. The service's reporter task, `set_sharing`, the switch and the card, and the last report shown. The probe's `--share` and `--forget`.
   - *Tests:*
     - a fake control plane receives nothing while sharing is off;
     - it receives only the sharing user's sessions when two users are connected;
     - it receives a delete when sharing is turned off.
5. **P6-6: the dashboard.**
   - `check-i18n.py` gains a dashboard mode.
   - *Test:* a Playwright smoke test in both languages against the committed fixture, which the Rust end-to-end test compares with real output so they can't drift. In CI.
6. **P6-7: wrap-up.** The end-to-end run (25 contributors on 6 networks, then the public routes and the dashboard), docs, the cost check, CI, and the merge.

**"Real aggregated data."** CI proves the whole pipeline with generated records. With no relays deployed and no players sharing, real figures can't exist yet, as in Phase 3. The live parts are met when deployed:
- the status page shows the relays' own data;
- the route and ISP pages fill in as buckets pass the threshold.

This stays an open item at the merge.

## Consequences

**Operations:**
- The control plane writes to disk for the first time: a state directory, a key file, and two address files to refresh monthly.
- The operator guide must say not to back up the crowd database or the key.
- The relays' registry entries need a country.

**Data:**
- Opting in is each person's choice, so data will be thin at first, and most buckets stay hidden until enough people share. Recommendations fall back to relay figures until then.
- One person can't move a published figure much: they get one voice, and their network counts four people at most.
- A group with many networks over many days still could. That is the limit of what a pseudonymous, opt-in design can resist without accounts.

**Privacy:**
- Sharing tells our server which games a network plays and when, to the hour for a day and then to the day. That is why only aggregates leave it.
- Asking for recommendations tells it nothing about which game.

**Rules:**
- Rule 8 holds: the address files are on our machines.
- The probe's own ISP lookup still asks Team Cymru for its local report, as ADR 003 records.

**Other ADRs:** ADR 007's list of what a player may change over the pipe gains sharing, which is per user.

## Known limits

- **City accuracy** is the address file's: often the capital, for mobile networks and CGNAT.
- **VPNs:** a VPN in front of the client makes its records look like the VPN's ISP. The connection check warns about virtual adapters, but records aren't filtered by it.
- **CGNAT limits:** on a shared CGNAT /24, the per-network limits can refuse an honest player's report, and one person there can use them up for the rest. Their reports are dropped, never retried.
- **Change delay:** deletions and new data show at the next daily recompute.

## Alternatives considered

- **PostgreSQL with TimescaleDB:** more than the volume needs, and a second service for every self-hoster. It stays possible behind the store module.
- **Counting networks instead of people:** needs no ID, but undercounts players behind CGNAT. Both are used: people for the count, networks to cap how much one place can add.
- **Rotating the contributor ID:** counts one person twice inside the 30-day window. Retention already bounds linkage.
- **MaxMind GeoLite2:** comparable data, but each installation needs an account and a licence key.
- **The client finding its own city and ISP:** would send the full address to a third party, as the probe does now. The server already sees the address, and looks up the truncated one itself.
- **A recommendation inside `/v1/relays` or `/v1/latency`:** those are the same for everyone, while the recommendation depends on the requester's network, and must be in hand before any measurement.
- **Machine-wide consent in `client.toml`:** one user's choice would share every user's sessions.
- **Exact counts, means and recomputing every few minutes:** each lets one person's records show through. Medians, bands and daily recomputes don't.
