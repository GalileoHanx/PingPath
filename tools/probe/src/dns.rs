//! A minimal DNS client (A and TXT queries over UDP), enough for the
//! public-IP and ASN lookups without pulling in a resolver library.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryType {
    A = 1,
    Txt = 16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record {
    A(Ipv4Addr),
    Txt(String),
}

/// Public resolvers used for lookups, tried in order.
pub const RESOLVERS: [Ipv4Addr; 3] = [
    Ipv4Addr::new(1, 1, 1, 1),
    Ipv4Addr::new(8, 8, 8, 8),
    Ipv4Addr::new(9, 9, 9, 9),
];

/// Sends one query to `server` and returns the answer records of the
/// requested type.
pub fn query(
    server: SocketAddr,
    name: &str,
    qtype: QueryType,
    timeout: Duration,
) -> io::Result<Vec<Record>> {
    let id = random_u16();
    let socket = UdpSocket::bind(if server.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    })?;
    socket.set_read_timeout(Some(timeout))?;
    socket.connect(server)?;
    socket.send(&build_query(id, name, qtype)?)?;

    let mut buf = [0u8; 4096];
    loop {
        let n = socket.recv(&mut buf)?;
        match parse_response(&buf[..n], id, qtype) {
            Ok(Some(records)) => return Ok(records),
            Ok(None) => continue, // a stray packet with another ID
            Err(e) => return Err(io::Error::new(io::ErrorKind::InvalidData, e)),
        }
    }
}

/// Queries the public resolvers in turn until one answers.
pub fn resolve(name: &str, qtype: QueryType) -> io::Result<Vec<Record>> {
    let mut last = io::Error::other("no resolver answered");
    for resolver in RESOLVERS {
        match query(
            SocketAddr::from((resolver, 53)),
            name,
            qtype,
            Duration::from_secs(2),
        ) {
            Ok(records) => return Ok(records),
            Err(e) => last = e,
        }
    }
    Err(last)
}

fn build_query(id: u16, name: &str, qtype: QueryType) -> io::Result<Vec<u8>> {
    let mut packet = Vec::with_capacity(32 + name.len());
    packet.extend_from_slice(&id.to_be_bytes());
    packet.extend_from_slice(&[0x01, 0x00]); // recursion desired
    packet.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]); // one question
    for label in name.trim_end_matches('.').split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("bad DNS name {name:?}"),
            ));
        }
        packet.push(label.len() as u8);
        packet.extend_from_slice(label.as_bytes());
    }
    packet.push(0);
    packet.extend_from_slice(&(qtype as u16).to_be_bytes());
    packet.extend_from_slice(&[0, 1]); // class IN
    Ok(packet)
}

/// `Ok(None)` if the packet answers a different query ID.
fn parse_response(packet: &[u8], id: u16, qtype: QueryType) -> Result<Option<Vec<Record>>, String> {
    let u16_at = |i: usize| -> Result<u16, String> {
        packet
            .get(i..i + 2)
            .map(|b| u16::from_be_bytes([b[0], b[1]]))
            .ok_or_else(|| "truncated DNS response".to_string())
    };
    if u16_at(0)? != id {
        return Ok(None);
    }
    let flags = u16_at(2)?;
    let rcode = flags & 0x000f;
    if rcode == 3 {
        return Ok(Some(Vec::new())); // NXDOMAIN: no such name
    }
    if rcode != 0 {
        return Err(format!("DNS server returned error code {rcode}"));
    }
    let questions = u16_at(4)?;
    let answers = u16_at(6)?;

    let mut pos = 12;
    for _ in 0..questions {
        pos = skip_name(packet, pos)? + 4;
    }
    let mut records = Vec::new();
    for _ in 0..answers {
        pos = skip_name(packet, pos)?;
        let rtype = u16_at(pos)?;
        let len = usize::from(u16_at(pos + 8)?);
        let data = packet
            .get(pos + 10..pos + 10 + len)
            .ok_or("truncated DNS record")?;
        pos += 10 + len;
        if rtype != qtype as u16 {
            continue; // e.g. a CNAME before the answer
        }
        match qtype {
            QueryType::A if data.len() == 4 => {
                records.push(Record::A(Ipv4Addr::new(data[0], data[1], data[2], data[3])));
            }
            QueryType::Txt => {
                let mut text = String::new();
                let mut i = 0;
                while let Some(&n) = data.get(i) {
                    let chunk = data
                        .get(i + 1..i + 1 + usize::from(n))
                        .ok_or("truncated TXT record")?;
                    text.push_str(&String::from_utf8_lossy(chunk));
                    i += 1 + usize::from(n);
                }
                records.push(Record::Txt(text));
            }
            QueryType::A => {}
        }
    }
    Ok(Some(records))
}

/// Returns the position just after the (possibly compressed) name at `pos`.
fn skip_name(packet: &[u8], mut pos: usize) -> Result<usize, String> {
    loop {
        let len = *packet.get(pos).ok_or("truncated DNS name")?;
        match len {
            0 => return Ok(pos + 1),
            l if l & 0xc0 == 0xc0 => return Ok(pos + 2), // compression pointer
            l => pos += 1 + usize::from(l),
        }
    }
}

fn random_u16() -> u16 {
    RandomState::new().build_hasher().finish() as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(id: u16, rtype: u16, rdata: &[u8]) -> Vec<u8> {
        let mut p = build_query(id, "example.com", QueryType::Txt).unwrap();
        p[2] = 0x81;
        p[3] = 0x80; // response, no error
        p[7] = 1; // one answer
        p.extend_from_slice(&[0xc0, 12]); // name: pointer to the question
        p.extend_from_slice(&rtype.to_be_bytes());
        p.extend_from_slice(&[0, 1, 0, 0, 0, 60]);
        p.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        p.extend_from_slice(rdata);
        p
    }

    #[test]
    fn builds_a_query() {
        let q = build_query(0x1234, "a.bc.", QueryType::A).unwrap();
        assert_eq!(
            q,
            [
                0x12, 0x34, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 1, b'a', 2, b'b', b'c', 0, 0, 1, 0, 1
            ]
        );
    }

    #[test]
    fn parses_txt_answers_split_into_chunks() {
        let r = response(7, 16, b"\x0513335\x08 | 1.1.1");
        assert_eq!(
            parse_response(&r, 7, QueryType::Txt),
            Ok(Some(vec![Record::Txt("13335 | 1.1.1".into())]))
        );
    }

    #[test]
    fn parses_a_answers() {
        let r = response(9, 1, &[198, 51, 100, 7]);
        assert_eq!(
            parse_response(&r, 9, QueryType::A),
            Ok(Some(vec![Record::A(Ipv4Addr::new(198, 51, 100, 7))]))
        );
    }

    #[test]
    fn ignores_other_ids_and_rejects_truncation() {
        let r = response(9, 1, &[198, 51, 100, 7]);
        assert_eq!(parse_response(&r, 10, QueryType::A), Ok(None));
        assert!(parse_response(&r[..r.len() - 2], 9, QueryType::A).is_err());
    }
}
