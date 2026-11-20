//! Conformance with the official x402 reference (x402-foundation/x402), both directions:
//!
//! * the spec's own examples (`fixtures/reference/spec.json`, copied verbatim by
//!   `scripts/x402-reference/extract-spec.py`, commit in the file) parse here and re-emit
//!   the same JSON; and payloads signed by the reference Python client
//!   (`fixtures/reference/python-signed.json`) verify here;
//! * what we emit (`fixtures/emitted.json`, a golden file this test regenerates and
//!   compares) is parsed and verified by the reference: `scripts/x402-reference/check.py`.
#![expect(clippy::unwrap_used, reason = "test fixtures")]

use std::path::PathBuf;

use alloy_primitives::{Address, B256, U256, address};
use alloy_signer_local::PrivateKeySigner;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use xpay_x402::exact::{self, ExactEvmPayload, TypedAuthorization};
use xpay_x402::types::FacilitatorRequest;
use xpay_x402::{
    Caip2, PaymentPayload, PaymentRequired, PaymentRequirements, ResourceInfo, SettleResponse,
    SupportedResponse, VerifyResponse, decode_header, encode_header, reason,
};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn read(name: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(fixture(name)).unwrap()).unwrap()
}

fn round_trip<T: Serialize + DeserializeOwned>(name: &str, v: &Value) {
    let parsed: T = serde_json::from_value(v.clone())
        .unwrap_or_else(|e| panic!("{name}: reference example does not parse: {e}"));
    assert_eq!(
        &serde_json::to_value(&parsed).unwrap(),
        v,
        "{name}: re-emitted differently"
    );
}

fn header_round_trip<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(
    name: &str,
    h: &str,
) {
    let parsed: T = decode_header(h.as_bytes())
        .unwrap_or_else(|e| panic!("{name}: reference header does not decode: {e}"));
    let again: T = decode_header(encode_header(&parsed).unwrap().as_bytes()).unwrap();
    assert_eq!(parsed, again, "{name}");
}

#[test]
fn the_reference_spec_examples_parse_and_re_emit_unchanged() {
    let doc = read("reference/spec.json");
    assert_eq!(doc["commit"], "e35bf8846f3e40f584f7235ea96c0841cb597b27");
    let vectors = doc["vectors"].as_array().unwrap();
    assert!(vectors.len() >= 19, "fixture lost vectors");
    for v in vectors {
        let name = v["name"].as_str().unwrap();
        let kind = v["kind"].as_str().unwrap();
        if let Some(h) = v["header"].as_str() {
            match kind {
                "payment_required" => header_round_trip::<PaymentRequired>(name, h),
                "payment_payload" => header_round_trip::<PaymentPayload>(name, h),
                "settle_response" => header_round_trip::<SettleResponse>(name, h),
                other => panic!("{name}: unexpected header kind {other}"),
            }
            continue;
        }
        let j = &v["json"];
        match kind {
            "payment_required" => round_trip::<PaymentRequired>(name, j),
            "payment_payload" => {
                round_trip::<PaymentPayload>(name, j);
                let p: PaymentPayload = serde_json::from_value(j.clone()).unwrap();
                if p.payload.contains_key("authorization") {
                    let e = ExactEvmPayload::from_payload(&p).unwrap();
                    TypedAuthorization::parse(&e.authorization)
                        .unwrap_or_else(|r| panic!("{name}: authorization: {r}"));
                }
            }
            "settle_response" => round_trip::<SettleResponse>(name, j),
            "verify_response" => round_trip::<VerifyResponse>(name, j),
            "facilitator_request" => round_trip::<FacilitatorRequest>(name, j),
            "supported" => round_trip::<SupportedResponse>(name, j),
            other => panic!("{name}: unknown kind {other}"),
        }
    }
}

#[test]
fn payloads_signed_by_the_reference_client_verify_here() {
    let doc = read("reference/python-signed.json");
    let signed = doc["signed"].as_array().unwrap();
    assert_eq!(signed.len(), 2, "one per USDC network");
    for s in signed {
        let name = s["name"].as_str().unwrap();
        let payment: PaymentPayload = serde_json::from_value(s["payload"].clone()).unwrap();
        let req: PaymentRequirements = serde_json::from_value(s["requirements"].clone()).unwrap();
        let now = s["now"].as_u64().unwrap();
        let ok = exact::check(&payment, &req, now).unwrap_or_else(|r| panic!("{name}: {r}"));
        assert_eq!(
            ok.payer.to_checksum(None),
            s["payer"].as_str().unwrap(),
            "{name}"
        );

        // The failure path: the same signature over a different amount is refused.
        let mut bumped = payment.clone();
        bumped.payload["authorization"]["value"] = json!("10001");
        let mut req_bumped = req.clone();
        req_bumped.amount = "10001".into();
        assert_eq!(
            exact::check(&bumped, &req_bumped, now),
            Err(reason::SIGNATURE),
            "{name}"
        );
    }
}

/// A fixed, published test key (32 bytes of 0x11): never holds funds.
fn test_signer() -> PrivateKeySigner {
    PrivateKeySigner::from_bytes(&B256::repeat_byte(0x11)).unwrap()
}

const NOW: u64 = 1_800_000_000;
const SELLER: Address = address!("0x209693Bc6afc0C5328bA36FaF03C514EF312287C");

fn requirements(network: &str, amount: u64) -> PaymentRequirements {
    exact::usdc_requirements(
        &Caip2::parse(network).unwrap(),
        U256::from(amount),
        SELLER,
        60,
    )
    .unwrap()
}

fn signed(req: &PaymentRequirements, edit: impl FnOnce(&mut TypedAuthorization)) -> PaymentPayload {
    let signer = test_signer();
    let mut auth = TypedAuthorization {
        from: signer.address(),
        to: req.pay_to.parse().unwrap(),
        value: req.amount.parse().unwrap(),
        valid_after: U256::from(NOW - 600),
        valid_before: U256::from(NOW + req.max_timeout_seconds),
        nonce: B256::repeat_byte(0x42),
    };
    edit(&mut auth);
    let payload = exact::sign(&signer, &auth, req).unwrap();
    PaymentPayload {
        x402_version: 2,
        resource: Some(ResourceInfo {
            url: "https://api.example.com/premium-data".into(),
            ..ResourceInfo::default()
        }),
        accepted: req.clone(),
        payload: payload.to_map(),
        extensions: None,
    }
}

#[expect(clippy::too_many_lines, reason = "the vector list reads as one table")]
fn emitted() -> Value {
    let sepolia = requirements(Caip2::BASE_SEPOLIA, 10_000);
    let base = requirements(Caip2::BASE, 10_000);
    let required = PaymentRequired {
        x402_version: 2,
        error: Some("PAYMENT-SIGNATURE header is required".into()),
        resource: ResourceInfo {
            url: "https://api.example.com/premium-data".into(),
            description: Some("Access to premium market data".into()),
            mime_type: Some("application/json".into()),
            ..ResourceInfo::default()
        },
        accepts: vec![sepolia.clone(), base.clone()],
        extensions: None,
    };
    let payment = signed(&sepolia, |_| {});
    let settled = SettleResponse {
        success: true,
        error_reason: None,
        error_message: None,
        payer: Some(test_signer().address().to_checksum(None)),
        transaction: format!("{}", B256::repeat_byte(0xab)),
        network: sepolia.network.clone(),
        amount: Some("10000".into()),
        extensions: None,
    };
    let refused = SettleResponse {
        success: false,
        error_reason: Some(reason::INSUFFICIENT_FUNDS.into()),
        transaction: String::new(),
        amount: None,
        ..settled.clone()
    };
    let vector = |name: &str, kind: &str, v: &dyn erased::Ser, header: bool| {
        let mut o = json!({ "name": name, "kind": kind, "json": v.json() });
        if header {
            o["header"] = Value::from(v.header());
        }
        o
    };
    let vectors = vec![
        vector("payment-required", "payment_required", &required, true),
        vector(
            "payment-payload-base-sepolia",
            "payment_payload",
            &payment,
            true,
        ),
        vector("settle-success", "settle_response", &settled, true),
        vector("settle-failure", "settle_response", &refused, true),
        vector(
            "facilitator-request",
            "facilitator_request",
            &FacilitatorRequest {
                x402_version: 2,
                payment_payload: payment.clone(),
                payment_requirements: sepolia.clone(),
            },
            false,
        ),
    ];
    let case = |name: &str, p: &PaymentPayload, r: &PaymentRequirements, expect: &str| json!({ "name": name, "now": NOW, "payload": p, "requirements": r, "expect": expect });
    let other = address!("0x000000000000000000000000000000000000dEaD");
    let mut wrong_net = payment.clone();
    wrong_net.accepted = base.clone();
    // Signed under "USDC" (the Base Sepolia name) for Base, whose token is "USD Coin".
    let mut sepolia_domain_on_base = signed(
        &PaymentRequirements {
            extra: sepolia.extra.clone(),
            ..base.clone()
        },
        |_| {},
    );
    sepolia_domain_on_base.accepted = base.clone();
    let mut bad_sig = payment.clone();
    bad_sig.payload["authorization"]["validBefore"] = json!((NOW + 61).to_string());
    let signed_cases = vec![
        case("valid-base-sepolia", &payment, &sepolia, "valid"),
        case(
            "valid-base-usd-coin-domain",
            &signed(&base, |_| {}),
            &base,
            "valid",
        ),
        case(
            "wrong-amount",
            &signed(&sepolia, |a| a.value = U256::from(9_999)),
            &sepolia,
            reason::VALUE_MISMATCH,
        ),
        case(
            "wrong-recipient",
            &signed(&sepolia, |a| a.to = other),
            &sepolia,
            reason::RECIPIENT_MISMATCH,
        ),
        case(
            "expired",
            &signed(&sepolia, |a| a.valid_before = U256::from(NOW - 1)),
            &sepolia,
            reason::VALID_BEFORE,
        ),
        case(
            "not-yet-valid",
            &signed(&sepolia, |a| a.valid_after = U256::from(NOW + 60)),
            &sepolia,
            reason::VALID_AFTER,
        ),
        case(
            "wrong-network",
            &wrong_net,
            &sepolia,
            reason::NETWORK_MISMATCH,
        ),
        case("tampered-signature", &bad_sig, &sepolia, reason::SIGNATURE),
        case(
            "sepolia-domain-name-on-base",
            &sepolia_domain_on_base,
            &base,
            reason::SIGNATURE,
        ),
    ];
    json!({ "vectors": vectors, "signed": signed_cases })
}

mod erased {
    pub(crate) trait Ser {
        fn json(&self) -> serde_json::Value;
        fn header(&self) -> String;
    }
    impl<T: serde::Serialize> Ser for T {
        fn json(&self) -> serde_json::Value {
            serde_json::to_value(self).unwrap()
        }
        fn header(&self) -> String {
            xpay_x402::encode_header(self).unwrap()
        }
    }
}

#[test]
fn what_we_emit_is_the_golden_file_the_reference_checks() {
    let now = emitted();
    let path = fixture("emitted.json");
    if std::env::var_os("X402_UPDATE_FIXTURES").is_some() {
        std::fs::write(&path, serde_json::to_string_pretty(&now).unwrap() + "\n").unwrap();
    }
    assert_eq!(
        read("emitted.json"),
        now,
        "re-run with X402_UPDATE_FIXTURES=1, then check.py"
    );
}

#[test]
fn our_signed_cases_get_the_reason_we_claim() {
    for s in emitted()["signed"].as_array().unwrap() {
        let payment: PaymentPayload = serde_json::from_value(s["payload"].clone()).unwrap();
        let req: PaymentRequirements = serde_json::from_value(s["requirements"].clone()).unwrap();
        let got = match exact::check(&payment, &req, NOW) {
            Ok(_) => "valid",
            Err(r) => r,
        };
        assert_eq!(got, s["expect"].as_str().unwrap(), "{}", s["name"]);
    }
}
