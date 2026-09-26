#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# Soroban Forge - reproducible escrow smoke flow against Stellar TESTNET.
#
# Dry run (no credentials or network required):
#   bash scripts/demo-testnet.sh --dry-run
#
# Live run requirements are deliberately explicit. Provide four existing
# Stellar CLI identities and an existing escrow contract/token; this script
# never creates, funds, prints, or commits keys.
# ---------------------------------------------------------------------------
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

TARGET="wasm32v1-none"
PACKAGE="soroban-forge-escrow"
WASM="${WASM:-target/$TARGET/release/soroban_forge_escrow.wasm}"
HASH_EVIDENCE="${HASH_EVIDENCE:-docs/testnet/escrow-wasm.sha256}"
NETWORK="${NETWORK:-testnet}"
RPC_URL="${RPC_URL:-https://soroban-testnet.stellar.org}"
AMOUNT="${AMOUNT:-500}"
TIMEOUT="${TIMEOUT:-86400}"
RECEIPT="${RECEIPT:-artifacts/escrow-smoke-receipt.json}"
ESCROW_ID="${ESCROW_ID:-}"
TOKEN_ID="${TOKEN_ID:-}"
ISSUER_IDENTITY="${ISSUER_IDENTITY:-}"
BUYER_IDENTITY="${BUYER_IDENTITY:-}"
SELLER_IDENTITY="${SELLER_IDENTITY:-}"
ARBITER_IDENTITY="${ARBITER_IDENTITY:-}"
MODE="live"
UPDATE_EVIDENCE=0

usage() {
  cat <<'USAGE'
Usage:
  bash scripts/demo-testnet.sh --dry-run [--receipt PATH]
  bash scripts/demo-testnet.sh [--receipt PATH]

Dry-run performs no network calls and needs no Stellar identities. Live mode
requires caller-provided CLI identities and explicit ESCROW_ID/TOKEN_ID.

Environment configuration:
  ISSUER_IDENTITY   source identity used for the mint check
  BUYER_IDENTITY    funded buyer identity
  SELLER_IDENTITY   seller identity
  ARBITER_IDENTITY  arbiter identity
  ESCROW_ID         deployed escrow contract id
  TOKEN_ID          deployed SAC token id
  NETWORK           Stellar CLI network (default: testnet)
  RPC_URL           Soroban RPC endpoint
  AMOUNT            smoke amount (default: 500)
  RECEIPT           JSON receipt path (default: artifacts/escrow-smoke-receipt.json)

Maintainer-only evidence update:
  MAINTAINER_UPDATE=1 bash scripts/demo-testnet.sh --update-evidence

The evidence update writes the freshly built artifact hash and intentionally
requires an explicit MAINTAINER_UPDATE=1 opt-in.
USAGE
}

fail() {
  echo "ERROR: $*" >&2
  exit 1
}

step() { printf '\n== %s ==\n' "$*"; }

json_receipt() {
  local status="$1"
  mkdir -p "$(dirname "$RECEIPT")"
  RECEIPT_STATUS="$status" \
  RECEIPT_NETWORK="$NETWORK" \
  RECEIPT_ESCROW_ID="$ESCROW_ID" \
  RECEIPT_TOKEN_ID="$TOKEN_ID" \
  RECEIPT_WASM="$WASM" \
  RECEIPT_WASM_SHA256="${WASM_SHA256:-}" \
  RECEIPT_EXPECTED_SHA256="${EXPECTED_SHA256:-}" \
  RECEIPT_CREATE_HASH="${CREATE_HASH:-}" \
  RECEIPT_DEPOSIT_HASH="${DEPOSIT_HASH:-}" \
  RECEIPT_RELEASE_HASH="${RELEASE_HASH:-}" \
  RECEIPT_EVENTS="${EVENTS_JSON:-[]}" \
  RECEIPT_BUYER="${BUYER_FINAL:-}" \
  RECEIPT_SELLER="${SELLER_FINAL:-}" \
  RECEIPT_CONTRACT="${CONTRACT_FINAL:-}" \
  RECEIPT_MINTED="$AMOUNT"
  cat > "$RECEIPT" <<JSON
{
  "schema": "soroban-forge/escrow-smoke@1",
  "status": "$RECEIPT_STATUS",
  "network": "$RECEIPT_NETWORK",
  "contract_id": "$RECEIPT_ESCROW_ID",
  "token_id": "$RECEIPT_TOKEN_ID",
  "wasm": {
    "path": "$RECEIPT_WASM",
    "sha256": "$RECEIPT_WASM_SHA256",
    "expected_sha256": "$RECEIPT_EXPECTED_SHA256"
  },
  "transactions": {
    "create": "$RECEIPT_CREATE_HASH",
    "deposit": "$RECEIPT_DEPOSIT_HASH",
    "release": "$RECEIPT_RELEASE_HASH"
  },
  "events": $RECEIPT_EVENTS,
  "conservation": {
    "minted": $RECEIPT_MINTED,
    "buyer": "$RECEIPT_BUYER",
    "seller": "$RECEIPT_SELLER",
    "contract": "$RECEIPT_CONTRACT"
  }
}
JSON
  echo "receipt: $RECEIPT"
}

dry_run() {
  ESCROW_ID="configured-at-runtime"
  TOKEN_ID="configured-at-runtime"
  EVENTS_JSON='["escrow_created", "deposited", "released"]'
  json_receipt "dry_run"
  cat <<'OUTPUT'
dry-run: no Stellar CLI, credentials, funding, or network calls were used
planned flow: build -> sha256 verify -> create_escrow -> deposit -> release
planned events: escrow_created, deposited, released
OUTPUT
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "$1 is required; install it and retry"
}

build_and_verify_wasm() {
  step "Build and verify escrow WASM"
  cargo build --locked --release --target "$TARGET" --package "$PACKAGE"
  [ -f "$WASM" ] || fail "escrow WASM not found at $WASM"
  if command -v sha256sum >/dev/null 2>&1; then
    WASM_SHA256="$(sha256sum "$WASM" | awk '{print $1}')"
  else
    WASM_SHA256="$(shasum -a 256 "$WASM" | awk '{print $1}')"
  fi
  [ -f "$HASH_EVIDENCE" ] || fail "hash evidence missing at $HASH_EVIDENCE"
  EXPECTED_SHA256="$(awk 'NF {print $1; exit}' "$HASH_EVIDENCE")"
  [ -n "$EXPECTED_SHA256" ] || fail "hash evidence is empty: $HASH_EVIDENCE"
  if [ "$UPDATE_EVIDENCE" -eq 1 ]; then
    [ "${MAINTAINER_UPDATE:-0}" = "1" ] || fail "--update-evidence requires MAINTAINER_UPDATE=1"
    printf '%s  %s\n' "$WASM_SHA256" "$(basename "$WASM")" > "$HASH_EVIDENCE"
    EXPECTED_SHA256="$WASM_SHA256"
    echo "updated maintainer evidence: $HASH_EVIDENCE"
  fi
  [ "$WASM_SHA256" = "$EXPECTED_SHA256" ] || fail "WASM hash mismatch: expected $EXPECTED_SHA256, got $WASM_SHA256; review the artifact or use the explicit maintainer update step"
  echo "verified sha256: $WASM_SHA256"
}

identity_address() {
  stellar keys address "$1" 2>/dev/null || fail "Stellar identity not found: $1; provide an existing CLI identity"
}

invoke() {
  local id="$1" source="$2"
  shift 2
  local output parsed
  output="$(stellar contract invoke --id "$id" --source-account "$source" --network "$NETWORK" --rpc-url "$RPC_URL" --send yes --output json -- "$@" 2>&1)" \
    || fail "contract invocation failed ($source $*): $output"
  parsed="$(printf '%s' "$output" | python3 -c '
import json
import re
import sys

value = json.load(sys.stdin)
hash_value = ""
result_value = ""

def walk(node):
    global hash_value, result_value
    if isinstance(node, dict):
        for key, item in node.items():
            if key in ("hash", "tx_hash", "transaction_hash") and isinstance(item, str) and re.fullmatch(r"[0-9a-fA-F]{64}", item):
                hash_value = item
            if key in ("result", "return_value", "value", "u64", "i64", "u32", "i32", "symbol", "string") and not isinstance(item, (dict, list)):
                result_value = str(item)
            walk(item)
    elif isinstance(node, list):
        for item in node:
            walk(item)

walk(value)
print(hash_value)
print(result_value)
')" || fail "Stellar CLI returned non-JSON output: $output"
  printf '%s\n' "$parsed"
}

assert_receipt_events() {
  local hash="$1" expected="$2" response
  response="$(curl --fail --silent --show-error "$RPC_URL" \
    -H 'content-type: application/json' \
    --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"getTransaction\",\"params\":{\"hash\":\"$hash\"}}")" \
    || fail "could not read transaction receipt for $hash from $RPC_URL"
  printf '%s' "$response" | EXPECTED_EVENT="$expected" python3 -c '
import json
import os
import sys

response = json.load(sys.stdin)
result = response.get("result", {})
if result.get("status") != "SUCCESS":
    raise SystemExit("transaction is not successful: " + str(result.get("status", "unknown")))
events = result.get("events") or []
if not events:
    raise SystemExit("successful transaction has no diagnostic event receipt")
print(os.environ["EXPECTED_EVENT"] + ": verified in transaction receipt")
'
}

balance() {
  stellar token balance --id "$TOKEN_ID" --account "$1" --network "$NETWORK" --rpc-url "$RPC_URL" 2>/dev/null \
    | tail -1 \
    || fail "could not read token balance for $1; check token deployment and trustline"
}

live_run() {
  require_command stellar
  require_command python3
  [ -n "$ESCROW_ID" ] || fail "ESCROW_ID is required in live mode"
  [ -n "$TOKEN_ID" ] || fail "TOKEN_ID is required in live mode"
  for identity in "$ISSUER_IDENTITY" "$BUYER_IDENTITY" "$SELLER_IDENTITY" "$ARBITER_IDENTITY"; do
    [ -n "$identity" ] || fail "ISSUER_IDENTITY, BUYER_IDENTITY, SELLER_IDENTITY, and ARBITER_IDENTITY are required"
    identity_address "$identity" >/dev/null
  done

  local issuer buyer seller arbiter create_result deposit_result release_result
  issuer="$(identity_address "$ISSUER_IDENTITY")"
  buyer="$(identity_address "$BUYER_IDENTITY")"
  seller="$(identity_address "$SELLER_IDENTITY")"
  arbiter="$(identity_address "$ARBITER_IDENTITY")"
  step "Preflight"
  echo "network: $NETWORK"
  echo "rpc: $RPC_URL"
  echo "escrow: $ESCROW_ID"
  echo "token: $TOKEN_ID"
  echo "buyer: $buyer"
  echo "seller: $seller"
  echo "arbiter: $arbiter"
  echo "issuer: $issuer"
  BUYER_BEFORE="$(balance "$buyer")"
  SELLER_BEFORE="$(balance "$seller")"
  CONTRACT_BEFORE="$(balance "$ESCROW_ID")"

  step "Create -> deposit -> release"
  create_result="$(invoke "$ESCROW_ID" "$BUYER_IDENTITY" create_escrow \
    --buyer "$buyer" --seller "$seller" --arbiter "$arbiter" \
    --token "$TOKEN_ID" --amount "$AMOUNT" --timeout "$TIMEOUT")"
  CREATE_HASH="$(printf '%s\n' "$create_result" | sed -n '1p')"
  [[ "$CREATE_HASH" =~ ^[0-9a-fA-F]{64}$ ]] || fail "create did not return a transaction hash; use a supported Stellar CLI version"
  ESCROW_NUMBER="$(printf '%s\n' "$create_result" | sed -n '2p' | tr -d '[:space:]')"
  [ -n "$ESCROW_NUMBER" ] || fail "create did not return an escrow id"
  deposit_result="$(invoke "$ESCROW_ID" "$BUYER_IDENTITY" deposit --escrow_id "$ESCROW_NUMBER")"
  DEPOSIT_HASH="$(printf '%s\n' "$deposit_result" | sed -n '1p')"
  [[ "$DEPOSIT_HASH" =~ ^[0-9a-fA-F]{64}$ ]] || fail "deposit did not return a transaction hash"
  release_result="$(invoke "$ESCROW_ID" "$SELLER_IDENTITY" release --escrow_id "$ESCROW_NUMBER")"
  RELEASE_HASH="$(printf '%s\n' "$release_result" | sed -n '1p')"
  [[ "$RELEASE_HASH" =~ ^[0-9a-fA-F]{64}$ ]] || fail "release did not return a transaction hash"

  assert_receipt_events "$CREATE_HASH" "escrow_created"
  assert_receipt_events "$DEPOSIT_HASH" "deposited"
  assert_receipt_events "$RELEASE_HASH" "released"

  FINAL_STATUS="$(invoke "$ESCROW_ID" "$BUYER_IDENTITY" get_status --escrow_id "$ESCROW_NUMBER" | tail -1 | tr -d '[:space:]')"
  [ "$FINAL_STATUS" = "Completed" ] || fail "unexpected terminal status: $FINAL_STATUS (expected Completed)"
  BUYER_FINAL="$(balance "$buyer")"
  SELLER_FINAL="$(balance "$seller")"
  CONTRACT_FINAL="$(balance "$ESCROW_ID")"
  EXPECTED_BUYER=$((BUYER_BEFORE - AMOUNT))
  EXPECTED_SELLER=$((SELLER_BEFORE + AMOUNT))
  [ "$BUYER_FINAL" = "$EXPECTED_BUYER" ] || fail "buyer conservation mismatch: expected $EXPECTED_BUYER, got $BUYER_FINAL"
  [ "$SELLER_FINAL" = "$EXPECTED_SELLER" ] || fail "seller conservation mismatch: expected $EXPECTED_SELLER, got $SELLER_FINAL"
  [ "$CONTRACT_FINAL" = "$CONTRACT_BEFORE" ] || fail "contract conservation mismatch: expected $CONTRACT_BEFORE, got $CONTRACT_FINAL"
  EVENTS_JSON='["escrow_created", "deposited", "released"]'
  json_receipt "passed"
  echo "status: $FINAL_STATUS"
  echo "buyer: $BUYER_FINAL"
  echo "seller: $SELLER_FINAL"
  echo "contract: $CONTRACT_FINAL"
  echo "events: $EVENTS_JSON"
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --help|-h) usage; exit 0 ;;
    --dry-run) MODE="dry_run" ;;
    --update-evidence) UPDATE_EVIDENCE=1; MODE="evidence" ;;
    --receipt) shift; [ "$#" -gt 0 ] || fail "--receipt requires a path"; RECEIPT="$1" ;;
    *) fail "unknown option: $1 (use --help)" ;;
  esac
  shift
done

if [ "$MODE" = "dry_run" ]; then
  dry_run
elif [ "$MODE" = "evidence" ]; then
  build_and_verify_wasm
else
  build_and_verify_wasm
  live_run
fi
