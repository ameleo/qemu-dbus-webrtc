// SPDX-License-Identifier: MPL-2.0

//! STUN (RFC 8489), just what a session needs to learn the address a NAT maps one of its UDP
//! sockets to: a Binding request to a STUN server, and the XOR-MAPPED-ADDRESS (or
//! MAPPED-ADDRESS) of its success response. That address is a server-reflexive candidate
//! (RFC 8445 5.1.1.2), which a client outside the NAT can reach once the session sent it its
//! own checks: no port to forward on most home routers.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

/// The magic cookie (RFC 8489 5).
const MAGIC_COOKIE: u32 = 0x2112_A442;
/// A Binding request and a Binding success response (RFC 8489 5).
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
/// The attributes read (RFC 8489 18.3).
const MAPPED_ADDRESS: u16 = 0x0001;
const XOR_MAPPED_ADDRESS: u16 = 0x0020;
/// The address families (RFC 8489 14.1).
const IPV4: u8 = 0x01;
const IPV6: u8 = 0x02;

/// How long the server has to answer each request, and the requests sent: a lost one is sent
/// again, as RFC 8489 6.2.1 has a client retransmit; a server that answers neither is left out.
const WAIT: Duration = Duration::from_millis(250);
const TRIES: u32 = 2;

/// A Binding request, with no attribute, for the transaction `id`.
pub fn request(id: &[u8; 12]) -> [u8; 20] {
    let mut message = [0u8; 20];
    message[0..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    // Message length 0: no attribute.
    message[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    message[8..20].copy_from_slice(id);
    message
}

/// The address a Binding success response for the transaction `id` gives: its
/// XOR-MAPPED-ADDRESS, or else its MAPPED-ADDRESS. `None` for another message, another
/// transaction or a malformed one.
pub fn mapped_address(message: &[u8], id: &[u8; 12]) -> Option<SocketAddr> {
    if message.len() < 20
        || u16::from_be_bytes([message[0], message[1]]) != BINDING_SUCCESS
        || message[4..8] != MAGIC_COOKIE.to_be_bytes()
        || message[8..20] != *id
    {
        return None;
    }
    let length = usize::from(u16::from_be_bytes([message[2], message[3]]));
    let attributes = message.get(20..20 + length)?;
    let (mut xored, mut plain) = (None, None);
    let mut at = 0;
    // Attributes: a 16-bit type, a 16-bit length, the value padded to 4 bytes (RFC 8489 14).
    while at + 4 <= attributes.len() {
        let kind = u16::from_be_bytes([attributes[at], attributes[at + 1]]);
        let size = usize::from(u16::from_be_bytes([attributes[at + 2], attributes[at + 3]]));
        let value = attributes.get(at + 4..at + 4 + size)?;
        match kind {
            XOR_MAPPED_ADDRESS => xored = address(value, Some(id)),
            MAPPED_ADDRESS => plain = address(value, None),
            _ => {}
        }
        at += 4 + size.next_multiple_of(4);
    }
    xored.or(plain)
}

/// A MAPPED-ADDRESS value, or an XOR-MAPPED-ADDRESS one when `xor` gives its transaction:
/// 8 zero bits, the family, the port, the address (RFC 8489 14.1, 14.2).
fn address(value: &[u8], xor: Option<&[u8; 12]>) -> Option<SocketAddr> {
    if value.len() < 4 {
        return None;
    }
    let cookie = MAGIC_COOKIE.to_be_bytes();
    let mut port = u16::from_be_bytes([value[2], value[3]]);
    // The port with the cookie's most significant 16 bits; the address with the cookie, and
    // for IPv6 the transaction after it.
    let mask: Vec<u8> = cookie
        .iter()
        .chain(xor.into_iter().flatten())
        .copied()
        .collect();
    if xor.is_some() {
        port ^= u16::from_be_bytes([cookie[0], cookie[1]]);
    }
    let unmask = |bytes: &[u8]| -> Vec<u8> {
        bytes
            .iter()
            .enumerate()
            .map(|(i, b)| if xor.is_some() { b ^ mask[i] } else { *b })
            .collect()
    };
    let ip: IpAddr = match value[1] {
        IPV4 => {
            let bytes: [u8; 4] = unmask(value.get(4..8)?).try_into().ok()?;
            Ipv4Addr::from(bytes).into()
        }
        IPV6 => {
            let bytes: [u8; 16] = unmask(value.get(4..20)?).try_into().ok()?;
            Ipv6Addr::from(bytes).into()
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

/// The address `server` sees `socket` at: a Binding request sent from it, and its response.
/// `None` when the server does not answer in time. The socket must be blocking, and no one
/// else reading it.
pub fn reflexive(socket: &UdpSocket, server: SocketAddr) -> Option<SocketAddr> {
    let rng = ring::rand::SystemRandom::new();
    let mut buffer = [0u8; 1500];
    for _ in 0..TRIES {
        let id: [u8; 12] = ring::rand::generate(&rng).ok()?.expose();
        socket.send_to(&request(&id), server).ok()?;
        let deadline = Instant::now() + WAIT;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            socket
                .set_read_timeout(Some(left.max(Duration::from_millis(1))))
                .ok()?;
            let Ok((size, from)) = socket.recv_from(&mut buffer) else {
                break;
            };
            if from != server {
                continue;
            }
            if let Some(mapped) = mapped_address(&buffer[..size], &id) {
                return Some(mapped);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bytes from a hex dump, with spaces.
    fn bytes(hex: &str) -> Vec<u8> {
        hex.split_whitespace()
            .map(|b| u8::from_str_radix(b, 16).unwrap())
            .collect()
    }

    const ID: [u8; 12] = [
        0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae,
    ];

    /// RFC 5769 2.2, the sample IPv4 response: 192.0.2.1 port 32853.
    #[test]
    fn rfc5769_ipv4_response() {
        let response = bytes(
            "01 01 00 3c 21 12 a4 42 b7 e7 a7 01 bc 34 d6 86 fa 87 df ae \
             80 22 00 0b 74 65 73 74 20 76 65 63 74 6f 72 20 \
             00 20 00 08 00 01 a1 47 e1 12 a6 43 \
             00 08 00 14 2b 91 f5 99 fd 9e 90 c3 8c 74 89 f9 2a f9 ba 53 f0 6b e7 d7 \
             80 28 00 04 c0 7d 4c 96",
        );
        assert_eq!(
            mapped_address(&response, &ID),
            Some("192.0.2.1:32853".parse().unwrap())
        );
        // Another transaction's response is not ours.
        let mut other = ID;
        other[0] ^= 1;
        assert_eq!(mapped_address(&response, &other), None);
        // Cut short: no address.
        assert_eq!(mapped_address(&response[..40], &ID), None);
    }

    /// RFC 5769 2.3, the sample IPv6 response: 2001:db8:1234:5678:11:2233:4455:6677 port 32853.
    #[test]
    fn rfc5769_ipv6_response() {
        let response = bytes(
            "01 01 00 48 21 12 a4 42 b7 e7 a7 01 bc 34 d6 86 fa 87 df ae \
             80 22 00 0b 74 65 73 74 20 76 65 63 74 6f 72 20 \
             00 20 00 14 00 02 a1 47 01 13 a9 fa a5 d3 f1 79 bc 25 f4 b5 be d2 b9 d9 \
             00 08 00 14 a3 82 95 4e 4b e6 7b f1 17 84 c9 7c 82 92 c2 75 bf e3 ed 41 \
             80 28 00 04 c8 fb 0b 4c",
        );
        assert_eq!(
            mapped_address(&response, &ID),
            Some(
                "[2001:db8:1234:5678:11:2233:4455:6677]:32853"
                    .parse()
                    .unwrap()
            )
        );
    }

    /// A request: Binding, no attribute, the cookie, the transaction.
    #[test]
    fn binding_request() {
        let message = request(&ID);
        assert_eq!(
            &message[..8],
            &[0x00, 0x01, 0x00, 0x00, 0x21, 0x12, 0xa4, 0x42]
        );
        assert_eq!(&message[8..], &ID);
    }

    /// A local STUN server, answering with the request's source: the address found.
    #[test]
    fn reflexive_address_from_a_server() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_address = server.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut buffer = [0u8; 1500];
            let (size, from) = server.recv_from(&mut buffer).unwrap();
            let id: [u8; 12] = buffer[8..20].try_into().unwrap();
            assert_eq!(size, 20);
            // A success response with the source as XOR-MAPPED-ADDRESS.
            let IpAddr::V4(ip) = from.ip() else { panic!() };
            let cookie = MAGIC_COOKIE.to_be_bytes();
            let mut response = vec![0x01, 0x01, 0x00, 0x0c];
            response.extend_from_slice(&cookie);
            response.extend_from_slice(&id);
            response.extend_from_slice(&[0x00, 0x20, 0x00, 0x08, 0x00, IPV4]);
            response.extend_from_slice(&(from.port() ^ 0x2112).to_be_bytes());
            response.extend(ip.octets().iter().zip(cookie).map(|(b, c)| b ^ c));
            server.send_to(&response, from).unwrap();
        });
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        assert_eq!(
            reflexive(&client, server_address),
            Some(client.local_addr().unwrap())
        );
    }
}
