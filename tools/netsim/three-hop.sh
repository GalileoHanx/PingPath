#!/usr/bin/env bash
# A small simulated network for tests: client -> router1 -> router2 -> server,
# each in its own Linux network namespace (needs root, iproute2 and, for
# --delay/--loss, the netem qdisc).
#
#   sudo tools/netsim/three-hop.sh up [--delay 20ms] [--loss 3%]
#   sudo ip netns exec ppclient <command>   # e.g. a test binary
#   sudo tools/netsim/three-hop.sh down
#
# Addresses: client 10.1.0.2, router1 10.1.0.1, router2 10.2.0.2,
# server 10.3.0.2. --delay and --loss apply on router2 -> server.
set -euo pipefail

NAMESPACES=(ppclient pprouter1 pprouter2 ppserver)

down() {
    for ns in "${NAMESPACES[@]}"; do ip netns del "$ns" 2>/dev/null || true; done
}

up() {
    local delay="" loss=""
    while [ $# -gt 0 ]; do
        case "$1" in
            --delay) delay="$2"; shift 2 ;;
            --loss) loss="$2"; shift 2 ;;
            *) echo "unknown option $1" >&2; exit 2 ;;
        esac
    done
    down
    for ns in "${NAMESPACES[@]}"; do ip netns add "$ns"; ip -n "$ns" link set lo up; done

    ip link add c0 netns ppclient type veth peer name r1a netns pprouter1
    ip link add r1b netns pprouter1 type veth peer name r2a netns pprouter2
    ip link add r2b netns pprouter2 type veth peer name s0 netns ppserver

    ip -n ppclient addr add 10.1.0.2/24 dev c0
    ip -n pprouter1 addr add 10.1.0.1/24 dev r1a
    ip -n pprouter1 addr add 10.2.0.1/24 dev r1b
    ip -n pprouter2 addr add 10.2.0.2/24 dev r2a
    ip -n pprouter2 addr add 10.3.0.1/24 dev r2b
    ip -n ppserver addr add 10.3.0.2/24 dev s0
    for pair in ppclient:c0 pprouter1:r1a pprouter1:r1b pprouter2:r2a pprouter2:r2b ppserver:s0; do
        ip -n "${pair%%:*}" link set "${pair##*:}" up
    done

    ip -n ppclient route add default via 10.1.0.1
    ip -n pprouter1 route add default via 10.2.0.2
    ip -n pprouter2 route add 10.1.0.0/24 via 10.2.0.1
    ip -n ppserver route add default via 10.3.0.1
    ip netns exec pprouter1 sysctl -qw net.ipv4.ip_forward=1
    ip netns exec pprouter2 sysctl -qw net.ipv4.ip_forward=1
    # Let unprivileged processes in the client namespace send ICMP.
    ip netns exec ppclient sysctl -qw net.ipv4.ping_group_range="0 2147483647"

    if [ -n "$delay$loss" ]; then
        ip netns exec pprouter2 tc qdisc add dev r2b root netem \
            ${delay:+delay "$delay"} ${loss:+loss "$loss"}
    fi
    echo "three-hop network is up; server is 10.3.0.2"
}

case "${1:-}" in
    up) shift; up "$@" ;;
    down) down ;;
    *) echo "usage: $0 up [--delay 20ms] [--loss 3%] | down" >&2; exit 2 ;;
esac
