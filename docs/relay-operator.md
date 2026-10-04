# Running PingPath servers

This guide sets up the server side of PingPath: one **control plane** that hands out session tokens, and **relays** that carry game traffic. The design is in [architecture.md](architecture.md) and ADRs [004](decisions/004-relay-wire-protocol.md) (wire protocol and tokens), [005](decisions/005-exit-egress.md) (how exits forward) and [006](decisions/006-control-plane.md) (the control plane's API and rules).

## What you run

| Service | Where | Listens on | Open to |
|---|---|---|---|
| Control plane | Anywhere reachable over HTTPS | `127.0.0.1:8080` (API), `127.0.0.1:9101` (health, metrics) | API: everyone, through a TLS reverse proxy. Admin: you only |
| Entry relay | Near players: Gulf cloud regions (Bahrain, UAE, Doha, Dammam, ...) | UDP `51820`, `127.0.0.1:9100` (health, metrics) | UDP: everyone. 9100: your monitoring only |
| Exit relay | In the game servers' region, e.g. Frankfurt for `eu-central` | the same | the same |

One relay can be both an entry and an exit. Pair each entry with an exit on the **same cloud provider** so the long leg rides that provider's backbone (PLAN.md section 10), and run entries on at least two providers so routes differ.

## 1. Get the binaries

Pick one:

- **CI artifacts.** Every CI run on GitHub publishes `pingpath-linux-x64` (`pingpath-relay`, `pingpath-control` and `SHA256SUMS`) under the run's *Artifacts*. They run on any x86-64 Linux with glibc 2.36 or newer (Debian 12, Ubuntu 22.04 and later).
- **Docker images**, built from the repository root:
  ```sh
  docker build -f infra/docker/Dockerfile --target relay   -t pingpath-relay .
  docker build -f infra/docker/Dockerfile --target control -t pingpath-control .
  ```
- **From source:** `cargo build --release -p pingpath-relay -p pingpath-control` (Rust 1.85 or newer).

For the Terraform modules, put `pingpath-relay` somewhere the relays can download it over HTTPS (a storage bucket or a GitHub release) and note its SHA-256.

## 2. Set up the control plane

```sh
pingpath-control keygen --out /etc/pingpath/issuer.seed   # prints the public key for relays
cp control/control.example.toml /etc/pingpath/control.toml
```

For every relay, create an API key it uses to report latency:

```sh
pingpath-control api-key --out fra-1.key   # prints api_key_sha256 = "..."
```

The key file goes to that relay (its `control.api_key_file`); the printed hash goes in the relay's `[[relays]]` entry in `control.toml`. The control plane stores only hashes.

Edit `control.toml`: list every relay with its ID, address, roles, regions (exits), WireGuard public key (exits; see step 3) and API key hash. Then check and run it:

```sh
pingpath-control check --config /etc/pingpath/control.toml
sudo install -m 0644 infra/systemd/pingpath-control.service /etc/systemd/system/
sudo systemctl enable --now pingpath-control
```

or with Docker: `docker run -d --name pingpath-control -v /etc/pingpath:/etc/pingpath:ro -p 127.0.0.1:8080:8080 pingpath-control` (with `listen = "0.0.0.0:8080"` in the config, since the container has its own localhost).

**Put it behind TLS.** Clients send their renewal secret to the API, so it must only be reachable over HTTPS. Any reverse proxy works; with Caddy:

```
control.example.net {
    reverse_proxy 127.0.0.1:8080
}
```

Caddy sets `X-Forwarded-For`, so keep `client_ip_header = "X-Forwarded-For"` in `control.toml`; per-network limits then count the player's network, not the proxy's. If clients reach the control plane directly, remove that line, or anyone could choose the network they are counted against.

## 3. Set up relays

### With Terraform (AWS and Google Cloud)

`infra/terraform/examples/gulf-frankfurt` deploys four relays: an entry in Bahrain (AWS) and one in Doha (Google), each paired with an exit in Frankfurt on the same provider. AWS's Bahrain region is opt-in; enable it in the account first.

```sh
cd infra/terraform/examples/gulf-frankfurt
cp terraform.tfvars.example terraform.tfvars   # fill in: project, issuer key, binary URL and SHA-256, API keys
terraform init
terraform apply
```

Each machine installs the relay with cloud-init, creates the exit's WireGuard key on the machine (it never leaves it), and starts the service. Then:

1. Read each exit's public key: `sudo journalctl -u pingpath-relay | grep "exit WireGuard public key"`.
2. Add the relays to `control.toml`; `terraform output control_plane_relays` has the entries, without the keys.
3. Restart the control plane.

Changing a relay's settings replaces its machine (and so its exit key); update `control.toml` afterwards. To use the modules for other regions, see `infra/terraform/modules/aws-relay` and `gcp-relay`; `relay-node` is the cloud-agnostic part.

### By hand, on any Linux server

```sh
sudo infra/install-relay.sh ./pingpath-relay
sudoedit /etc/pingpath/relay.toml
sudo pingpath-relay check --config /etc/pingpath/relay.toml
sudo systemctl enable --now pingpath-relay
```

The script installs the binary, a `pingpath` system user, the systemd unit and kernel settings, creates `/etc/pingpath/relay.toml` from the example and a WireGuard key, and prints the public key. Run it again to upgrade; it keeps the config and keys. If the machine has no cloud firewall in front, use `infra/firewall/pingpath-relay.nft`.

With Docker: mount `/etc/pingpath` read-only and publish UDP `51820`; `pingpath-relay keygen --out relay.key` makes the key.

### The relay config

`relay/relay.example.toml` documents every setting. The ones you must set:

| Setting | What |
|---|---|
| `id`, `name` | Must match the relay's entry in `control.toml` |
| `roles` | `["entry"]`, `["exit"]` or both |
| `regions` | Exits: the game regions this relay serves, as in the game definitions (e.g. `eu-central`). An exit only forwards to servers in these regions (rule 3) |
| `private_key_file` | Exits: the WireGuard key |
| `issuer_public_keys` | The control plane's public key. List two while changing it |
| `[[relays]]` | Every other relay. An entry forwards only to exits listed here; every relay probes the others |
| `[control]` | The control plane's URL and this relay's API key file, for latency reports |

The `[limits]` defaults suit a small relay; they are listed in ADR 004.

## 4. Operating

**Health:** `curl -s 127.0.0.1:9100/healthz` (relay) and `curl -s 127.0.0.1:9101/healthz` (control plane) show the version, uptime, session counts and, for relays, the latest round trip to every other relay.

**Metrics** (Prometheus, same ports, `/metrics`). The ones to watch:

| Metric | Meaning |
|---|---|
| `pingpath_relay_entry_sessions`, `pingpath_relay_exit_sessions`, `pingpath_relay_flows` | Current load |
| `pingpath_relay_processing_seconds` | Time from receiving a frame to sending it on; p99 should stay well under 1 ms |
| `pingpath_relay_drops_total{reason=...}` | Everything not forwarded, by reason (below) |
| `pingpath_relay_peer_rtt_ms`, `pingpath_relay_peer_loss_pct` | Latency and loss to the other relays |
| `pingpath_relay_region_rtt_ms`, `pingpath_relay_region_loss_pct` | Exits: latency and loss to the game regions they serve (through the definitions' ping targets); clients use it to estimate paths |
| `pingpath_control_sessions_issued_total`, `pingpath_control_refusals_total{reason=...}` | Sessions granted, and refusals by reason |

Drop reasons that point at a problem:

| Reason | Likely cause |
|---|---|
| `unknown_exit`, `wrong_exit` | An entry's `[[relays]]` lacks an exit the control plane hands out, or IDs differ between the configs |
| `wrong_region`, `game_not_routable` | The control plane and the exit disagree about regions or game definitions; deploy the same version everywhere |
| `expired_token` | Clocks are off: run NTP on every server (60 s of skew is tolerated) |
| `stale_path` | Replies with nowhere to go, usually a client that disappeared; a steady rise means clients lose their path |
| `relay_full`, `flow_limit`, `rate_limited` | Limits reached; check whether it is real load before raising them |
| `not_allowlisted` | Clients sending to servers missing from the game definition; report it so the definition can be fixed |
| `hello_flood`, `probe_flood`, `bad_token` | Someone sending junk; the relay drops it cheaply |

Relays also log a one-line summary of drops every minute.

**Logs** go to the journal and to `/var/log/pingpath/` as JSON lines, kept 7 days. They never hold players' addresses. `pingpath-report --log-dir /var/log/pingpath` (build it with `cargo build --release -p pingpath-diagnostics`) turns them into one report you can attach to an issue.

**Changing the issuer key:** create a new key, add its public key to every relay's `issuer_public_keys` next to the old one and restart them, switch the control plane to the new seed, wait 15 minutes for old tokens to expire, then remove the old key from the relays.

**Upgrading:** relays verify tokens offline, so restarting the control plane doesn't interrupt anyone in a match. Restarting an exit ends its sessions; clients reconnect, which the game server may see as a new connection. Upgrade exits when they're quiet.

## Security checklist

- Health and metrics stay on localhost or a monitoring network; the firewall rules and Terraform modules only open them to `monitoring_cidrs`.
- SSH only from your admin addresses (`admin_cidrs`), or no SSH at all.
- The control plane API only over HTTPS.
- Key files are readable by root and the `pingpath` group only (`install-relay.sh` and cloud-init set this). The exit's WireGuard key never leaves its machine.
- Every server runs NTP.
- A relay's API key is in its cloud user data, readable from inside the instance; it only allows reporting latency for that relay.
