# Escrow Contract

Secure fund custody for buyer-seller transactions with optional arbiter dispute resolution.

## Interface

```rust
fn create_escrow(buyer, seller, arbiter, token, amount, timeout) -> Result<u64, ForgeError>
fn deposit(escrow_id) -> Result<(), ForgeError>
fn release(escrow_id) -> Result<(), ForgeError>
fn release_partial(escrow_id, amount) -> Result<(), ForgeError>
fn refund(escrow_id) -> Result<(), ForgeError>
fn dispute(escrow_id, claimant) -> Result<(), ForgeError>
fn resolve(escrow_id, in_favor_of_seller) -> Result<(), ForgeError>
fn cancel(escrow_id) -> Result<(), ForgeError>
fn get_status(escrow_id) -> Result<EscrowStatus, ForgeError>
fn get_escrow(escrow_id) -> Result<EscrowData, ForgeError>
fn escrows_for_participant(participant, cursor, limit) -> ParticipantEscrowsPage
fn touch_ttl(escrow_id) -> Result<(), ForgeError>
```

## Lifecycle

```text
Pending --deposit--> Funded --release_partial (×n)--> Funded  (partial)
                    |                                  |
                    |                                  +--> Completed (final partial)
                    |        --release--> Completed (full, direct)
                    |        --refund--> Refunded   (buyer back, remaining only)
                    |        --dispute--> Disputed --resolve--> Completed | Refunded
         --cancel--> Cancelled (before funding only)
```

## States

- `Pending` — Created but not funded
- `Funded` — Funds deposited; partial releases may be applied
- `Completed` — Released to seller (full release or final partial release)
- `Refunded` — Remaining balance returned to buyer
- `Disputed` — Under arbitration (remaining balance frozen)
- `Cancelled` — Cancelled before funding

## Partial Release

`release_partial(escrow_id, amount)` allows the seller to receive the escrow
balance incrementally while the escrow remains `Funded`.

### Accounting

```text
remaining = deposited - released
deposited = amount field set at create_escrow
released  = cumulative amount transferred to seller via release_partial
```

- Each successful `release_partial` call:
  1. Validates `status == Funded`, `amount > 0`, `amount <= remaining`
  2. Transfers exactly `amount` to the seller (transfer-before-state)
  3. Increments `released` by `amount`
  4. If `amount == remaining`: transitions to `Completed`
- `release` (the original full-release entrypoint) pays the **remaining** balance
  in one call and is backward-compatible — it behaves identically to before
  when no partial releases have been made.
- `refund` and `resolve` operate on the **remaining** balance only.
- `dispute` freezes the **remaining** balance.

### Authorization

`release_partial` is seller-authorized. Invalid requests (wrong status,
non-positive amount, amount exceeding remaining) return `InvalidInput` and
do not modify any storage.

### Events

`release_partial` emits `PartiallyReleased` (not `Released`). The event
carries `partial_amount` (the incremental transfer) and the full `EscrowData`
(including updated `released` and `status`). The existing `Released` event
remains exclusively for the `release` entrypoint and signals terminal
completion to indexers.

## Storage Compatibility

`EscrowData` now carries a `released: i128` field. Records written by
earlier contract versions do not contain this field.

**Strategy**: two-type fallback decode in `load_escrow`.
1. Attempt deserialization as current `EscrowData` (10 fields including `released`).
2. On failure, attempt as `EscrowDataV1` (9 fields, no `released`).
3. Convert `EscrowDataV1` → `EscrowData` by defaulting `released = 0`.

This works because Soroban `#[contracttype]` structs are stored as XDR
symbol-keyed maps. The host rejects deserialization when the map's entry count
differs from the struct's field count; old 9-field records fail step 1 and
succeed at step 2. `EscrowDataV1` is a read-only migration type; new code
never writes it. Lazy migration: the first state-changing call on an old record
writes the current schema back.

`DataKey::Escrow(id)` is unchanged.

## WASM Budget

Current size: ~28 KB  
Limit: < 150 KB

## Feature Flags

- `test-utils` — enables test-only helpers (proptest, authz tests)
