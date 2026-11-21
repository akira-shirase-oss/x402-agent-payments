//! Fuzz target for the untrusted `PAYMENT-SIGNATURE` header, run as a property test on the
//! pinned stable toolchain: arbitrary bytes, arbitrary base64 of arbitrary JSON, and
//! single-byte mutations of a valid header. Whatever arrives, the layer answers a client
//! error, never panics, never serves and never claims a nonce.
#![expect(clippy::unwrap_used, reason = "test fixtures")]

mod support;

use std::sync::atomic::Ordering;

use axum::http::{HeaderValue, StatusCode};
use base64::Engine as _;
use proptest::prelude::*;
use support::*;
use xpay_x402::Caip2;

fn refused(status: StatusCode) -> bool {
    [
        StatusCode::BAD_REQUEST,
        StatusCode::PAYMENT_REQUIRED,
        StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
    ]
    .contains(&status)
}

fn run(header: &str) -> (StatusCode, usize, usize) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let w = paywall(Setup::default());
        let a = get_paid(&w.app, Some(header)).await;
        (a.status, w.served.load(Ordering::SeqCst), w.store.len())
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2_000, ..ProptestConfig::default() })]

    #[test]
    fn arbitrary_header_text_is_refused(raw in "[ -~]{0,9000}") {
        prop_assume!(HeaderValue::from_str(&raw).is_ok());
        let (status, served, claims) = run(&raw);
        prop_assert!(refused(status), "{status}");
        prop_assert_eq!((served, claims), (0, 0));
    }

    #[test]
    fn base64_of_arbitrary_json_is_refused(json in any::<Vec<u8>>()) {
        let h = base64::engine::general_purpose::STANDARD.encode(json);
        let (status, served, claims) = run(&h);
        prop_assert!(refused(status), "{status}");
        prop_assert_eq!((served, claims), (0, 0));
    }

    #[test]
    fn a_mutated_valid_header_is_never_served(pos in any::<prop::sample::Index>(), byte in 0x21u8..0x7f) {
        let valid = header(&pay(&offer(Caip2::BASE_SEPOLIA), 0x77));
        let mut bytes = valid.clone().into_bytes();
        let i = pos.index(bytes.len());
        prop_assume!(bytes[i] != byte);
        bytes[i] = byte;
        let mutated = String::from_utf8(bytes).unwrap();
        let (status, served, _) = run(&mutated);
        // A mutation of a padding bit or of a field the seller ignores (an unknown key, the
        // buyer's copy of `extra`: the seller checks against its own offer) can leave every
        // signed field intact; that payment is legitimately served, once.
        if status == StatusCode::OK {
            let got: xpay_x402::PaymentPayload = xpay_x402::decode_header(mutated.as_bytes()).unwrap();
            let want = pay(&offer(Caip2::BASE_SEPOLIA), 0x77);
            // Hex is case-insensitive: `0xAB` and `0xab` are the same signature.
            let lower = |m: &serde_json::Map<String, serde_json::Value>| serde_json::Value::Object(m.clone()).to_string().to_lowercase();
            prop_assert_eq!(lower(&got.payload), lower(&want.payload));
            let (g, w) = (&got.accepted, &want.accepted);
            prop_assert_eq!((&g.scheme, &g.network, &g.amount), (&w.scheme, &w.network, &w.amount));
            prop_assert!(g.asset.eq_ignore_ascii_case(&w.asset) && g.pay_to.eq_ignore_ascii_case(&w.pay_to));
            prop_assert_eq!(served, 1);
        } else {
            prop_assert!(refused(status), "{status}");
            prop_assert_eq!(served, 0);
        }
    }
}
