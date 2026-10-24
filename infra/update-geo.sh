#!/bin/sh
# Fetches DB-IP's free "IP to City Lite" and "IP to ASN Lite" databases
# (MMDB, CC BY 4.0, no account needed) for the control plane's crowd data
# (docs/decisions/012-crowd-data-and-dashboard.md). DB-IP publishes them
# monthly; run this monthly, e.g. from a systemd timer or cron:
#
#   infra/update-geo.sh /var/lib/pingpath/geo
#
# Each file is downloaded and unpacked beside the old one, then renamed
# into place, so the control plane (which reloads a file when it changes)
# never reads half a file. If this month's files aren't out yet, last
# month's are used.
set -eu

dir="${1:-/var/lib/pingpath/geo}"
base="https://download.db-ip.com/free"
mkdir -p "$dir"

fetch() {
    name="$1" # dbip-city-lite or dbip-asn-lite
    for month in "$(date -u +%Y-%m)" "$(date -u -d "$(date -u +%Y-%m-01) -1 day" +%Y-%m 2>/dev/null || date -u -v-1m +%Y-%m)"; do
        tmp="$dir/.$name.mmdb.tmp"
        if curl -fsSL --retry 3 "$base/$name-$month.mmdb.gz" | gunzip > "$tmp" && [ -s "$tmp" ]; then
            mv -f "$tmp" "$dir/$name.mmdb"
            echo "updated $dir/$name.mmdb ($month)"
            return 0
        fi
        rm -f "$tmp"
    done
    echo "could not fetch $name; keeping the file there is" >&2
    return 1
}

status=0
fetch dbip-city-lite || status=1
fetch dbip-asn-lite || status=1
exit "$status"
