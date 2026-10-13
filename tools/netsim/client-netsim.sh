#!/usr/bin/env bash
# The client's split-tunnel data path on a real kernel: the player's PC,
# the relays and the game servers, each in its own Linux network namespace
# (needs root and iproute2).
#
#   sudo tools/netsim/client-netsim.sh up [--no-direct-exit]
#   sudo ip netns exec ppgame <command>      # e.g. the client netns test
#   sudo tools/netsim/client-netsim.sh loss 10.2.1.11 3   # 3% loss each way via entry-a
#   sudo tools/netsim/client-netsim.sh loss-off
#   sudo tools/netsim/client-netsim.sh down
#
#   ppgame (player's PC)        pprelays (relays + "the internet")      ppserver
#   g0 10.1.0.2/24 ---veth--- rg0 10.1.0.1/24     rs0 10.3.0.1/24 ---veth--- s0 10.3.0.2/24
#   default via 10.1.0.1        lo: 10.2.1.11 entry-a                    lo: 10.3.1.10 game server
#   (the test adds its TUN)         10.2.1.12 entry-b                        10.3.2.10 other server
#                                   10.2.1.21 exit                       default via 10.3.0.1
#                                   10.2.1.30 control plane
#
# pprelays forwards between ppgame and ppserver, so ppgame reaches the
# relays, the control plane and both servers directly through its default
# route, and the exit reaches the servers. The exit egresses from 10.3.0.1,
# so the echo server tells tunnelled traffic (from 10.3.0.1) from direct
# traffic (from 10.1.0.2). The game's range is 10.3.1.0/24; 10.3.2.0/24 is
# not in it. Relay addresses are /32s on lo with a blackhole for
# 10.2.1.0/24 behind them: deleting a relay's address makes it vanish
# silently (no ICMP), which is how the test kills a relay:
#   ip -n pprelays addr del 10.2.1.11/32 dev lo    # entry-a gone
#   ip -n pprelays addr add 10.2.1.11/32 dev lo    # entry-a back
# --no-direct-exit drops the player's packets to the exit (the entries still
# reach it), so the client always picks a path through an entry and entry
# failover can be tested without netem delays.
set -euo pipefail

NAMESPACES=(ppgame pprelays ppserver)

down() {
    for ns in "${NAMESPACES[@]}"; do ip netns del "$ns" 2>/dev/null || true; done
}

up() {
    local direct_exit=yes
    while [ $# -gt 0 ]; do
        case "$1" in
            --no-direct-exit) direct_exit=no; shift ;;
            *) echo "unknown option $1" >&2; exit 2 ;;
        esac
    done
    down
    for ns in "${NAMESPACES[@]}"; do ip netns add "$ns"; ip -n "$ns" link set lo up; done

    ip link add g0 netns ppgame type veth peer name rg0 netns pprelays
    ip link add rs0 netns pprelays type veth peer name s0 netns ppserver

    ip -n ppgame addr add 10.1.0.2/24 dev g0
    ip -n pprelays addr add 10.1.0.1/24 dev rg0
    ip -n pprelays addr add 10.3.0.1/24 dev rs0
    ip -n ppserver addr add 10.3.0.2/24 dev s0
    for addr in 10.2.1.11 10.2.1.12 10.2.1.21 10.2.1.30; do
        ip -n pprelays addr add "$addr/32" dev lo
    done
    for addr in 10.3.1.10 10.3.2.10; do ip -n ppserver addr add "$addr/32" dev lo; done
    for pair in ppgame:g0 pprelays:rg0 pprelays:rs0 ppserver:s0; do
        ip -n "${pair%%:*}" link set "${pair##*:}" up
    done

    ip -n ppgame route add default via 10.1.0.1
    ip -n pprelays route add 10.3.1.0/24 via 10.3.0.2
    ip -n pprelays route add 10.3.2.0/24 via 10.3.0.2
    # A deleted relay address falls through to this: dropped without ICMP.
    ip -n pprelays route add blackhole 10.2.1.0/24
    ip -n ppserver route add default via 10.3.0.1
    ip netns exec pprelays sysctl -qw net.ipv4.ip_forward=1
    # Packets the client writes into its TUN arrive "from" the game server;
    # never let reverse-path filtering drop them while routes change.
    ip netns exec ppgame sysctl -qw net.ipv4.conf.all.rp_filter=0
    ip netns exec ppgame sysctl -qw net.ipv4.conf.default.rp_filter=0

    if [ "$direct_exit" = no ]; then
        # Local delivery is decided by the "local" table at rule 0, so move
        # that lookup behind a rule that drops ppgame -> exit.
        ip -n pprelays rule add pref 100 lookup local
        ip -n pprelays rule del pref 0
        ip -n pprelays rule add pref 10 iif rg0 to 10.2.1.21/32 blackhole
    fi
    echo "client netsim is up: entries 10.2.1.11 10.2.1.12, exit 10.2.1.21," \
        "control 10.2.1.30; game server 10.3.1.10, other server 10.3.2.10"
}

# Random loss on one relay's path, both ways (Phase 4's done-when test):
# packets from ppgame to that address and from it back to ppgame. tc netem
# behind a prio qdisc and a u32 filter, so only that relay's traffic is
# hit; on a kernel without netem (some containers), iptables' statistic
# match drops the same share instead. Says which it used.
loss() {
    local addr="$1" pct="$2"
    loss_off
    if tc -n ppgame qdisc add dev g0 root handle 1: prio bands 3 \
        priomap 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 2>/dev/null &&
        tc -n ppgame qdisc add dev g0 parent 1:3 handle 30: netem loss "$pct%" 2>/dev/null; then
        tc -n ppgame filter add dev g0 parent 1: protocol ip prio 1 u32 \
            match ip dst "$addr/32" flowid 1:3
        tc -n pprelays qdisc add dev rg0 root handle 1: prio bands 3 \
            priomap 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1
        tc -n pprelays qdisc add dev rg0 parent 1:3 handle 30: netem loss "$pct%"
        tc -n pprelays filter add dev rg0 parent 1: protocol ip prio 1 u32 \
            match ip src "$addr/32" flowid 1:3
        echo "netem: $pct% loss each way on $addr"
    else
        loss_off
        local p
        p=$(awk "BEGIN { print $pct / 100 }")
        # Only what crosses the veth, as with netem: the entry's own traffic
        # to the exit stays inside pprelays.
        ip netns exec ppgame iptables -A OUTPUT -o g0 -d "$addr" \
            -m statistic --mode random --probability "$p" -j DROP
        ip netns exec pprelays iptables -A OUTPUT -o rg0 -s "$addr" \
            -m statistic --mode random --probability "$p" -j DROP
        echo "no netem in this kernel; iptables: $pct% loss each way on $addr"
    fi
}

loss_off() {
    tc -n ppgame qdisc del dev g0 root 2>/dev/null || true
    tc -n pprelays qdisc del dev rg0 root 2>/dev/null || true
    ip netns exec ppgame iptables -F OUTPUT 2>/dev/null || true
    ip netns exec pprelays iptables -F OUTPUT 2>/dev/null || true
}

case "${1:-}" in
    up) shift; up "$@" ;;
    down) down ;;
    loss) [ $# -eq 3 ] || { echo "usage: $0 loss <address> <percent>" >&2; exit 2; }
        loss "$2" "$3" ;;
    loss-off) loss_off ;;
    *) echo "usage: $0 up [--no-direct-exit] | down | loss <address> <percent> | loss-off" >&2; exit 2 ;;
esac
