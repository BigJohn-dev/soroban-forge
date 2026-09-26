# Known Limitations

This document states, without marketing, what the Soroban Forge contracts
**do not do** today. Everything here is a deliberate, tracked scoping
decision — not an oversight.

**Last verified against:** the SDK 27 migration (workspace v0.2.0).

---

## Resolved in the SDK 27 / Phase 1 migration

These were the headline gaps in v0.1.0; all are closed **for the escrow
contract** and remain open for the other four:

1. **No token settlement** — escrow now performs real SEP-41 transfers
   (`deposit` pulls from the buyer, `release`/`refund`/`resolve` pay out)
   with transfer-before-state ordering so a failed transfer leaves no
   partial state. Marketplace royalties, vesting, and DAO governance
   (proposal bonds) have since gained settlement the same way;
   subscriptions still move nothing.
2. **Instance-only storage** — escrow, multi-sig wallet, DAO governance, and marketplace royalties now use **persistent** entries with TTL bumps on every write and permissionless `touch_ttl` keeper entrypoints. Vesting and subscriptions still use instance storage.
3. **No events** — escrow, multi-sig wallet, DAO governance, subscription payments, and marketplace royalties emit lifecycle events; only vesting remains silent.
4. **Arbiter stored but unreachable** — `dispute` (claimant-authorized)
   and `resolve` (arbiter-only, final) make the third party live.
   `Disputed` is a real state, verified by tests.
5. **Toolchain pin** — the workspace now builds on **stable** Rust with
   soroban-sdk **27.0.6** and the `wasm32v1-none` target. The old
   Rust 1.96.0 / SDK 21.5.1 pin is gone.

## Still open

### 1. Token settlement for subscription payments

Subscriptions remain state machines: amounts are validated and stored, never
moved. (Marketplace royalties moved off this list: `settle_sale` transfers
real SEP-41 tokens with the escrow pattern. Multi-sig wallet also
moved off this list: `execute` performs real cross-contract
invocations via `try_invoke_contract`. Vesting also moved off this
list: `claim` transfers the vested amount to the beneficiary with
transfer-before-state ordering. DAO governance also moved off this list:
`propose` pulls a SEP-41 proposal bond into contract custody and the bond is
refunded to the proposer (`Executed`, `Cancelled`) or forfeited to the
configured treasury (`Defeated`) in the same frame as the terminal
transition.) The remaining contract gets its own tranche using the escrow
pattern (see
[RESUBMISSION.md](RESUBMISSION.md#phase-1--flagship-escrow-primitive-3-weeks)).

### 2. Instance-only storage outside escrow, multi-sig wallet, DAO governance, and marketplace royalties

Vesting and subscriptions keep state in `env.storage().instance()`.
Long-lived records there still face the byte budget and TTL-expiry
bricking problem. (Escrow, multi-sig wallet, DAO governance, and marketplace royalties migrated per-record data to persistent storage with `touch_ttl` keeper entrypoints).

### 3. No events in vesting contract

Vesting remains without an event module. Escrow, multi-sig wallet, DAO governance, subscription payments, and marketplace royalties emit typed on-chain events.


### 4. Negative authorization coverage outside escrow, vesting, and DAO governance

**Closed for escrow, vesting, and DAO governance** (was the open item here): dedicated
negative-auth suites (`crates/escrow/src/authz.rs`, `crates/vesting/src/authz.rs`,
`crates/dao-governance/src/authz.rs`) prove per entrypoint that a wrong signer is
rejected by the host, that armed signatures cannot be replayed over different
arguments or schedule IDs, and — via `env.auths()` tree assertions — pin the exact
authorized-invocation tree every creation and payout path demands. The DAO suite
additionally covers the bond-bearing paths: the proposer's signature must carry the
nested token `transfer` authorization for the bond pull, and the outgoing
refund/forfeit transfers are covered by contract self-authorization (blank envelope).
They also document the verified mechanics: contract self-authorization is implicit
(the host auto-approves `require_auth` from the executing contract), which is why
a party signature alone legitimately completes a payout.

Still open: the other three contracts' entrypoints are proven at call-graph
level only; subscriptions still move nothing.

### 5. Vesting rounding residue

Vesting claims use floor division; per-claim residue (at most one stroop
× number of claims) stays in the contract until the final claim, where it
is paid out in full (settlement landed: residue is claimable, never lost,
asserted by `floor_division_residue_stays_claimable_until_final_claim`).
No dust-sweep entrypoint — revisit only as a convenience.

### 6. Testnet only — no mainnet deployment

The escrow contract **is deployed on testnet** with a verified receipt
round (see the README proof table and `scripts/demo-testnet.sh`). There
is no mainnet deployment, and testnet receipts are not a substitute for
an audit or a mainnet beta.

The mainnet path is prepared and **partially executed**:
`scripts/deploy-mainnet.sh` mirrors the testnet demo against Pubnet
(same three rounds, zero-value smoke asset, explicit `--yes` cost gate,
balance preflight, WASM-hash cross-check against the provenance
manifest).

Executed so far: the smoke SAC (`smoke:<issuer>`,
`CBBCLWWUZSO25MYJEU2JBGCJK2F2GQ3WQVM2WMHTSGRP3GVKRGIDN4CW`) is live on
mainnet, deployed through the script's flow with Horizon confirmation.
Not executed: the escrow contract itself. The WASM upload simulates at
**17.57 XLM resource/rent cost** (deterministic — measured via RPC
simulation, `min_resource_fee` 175,684,169 stroops, plus inclusion
fee), and the run was deferred pending funding rather than attempted
with insufficient balance. A mainnet escrow receipt, once it exists,
will be added to the README proof table with its own explorer links.
Total one-time cost to finish: ~18–19 XLM on the issuer account.

### 7. `packages/` are minimal

The TypeScript SDK is now a **generated client from the deployed escrow
contract's ABI** (`@soroban-forge/escrow-client`, contract ID embedded) —
replacing the v0.1.0 console-log placeholder. It has no dedicated test
suite of its own yet, and the Next.js example remains a static landing
page (stale `teachlink` links fixed; a real demo UI is future work).

### 8. Single maintainer

All commits are by one person. Contributor-facing process exists — scoped
issues with acceptance criteria, fork-first workflow — but no external
contributions have landed yet.

## Design notes (not limitations, but worth knowing)

- **Either-party authorization** uses an explicit claimant parameter
  (`dispute(escrow_id, claimant)`) because Soroban cannot express
  "require_auth by A *or* B" in a single entrypoint. The claimant must be
  a party to the escrow and must actually authorize; the check runs
  before `require_auth` so outsider claims fail cheaply.
- **Token errors are bucketed**, not forwarded: a token-contract failure
  surfaces as `ForgeError::TokenTransferFailed` because a client cannot
  tell which contract produced a forwarded discriminant. Root causes stay
  visible in diagnostic events.
- **Escrow release is seller-confirmed**: the paid party confirms
  delivery. Buyer-confirmed release was the v0.1.0 behavior and is what
  made the old contract a confirmation flow rather than escrow.
- **Contract self-authorization is implicit**: the Soroban host
  auto-approves `require_auth` when the demand comes from the currently
  executing contract. That is what makes the custody pattern work —
  outgoing payouts need no `__check_auth` — and it is why the recorded
  authorized-invocation tree for a payout shows only the party's
  entrypoint frame. Verified, not assumed: see the authz test module's
  documentation.

## Out of scope for the flagship phase (deliberate)

- Settlement work for subscription payments (the last contract that never
  moves tokens)
- Weighted voting, plan management, multi-recipient royalties
- Formal verification, external audit (planned before any mainnet use)
