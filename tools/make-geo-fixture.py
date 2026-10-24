#!/usr/bin/env python3
"""Writes the small address databases the control plane's crowd tests read
(docs/decisions/012-crowd-data-and-dashboard.md), in the MaxMind DB format
DB-IP's free "IP to City Lite" and "IP to ASN Lite" files use, with the same
field names. Only documentation and benchmarking ranges, which no real
player has.

    python3 tools/make-geo-fixture.py control/tests/data

writes geo-city.mmdb and geo-asn.mmdb there. Standard library only.
"""

import ipaddress
import struct
import sys
from pathlib import Path

# (network, city or None, country ISO code, country name, ASN, ISP).
NETWORKS = [
    # Six networks in one city, for the end-to-end test's 25 contributors.
    ("198.18.0.0/15", "Riyadh", "SA", "Saudi Arabia", 64500, "Example Telecom"),
    ("198.51.100.0/24", "Riyadh", "SA", "Saudi Arabia", 64500, "Example Telecom"),
    ("203.0.113.0/24", "Jeddah", "SA", "Saudi Arabia", 64501, "Example Mobile"),
    # A network with no city: the country stands in.
    ("192.0.2.0/24", None, "KW", "Kuwait", 64502, "Example Kuwait"),
    ("2001:db8::/32", "Dubai", "AE", "United Arab Emirates", 64503, "Example Emirates"),
]

RECORD_SIZE = 24
METADATA_MARKER = b"\xab\xcd\xefMaxMind.com"


def size_bytes(type_id, size):
    """The control byte(s) for a value of `type_id` and `size`."""
    if size < 29:
        head, extra = size, b""
    elif size < 29 + 256:
        head, extra = 29, bytes([size - 29])
    elif size < 285 + 65536:
        head, extra = 30, struct.pack(">H", size - 285)
    else:
        head, extra = 31, struct.pack(">I", size - 65821)[1:]
    if type_id <= 7:
        return bytes([(type_id << 5) | head]) + extra
    # Extended types: type bits 0, then a byte holding the type minus 7.
    return bytes([head, type_id - 7]) + extra


def encode(value):
    if isinstance(value, bool):
        return size_bytes(14, 1 if value else 0)
    if isinstance(value, str):
        data = value.encode("utf-8")
        return size_bytes(2, len(data)) + data
    if isinstance(value, float):
        return size_bytes(3, 8) + struct.pack(">d", value)
    if isinstance(value, Uint):
        data = value.value.to_bytes(value.width, "big").lstrip(b"\x00")
        return size_bytes(value.type_id, len(data)) + data
    if isinstance(value, int):
        return encode(Uint(value, 4, 6))
    if isinstance(value, dict):
        out = size_bytes(7, len(value))
        for key, item in value.items():
            out += encode(key) + encode(item)
        return out
    if isinstance(value, list):
        out = size_bytes(11, len(value))
        for item in value:
            out += encode(item)
        return out
    raise TypeError(f"cannot encode {value!r}")


class Uint:
    def __init__(self, value, width, type_id):
        self.value, self.width, self.type_id = value, width, type_id


def u16(value):
    return Uint(value, 2, 5)


def u32(value):
    return Uint(value, 4, 6)


def u64(value):
    return Uint(value, 8, 9)


def bits(network):
    """The network as a path of bits in an IPv6 tree (IPv4 under ::/96)."""
    if network.version == 4:
        address = int(network.network_address)
        length = 96 + network.prefixlen
    else:
        address = int(network.network_address)
        length = network.prefixlen
    return [(address >> (127 - i)) & 1 for i in range(length)]


def write(path, database_type, entries):
    """`entries`: (network, data map) pairs that don't overlap."""
    # The tree: nodes as [left, right], each a node index, ("data", i) or None.
    nodes = [[None, None]]
    data_maps = []
    for network, data in entries:
        data_maps.append(data)
        path_bits = bits(ipaddress.ip_network(network))
        node = 0
        for depth, bit in enumerate(path_bits):
            last = depth == len(path_bits) - 1
            if last:
                nodes[node][bit] = ("data", len(data_maps) - 1)
            else:
                child = nodes[node][bit]
                if child is None:
                    nodes.append([None, None])
                    child = len(nodes) - 1
                    nodes[node][bit] = child
                node = child

    node_count = len(nodes)
    data_section = b""
    offsets = []
    for data in data_maps:
        offsets.append(len(data_section))
        data_section += encode(data)

    def record(value):
        if value is None:
            return node_count
        if isinstance(value, tuple):
            return node_count + 16 + offsets[value[1]]
        return value

    tree = b""
    for left, right in nodes:
        for value in (left, right):
            tree += record(value).to_bytes(RECORD_SIZE // 8, "big")

    metadata = {
        "binary_format_major_version": u16(2),
        "binary_format_minor_version": u16(0),
        "build_epoch": u64(1767225600),
        "database_type": database_type,
        "description": {"en": "PingPath test data: documentation ranges only"},
        "ip_version": u16(6),
        "languages": ["en"],
        "node_count": u32(node_count),
        "record_size": u16(RECORD_SIZE),
    }
    path.write_bytes(tree + b"\x00" * 16 + data_section + METADATA_MARKER + encode(metadata))


def main():
    out = Path(sys.argv[1] if len(sys.argv) > 1 else "control/tests/data")
    out.mkdir(parents=True, exist_ok=True)
    city_entries = []
    asn_entries = []
    for network, city, iso, country, asn, isp in NETWORKS:
        place = {"country": {"iso_code": iso, "names": {"en": country}}}
        if city:
            place["city"] = {"names": {"en": city}}
        city_entries.append((network, place))
        asn_entries.append(
            (network, {"autonomous_system_number": u32(asn), "autonomous_system_organization": isp})
        )
    write(out / "geo-city.mmdb", "DBIP-City-Lite", city_entries)
    write(out / "geo-asn.mmdb", "DBIP-ASN-Lite", asn_entries)
    print(f"wrote {out / 'geo-city.mmdb'} and {out / 'geo-asn.mmdb'}")


if __name__ == "__main__":
    main()
