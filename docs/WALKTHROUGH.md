# Reviewer walkthrough: reproducible escrow smoke flow

This walkthrough verifies the escrow WASM, a live `create_escrow -> deposit ->
release` flow, lifecycle events, terminal status, and token conservation.
The live path uses caller-provided Stellar CLI identities. It never creates,
funds, prints, or commits private keys.

## Step 0: prerequisites

Install the following on a clean machine:

- Rust stable through [rustup](https://rustup.rs)
- The workspace target: `rustup target add wasm32v1-none`
- Stellar CLI v28 or newer: `cargo install stellar-cli --locked`
- Bash, Python 3, and curl
- `sha256sum` or `shasum -a 256`

Confirm the versions before continuing:

```bash
rustc --version
stellar --version
python3 --version
bash --version
```

## Step 1: validate the harness without credentials

```bash
bash scripts/demo-testnet.sh --help
bash scripts/demo-testnet.sh --dry-run \
  --receipt artifacts/escrow-smoke-receipt.json
```

Expected output includes:

```text
dry-run: no Stellar CLI, credentials, funding, or network calls were used
planned flow: build -> sha256 verify -> create_escrow -> deposit -> release
planned events: escrow_created, deposited, released
```

The receipt is ignored by git and has this shape:

```json
{
  "schema": "soroban-forge/escrow-smoke@1",
  "status": "dry_run",
  "transactions": { "create": "", "deposit": "", "release": "" },
  "events": ["escrow_created", "deposited", "released"]
}
```

## Step 2: configure caller-provided testnet identities

Use existing Stellar CLI identities. Do not put secret keys in environment
variables or files. The values below are identity names already stored in the
local Stellar CLI key store, not private keys:

```bash
export ISSUER_IDENTITY=my-testnet-issuer
export BUYER_IDENTITY=my-testnet-buyer
export SELLER_IDENTITY=my-testnet-seller
export ARBITER_IDENTITY=my-testnet-arbiter
export ESCROW_ID=CC...
export TOKEN_ID=CA...
```

The buyer must be funded and hold at least `AMOUNT` units of `TOKEN_ID`. The
seller must have the required trustline. The issuer identity is checked as a
configured participant; the harness does not mint or fund accounts.

## Step 3: run the live smoke flow

```bash
AMOUNT=500 \
  bash scripts/demo-testnet.sh \
  --receipt artifacts/escrow-smoke-receipt.json
```

The script first runs the locked build:

```bash
cargo build --locked --release \
  --target wasm32v1-none \
  --package soroban-forge-escrow
```

It verifies the resulting artifact against
[`docs/testnet/escrow-wasm.sha256`](testnet/escrow-wasm.sha256), then runs:

```text
create_escrow -> deposit -> release
```

The live run fails with an actionable error if an identity, contract ID, token,
trustline, artifact hash, transaction receipt, terminal status, or balance
conservation check is invalid.

## Step 4: inspect the receipt

A successful receipt records:

- The network, escrow contract ID, and token ID.
- The built WASM path and SHA-256 values.
- The create, deposit, and release transaction hashes.
- The expected lifecycle event names and receipt verification status.
- Final buyer, seller, and contract balances.

The final status must be `Completed`. For an amount of 500, the buyer must lose
500, the seller must gain 500, and the contract balance must return to its
pre-flow value.

```bash
python3 -m json.tool artifacts/escrow-smoke-receipt.json
```

## Maintainer-only artifact evidence update

A source or toolchain change can legitimately change the WASM hash. Do not
silence a mismatch by changing the evidence file during an ordinary run. A
maintainer may intentionally update it with:

```bash
MAINTAINER_UPDATE=1 \
  bash scripts/demo-testnet.sh --update-evidence
```

Review the resulting hash and commit the evidence change with the corresponding
source/toolchain change.

## Repository verification

```bash
cargo test --workspace --all-targets --locked
make lint
```

`make lint` requires GNU Make. On Windows, use a Unix-like shell or run the
underlying Cargo lint command from the Makefile directly.
