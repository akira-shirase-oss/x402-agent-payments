"""Copies the JSON examples and base64 header examples out of the official x402 spec
(x402-foundation/x402, `specs/`) into crates/xpay-x402/tests/fixtures/reference/spec.json,
verbatim, each with the file and line it came from.

    extract-spec.py <x402 checkout> <commit>
"""

import json
import re
import sys

KINDS = {
    ("x402Version", "accepts"): "payment_required",
    ("x402Version", "accepted"): "payment_payload",
    ("success",): "settle_response",
    ("isValid",): "verify_response",
    ("paymentPayload",): "facilitator_request",
    ("kinds",): "supported",
}
FILES = [
    "specs/x402-specification-v2.md",
    "specs/schemes/exact/scheme_exact_evm.md",
    "specs/transports-v2/http.md",
]


def kind_of(obj):
    for keys, kind in KINDS.items():
        if all(k in obj for k in keys):
            return kind
    return None


def main(root, commit):
    vectors = []
    for f in FILES:
        text = open(f"{root}/{f}").read()
        for m in re.finditer(r"```json\n(.*?)```", text, re.S):
            line = text[: m.start()].count("\n") + 2
            try:
                obj = json.loads(m.group(1))
            except json.JSONDecodeError:
                continue  # annotated pseudo-JSON in the prose, not an example
            kind = kind_of(obj) if isinstance(obj, dict) else None
            if kind:
                vectors.append({"name": f"{f}:{line}", "kind": kind, "json": obj})
        for m in re.finditer(r"^(PAYMENT-REQUIRED|PAYMENT-SIGNATURE|PAYMENT-RESPONSE): (\S+)$",
                             text, re.M):
            line = text[: m.start()].count("\n") + 1
            kind = {"PAYMENT-REQUIRED": "payment_required",
                    "PAYMENT-SIGNATURE": "payment_payload",
                    "PAYMENT-RESPONSE": "settle_response"}[m.group(1)]
            vectors.append({"name": f"{f}:{line}", "kind": kind, "header": m.group(2)})
    out = {"source": "https://github.com/x402-foundation/x402", "commit": commit,
           "vectors": vectors}
    with open("crates/xpay-x402/tests/fixtures/reference/spec.json", "w") as fh:
        json.dump(out, fh, indent=2)
        fh.write("\n")
    print(f"{len(vectors)} vectors")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
