"""Reference-side conformance: the official x402 Python package parses what xpay-x402 emits,
and its exact-EVM facilitator accepts (or refuses, with the same reason) what we sign.

    check.py verify   <emitted.json>   # our vectors through the reference
    check.py generate <out.json>       # reference-signed vectors for the Rust side

Offline: the facilitator signer is a stub with no RPC (EOA path: ecrecover only), and the
reference's clock is pinned to each vector's `now`.
"""

import json
import sys

from eth_account import Account
from x402.http import utils as http_utils
from x402.mechanisms.evm.exact import client as exact_client
from x402.mechanisms.evm.exact import facilitator as exact_facilitator
from x402.mechanisms.evm.signers import EthAccountSigner
from x402.schemas import (
    PaymentPayload,
    PaymentRequired,
    PaymentRequirements,
    SettleResponse,
    SupportedResponse,
    VerifyRequest,
    VerifyResponse,
)

# A fixed, published test key (32 bytes of 0x11): never holds funds.
TEST_KEY = "0x" + "11" * 32

MODELS = {
    "payment_required": PaymentRequired,
    "payment_payload": PaymentPayload,
    "settle_response": SettleResponse,
    "verify_response": VerifyResponse,
    "facilitator_request": VerifyRequest,
    "supported": SupportedResponse,
}
HEADER_DECODERS = {
    "payment_required": http_utils.decode_payment_required_header,
    "payment_payload": http_utils.decode_payment_signature_header,
    "settle_response": http_utils.decode_payment_response_header,
}


class Clock:
    def __init__(self, now):
        self.now = now

    def time(self):
        return float(self.now)


class OfflineSigner:
    """FacilitatorEvmSigner with no chain: every payer is an EOA, every asset a contract."""

    def __init__(self, assets):
        self.assets = {a.lower() for a in assets}

    def get_addresses(self):
        return []

    def get_code(self, address):
        return b"\x60" if address.lower() in self.assets else b""

    def get_chain_id(self):
        raise RuntimeError("offline")

    def read_contract(self, *args, **kwargs):
        raise RuntimeError("offline")


def dump(model):
    return model.model_dump(by_alias=True, exclude_none=True, mode="json")


def strip_none(v):
    if isinstance(v, dict):
        return {k: strip_none(x) for k, x in v.items() if x is not None}
    if isinstance(v, list):
        return [strip_none(x) for x in v]
    return v


def verify(path):
    doc = json.load(open(path))
    failures = 0
    for v in doc["vectors"]:
        model = MODELS[v["kind"]]
        parsed = model.model_validate(v["json"])
        # The reference drops no field we emit and changes no value.
        if strip_none(dump(parsed)) != strip_none(v["json"]):
            print(f"FAIL {v['name']}: reference re-emits {dump(parsed)}")
            failures += 1
        if "header" in v:
            decoded = HEADER_DECODERS[v["kind"]](v["header"])
            if dump(decoded) != dump(parsed):
                print(f"FAIL {v['name']}: header decodes differently")
                failures += 1
        print(f"ok   parse {v['name']}")
    for s in doc["signed"]:
        payload = PaymentPayload.model_validate(s["payload"])
        req = PaymentRequirements.model_validate(s["requirements"])
        exact_facilitator.time = Clock(s["now"])
        scheme = exact_facilitator.ExactEvmScheme(OfflineSigner([req.asset]))
        result, _ = scheme._verify(payload, req, simulate=False)
        got = "valid" if result.is_valid else result.invalid_reason
        if got != s["expect"]:
            print(f"FAIL {s['name']}: reference says {got}, we say {s['expect']}")
            failures += 1
        else:
            print(f"ok   verify {s['name']}: {got}")
    if failures:
        sys.exit(f"{failures} reference conformance failure(s)")


def generate(path, now=1_800_000_000):
    signer = EthAccountSigner(Account.from_key(TEST_KEY))
    out = []
    for network, asset, name in [
        ("eip155:84532", "0x036CbD53842c5426634e7929541eC2318f3dCF7e", "USDC"),
        ("eip155:8453", "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913", "USD Coin"),
    ]:
        req = PaymentRequirements(
            scheme="exact",
            network=network,
            amount="10000",
            asset=asset,
            pay_to="0x209693Bc6afc0C5328bA36FaF03C514EF312287C",
            max_timeout_seconds=60,
            extra={"name": name, "version": "2"},
        )
        exact_client.time = Clock(now)
        inner = exact_client.ExactEvmScheme(signer).create_payment_payload(req)
        payload = PaymentPayload(
            x402_version=2,
            resource={"url": "https://api.example.com/premium-data"},
            accepted=req,
            payload=inner,
        )
        out.append({"name": f"python-signed-{network}", "now": now, "payer": signer.address,
                    "payload": dump(payload), "requirements": dump(req)})
    json.dump({"source": "x402==2.28.0 (PyPI) ExactEvmScheme client", "signed": out},
              open(path, "w"), indent=2)
    open(path, "a").write("\n")


if __name__ == "__main__":
    {"verify": verify, "generate": generate}[sys.argv[1]](sys.argv[2])
