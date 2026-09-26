# DAO Governance Contract

On-chain proposal system with voting, bonded proposal creation, and
permissionless execution of approved opaque actions.

## Interface

```rust
fn configure_bond(token, amount, treasury) -> Result<(), ForgeError>
fn get_bond_config() -> Result<BondConfig, ForgeError>
fn propose(proposer, target, action, duration) -> Result<u64, ForgeError>
fn vote(proposal_id, voter, support) -> Result<(), ForgeError>
fn execute(proposal_id) -> Result<(), ForgeError>
fn cancel_proposal(proposal_id, proposer) -> Result<(), ForgeError>
fn get_proposal(proposal_id) -> Result<Proposal, ForgeError>
fn get_proposal_count() -> u64
fn get_proposals(offset, limit) -> Result<Vec<Proposal>, ForgeError>
fn has_voted(proposal_id, voter) -> Result<bool, ForgeError>
fn touch_ttl(proposal_id) -> Result<(), ForgeError>
```


`action` is forwarded as a single `Bytes` argument to the target contract's
`execute` entrypoint. Voting remains one vote per voter with a strict
majority. After the deadline, the first `execute` call finalises the vote; a
succeeded proposal is then dispatched by a subsequent permissionless
`execute` call. Only a successful target invocation changes `Succeeded` to
`Executed`. A target revert returns
`ForgeError::ContractInvocationFailed` and leaves the proposal retryable in
`Succeeded`.

The DAO call itself is permissionless after voting has ended. A target's own
`require_auth` is not implicitly satisfied by the DAO's cross-contract call;
targets that require authorization must receive an authorization path that
the target contract accepts.

## Proposal bonds

Every proposal is backed by a bond in a SEP-41 token: paid when the
proposal is created, settled when it reaches a terminal state.

**Configuration.** `configure_bond(token, amount, treasury)` is a one-time,
permissionless write — first caller wins, later calls return
`ForgeError::AlreadyInitialized`, mirroring the multi-sig `initialize`
pattern. The treasury address is fixed here, once, rather than chosen by
the party that later receives forfeited bonds. A non-positive amount is
rejected with `ForgeError::InvalidInput`. While no bond is configured,
`propose` returns `ForgeError::NotInitialized`: free proposals are never
accepted.

**Posting.** `propose` pulls `amount` of `token` from the proposer into
contract custody *before* any state write, then stores `bond_token`,
`bond_amount`, and `bond_state = Posted` on the proposal and increments the
running custody total (`BondHeld`). A proposer without sufficient balance
fails with `ForgeError::TokenTransferFailed` and no proposal is created.

**Settlement.** Release happens in the same frame as the single terminal
transition — there is no separate refund call, and a bond can only be
released once:

| Transition | Trigger | Bond |
|---|---|---|
| `Active → Succeeded` | majority after the deadline | stays in custody (`Posted`) |
| `Succeeded → Executed` | successful target dispatch | refunded to the proposer (`Refunded`) |
| `Active → Defeated` | no majority after the deadline | forfeited to the configured treasury (`Forfeited`) |
| any → `Cancelled` | proposer revokes the proposal | refunded to the proposer (`Refunded`) |

**Ordering and failure.** Each path validates first (proposal state,
deadline, checked custody arithmetic), then moves the token, then writes
state and emits events. A failed transfer reverts the entire invocation —
including a target dispatch that already ran — so the proposal stays
retryable and the custody total never drifts. Token failures are bucketed
as `ForgeError::TokenTransferFailed`; custody arithmetic that would cross
the `i128` boundary surfaces as `ForgeError::ArithmeticOverflow`. The
outgoing transfers need no external signer: contract self-authorization is
implicit in Soroban.

## States

- `Active` — Voting in progress
- `Succeeded` — Majority reached, passed and ready for dispatch
- `Defeated` — Quorum reached, failed
- `Executed` — On-chain action executed
- `Cancelled` — Revoked by proposer
- `Expired` — Voting ended without quorum

Each proposal additionally records `bond_state`: `Posted` (in custody),
`Refunded` (back with the proposer), or `Forfeited` (paid to the treasury).

## Events

The contract emits structured events for all state changes using the standard `proposal_id` topic pattern:

- **`Proposed`**: Emitted when a new proposal is created.
  - Topics: `proposal_id: u64`
  - Data: `data: Proposal` (the full initial proposal record)
- **`VoteCast`**: Emitted when a voter casts a vote.
  - Topics: `proposal_id: u64`
  - Data: `voter: Address`, `support: bool`
- **`Finalised`**: Emitted when a proposal transitions state during execution (e.g. `Active` → `Succeeded`, `Active` → `Defeated`, or `Succeeded` → `Executed`).
  - Topics: `proposal_id: u64`
  - Data: `state: ProposalState`, `for_votes: i128`, `against_votes: i128`
- **`BondPosted`**: Emitted by `propose` once the bond is in custody.
  - Topics: `proposal_id: u64`
  - Data: `token: Address`, `amount: i128`
- **`BondReleased`**: Emitted when the bond settles (refund or forfeit).
  - Topics: `proposal_id: u64`
  - Data: `token: Address`, `amount: i128`, `to: Address`, `forfeited: bool`

Whenever a bond moves, the bond token's own `transfer` event appears in the
same invocation. Indexers should filter events by the emitting contract
address to separate the DAO's records from the token's.

## Storage & TTL Maintenance

Proposal records (`DataKey::Proposal(u64)`) are stored in persistent storage. `DataKey::Count`, `DataKey::Bond`, `DataKey::BondHeld`, and `DataKey::Vote` entries remain in instance storage.

`propose`, `vote`, `execute`, and `cancel_proposal` extend proposal persistent storage TTL on every write to a 30-day horizon (`30 * DAY_IN_LEDGERS = 518,400` ledgers).

A permissionless public keeper entrypoint `touch_ttl(proposal_id)` allows anyone to bump a proposal's persistent TTL without modifying its state. If the proposal ID does not exist, `touch_ttl` returns `ForgeError::NotFound`.
