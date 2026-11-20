//! Who a request comes from, for budgets and for remembering where an account signs in.
//!
//! The client is the TCP peer: IPv4 as is, IPv6 by its /64 (one host usually holds a whole
//! /64, so rotating within it buys nothing), an IPv4-mapped IPv6 address as IPv4. Behind a
//! reverse proxy every client shares the proxy's address unless the proxy is trusted; then
//! the right-most `X-Forwarded-For` entry — the hop the proxy itself appended — is the
//! client. Entries to its left were written by the client and are never believed (from
//! Service Monitor's `throttle.rs`).

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use axum::http::HeaderMap;

/// An IPv4 address, or an IPv6 /64.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClientKey {
    /// An IPv4 client.
    V4(Ipv4Addr),
    /// The first four segments of an IPv6 client.
    V6Net([u16; 4]),
}

impl ClientKey {
    /// The key for one address.
    pub fn of(ip: IpAddr) -> Self {
        match ip {
            IpAddr::V4(v4) => Self::V4(v4),
            IpAddr::V6(v6) => {
                if let Some(v4) = v6.to_ipv4_mapped() {
                    Self::V4(v4)
                } else {
                    let s = v6.segments();
                    Self::V6Net([s[0], s[1], s[2], s[3]])
                }
            }
        }
    }

    /// The client behind a request from `peer`: the peer itself, or — only when the peer is
    /// a trusted proxy — the right-most `X-Forwarded-For` entry. If that hop does not parse
    /// the client is the peer, never an entry further left.
    pub fn from_request(peer: SocketAddr, headers: &HeaderMap, trusted: &[IpAddr]) -> Self {
        if trusted.contains(&peer.ip())
            && let Some(last_line) = headers.get_all("x-forwarded-for").iter().next_back()
            && let Some(hop) = last_line
                .to_str()
                .ok()
                .and_then(|line| line.rsplit(',').next())
            && let Ok(client) = hop.trim().parse::<IpAddr>()
        {
            return Self::of(client);
        }
        Self::of(peer.ip())
    }
}

impl fmt::Display for ClientKey {
    /// A stable text form, for storing (`4:198.51.100.1`, `6:2001:db8:0:1::/64`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::V4(v4) => write!(f, "4:{v4}"),
            Self::V6Net(s) => write!(f, "6:{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(ip: &str) -> ClientKey {
        ClientKey::of(ip.parse().expect("ip"))
    }

    #[test]
    fn ipv6_clients_are_grouped_by_64_and_mapped_v4_is_v4() {
        assert_eq!(key("2001:db8::1"), key("2001:db8::ffff:1234"));
        assert_ne!(key("2001:db8::1"), key("2001:db8:0:1::1"));
        assert_eq!(key("::ffff:198.51.100.1"), key("198.51.100.1"));
        assert_ne!(key("198.51.100.1"), key("198.51.100.2"));
        assert_eq!(key("2001:db8:0:1::7").to_string(), "6:2001:db8:0:1::/64");
        assert_eq!(key("198.51.100.1").to_string(), "4:198.51.100.1");
    }

    #[test]
    fn forwarded_for_is_believed_only_from_a_trusted_proxy_and_only_its_hop() {
        let proxy: SocketAddr = "127.0.0.1:5000".parse().expect("addr");
        let stranger: SocketAddr = "203.0.113.5:5000".parse().expect("addr");
        let trusted = [proxy.ip()];
        let mut headers = HeaderMap::new();
        headers.append(
            "x-forwarded-for",
            "6.6.6.6, 198.51.100.9".parse().expect("hv"),
        );
        assert_eq!(
            ClientKey::from_request(proxy, &headers, &trusted),
            key("198.51.100.9")
        );
        assert_eq!(
            ClientKey::from_request(stranger, &headers, &trusted),
            key("203.0.113.5")
        );
        assert_eq!(
            ClientKey::from_request(proxy, &headers, &[]),
            key("127.0.0.1")
        );
        for forwarded in [
            vec!["6.6.6.6, 198.51.100.9:443"],
            vec!["6.6.6.6, not-an-ip"],
            vec!["6.6.6.6,"],
            vec!["6.6.6.6", "garbage"],
        ] {
            let mut headers = HeaderMap::new();
            for value in &forwarded {
                headers.append("x-forwarded-for", value.parse().expect("hv"));
            }
            assert_eq!(
                ClientKey::from_request(proxy, &headers, &trusted),
                key("127.0.0.1"),
                "{forwarded:?}"
            );
        }
    }
}
