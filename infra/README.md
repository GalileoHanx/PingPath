# infra

Everything needed to run PingPath's servers. The operator guide,
[docs/relay-operator.md](../docs/relay-operator.md), walks through a
deployment end to end.

| Path | What |
|---|---|
| `docker/Dockerfile` | Relay and control plane images (`--target relay`, `--target control`): unprivileged, distroless |
| `systemd/pingpath-relay.service`, `systemd/pingpath-control.service` | Locked-down systemd units running as the `pingpath` user |
| `systemd/99-pingpath-relay.conf` | Kernel settings: ephemeral port range for the exit's flows, reserved relay ports, UDP buffers |
| `firewall/pingpath-relay.nft` | Host firewall for machines without a cloud firewall: UDP open, SSH and metrics restricted |
| `install-relay.sh` | Installs or upgrades a relay on a systemd host |
| `terraform/modules/relay-node` | Cloud-agnostic: renders a relay's config and the cloud-init that installs it |
| `terraform/modules/aws-relay` | A relay on EC2 with an Elastic IP and a security group |
| `terraform/modules/gcp-relay` | A relay on Compute Engine with a static address and firewall rules |
| `terraform/examples/gulf-frankfurt` | Entry relays in Bahrain (AWS) and Doha (Google), each paired with an exit in Frankfurt on the same provider |

What stays private on every relay: health and metrics (`127.0.0.1:9100`
unless you open it to your monitoring addresses), SSH (admin addresses
only), the exit's WireGuard private key (created on the machine, never
copied off it) and the relay's API key.

CI checks the Terraform with `terraform fmt -check` and `terraform validate`,
and builds both images.
