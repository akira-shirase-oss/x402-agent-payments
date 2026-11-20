//! Fuzz target for the untrusted client address inputs, run as a property test: the
//! `X-Forwarded-For` value a trusted proxy passes on, and the operator's proxy ranges.
//! Whatever the header says, a valid payment is served once and never panics; a range
//! parses or is refused, and a parsed range contains its own address.
#![expect(clippy::unwrap_used, reason = "test fixtures")]

mod support;

use std::net::IpAddr;
use std::sync::atomic::Ordering;

use axum::http::{HeaderValue, StatusCode};
use proptest::prelude::*;
use support::*;
use xpay_seller::ProxyNet;
use xpay_x402::Caip2;

fn run(forwarded: HeaderValue) -> (StatusCode, usize) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let w = paywall(Setup {
            trusted_proxies: vec!["127.0.0.1".parse().unwrap(), "10.0.0.0/8".parse().unwrap()],
            ..Setup::default()
        });
        let h = header(&pay(&offer(Caip2::BASE_SEPOLIA), 1));
        let peer = "127.0.0.1:1000".parse().unwrap();
        let a = get_paid_via(&w.app, Some(&h), peer, Some(forwarded)).await;
        (a.status, w.served.load(Ordering::SeqCst))
    })
}

fn hop() -> impl Strategy<Value = String> {
    prop_oneof![
        any::<[u8; 4]>().prop_map(|b| IpAddr::from(b).to_string()),
        any::<[u8; 4]>().prop_map(|b| format!("10.{}.{}.{}", b[0], b[1], b[2])),
        any::<[u8; 16]>().prop_map(|b| IpAddr::from(b).to_string()),
        "[ -~]{0,48}",
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1_000, ..ProptestConfig::default() })]

    #[test]
    fn any_forwarded_chain_serves_a_valid_payment_once(hops in prop::collection::vec(hop(), 0..24)) {
        let Ok(value) = HeaderValue::from_str(&hops.join(", ")) else {
            return Ok(());
        };
        let (status, served) = run(value);
        prop_assert_eq!(status, StatusCode::OK);
        prop_assert_eq!(served, 1);
    }

    #[test]
    fn arbitrary_bytes_as_forwarded_never_panic(raw in prop::collection::vec(any::<u8>(), 0..256)) {
        let Ok(value) = HeaderValue::from_bytes(&raw) else {
            return Ok(());
        };
        let (status, served) = run(value);
        prop_assert_eq!(status, StatusCode::OK);
        prop_assert_eq!(served, 1);
    }

    #[test]
    fn a_proxy_range_parses_or_is_refused(s in "[ -~]{0,64}", ip in any::<[u8; 16]>(), p in 0u8..=140) {
        let _ = s.parse::<ProxyNet>();
        let addr = IpAddr::from(ip);
        if let Ok(net) = format!("{addr}/{p}").parse::<ProxyNet>() {
            prop_assert!(net.contains(addr));
        }
        let v4 = IpAddr::from([ip[0], ip[1], ip[2], ip[3]]);
        if let Ok(net) = format!("{v4}/{p}").parse::<ProxyNet>() {
            prop_assert!(net.contains(v4));
            prop_assert!((1..=32).contains(&p));
        }
    }
}
