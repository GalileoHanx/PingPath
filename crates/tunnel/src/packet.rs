//! IPv4/UDP packets as they come out of and go into the tunnel.
//!
//! Parsing is lenient about checksums: everything inside the tunnel is
//! already authenticated by WireGuard, and the exit forwards only the UDP
//! payload, letting its own kernel build fresh headers. Packets built here
//! carry correct checksums, because the client's operating system checks
//! them.

use std::net::{Ipv4Addr, SocketAddrV4};

use thiserror::Error;

pub const IPV4_HEADER_LEN: usize = 20;
pub const UDP_HEADER_LEN: usize = 8;
pub const PROTOCOL_TCP: u8 = 6;
pub const PROTOCOL_UDP: u8 = 17;
/// The exit relay answers UDP sent to this address inside the tunnel with
/// the same payload: an authenticated round trip that tests the whole path
/// and keeps WireGuard's timers running when the game is quiet. It's in
/// carrier-grade NAT space, which game definitions may never list, so it can
/// never collide with a real server.
pub const TUNNEL_ECHO: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(100, 64, 0, 1), 7);
/// Largest UDP payload a tunnel packet can carry.
pub const MAX_TUNNEL_UDP_PAYLOAD: usize =
    crate::frame::TUNNEL_MTU - IPV4_HEADER_LEN - UDP_HEADER_LEN;
/// Largest UDP payload an IPv4 packet can carry.
pub const MAX_UDP_PAYLOAD: usize = u16::MAX as usize - IPV4_HEADER_LEN - UDP_HEADER_LEN;

/// A UDP datagram inside an IPv4 packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UdpPacket<'a> {
    pub src: SocketAddrV4,
    pub dst: SocketAddrV4,
    pub payload: &'a [u8],
}

/// Why a packet from the tunnel can't be forwarded. Each is counted
/// separately by the relay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PacketError {
    #[error("not an IPv4 packet (IP version {0})")]
    NotIpv4(u8),
    #[error("malformed IPv4 header")]
    Malformed,
    #[error("IPv4 fragment")]
    Fragment,
    #[error("IP protocol {0} is not UDP")]
    NotUdp(u8),
}

/// Parses an IPv4 packet carrying UDP.
pub fn parse_udp(packet: &[u8]) -> Result<UdpPacket<'_>, PacketError> {
    let version = packet.first().map_or(0, |b| b >> 4);
    if version != 4 {
        return Err(PacketError::NotIpv4(version));
    }
    if packet.len() < IPV4_HEADER_LEN {
        return Err(PacketError::Malformed);
    }
    let header_len = usize::from(packet[0] & 0x0f) * 4;
    let total_len = usize::from(u16::from_be_bytes([packet[2], packet[3]]));
    if header_len < IPV4_HEADER_LEN || total_len < header_len || total_len > packet.len() {
        return Err(PacketError::Malformed);
    }
    let flags_and_offset = u16::from_be_bytes([packet[6], packet[7]]);
    let more_fragments = flags_and_offset & 0x2000 != 0;
    let offset = flags_and_offset & 0x1fff;
    if more_fragments || offset != 0 {
        return Err(PacketError::Fragment);
    }
    let protocol = packet[9];
    if protocol != PROTOCOL_UDP {
        return Err(PacketError::NotUdp(protocol));
    }

    let udp = &packet[header_len..total_len];
    if udp.len() < UDP_HEADER_LEN {
        return Err(PacketError::Malformed);
    }
    let udp_len = usize::from(u16::from_be_bytes([udp[4], udp[5]]));
    if udp_len < UDP_HEADER_LEN || udp_len > udp.len() {
        return Err(PacketError::Malformed);
    }
    let addr =
        |at: usize| Ipv4Addr::new(packet[at], packet[at + 1], packet[at + 2], packet[at + 3]);
    let port = |at: usize| u16::from_be_bytes([udp[at], udp[at + 1]]);
    Ok(UdpPacket {
        src: SocketAddrV4::new(addr(12), port(0)),
        dst: SocketAddrV4::new(addr(16), port(2)),
        payload: &udp[UDP_HEADER_LEN..udp_len],
    })
}

/// Builds an IPv4/UDP packet with correct IP and UDP checksums.
///
/// # Panics
/// If `payload` is longer than [`MAX_UDP_PAYLOAD`].
pub fn build_udp(src: SocketAddrV4, dst: SocketAddrV4, payload: &[u8]) -> Vec<u8> {
    assert!(
        payload.len() <= MAX_UDP_PAYLOAD,
        "UDP payload too large for IPv4"
    );
    let udp_len = (UDP_HEADER_LEN + payload.len()) as u16;
    let total_len = IPV4_HEADER_LEN as u16 + udp_len;

    let mut packet = Vec::with_capacity(usize::from(total_len));
    packet.extend_from_slice(&[0x45, 0]); // version 4, 20-byte header, no DSCP
    packet.extend_from_slice(&total_len.to_be_bytes());
    packet.extend_from_slice(&[0, 0, 0x40, 0]); // ID 0, don't fragment
    packet.extend_from_slice(&[64, PROTOCOL_UDP, 0, 0]); // TTL, protocol, checksum placeholder
    packet.extend_from_slice(&src.ip().octets());
    packet.extend_from_slice(&dst.ip().octets());
    let header_checksum = checksum(&[&packet[..IPV4_HEADER_LEN]]);
    packet[10..12].copy_from_slice(&header_checksum.to_be_bytes());

    packet.extend_from_slice(&src.port().to_be_bytes());
    packet.extend_from_slice(&dst.port().to_be_bytes());
    packet.extend_from_slice(&udp_len.to_be_bytes());
    packet.extend_from_slice(&[0, 0]); // checksum placeholder
    packet.extend_from_slice(payload);

    let mut pseudo = [0u8; 12];
    pseudo[..4].copy_from_slice(&src.ip().octets());
    pseudo[4..8].copy_from_slice(&dst.ip().octets());
    pseudo[9] = PROTOCOL_UDP;
    pseudo[10..12].copy_from_slice(&udp_len.to_be_bytes());
    let udp_checksum = match checksum(&[&pseudo, &packet[IPV4_HEADER_LEN..]]) {
        0 => 0xffff, // zero means "no checksum" in UDP over IPv4
        sum => sum,
    };
    packet[IPV4_HEADER_LEN + 6..IPV4_HEADER_LEN + 8].copy_from_slice(&udp_checksum.to_be_bytes());
    packet
}

/// The Internet checksum (RFC 1071) over the concatenation of `parts`.
fn checksum(parts: &[&[u8]]) -> u16 {
    let mut sum: u32 = 0;
    let mut odd: Option<u8> = None;
    for &byte in parts.iter().flat_map(|p| p.iter()) {
        match odd.take() {
            Some(high) => sum += u32::from(u16::from_be_bytes([high, byte])),
            None => odd = Some(byte),
        }
    }
    if let Some(high) = odd {
        sum += u32::from(u16::from_be_bytes([high, 0]));
    }
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> SocketAddrV4 {
        s.parse().unwrap()
    }

    #[test]
    fn matches_a_reference_packet() {
        // Reference bytes computed independently (Python, RFC 1071).
        let packet = build_udp(addr("10.0.0.2:40000"), addr("198.51.100.7:27015"), b"hello");
        let expected: [u8; 33] = [
            0x45, 0x00, 0x00, 0x21, 0x00, 0x00, 0x40, 0x00, 0x40, 0x11, 0x06, 0x90, 0x0a, 0x00,
            0x00, 0x02, 0xc6, 0x33, 0x64, 0x07, 0x9c, 0x40, 0x69, 0x87, 0x00, 0x0d, 0x81, 0xfd,
            b'h', b'e', b'l', b'l', b'o',
        ];
        assert_eq!(packet, expected);
    }

    #[test]
    fn round_trips() {
        let packet = build_udp(
            addr("10.1.2.3:1234"),
            addr("203.0.113.9:7777"),
            b"odd length payload!",
        );
        let parsed = parse_udp(&packet).unwrap();
        assert_eq!(parsed.src, addr("10.1.2.3:1234"));
        assert_eq!(parsed.dst, addr("203.0.113.9:7777"));
        assert_eq!(parsed.payload, b"odd length payload!");
        // A correct header sums to all ones.
        assert_eq!(checksum(&[&packet[..IPV4_HEADER_LEN]]), 0);
    }

    #[test]
    fn rejects_what_the_exit_cannot_forward() {
        let good = build_udp(addr("10.0.0.2:1"), addr("198.51.100.1:2"), b"x");

        let mut tcp = good.clone();
        tcp[9] = PROTOCOL_TCP;
        assert_eq!(parse_udp(&tcp), Err(PacketError::NotUdp(PROTOCOL_TCP)));

        let mut fragment = good.clone();
        fragment[6] = 0x20; // more fragments
        assert_eq!(parse_udp(&fragment), Err(PacketError::Fragment));

        let mut ipv6 = good.clone();
        ipv6[0] = 0x60;
        assert_eq!(parse_udp(&ipv6), Err(PacketError::NotIpv4(6)));

        assert_eq!(
            parse_udp(&good[..good.len() - 2]),
            Err(PacketError::Malformed)
        );
        let mut lying_udp_len = good.clone();
        lying_udp_len[24..26].copy_from_slice(&200u16.to_be_bytes());
        assert_eq!(parse_udp(&lying_udp_len), Err(PacketError::Malformed));
        assert_eq!(parse_udp(&[]), Err(PacketError::NotIpv4(0)));
    }

    #[test]
    fn honours_ip_options_and_trailing_bytes() {
        let plain = build_udp(addr("10.0.0.2:5"), addr("198.51.100.1:6"), b"data");
        // Insert 4 bytes of options: IHL 6.
        let mut with_options = plain[..20].to_vec();
        with_options[0] = 0x46;
        with_options[2..4].copy_from_slice(&((plain.len() + 4) as u16).to_be_bytes());
        with_options.extend_from_slice(&[1, 1, 1, 1]);
        with_options.extend_from_slice(&plain[20..]);
        with_options.extend_from_slice(&[0xaa; 3]); // padding past total length
        let parsed = parse_udp(&with_options).unwrap();
        assert_eq!(parsed.payload, b"data");
    }
}
