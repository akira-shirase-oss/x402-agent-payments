//! The wallet against a scripted seller on a raw socket: what it signs, and when a
//! reservation may leave the cap.
#![expect(clippy::unwrap_used, reason = "test fixtures")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use alloy_primitives::{Address, B256, U256, address};
use alloy_signer_local::PrivateKeySigner;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use xpay_wallet::{AllowedHost, Limits, Policy, Wallet, WalletError};
use xpay_x402::clock::ManualClock;
use xpay_x402::{Caip2, PaymentRequired, ResourceInfo, X402_VERSION, encode_header, exact};

const NOW: u64 = 1_800_000_000;
const SELLER: Address = address!("0x209693Bc6afc0C5328bA36FaF03C514EF312287C");
const STRANGER: Address = address!("0x00000000000000000000000000000000DeaDBeef");
const PRICE: u64 = 10_000;

/// What the scripted seller does with the n-th proof it receives (0-based).
#[derive(Clone, Copy)]
enum Then {
    /// Close the connection without an answer.
    Hangup,
    /// 402 with `error`.
    Refuse(&'static str),
    /// This status with a JSON body `{"error": ..}`.
    Answer(u16, &'static str),
}

struct Seller {
    port: u16,
    proofs: Arc<AtomicUsize>,
    seen: Arc<std::sync::Mutex<Vec<String>>>,
}

async fn seller(pay_to: Address, script: Vec<Then>) -> Seller {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let proofs = Arc::new(AtomicUsize::new(0));
    let seen = proofs.clone();
    let texts = Arc::new(std::sync::Mutex::new(Vec::new()));
    let kept = texts.clone();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = s.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let text = String::from_utf8_lossy(&buf).to_ascii_lowercase();
            let mut answer = None;
            let error = if text.contains("payment-signature:") {
                if let Some(line) = text.lines().find(|l| l.starts_with("payment-signature:")) {
                    kept.lock().unwrap().push(line.to_owned());
                }
                let n = seen.fetch_add(1, Ordering::SeqCst);
                match script.get(n).copied().unwrap_or(Then::Refuse("x")) {
                    Then::Hangup => continue,
                    Then::Refuse(e) => Some(e.to_owned()),
                    Then::Answer(status, e) => {
                        answer = Some(answer_error(status, e));
                        None
                    }
                }
            } else {
                None
            };
            let resp = answer.unwrap_or_else(|| answer_402(port, pay_to, error));
            let _ = s.write_all(resp.as_bytes()).await;
            let _ = s.shutdown().await;
        }
    });
    Seller {
        port,
        proofs,
        seen: texts,
    }
}

fn answer_402(port: u16, pay_to: Address, error: Option<String>) -> String {
    let required = PaymentRequired {
        x402_version: X402_VERSION,
        error,
        resource: ResourceInfo {
            url: format!("http://127.0.0.1:{port}/paid"),
            ..ResourceInfo::default()
        },
        accepts: vec![
            exact::usdc_requirements(
                &Caip2::parse(Caip2::BASE_SEPOLIA).unwrap(),
                U256::from(PRICE),
                pay_to,
                60,
            )
            .unwrap(),
        ],
        extensions: None,
    };
    let h = encode_header(&required).unwrap();
    format!(
        "HTTP/1.1 402 Payment Required\r\npayment-required: {h}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
    )
}

fn answer_error(status: u16, error: &str) -> String {
    let body = format!("{{\"error\":\"{error}\"}}");
    format!(
        "HTTP/1.1 {status} Error\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn distinct(seen: &[String]) -> usize {
    seen.iter().collect::<std::collections::BTreeSet<_>>().len()
}

fn wallet(allow_hosts: Vec<String>) -> Wallet {
    let signer = PrivateKeySigner::from_bytes(&B256::repeat_byte(0x11)).unwrap();
    let address = signer.address();
    Wallet::new(
        Arc::new(signer),
        address,
        Policy {
            allow_hosts: allow_hosts
                .into_iter()
                .map(|h| AllowedHost::new(h, vec![SELLER]))
                .collect(),
            networks: vec![Caip2::parse(Caip2::BASE_SEPOLIA).unwrap()],
            max_per_payment: U256::from(PRICE),
            total_cap: U256::from(PRICE * 10),
            max_timeout_seconds: 300,
        },
        Limits {
            retries: 2,
            backoff: std::time::Duration::from_millis(1),
            ..Limits::default()
        },
        Arc::new(ManualClock::at(NOW)),
    )
    .unwrap()
}

#[tokio::test]
async fn a_lost_answer_then_a_402_on_resend_keeps_the_reservation() {
    let s = seller(
        SELLER,
        vec![
            Then::Hangup,
            Then::Refuse("invalid_exact_evm_nonce_already_used"),
        ],
    )
    .await;
    let w = wallet(vec![format!("127.0.0.1:{}", s.port)]);
    let r = w.get(&format!("http://127.0.0.1:{}/paid", s.port)).await;
    assert!(r.is_err(), "{r:?}");
    assert_eq!(
        s.proofs.load(Ordering::SeqCst),
        2,
        "the same proof was resent"
    );
    assert_eq!(w.committed(), U256::from(PRICE), "the reservation stays");
}

#[tokio::test]
async fn any_402_after_the_proof_left_keeps_the_reservation_and_signs_nothing_new() {
    let s = seller(SELLER, vec![Then::Refuse("insufficient_funds")]).await;
    let w = wallet(vec![format!("127.0.0.1:{}", s.port)]);
    let url = format!("http://127.0.0.1:{}/paid", s.port);
    let first = w.get(&url).await;
    assert!(matches!(first, Err(WalletError::Refused(_))), "{first:?}");
    let nonce = w.unresolved()[0].nonce;
    // The refused proof may still settle elsewhere until it expires: no second payment.
    for _ in 0..2 {
        let r = w.get(&url).await;
        assert!(
            matches!(r, Err(WalletError::PaymentPending { nonce: n }) if n == nonce),
            "{r:?}"
        );
    }
    assert_eq!(s.proofs.load(Ordering::SeqCst), 1);
    assert_eq!(w.committed(), U256::from(PRICE));
    assert_eq!(w.unresolved().len(), 1);
}

#[tokio::test]
async fn a_bare_host_entry_does_not_match_another_port() {
    let s = seller(SELLER, vec![]).await;
    let w = wallet(vec!["127.0.0.1".into()]);
    let r = w.get(&format!("http://127.0.0.1:{}/paid", s.port)).await;
    assert!(matches!(r, Err(WalletError::HostNotAllowed)), "{r:?}");
    assert_eq!(s.proofs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn plain_http_to_a_non_loopback_host_is_refused_before_any_request() {
    let w = wallet(vec!["192.0.2.1".into()]);
    let r = w.get("http://192.0.2.1/paid").await;
    assert!(matches!(r, Err(WalletError::Url)), "{r:?}");
}

#[tokio::test]
async fn an_offer_to_a_pay_to_not_on_the_list_is_refused_before_signing() {
    let s = seller(STRANGER, vec![]).await;
    let w = wallet(vec![format!("127.0.0.1:{}", s.port)]);
    let r = w.get(&format!("http://127.0.0.1:{}/paid", s.port)).await;
    assert!(matches!(r, Err(WalletError::NoAcceptableOffer)), "{r:?}");
    assert_eq!(
        s.proofs.load(Ordering::SeqCst),
        0,
        "nothing was signed or sent"
    );
    assert_eq!(w.committed(), U256::ZERO);
}

#[tokio::test]
async fn seller_error_text_is_not_echoed() {
    let s = seller(SELLER, vec![Then::Refuse("<script>\u{1b}[31m")]).await;
    let w = wallet(vec![format!("127.0.0.1:{}", s.port)]);
    let r = w.get(&format!("http://127.0.0.1:{}/paid", s.port)).await;
    let text = format!(
        "{:?} {}",
        r,
        r.as_ref()
            .err()
            .map(ToString::to_string)
            .unwrap_or_default()
    );
    assert!(!text.contains("script"), "{text}");
}

/// The chain's answer and its finalized block's timestamp.
struct Chain(Result<bool, String>, u64);

/// A finalized block well past every proof's `validBefore`.
const LATE: u64 = NOW + 3600;

impl xpay_wallet::AuthorizationState for Chain {
    fn authorization_state<'a>(
        &'a self,
        _: &'a Caip2,
        _: Address,
        _: Address,
        _: B256,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<xpay_wallet::AuthorizationStatus, String>>
                + Send
                + 'a,
        >,
    > {
        let status = self.0.clone().map(|used| xpay_wallet::AuthorizationStatus {
            used,
            block_number: 1,
            block_timestamp: self.1,
        });
        Box::pin(std::future::ready(status))
    }
}

fn wallet_at(allow: String, clock: ManualClock, max_unresolved: usize) -> Wallet {
    let signer = PrivateKeySigner::from_bytes(&B256::repeat_byte(0x11)).unwrap();
    let address = signer.address();
    Wallet::new(
        Arc::new(signer),
        address,
        Policy {
            allow_hosts: vec![AllowedHost::new(allow, vec![SELLER])],
            networks: vec![Caip2::parse(Caip2::BASE_SEPOLIA).unwrap()],
            max_per_payment: U256::from(PRICE),
            total_cap: U256::from(PRICE * 10),
            max_timeout_seconds: 300,
        },
        Limits {
            max_unresolved,
            retries: 2,
            backoff: std::time::Duration::from_millis(1),
            ..Limits::default()
        },
        Arc::new(clock),
    )
    .unwrap()
}

#[tokio::test]
async fn only_the_chain_releases_a_sent_proof_and_only_once_it_expired_unused() {
    let s = seller(SELLER, vec![Then::Refuse("x"), Then::Refuse("x")]).await;
    let clock = ManualClock::at(NOW);
    let w = wallet_at(format!("127.0.0.1:{}", s.port), clock.clone(), 16);
    let _ = w.get(&format!("http://127.0.0.1:{}/paid", s.port)).await;
    let _ = w.get(&format!("http://127.0.0.1:{}/other", s.port)).await;
    assert_eq!(w.unresolved().len(), 2);
    // Before validBefore (+ margin) nothing is released, whatever the chain says.
    let r = w.reconcile(&Chain(Ok(false), LATE)).await;
    assert_eq!((r.released, r.waiting), (0, 2));
    clock.advance(60 + xpay_wallet::EXPIRY_MARGIN_SECS + 1);
    // The chain not answering keeps the reservation.
    let r = w.reconcile(&Chain(Err("down".into()), LATE)).await;
    assert_eq!((r.released, r.waiting), (0, 2));
    assert_eq!(w.committed(), U256::from(PRICE * 2));
    // Used on chain: spent, never released.
    let r = w.reconcile(&Chain(Ok(true), LATE)).await;
    assert_eq!((r.spent, r.released), (2, 0));
    assert_eq!(w.committed(), U256::from(PRICE * 2));
    assert_eq!(w.unresolved().len(), 0);
}

#[tokio::test]
async fn an_expired_unused_authorisation_goes_back_to_the_cap() {
    let s = seller(SELLER, vec![Then::Hangup, Then::Hangup, Then::Hangup]).await;
    let clock = ManualClock::at(NOW);
    let w = wallet_at(format!("127.0.0.1:{}", s.port), clock.clone(), 16);
    let r = w.get(&format!("http://127.0.0.1:{}/paid", s.port)).await;
    assert!(matches!(r, Err(WalletError::Unsettled(None))), "{r:?}");
    assert_eq!(w.committed(), U256::from(PRICE));
    clock.advance(60 + xpay_wallet::EXPIRY_MARGIN_SECS + 1);
    let r = w.reconcile(&Chain(Ok(false), LATE)).await;
    assert_eq!(r.released, 1);
    assert_eq!(w.committed(), U256::ZERO);
}

#[tokio::test]
async fn unresolved_proofs_are_bounded() {
    let s = seller(SELLER, vec![Then::Refuse("x")]).await;
    let w = wallet_at(format!("127.0.0.1:{}", s.port), ManualClock::at(NOW), 1);
    let _ = w.get(&format!("http://127.0.0.1:{}/paid", s.port)).await;
    let r = w.get(&format!("http://127.0.0.1:{}/other", s.port)).await;
    assert!(matches!(r, Err(WalletError::TooManyUnresolved)), "{r:?}");
    assert_eq!(
        s.proofs.load(Ordering::SeqCst),
        1,
        "nothing signed past the bound"
    );
    assert_eq!(w.committed(), U256::from(PRICE));
}

#[tokio::test]
async fn plain_http_to_localhost_by_name_is_refused() {
    let s = seller(SELLER, vec![]).await;
    let w = wallet(vec![format!("localhost:{}", s.port)]);
    let r = w.get(&format!("http://localhost:{}/paid", s.port)).await;
    assert!(matches!(r, Err(WalletError::Url)), "{r:?}");
    assert_eq!(s.proofs.load(Ordering::SeqCst), 0);
}

/// Answers the unpaid probe with a 402, then stops listening: the proof can never connect.
async fn seller_then_gone(pay_to: Address) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut s, _) = l.accept().await.unwrap();
        drop(l);
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = s.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let _ = s.write_all(answer_402(port, pay_to, None).as_bytes()).await;
        let _ = s.shutdown().await;
    });
    port
}

#[tokio::test]
async fn a_proof_that_never_connected_gives_the_reservation_back() {
    let port = seller_then_gone(SELLER).await;
    let w = wallet(vec![format!("127.0.0.1:{port}")]);
    let r = w.get(&format!("http://127.0.0.1:{port}/paid")).await;
    assert!(r.is_err(), "{r:?}");
    assert_eq!(w.committed(), U256::ZERO, "nothing left the process");
    assert_eq!(w.unresolved().len(), 0);
}

#[tokio::test]
async fn a_local_clock_ahead_of_the_chain_does_not_release() {
    let s = seller(SELLER, vec![Then::Hangup, Then::Hangup, Then::Hangup]).await;
    let clock = ManualClock::at(NOW);
    let w = wallet_at(format!("127.0.0.1:{}", s.port), clock.clone(), 16);
    let _ = w.get(&format!("http://127.0.0.1:{}/paid", s.port)).await;
    assert_eq!(w.committed(), U256::from(PRICE));
    // The local clock runs an hour ahead; the finalized block is still at validBefore.
    clock.advance(3600);
    let r = w.reconcile(&Chain(Ok(false), NOW + 60)).await;
    assert_eq!(r.released, 0, "released on the local clock alone");
    assert_eq!(w.committed(), U256::from(PRICE));
}

#[tokio::test]
async fn paying_again_for_the_same_url_resends_the_live_proof_instead_of_a_new_one() {
    let s = seller(SELLER, vec![Then::Hangup; 8]).await;
    let w = wallet_at(format!("127.0.0.1:{}", s.port), ManualClock::at(NOW), 16);
    let url = format!("http://127.0.0.1:{}/paid", s.port);
    assert!(w.get(&url).await.is_err());
    assert!(w.get(&url).await.is_err());
    let seen = s.seen.lock().unwrap().clone();
    assert!(seen.len() >= 2, "{seen:?}");
    assert!(seen.iter().all(|p| *p == seen[0]), "one proof, resent");
    assert_eq!(w.committed(), U256::from(PRICE), "paid at most once");
}

#[tokio::test]
async fn a_504_settle_timeout_keeps_the_proof_and_a_second_get_signs_nothing_new() {
    let s = seller(SELLER, vec![Then::Answer(504, "settle_timeout"); 8]).await;
    let w = wallet(vec![format!("127.0.0.1:{}", s.port)]);
    let url = format!("http://127.0.0.1:{}/paid", s.port);
    let first = w.get(&url).await;
    assert!(
        matches!(first, Err(WalletError::Unsettled(Some(504)))),
        "{first:?}"
    );
    let second = w.get(&url).await;
    assert!(
        matches!(second, Err(WalletError::Unsettled(Some(504)))),
        "the same proof is resent: {second:?}"
    );
    let seen = s.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "one send per get: {seen:?}");
    assert_eq!(distinct(&seen), 1, "one proof, resent");
    assert_eq!(w.committed(), U256::from(PRICE), "paid at most once");
    assert!(w.unresolved()[0].resumable);
}

#[tokio::test]
async fn a_5xx_after_the_proof_left_keeps_it_resumable() {
    for (status, error) in [
        (500, "unexpected_settle_error"),
        (502, "settle_answer_mismatch"),
        (504, "settlement_pending"),
    ] {
        let s = seller(SELLER, vec![Then::Answer(status, error); 4]).await;
        let w = wallet(vec![format!("127.0.0.1:{}", s.port)]);
        let url = format!("http://127.0.0.1:{}/paid", s.port);
        let _ = w.get(&url).await;
        let _ = w.get(&url).await;
        let seen = s.seen.lock().unwrap().clone();
        assert_eq!(distinct(&seen), 1, "{status}: {seen:?}");
        assert_eq!(w.committed(), U256::from(PRICE), "{status}");
        assert!(w.unresolved()[0].resumable, "{status}");
    }
}

#[tokio::test]
async fn a_409_the_seller_will_not_serve_stops_resends_and_signs_nothing_new() {
    let s = seller(SELLER, vec![Then::Answer(409, "payment_not_delivered"); 4]).await;
    let w = wallet(vec![format!("127.0.0.1:{}", s.port)]);
    let url = format!("http://127.0.0.1:{}/paid", s.port);
    let first = w.get(&url).await;
    assert!(
        matches!(first, Err(WalletError::Unsettled(Some(409)))),
        "{first:?}"
    );
    let second = w.get(&url).await;
    assert!(
        matches!(second, Err(WalletError::PaymentPending { .. })),
        "{second:?}"
    );
    assert_eq!(s.proofs.load(Ordering::SeqCst), 1);
    assert_eq!(w.committed(), U256::from(PRICE));
}

#[tokio::test]
async fn two_racing_gets_of_one_url_sign_one_proof() {
    let s = seller(SELLER, vec![Then::Hangup; 16]).await;
    let w = wallet(vec![format!("127.0.0.1:{}", s.port)]);
    let url = format!("http://127.0.0.1:{}/paid", s.port);
    let (a, b) = tokio::join!(w.get(&url), w.get(&url));
    assert!(a.is_err() && b.is_err(), "{a:?} {b:?}");
    let seen = s.seen.lock().unwrap().clone();
    assert_eq!(distinct(&seen), 1, "{seen:?}");
    assert_eq!(w.committed(), U256::from(PRICE));
    assert_eq!(w.unresolved().len(), 1);
}

#[tokio::test]
async fn a_new_proof_is_signed_only_after_the_last_may_no_longer_settle() {
    let s = seller(SELLER, vec![Then::Refuse("insufficient_funds"); 2]).await;
    let clock = ManualClock::at(NOW);
    let w = wallet_at(format!("127.0.0.1:{}", s.port), clock.clone(), 16);
    let url = format!("http://127.0.0.1:{}/paid", s.port);
    let _ = w.get(&url).await;
    clock.advance(60 + xpay_wallet::EXPIRY_MARGIN_SECS);
    let r = w.get(&url).await;
    assert!(
        matches!(r, Err(WalletError::PaymentPending { .. })),
        "{r:?}"
    );
    clock.advance(1);
    let r = w.get(&url).await;
    assert!(matches!(r, Err(WalletError::Refused(_))), "{r:?}");
    assert_eq!(s.proofs.load(Ordering::SeqCst), 2);
}
