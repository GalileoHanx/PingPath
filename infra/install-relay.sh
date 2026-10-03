#!/bin/sh
# Installs a PingPath relay on a systemd Linux machine (Ubuntu, Debian and
# similar), for hosts not set up by the Terraform modules. Run as root:
#
#   sudo infra/install-relay.sh ./pingpath-relay
#
# It installs the binary, a `pingpath` system user, the systemd unit and
# kernel settings, and creates /etc/pingpath/relay.toml from the example if
# there is none. Edit that file, then: systemctl enable --now pingpath-relay
# Running it again upgrades the binary and keeps the config and keys.
set -eu

binary=${1:?usage: install-relay.sh PATH-TO-pingpath-relay}
here=$(cd "$(dirname "$0")" && pwd)
repo=$(dirname "$here")

[ "$(id -u)" -eq 0 ] || { echo "run as root" >&2; exit 1; }
"$binary" --version >/dev/null

id pingpath >/dev/null 2>&1 || useradd --system --no-create-home --shell /usr/sbin/nologin pingpath
install -m 0755 "$binary" /usr/local/bin/pingpath-relay
install -D -m 0644 "$here/systemd/pingpath-relay.service" /etc/systemd/system/pingpath-relay.service
install -D -m 0644 "$here/systemd/99-pingpath-relay.conf" /etc/sysctl.d/99-pingpath-relay.conf
sysctl --system >/dev/null

install -d -m 0750 -o root -g pingpath /etc/pingpath
if [ ! -f /etc/pingpath/relay.toml ]; then
    install -m 0640 -o root -g pingpath "$repo/relay/relay.example.toml" /etc/pingpath/relay.toml
    echo "created /etc/pingpath/relay.toml from the example: edit it before starting"
fi
if [ ! -f /etc/pingpath/relay.key ]; then
    /usr/local/bin/pingpath-relay keygen --out /etc/pingpath/relay.key
    chgrp pingpath /etc/pingpath/relay.key
    chmod 0640 /etc/pingpath/relay.key
fi

systemctl daemon-reload
if systemctl is-active --quiet pingpath-relay; then
    systemctl restart pingpath-relay
    echo "pingpath-relay upgraded and restarted"
else
    echo "installed; when the config is ready: systemctl enable --now pingpath-relay"
fi
