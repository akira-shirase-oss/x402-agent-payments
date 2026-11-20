//! The seller layer against the mock facilitator, in process: the 402 → pay → 200 loop and
//! every failure the contract names (`docs/END_GOAL.md`, M1). Time is a manual clock and
//! tokio's paused clock, never the machine's speed.
#![expect(clippy::unwrap_used, reason = "test fixtures")]
#![allow(
    clippy::many_single_char_names,
    reason = "short fixture names read better in tables of cases"
)]

mod support;

use std::sync::atomic::Ordering;
use std::time::Duration;

use alloy_primitives::{B256, U256, address};
use axum::http::StatusCode;
use serde_json::json;
use support::*;
use xpay_facilitator_mock::Faults;
use xpay_seller::{ClaimState, NonceKey, SettleMode};
use xpay_x402::exact::{self, TypedAuthorization};
use xpay_x402::{
    Caip2, MAX_HEADER_BYTES, PaymentRequired, SettleResponse, decode_header, encode_header, reason,
};

fn key_of(p: &xpay_x402::PaymentPayload) -> NonceKey {
    let e = exact::ExactEvmPayload::from_payload(p).unwrap();
    let a = TypedAuthorization::parse(&e.authorization).unwrap();
    NonceKey {
        asset: p.accepted.asset.parse().unwrap(),
        payer: a.from,
        nonce: a.nonce,
    }
}

fn error_of(a: &Answer) -> &str {
    a.body["error"].as_str().unwrap_or_default()
}

#[tokio::test]
async fn no_payment_answers_402_with_the_requirements_and_per_network_domains() {
    let w = paywall(Setup {
        offers: vec![offer(Caip2::BASE_SEPOLIA), offer(Caip2::BASE)],
        ..Setup::default()
    });
    let a = get_paid(&w.app, None).await;
    assert_eq!(a.status, StatusCode::PAYMENT_REQUIRED);
    let required: PaymentRequired =
        decode_header(a.headers["payment-required"].as_bytes()).unwrap();
    assert_eq!(required.x402_version, 2);
    let names: Vec<_> = required
        .accepts
        .iter()
        .map(|r| {
            (
                r.network.to_string(),
                r.extra.as_ref().unwrap()["name"].clone(),
            )
        })
        .collect();
    assert_eq!(
        names,
        [
            ("eip155:84532".to_owned(), json!("USDC")),
            ("eip155:8453".to_owned(), json!("USD Coin")),
        ]
    );
    assert_eq!(
        a.body["accepts"][0]["payTo"],
        json!(SELLER.to_checksum(None))
    );
    assert_eq!(w.served.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn pays_then_serves_once_with_the_settlement_in_payment_response() {
    let w = paywall(Setup::default());
    let p = pay(&offer(Caip2::BASE_SEPOLIA), 1);
    let a = get_paid(&w.app, Some(&header(&p))).await;
    assert_eq!(a.status, StatusCode::OK, "{:?}", a.body);
    let settle: SettleResponse = decode_header(a.headers["payment-response"].as_bytes()).unwrap();
    assert!(settle.success);
    assert_eq!(settle.network.as_str(), Caip2::BASE_SEPOLIA);
    assert_eq!(
        settle.payer.as_deref(),
        Some(buyer().address().to_checksum(None).as_str())
    );
    assert_eq!(w.served.load(Ordering::SeqCst), 1);
    assert_eq!(w.mock.settled_count(), 1);
    assert!(matches!(
        w.store.state(&key_of(&p)),
        Some(ClaimState::Settled { .. })
    ));
}

#[tokio::test]
async fn a_replayed_payment_is_refused_and_never_served_twice() {
    let w = paywall(Setup::default());
    let h = header(&pay(&offer(Caip2::BASE_SEPOLIA), 2));
    assert_eq!(get_paid(&w.app, Some(&h)).await.status, StatusCode::OK);
    let again = get_paid(&w.app, Some(&h)).await;
    assert_eq!(again.status, StatusCode::PAYMENT_REQUIRED);
    assert_eq!(error_of(&again), reason::NONCE_REUSED);
    assert_eq!(w.served.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn one_payment_sent_concurrently_is_served_exactly_once() {
    let w = paywall(Setup::default());
    let h = header(&pay(&offer(Caip2::BASE_SEPOLIA), 3));
    let all = futures_join(64, |_| {
        let app = w.app.clone();
        let h = h.clone();
        async move { get_paid(&app, Some(&h)).await.status }
    })
    .await;
    assert_eq!(
        all.iter().filter(|s| **s == StatusCode::OK).count(),
        1,
        "{all:?}"
    );
    assert!(all.iter().all(|s| {
        [
            StatusCode::OK,
            StatusCode::CONFLICT,
            StatusCode::PAYMENT_REQUIRED,
        ]
        .contains(s)
    }));
    assert_eq!(w.served.load(Ordering::SeqCst), 1);
    assert_eq!(w.mock.settled_count(), 1);
}

async fn futures_join<F, Fut>(n: usize, f: F) -> Vec<StatusCode>
where
    F: Fn(usize) -> Fut,
    Fut: std::future::Future<Output = StatusCode> + Send + 'static,
{
    let tasks: Vec<_> = (0..n).map(|i| tokio::spawn(f(i))).collect();
    let mut out = Vec::new();
    for t in tasks {
        out.push(t.await.unwrap());
    }
    out
}

#[tokio::test]
async fn an_expired_authorization_is_refused_before_any_claim() {
    let w = paywall(Setup::default());
    let p = pay(&offer(Caip2::BASE_SEPOLIA), 4);
    w.clock.advance(60);
    let a = get_paid(&w.app, Some(&header(&p))).await;
    assert_eq!(
        (a.status, error_of(&a)),
        (StatusCode::PAYMENT_REQUIRED, reason::VALID_BEFORE)
    );
    assert!(w.store.is_empty());
    assert_eq!(w.mock.settled_count(), 0);
}

#[tokio::test]
async fn an_authorization_valid_too_long_is_refused() {
    let w = paywall(Setup::default());
    let req = offer(Caip2::BASE_SEPOLIA);
    let b = buyer();
    let auth = TypedAuthorization {
        from: b.address(),
        to: SELLER,
        value: U256::from(10_000),
        valid_after: U256::ZERO,
        valid_before: U256::from(NOW + 86_400),
        nonce: B256::repeat_byte(5),
    };
    let mut p = pay(&req, 5);
    p.payload = exact::sign(&b, &auth, &req).unwrap().to_map();
    let a = get_paid(&w.app, Some(&header(&p))).await;
    assert_eq!(
        (a.status, error_of(&a)),
        (StatusCode::PAYMENT_REQUIRED, reason::VALID_BEFORE)
    );
}

#[tokio::test]
async fn wrong_amount_recipient_or_network_is_refused() {
    let w = paywall(Setup::default());
    let good = offer(Caip2::BASE_SEPOLIA);
    let b = buyer();
    let signed = |edit: &dyn Fn(&mut TypedAuthorization)| {
        let mut p = pay(&good, 6);
        let e = exact::ExactEvmPayload::from_payload(&p).unwrap();
        let mut a = TypedAuthorization::parse(&e.authorization).unwrap();
        edit(&mut a);
        p.payload = exact::sign(&b, &a, &good).unwrap().to_map();
        header(&p)
    };
    let cases = [
        (
            "signed amount below the price",
            signed(&|a| a.value = U256::from(9_999)),
            reason::VALUE_MISMATCH,
        ),
        (
            "recipient is not the seller",
            signed(&|a| a.to = address!("0x000000000000000000000000000000000000dEaD")),
            reason::RECIPIENT_MISMATCH,
        ),
        (
            "an offer the seller never made (cheaper)",
            {
                let mut cheap = good.clone();
                cheap.amount = "1".into();
                header(&pay(&cheap, 7))
            },
            reason::REQUIREMENTS,
        ),
        (
            "another network than offered",
            header(&pay(&offer(Caip2::BASE), 8)),
            reason::REQUIREMENTS,
        ),
        (
            "Base Sepolia's domain name on Base",
            {
                let w2_offer = offer(Caip2::BASE);
                let mut p = pay(&w2_offer, 9);
                let sepolia_named = xpay_x402::PaymentRequirements {
                    extra: good.extra.clone(),
                    ..w2_offer.clone()
                };
                let e = exact::ExactEvmPayload::from_payload(&p).unwrap();
                let a = TypedAuthorization::parse(&e.authorization).unwrap();
                p.payload = exact::sign(&b, &a, &sepolia_named).unwrap().to_map();
                header(&p)
            },
            "base-only",
        ),
    ];
    for (what, h, want) in cases {
        if want == "base-only" {
            let base = paywall(Setup {
                offers: vec![offer(Caip2::BASE)],
                ..Setup::default()
            });
            let a = get_paid(&base.app, Some(&h)).await;
            assert_eq!(
                (a.status, error_of(&a)),
                (StatusCode::PAYMENT_REQUIRED, reason::SIGNATURE),
                "{what}"
            );
            continue;
        }
        let a = get_paid(&w.app, Some(&h)).await;
        assert_eq!(
            (a.status, error_of(&a)),
            (StatusCode::PAYMENT_REQUIRED, want),
            "{what}"
        );
    }
    assert!(
        w.store.is_empty(),
        "nothing refused offline is ever claimed"
    );
    assert_eq!(w.served.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn a_settle_timeout_holds_the_claim_and_never_serves() {
    let w = paywall(Setup {
        faults: Faults {
            settle_delay: Some(Duration::from_secs(3600)),
            ..Faults::default()
        },
        ..Setup::default()
    });
    let p = pay(&offer(Caip2::BASE_SEPOLIA), 10);
    let a = get_paid(&w.app, Some(&header(&p))).await;
    assert_eq!(
        (a.status, error_of(&a)),
        (StatusCode::GATEWAY_TIMEOUT, "settle_timeout")
    );
    assert_eq!(
        w.store.state(&key_of(&p)),
        Some(ClaimState::Held {
            reason: "settle_timeout"
        })
    );
    // The retry with the same proof is not served and does not release the claim.
    w.mock.set_faults(Faults::default());
    let again = get_paid(&w.app, Some(&header(&p))).await;
    assert_eq!(again.status, StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(
        w.store.state(&key_of(&p)),
        Some(ClaimState::Held {
            reason: "settle_timeout"
        })
    );
    assert_eq!(w.served.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn settlement_pending_or_a_lost_answer_holds_the_claim() {
    for (faults, status, why) in [
        (
            Faults {
                settle_pending: true,
                ..Faults::default()
            },
            StatusCode::GATEWAY_TIMEOUT,
            reason::SETTLEMENT_PENDING,
        ),
        (
            Faults {
                settle_lost_answer: true,
                ..Faults::default()
            },
            StatusCode::BAD_GATEWAY,
            reason::SETTLE_ERROR,
        ),
    ] {
        let w = paywall(Setup {
            faults,
            ..Setup::default()
        });
        let p = pay(&offer(Caip2::BASE_SEPOLIA), 11);
        let a = get_paid(&w.app, Some(&header(&p))).await;
        assert_eq!((a.status, error_of(&a)), (status, why));
        assert_eq!(
            w.store.state(&key_of(&p)),
            Some(ClaimState::Held { reason: why })
        );
        assert_eq!(
            w.mock.settled_count(),
            1,
            "the money moved; the claim must stay"
        );
        assert_eq!(w.served.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn a_refused_settlement_releases_the_claim_and_says_why() {
    let w = paywall(Setup {
        faults: Faults {
            settle_refuse: Some(reason::INSUFFICIENT_FUNDS),
            ..Faults::default()
        },
        ..Setup::default()
    });
    let p = pay(&offer(Caip2::BASE_SEPOLIA), 12);
    let a = get_paid(&w.app, Some(&header(&p))).await;
    assert_eq!(
        (a.status, error_of(&a)),
        (StatusCode::PAYMENT_REQUIRED, reason::INSUFFICIENT_FUNDS)
    );
    let settle: SettleResponse = decode_header(a.headers["payment-response"].as_bytes()).unwrap();
    assert!(!settle.success);
    assert!(w.store.is_empty());
    // Funded now, the same authorisation goes through.
    w.mock.set_faults(Faults::default());
    assert_eq!(
        get_paid(&w.app, Some(&header(&p))).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn an_unreachable_facilitator_at_verify_releases_the_claim() {
    let w = paywall(Setup {
        faults: Faults {
            verify_unreachable: true,
            ..Faults::default()
        },
        ..Setup::default()
    });
    let p = pay(&offer(Caip2::BASE_SEPOLIA), 13);
    let a = get_paid(&w.app, Some(&header(&p))).await;
    assert_eq!(a.status, StatusCode::BAD_GATEWAY);
    assert!(w.store.is_empty());
    assert_eq!(w.served.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn malformed_and_oversized_headers_are_refused() {
    let w = paywall(Setup::default());
    for bad in ["%%%", "e30=", "bm90IGpzb24=", ""] {
        let a = get_paid(&w.app, Some(bad)).await;
        assert_eq!(
            (a.status, error_of(&a)),
            (StatusCode::BAD_REQUEST, reason::PAYLOAD),
            "{bad:?}"
        );
    }
    let huge = "A".repeat(MAX_HEADER_BYTES + 1);
    assert_eq!(
        get_paid(&w.app, Some(&huge)).await.status,
        StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE
    );
    let mut v1 = serde_json::to_value(pay(&offer(Caip2::BASE_SEPOLIA), 14)).unwrap();
    v1["x402Version"] = json!(1);
    let a = get_paid(&w.app, Some(&encode_header(&v1).unwrap())).await;
    assert_eq!(
        (a.status, error_of(&a)),
        (StatusCode::PAYMENT_REQUIRED, reason::VERSION)
    );
    assert_eq!(w.served.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_full_nonce_store_sheds_instead_of_forgetting() {
    let w = paywall(Setup {
        faults: Faults {
            settle_pending: true,
            ..Faults::default()
        },
        capacity: 1,
        ..Setup::default()
    });
    assert_eq!(
        get_paid(&w.app, Some(&header(&pay(&offer(Caip2::BASE_SEPOLIA), 15))))
            .await
            .status,
        StatusCode::GATEWAY_TIMEOUT
    );
    let a = get_paid(&w.app, Some(&header(&pay(&offer(Caip2::BASE_SEPOLIA), 16)))).await;
    assert_eq!(
        (a.status, error_of(&a)),
        (StatusCode::SERVICE_UNAVAILABLE, "payments_busy")
    );
    assert_eq!(a.headers["retry-after"], "1");
}

#[tokio::test]
async fn settle_after_mode_serves_then_settles_and_a_failed_handler_releases() {
    let w = paywall(Setup {
        mode: SettleMode::SettleAfter,
        ..Setup::default()
    });
    let p = pay(&offer(Caip2::BASE_SEPOLIA), 17);
    let a = get_paid(&w.app, Some(&header(&p))).await;
    assert_eq!(a.status, StatusCode::OK);
    assert!(a.headers.contains_key("payment-response"));
    assert_eq!(
        get_paid(&w.app, Some(&header(&p))).await.status,
        StatusCode::PAYMENT_REQUIRED
    );
    assert_eq!(w.served.load(Ordering::SeqCst), 1);

    let failing = paywall(Setup {
        mode: SettleMode::SettleAfter,
        handler_status: StatusCode::INTERNAL_SERVER_ERROR,
        ..Setup::default()
    });
    let a = get_paid(&failing.app, Some(&header(&p))).await;
    assert_eq!(a.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        failing.mock.settled_count(),
        0,
        "nothing settled for a failed handler"
    );
    assert!(failing.store.is_empty());
}

#[tokio::test(start_paused = true)]
async fn settle_after_mode_drops_the_response_when_settlement_times_out() {
    let w = paywall(Setup {
        mode: SettleMode::SettleAfter,
        faults: Faults {
            settle_delay: Some(Duration::from_secs(3600)),
            ..Faults::default()
        },
        ..Setup::default()
    });
    let p = pay(&offer(Caip2::BASE_SEPOLIA), 18);
    let a = get_paid(&w.app, Some(&header(&p))).await;
    assert_eq!(a.status, StatusCode::GATEWAY_TIMEOUT);
    assert!(
        a.body.get("error").is_some(),
        "the handler's body is not sent"
    );
    assert_eq!(
        w.store.state(&key_of(&p)),
        Some(ClaimState::Held {
            reason: "settle_timeout"
        })
    );
}

/// A facilitator that says yes to everything, every time: a replayed or concurrent copy
/// is then stopped by the layer's claim alone, not by the facilitator's own bookkeeping.
struct AlwaysYes;

impl xpay_seller::Facilitator for AlwaysYes {
    fn verify(
        &self,
        _: xpay_x402::FacilitatorRequest,
    ) -> xpay_x402::facilitator::BoxFuture<
        '_,
        Result<xpay_x402::VerifyResponse, xpay_seller::FacilitatorError>,
    > {
        Box::pin(async {
            Ok(xpay_x402::VerifyResponse {
                is_valid: true,
                invalid_reason: None,
                invalid_message: None,
                payer: None,
                extensions: None,
            })
        })
    }

    fn settle(
        &self,
        req: xpay_x402::FacilitatorRequest,
    ) -> xpay_x402::facilitator::BoxFuture<'_, Result<SettleResponse, xpay_seller::FacilitatorError>>
    {
        Box::pin(async move {
            tokio::task::yield_now().await;
            Ok(SettleResponse {
                success: true,
                error_reason: None,
                error_message: None,
                payer: None,
                transaction: format!("0x{}", "ab".repeat(32)),
                network: req.payment_requirements.network,
                amount: None,
                extensions: None,
            })
        })
    }
}

#[tokio::test]
async fn the_claim_alone_stops_replays_when_the_facilitator_always_says_yes() {
    let w = paywall(Setup {
        facilitator: Some(std::sync::Arc::new(AlwaysYes)),
        ..Setup::default()
    });
    let h = header(&pay(&offer(Caip2::BASE_SEPOLIA), 19));
    let all = futures_join(64, |_| {
        let app = w.app.clone();
        let h = h.clone();
        async move { get_paid(&app, Some(&h)).await.status }
    })
    .await;
    assert_eq!(
        all.iter().filter(|s| **s == StatusCode::OK).count(),
        1,
        "{all:?}"
    );
    let later = get_paid(&w.app, Some(&h)).await;
    assert_eq!(
        (later.status, error_of(&later)),
        (StatusCode::PAYMENT_REQUIRED, reason::NONCE_REUSED)
    );
    assert_eq!(w.served.load(Ordering::SeqCst), 1);
}
