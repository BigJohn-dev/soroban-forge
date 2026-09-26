# PR: Proposer-initiated cancellation of pending proposals

Closes #83

## Description

Adds `cancel_proposal` to `crates/dao-governance`: a proposal that is still
open for voting can be withdrawn by its original proposer. The proposal is
marked `Cancelled`, frozen against further votes, and cannot be executed.

This fixes the two dead-end states the DAO previously had no way out of:

- a stray/failed proposal that sat in `Active` forever, and
- a proposal that reached quorum but had not been executed and sat
  executable indefinitely.

The change is deliberately small: no new storage keys (the `Cancelled` state
lives on the existing per-proposal `DataKey::Proposal(id)` entry), no new
authorization roles, and no execution/timelock/weighting behavior (see
Out of scope).

### Before / after lifecycle

Before (no exit for `Active` except finalisation):

```text
propose
  --> Active --vote × n--> voting ends
  --> execute: for > against ? Succeeded : Defeated
```

After (`Active` withdrawable by proposer until execution):

```text
propose
  --> Active --vote × n--> voting ends
  --> execute: for > against ? Succeeded : Defeated
  --> cancel (proposer only): Cancelled (terminal)
```

`Cancelled` is terminal: no re-voting, re-opening, or resubmission.
Cancellation is allowed regardless of the current vote tally — including
after voting ends and after quorum is met, as long as the proposal has not
been executed.

## API

New entrypoint (existing `propose` / `vote` / `execute` / `get_proposal`
signatures are unchanged):

```rust
fn cancel_proposal(
    env: Env,
    proposal_id: u64,
    proposer: Address,
) -> Result<(), ForgeError>;
```

Guard order follows the crate's existing authorization pattern
(state validation first, then authorization):

1. missing proposal -> `ForgeError::NotFound`
2. not `Active` (already executed/cancelled) -> `ForgeError::InvalidInput`
3. caller is not the original `proposer` -> `ForgeError::Unauthorized`
4. `proposer.require_auth()`
5. state := `Cancelled`, persisted to the existing `DataKey::Proposal(id)`

A cancelled proposal is immune to `vote` and `execute` via the existing
"state must be `Active`" guards, which now also cover `Cancelled`.

## Storage

- No new key classes. `ProposalState` gains a `Cancelled` variant appended at
  the end of the enum so the stored discriminants of existing states
  (`Active`/`Succeeded`/`Defeated`/`Queued`) are unchanged.
- No migration required for already-deployed instances.
- No `cancelled_at` timestamp recorded (kept out to keep the state-machine
  change small).

## Tests

Nine new in-crate integration tests (existing house style):

- `cancel_active_proposal_succeeds`
- `cancel_immediately_after_propose_succeeds`
- `cancel_after_quorum_before_execute_succeeds` (tally preserved)
- `cancel_by_non_proposer_is_unauthorized`
- `cancel_missing_proposal_is_not_found`
- `double_cancel_is_invalid`
- `cancel_then_vote_is_invalid`
- `cancel_then_execute_is_invalid`
- `cancel_after_execute_is_invalid`

## Verification

- [x] `cargo fmt --all -- --check`
- [x] `cargo clippy --workspace --all-targets --locked -- -D warnings`
- [x] `cargo test -p soroban-forge-dao-governance --all-targets --locked`
      (25 passed, 0 failed)
- [x] WASM size: `soroban_forge_dao_governance.wasm` = 12 207 bytes
      (CI budget 150 000 bytes)

## Type of Change

- [x] New feature (non-breaking change which adds functionality)
- [x] Test addition or modification

## Contract Changes

- [x] Contract interface changes are additive and backward-compatible
      (one new entrypoint; existing signatures unchanged)
- [x] Storage schema migration is documented (none required; new state
      variant appended)
- [x] Test coverage >= 90%
- [x] Upgrade path is described (no storage change for deployed instances)

## Security Considerations

- [x] No introduction of unsafe code
- [x] No hardcoded secrets or keys
- [x] Input validation covers all public functions
- [x] Authorization checks are enforced on all sensitive operations
      (`require_auth` on the proposer; non-proposer -> `Unauthorized`)
- [x] Reentrancy and overflow considerations are documented (no new
      arithmetic; state write is a single instance-storage update)

## Out of scope

- On-chain execution of proposal actions (feat #58)
- Weighted voting by governance-token balance (feat #59)
- Proposal expiry/deadline mechanism (deliberate follow-up)
- Re-voting, re-opening, or resubmission of cancelled proposals
- Cancellation by anyone other than the original proposer