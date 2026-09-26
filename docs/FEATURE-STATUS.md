# Feature Status Matrix

Per-entrypoint status across all six contracts. **Implemented** means:
implemented, tested, and covered by workspace CI. The escrow contract is
the flagship: it moves real SEP-41 tokens; marketplace royalties settles
its splits the same way via `settle_sale`; vesting settles its claims via
`claim`. The two remaining state machines (DAO governance, subscriptions)
are honestly labeled where they track but do not settle.

**Last verified against:** the SDK 27 migration (workspace v0.2.0).

---

## Escrow (`crates/escrow`) — **flagship**

| Entrypoint | Status | Notes |
|---|---|---|
| `create_escrow` | ✅ Implemented | Validates amount/timeout, buyer+seller auth, takes the SEP-41 token address |
| `deposit` | ✅ Implemented | **Real token transfer** buyer → contract, before any state write |
| `release` | ✅ Implemented | Seller-authorized; **real token transfer** contract → seller |
| `refund` | ✅ Implemented | Seller pre-deadline / buyer post-deadline; **real token transfer** |
| `dispute` | ✅ Implemented | Claimant (buyer or seller) authorized, `Funded` only — see [design notes](KNOWN-LIMITATIONS.md#design-notes-not-limitations-but-worth-knowing) |
| `resolve` | ✅ Implemented | Arbiter-only, final; pays either direction via **real token transfer** |
| `cancel` | ✅ Implemented | Buyer, `Pending` only |
| `get_status` / `get_escrow` | ✅ Implemented | Read-only |
| `touch_ttl` | ✅ Implemented | Permissionless TTL keeper for the escrow's persistent entry |
| Events | ✅ Implemented | `EscrowCreated`, `Deposited`, `Released`, `Refunded`, `Disputed`, `Resolved`, `Cancelled`; id as topic |
| Storage | ✅ Persistent + TTL | Per-id persistent entries; instance storage only for the id counter |
| Tests | ✅ 49 | Full lifecycle, dispute paths, failure ordering, conservation property, **randomized property suite** (proptest): conservation over random paths, tamper-resilient pool conservation, fund safety over arbitrary call sequences; **negative-auth suite** (`authz.rs`): per-entrypoint wrong-signer rejection, signature/args replay rejection, `env.auths()` authorization-tree assertions |

## Vesting (`crates/vesting`)

Two schedule shapes share one id space and one claim path: **linear**
(cliff + ramp, unchanged) and **tranche** (explicit unlock table).

| Entrypoint | Status | Notes |
|---|---|---|
| `create_schedule` | ✅ Implemented | Linear shape; validates `total_amount > 0`, `duration > 0`, `cliff <= duration` |
| `create_tranche_schedule` | ✅ Implemented | Tranche shape; validates non-empty table, ≤ `MAX_TRANCHES` (32) entries, `amount > 0`, strictly increasing `unlock_at`, sum within `i128::MAX`; beneficiary-authorized, table stored once and immutable |
| `claim` | ✅ Implemented | Both kinds; **real token transfer** contract → beneficiary before the state write (transfer-before-state); zero-claim calls skip the transfer; a failed transfer surfaces as `ForgeError::TokenTransferFailed` with `claimed`/`status` unchanged |
| `claimable` | ✅ Implemented | Read-only; kind-aware (ramp vs cumulative unlock step function) |
| `get_status` | ✅ Implemented | Read-only; derived from ledger time + claimed amount, both kinds |
| `get_tranche_schedule` | ✅ Implemented | Read-only record view, immutable unlock table included (`NotFound` for a linear id; the linear `get_schedule` view is tracked separately) |
| Revocation | ❌ Not implemented | `Revoked` status reserved |
| Enumerable schedules per beneficiary | ❌ Not implemented | Follow-up |
| `VestingSchedule.token` field | ✅ Wired | Read by `claim` for the SEP-41 payout |
| Tests | ✅ 51 | 27 linear (unchanged) + 24 tranche: table validation, boundary claims at/around every unlock, real-SAC settlement, interleaved claims at arbitrary timestamps, `u64::MAX` offset boundary, transfer-failure and undeployed-token paths, both kinds on one id space |

## Multi-Sig Wallet (`crates/multi-sig-wallet`)

| Entrypoint | Status | Notes |
|---|---|---|
| `initialize` | ✅ Implemented | Owner set + threshold validation |
| `submit` | ✅ Implemented | Creates pending transaction record |
| `confirm` | ✅ Implemented | One-confirmation-per-owner enforced |
| `execute` | ✅ Implemented | Threshold check + cross-contract `try_invoke_contract` to recorded `target`; status flip **after** invocation; target revert surfaces as `ForgeError::ContractInvocationFailed` and leaves tx `Pending`; events emitted |
| `get_threshold` / `get_tx` | ✅ Implemented | Read-only |

## DAO Governance (`crates/dao-governance`)

| Entrypoint | Status | Notes |
|---|---|---|
| `propose` | ✅ Implemented | Deadline validation |
| `vote` | ✅ Implemented | One-vote-per-voter enforced |
| `execute` | ⚠️ State only | Majority finalisation flip; **executes nothing on-chain** |
| `get_proposal` | ✅ Implemented | Read-only |
| Weighted voting | ❌ Not implemented | Follow-up |

## Subscription Payments (`crates/subscription-payments`)

| Entrypoint | Status | Notes |
|---|---|---|
| `subscribe` | ✅ Implemented | Plan validation, periodic scheduling |
| `charge` | ⚠️ State only | Advances one period per call; **charges nothing** |
| `cancel` | ✅ Implemented | Subscriber or owner |
| `get_subscription` | ✅ Implemented | Read-only |
| Plan management | ❌ Not implemented | Follow-up |

## Marketplace Royalties (`crates/marketplace-royalties`)

| Entrypoint | Status | Notes |
|---|---|---|
| `set_royalty` | ✅ Implemented | Basis-point caps validated |
| `distribute` | ⚠️ Computes only | Pure split math; **pays no recipients** (use `settle_sale`) |
| `settle_sale` | ✅ Implemented | **Real token transfers** payer → seller, then payer → royalty recipient; transfer-before-state, totals committed last |
| `get_royalty` | ✅ Implemented | Read-only |
| `get_settlement_summary` | ✅ Implemented | Read-only; cumulative sales, volume, and royalties per collection |
| Multi-recipient splits | ❌ Not implemented | Follow-up |

---

## Cross-cutting

| Concern | Status | Notes |
|---|---|---|
| Checked arithmetic | ✅ Workspace-wide | Overflow-safe; vesting guards documented |
| `require_auth` on every state change | ✅ Workspace-wide | Escrow: proven against wrong signers via the negative-auth suite (`authz.rs`) + authorization-tree assertions; other five: call-graph level only (see [Known Limitations §4](KNOWN-LIMITATIONS.md)) |
| Events | ⚠️ Escrow only | Full lifecycle events on escrow; none on the other five |
| Persistent storage + TTL | ⚠️ Escrow only | Per-id persistent entries + `touch_ttl` keeper; others instance-only |
| SEP-41 token settlement | ⚠️ Escrow + royalties + multi-sig + vesting | Real transfers with transfer-before-state ordering on escrow (`deposit`/`release`/`refund`/`resolve`) and vesting (`claim`); marketplace `settle_sale` settles splits; multi-sig `execute` performs cross-contract `try_invoke_contract` on opaque-payload txs; DAO governance and subscriptions still store amounts only |
| Testnet deployment | ✅ Escrow deployed | Contract ID, WASM sha256, and receipt rounds in the README "Proof at a glance" table; the other five are not deployed |
| Mainnet deployment | ⚠️ Partial | Smoke SAC live (`CBBCLWWU…DN4CW`, Horizon-confirmed); escrow WASM upload measured at **17.57 XLM rent** via simulation and deferred pending funding — see [Known Limitations §6](KNOWN-LIMITATIONS.md) |
| TypeScript SDK | ✅ Generated | `@soroban-forge/escrow-client` generated from the deployed escrow ABI (no own test suite yet) |
| CI (fmt/clippy/test/audit/WASM size/provenance) | ✅ Enforced | `--locked`, `-D warnings`, stable toolchain, `wasm32v1-none`, size budget, **provenance manifest job** (SHA-256 of all six WASM artifacts from a clean rebuild) |
| External audit | ❌ Not performed | Planned as a grant-funded tranche deliverable before any mainnet value custody |
| Soroban SDK version | ✅ 27.0.6 | Stable Rust; `wasm32v1-none` target |
