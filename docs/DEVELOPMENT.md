# Development Guide

This guide covers the development workflow for Soroban Forge. All commands documented here are tested and work in the current repository.

## Prerequisites

### Developer CLI (soroban-forge)

Build and run the developer CLI:

```bash
# Build CLI binary
cargo build -p soroban-forge-cli

# Scaffold a new Soroban contract crate
cargo run -p soroban-forge-cli -- new my-token

# Scaffold with custom path
cargo run -p soroban-forge-cli -- new my-token --path ./custom/path/my-token
```


### Rust and Toolchain

The project requires **stable Rust** and the `wasm32v1-none` target for Soroban contracts.

```bash
# Install or update Rust
rustup update stable

# Install required components
rustup component add rustfmt clippy

# Add WASM target required by soroban-sdk
rustup target add wasm32v1-none
```

### Soroban CLI (Optional)

For deployment and contract interaction:

```bash
cargo install soroban-cli
```

### soroban-sdk Version

The project uses soroban-sdk 27.x. See `rust-toolchain.toml` for the exact pinned version.

## Clone and Setup

```bash
# Clone the repository
git clone https://github.com/Meet-hybrid/soroban-forge.git
cd soroban-forge

# Verify workspace loads
cargo metadata --locked --no-deps --format-version 1 > /dev/null
```

## Build Commands

### Full Workspace Build

```bash
make build
# or: cargo build --workspace --all-targets
```

### Release Build (WASM Artifacts)

```bash
cargo build --release --target wasm32v1-none -p soroban-forge-escrow
# Builds: target/wasm32v1-none/release/soroban_forge_escrow.wasm
```

### Build All Contracts for Release

```bash
cargo build --locked --release --target wasm32v1-none \
  --package soroban-forge-escrow \
  --package soroban-forge-vesting \
  --package soroban-forge-multi-sig-wallet \
  --package soroban-forge-dao-governance \
  --package soroban-forge-subscription-payments \
  --package soroban-forge-marketplace-royalties
```

### Build and Check WASM with the Developer CLI

The CLI can build one contract or all six contract packages for the required
`wasm32v1-none` target. WASM builds always use the optimized release profile.
Use `--package` to select a single package; without it, the CLI builds all
contract packages and skips the host-only CLI and shared utility crates.

```bash
# Build all contract WASM artifacts
cargo run -p soroban-forge-cli -- build --wasm

# Build one contract and verify its artifact against the 150,000-byte budget
cargo run -p soroban-forge-cli -- build --wasm --check-size \
  --package soroban-forge-escrow

# Build all contracts and print a size table; --check-size implies --wasm
cargo run -p soroban-forge-cli -- build --check-size
```

Size verification inspects `.wasm` files under
`target/wasm32v1-none/release/` (or `$CARGO_TARGET_DIR/wasm32v1-none/release/`
when `CARGO_TARGET_DIR` is set). It reports each artifact's byte and KB size,
the 150,000-byte budget, and a `PASS` or `FAIL` status. If any artifact exceeds
the budget, or no artifact is found for the selected package, the command exits
with an error. If the target is missing, install it with
`rustup target add wasm32v1-none` and rerun the build.

Example output:

```text
Contract                                  Size (bytes)   Size (KB)   Budget (bytes)  Status
----------------------------------------  ------------  ----------  ---------------  ------
soroban_forge_dao_governance.wasm                12309       12.02           150000  PASS
soroban_forge_escrow.wasm                        20425       19.94           150000  PASS
soroban_forge_marketplace_royalties.wasm         15099       14.74           150000  PASS
soroban_forge_multi_sig_wallet.wasm              23571       23.01           150000  PASS
soroban_forge_subscription_payments.wasm         10877       10.62           150000  PASS
soroban_forge_vesting.wasm                       14596       14.25           150000  PASS
```

## Test Commands

### Full Test Suite

```bash
make test
# or: cargo test --workspace --all-targets --locked
```

### Test Individual Contract

```bash
# Escrow
cargo test --workspace --package soroban-forge-escrow

# Vesting
cargo test --workspace --package soroban-forge-vesting

# Multi-Sig Wallet
cargo test --workspace --package soroban-forge-multi-sig-wallet

# DAO Governance
cargo test --workspace --package soroban-forge-dao-governance

# Subscription Payments
cargo test --workspace --package soroban-forge-subscription-payments

# Marketplace Royalties
cargo test --workspace --package soroban-forge-marketplace-royalties
```

## Lint and Format

### Code Formatting

```bash
make format
# or: cargo fmt --all
```

### Format Check (CI)

```bash
make format-check
# or: cargo fmt --all -- --check
```

### Linting

```bash
make lint
# or: cargo clippy --workspace --all-targets -- -D warnings
```

### Security Audit

```bash
make audit
# or: cargo audit
```

## Documentation

### Generate Rust Documentation

```bash
make doc
# or: cargo doc --workspace --no-deps --locked --document-private-items
```

### Run Full Release Checks

```bash
make release
# Runs: format, lint, audit, test, build-release
```

## Repository Commands Reference

| Command        | Purpose                 |
| -------------- | ----------------------- |
| `make build`   | Build workspace         |
| `make test`    | Run all tests           |
| `make format`  | Format code             |
| `make lint`    | Run clippy              |
| `make audit`   | Check dependencies      |
| `make doc`     | Generate docs           |
| `make clean`   | Clean build artifacts   |
| `make release` | Full pre-release checks |

## Contract Testing Notes

- Tests use Soroban SDK's `Env` test harness
- Mock authentication (`mock_all_auths`) for positive tests
- Integration tests use Stellar Asset Contract (SAC) fixtures
- Escrow includes property testing for conservation invariant

## Escrow Storage, TTL, and Token Trust

### Persistent storage and TTL

Escrow records are stored as per-id **persistent** entries. Each record is
extended to a **30-day TTL** when it is written. State-changing escrow
operations that update the record extend the entry's TTL using the same
threshold-and-extend-to pattern. `touch_ttl` is permissionless and can extend
an existing entry while it remains present in persistent storage.

An active escrow with no state-changing activity can eventually reach expiry.
Once the persistent escrow entry has expired, `touch_ttl` cannot recover it:
the current implementation calls `load_escrow` before attempting the TTL
extension, and a missing entry is reported as `NotFound`. The expired record
is therefore inaccessible through the current contract interface. Expiration
of the record does not remove the token balance; the funds remain in the token
contract at the escrow contract's address. Long-lived active escrows therefore
require a keeper to call `touch_ttl` before expiry. Anyone may perform this
keeper action because `touch_ttl` is permissionless.

### Token trust model

`create_escrow` accepts a user-specified token address. The escrow does not
validate whether that address is a deployed token contract and does not itself
enforce SEP-41 compliance. The design assumes the supplied token follows the
expected SEP-41 interface and behavior. Token transfer failures are handled
through the existing `ForgeError::TokenTransferFailed` path. A malicious or
non-compliant token is an external trust assumption, not a condition that the
escrow currently validates against.

## CI Pipeline

The CI workflow (`.github/workflows/ci.yml`) runs:

1. **rustfmt** - Code formatting check
2. **clippy** - Linting with strict warnings
3. **build** - Full workspace build + docs
4. **test** - Test suite execution
5. **audit** - Dependency vulnerability scan
6. **dependency-policy** - License, source, and ban checks
7. **wasm-size** - Contract size budget enforcement
8. **provenance** - Build reproducibility verification

ForgeBot (`.github/workflows/forgebot.yml`, `scripts/forgebot/`) reports these
results back to each pull request as a single sticky comment and publishes an
informational `ForgeBot / ready-for-review` commit status. It does not add or
replace any check, and it never approves or merges a pull request. See
[ForgeBot](FORGEBOT.md) for details.

### Dependency Policy (cargo-deny)

Soroban Forge enforces a dependency policy via [cargo-deny](https://embarkstudios.github.io/cargo-deny/) to ensure compliance with license compatibility, source integrity, and ban policies.

**Policy:**

- **Licenses**: Only permissive licenses (MIT, Apache-2.0, BSD, ISC, Unicode, CC0) are allowed. GPL/AGPL/SSPL are denied.
- **Sources**: All dependencies must come from crates.io; git and path dependencies are banned to prevent supply-chain surprises.
- **Bans**: Multiple versions of the same crate are flagged; justified exceptions are documented in `deny.toml`.

**Configuration:**
The policy is defined in [deny.toml](../../deny.toml) at the workspace root. Each non-default choice is commented to explain the rationale.

**Running checks locally:**

```bash
# Install cargo-deny
cargo install --locked cargo-deny

# Check licenses
cargo deny check licenses

# Check sources
cargo deny check sources

# Check bans and duplicates
cargo deny check bans
```

**Adding exceptions:**
If a dependency legitimately requires an exception (e.g., an older crate with an undeclared license, or a tool-only dev dependency), add it to `deny.toml` with a clear comment explaining why it is justified. Example:

```toml
[licenses]
exceptions = [
    # { name = "crate-name", allow = ["MIT"] },  # Reason: <justification>
]
```

All exceptions must be reviewed and approved before merge.

## Common Issues

### WASM Build Fails

Ensure `wasm32v1-none` target is installed:

```bash
rustup target add wasm32v1-none
```

### Lock File Conflicts

Use `--locked` flag to ensure reproducible builds:

```bash
cargo build --locked
```

### Clippy Failures

The project uses `-D warnings` (fail on warnings). Fix issues reported by clippy or document intentional deviations.

## WASM Size Budget

Contracts have a maximum size of 150,000 bytes. This is enforced in CI.
