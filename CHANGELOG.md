# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **Tranche vesting schedules** (`create_tranche_schedule`): a second
  schedule shape whose unlock table is an explicit, ordered list of
  `(unlock_at, amount)` tranches, so a grant agreement like "25% at TGE, 25%
  at +6 months, 50% at +12 months" is expressible instead of approximated with
  several schedules and custody pots. Additive by design: a new
  `TrancheSchedule` record type plus a `DataKey::TrancheSchedule` variant, so
  the linear record's wire shape and its floor-division math (Known
  Limitations §5) are untouched; both kinds share the one monotonic id
  counter and the one claim path. `claim`/`claimable`/`get_status` are
  kind-aware, and `get_tranche_schedule` is the tranche-side record view
  (the linear `get_schedule` view is tracked separately). The vested amount is
  the cumulative sum of every tranche whose offset has elapsed — a step
  function, not a ramp — computed by a bounded scan (tables are capped at
  `MAX_TRANCHES = 32` and stored once, immutable), with checked arithmetic on
  every cumulative sum and progress compared on the elapsed offset so a
  `u64::MAX` offset cannot overflow. Settlement is unchanged: the SEP-41
  transfer still runs before the state write, a zero claim issues no transfer,
  and a failed transfer (underfunded contract, undeployed token) leaves
  `claimed`/`status` untouched. 24 new tests (51 in the crate): table
  validation, boundary claims at and around every unlock, real-SAC settlement
  per step, interleaved claims at arbitrary timestamps with conservation,
  transfer-failure and undeployed-token paths, and both kinds on one id space.
- **Negative-authorization test suite** for escrow
  (`crates/escrow/src/authz.rs`, 19 tests): per entrypoint, proves a wrong
  signer is rejected by the host, that an armed signature cannot be
  replayed over different arguments (create over a changed amount, deposit
  pull over a changed amount), that a dispute claim requires the claimant's
  own signature, and that a blank envelope aborts every state-changing
  call without writing state. Includes `env.auths()` assertions pinning the
  authorized-invocation tree of every payout path. Verified finding, now
  documented: contract self-authorization is implicit in the Soroban host,
  so the recorded tree for a payout is the party's entrypoint frame only —
  and a seller signature alone legitimately completes a release.
- **Reviewer walkthrough** (`docs/WALKTHROUGH.md`): step-by-step expected
  output for verifying the repo end-to-end, including the testnet receipt
  round.
- **Provenance manifest gate** (`scripts/provenance.sh` + a CI job):
  builds all six contract WASMs, records SHA-256 hashes per artifact at a
  named git revision into `provenance-manifest.json`, verifies them from a
  clean rebuild, and uploads the manifest as a CI artifact.
- **Mainnet deploy + smoke script** (`scripts/deploy-mainnet.sh`): mirrors
  the testnet demo against Pubnet — same three escrow rounds over a
  zero-value smoke asset, with an explicit cost-confirmation gate,
  best-effort XLM preflight (stroop-correct), idempotent trustlines,
  WASM-hash cross-check against the provenance manifest, and explorer
  receipts. Prepared, not yet executed: needs four pre-funded mainnet
  identities (human step). Known limitation: the installed stellar CLI
  version may disagree with the preflight invocations — the script then
  warns and continues rather than failing mid-run.

### Fixed
- **Security audit is clean without ignores.** `time` was updated to
  0.3.47 in the lockfile, clearing RUSTSEC-2026-0009 (an earlier note
  claimed the SDK 27 migration removed it — it did not; the advisory
  remained and CI correctly failed). The unused direct `rand` dependency
  was dropped, and the weekly audit workflow's stale ignore was removed.
  Remaining audit output is warnings only, both transitive and not
  actionable from this workspace: `paste` (unmaintained, via
  soroban-sdk) and `rand` 0.8 (unsound edge case, via
  soroban-env-host's crypto stack); warnings do not fail the gate.

### Changed
- **Workspace migrated to soroban-sdk 27.0.6 on stable Rust** (was 21.5.1
  pinned to Rust 1.96.0). Contracts build for `wasm32v1-none` (escrow
  WASM: ~18 KB). Test registration uses `env.register(Contract, ())`.
- **Escrow contract rebuilt as the flagship primitive** (v0.1.0's was a
  state machine only):
  - Real SEP-41 token settlement — `deposit` pulls from the buyer,
    `release`/`refund`/`resolve` pay out — with **transfer-before-state
    ordering** so a failed transfer leaves storage untouched.
  - **Dispute flow implemented**: `dispute(escrow_id, claimant)` by the
    buyer or seller, `resolve(escrow_id, in_favor_of_seller)` by the
    arbiter, final. `Disputed` is now a live state, not reserved.
  - **Release is seller-confirmed** (the paid party confirms delivery);
    the v0.1.0 buyer-confirmed semantics are gone.
  - **Lifecycle events**: `EscrowCreated`, `Deposited`, `Released`,
    `Refunded`, `Disputed`, `Resolved`, `Cancelled` (escrow id as topic).
  - **Per-record persistent storage** with TTL bumps on every write and a
    permissionless `touch_ttl` keeper entrypoint; only the id counter
    remains in instance storage.
  - `create_escrow` now takes the `token` address; timeout refund
    semantics unchanged.
- Token failures are bucketed as `ForgeError::TokenTransferFailed`
  (new shared error variant, code 11) rather than forwarding opaque
  token discriminants.
- Workspace version bumped to 0.2.0 (path-dependency versions updated).

### Added
- **Randomized property suite** for the escrow contract (`props.rs`,
  proptest): P1 conservation over random terminal paths, P2 pool
  conservation under storage-tampering adversaries (`env.as_contract`),
  P3 fund safety over arbitrary call sequences checked against an
  independent state-machine mirror, including outsider-dispute
  rejection. Escrow tests 27 → 30; workspace 107. The suite's first run
  caught a bug — in the test's own conservation formula, fixed, with the
  minimal counterexample preserved in a comment.
- `docs/REJECTION-PROOFING.md` (pre-submission checklist with owners)
  and `docs/DESIGN-PARTNER-OUTREACH.md` (pilot outreach template).
- **Generated TypeScript client** for the deployed escrow contract
  (`packages/typescript-sdk`, published as `@soroban-forge/escrow-client`):
  produced by `stellar contract bindings typescript` from the testnet
  contract's ABI, with the deployed contract ID embedded and every
  method typed with doc comments from the Rust source. Replaces the
  v0.1.0 console-log placeholder SDK.
- `docs/GRANT-APPLICATION.md`: application narrative with the live
  testnet proof links, the rejection-cause resolutions, and a
  tranche-scoped ask.
- Escrow test suite grew from 16 to 27 tests, including a **conservation
  property** asserting `deposited == paid out` on every terminal path ×
  timeout combination, insufficient-balance failure ordering, dispute
  freeze coverage, and TTL-keeper behavior. Workspace total: 104 tests.
- `docs/FEATURE-STATUS.md` (per-entrypoint status matrix) and
  `docs/RESUBMISSION.md` (phased plan); `docs/KNOWN-LIMITATIONS.md`
  rewritten to the post-migration state.

### Removed
- Internal maintainer-process docs (`docs/maintainers/`) and
  `.github/settings.yml` from the public tree; contributor-facing work remains
  tracked as labeled [issues](https://github.com/Meet-hybrid/soroban-forge/issues).

### Added
- **Vesting contract** (implemented): `create_schedule`, `claim`, `claimable`,
  and `get_status` with linear release after a cliff, `require_auth`
  enforcement, checked arithmetic, and 21 in-crate `Env`-based tests covering
  the cliff/duration boundaries, no-overpay claims, overflow reporting, and
  missing-schedule errors.
- **Multi-sig wallet contract** (implemented): `initialize`, `submit`, `confirm`,
  `execute`, `get_threshold`, and `get_tx` with owner/threshold configuration,
  one-confirmation-per-owner enforcement, `require_auth` checks, and 18
  in-crate `Env`-based tests.
- **DAO governance contract** (implemented): `propose`, `vote`, `execute`, and
  `get_proposal` with voting deadlines, one-vote-per-voter enforcement,
  majority finalisation, and 16 in-crate `Env`-based tests.
- **Subscription payments contract** (implemented): `subscribe`, `charge`,
  `cancel`, and `get_subscription` with period-based billing that catches up
  one period per call, and 12 in-crate `Env`-based tests.
- **Marketplace royalties contract** (implemented): `set_royalty`, `distribute`,
  and `get_royalty` with basis-point royalty splits capped at 100%, and 10
  in-crate `Env`-based tests.

### In progress
- Token settlement (SAC transfers) is out of scope for all contracts; each
  tracks state, authorisation, and (where applicable) timing. Reserved states
  (`Disputed`, `Revoked`, `Queued`, `PastDue`, `Rejected`, `Disabled`) and
  features such as weighted voting, plan management, and multi-recipient
  royalties land in follow-ups.

## [0.1.0] - 2026-08-04

### Added
- Virtual Cargo workspace of nine crates with shared `workspace.package`
  metadata (version, edition, rust-version, license, repository).
- `shared-utils` crate: `ForgeError`, `StorageEntry` storage pattern, and
  shared types (`Party`, `TimeBounds`, `PaginatedResult`, `PaginationCursor`).
- `test-utils` crate: Soroban `Env` test harness (`new_env`) and mock account
  sets (`TestAccounts`).
- **Escrow contract** (implemented): `create_escrow`, `deposit`, `release`,
  `refund`, `cancel`, and `get_status` with `require_auth` authorization,
  deadline-based refunds, checked arithmetic, and 16 in-crate `Env`-based
  tests covering the full state machine and error paths.
- Public interfaces and storage types for the vesting, multi-sig-wallet,
  dao-governance, subscription-payments, and marketplace-royalties contracts
  (implementations land via the issue backlog).
- Developer CLI (`soroban-forge-cli`) with `build`, `test`, `lint`, and
  `deploy` subcommands, wired into the workspace.
- Language bindings and examples under `packages/`: TypeScript SDK, Next.js
  reference application, and deployment templates (not covered by workspace
  CI).
- Continuous integration (`.github/workflows/ci.yml`): Rustfmt, Clippy
  (`-D warnings`), Build, Test, Security Audit, and a WASM Size Check with a
  per-contract size budget; a `Release` workflow that builds WASM artifacts
  and drafts a GitHub release on `v*` tags.
- Maintainer tooling: reusable `scripts/` for label and issue creation.
- MIT OR Apache-2.0 dual license, security policy, and contribution
  guidelines.

### Changed
- Committed `Cargo.lock` for reproducible builds; all CI cargo commands use
  `--locked`.
- Replaced raw `Val` fields used under `#[contracttype]` with serializable
  `Bytes` (`StorageEntry`, `PaginatedResult`, `WalletTx`, `Proposal`).
- Corrected repository URLs and the CI badge from `teachlink` to
  `Meet-hybrid`.
- Pinned the toolchain to Rust 1.96.0 (`rust-toolchain.toml` and CI): the
  pinned soroban-sdk 21.x does not compile on newer stable toolchains.
- Main branch protection: one approving review required (repository
  administrators exempt), six required status checks, linear history.
- Dependabot updates grouped to reduce pull-request noise; risky major
  bumps (soroban-sdk, rand) intentionally ungrouped for triage.

### Fixed
- CI WASM-size check built no WASM artifacts silently; it now builds with
  `--target wasm32-unknown-unknown` and fails when no artifacts are produced.
- Escrow WASM build failure (missing `#![no_std]`).
- `audit.yml` invalid YAML and missing `checks: write` permission.
- CLI crate did not compile and was excluded from the workspace; rewired,
  clippy-clean, and smoke-tested.

### Security
- `unsafe` code is forbidden workspace-wide.
- `cargo audit` runs in CI. `RUSTSEC-2026-0009` (`time` 0.3.44) is a
  transitive dependency of the pinned soroban-sdk 21.x chain, is not
  compiled into the workspace graph, and is ignored in CI with rationale
  until the soroban-sdk 27 migration (issue #14) removes it.
