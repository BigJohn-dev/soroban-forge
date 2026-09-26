# Multi-Signature Wallet Contract

Multi-owner wallet with configurable approval threshold and transaction queue.

## Interface

```rust
fn submit_transaction(proposer, destination, function_name, args) -> Result<u64, ForgeError>
fn approve(tx_id, approver) -> Result<(), ForgeError>
fn revoke(tx_id, approver) -> Result<(), ForgeError>
fn execute(tx_id) -> Result<(), ForgeError>
fn get_transaction(tx_id) -> Result<Transaction, ForgeError>
fn add_owner(owner) -> Result<(), ForgeError>
fn remove_owner(owner) -> Result<(), ForgeError>
fn update_threshold(new_threshold) -> Result<(), ForgeError>
```

## Transaction query views

The wallet exposes read-only views for loading the transaction queue without
one contract call per transaction:

```rust
fn get_transactions(offset: u32, limit: u32) -> Result<Vec<WalletTx>, ForgeError>
fn get_transactions_by_status(
    status: TxStatus,
    offset: u32,
    limit: u32,
) -> Result<Vec<WalletTx>, ForgeError>
```

Both return transactions in ascending `tx_id` order. For
`get_transactions`, `offset` is zero-based in the complete sequence (so
offset `0` starts at transaction id `1`). For
`get_transactions_by_status`, `offset` is zero-based among transactions
matching the requested status. Each returns at most `limit` records; ranges
past the end return an empty vector or the remaining records. A `limit` of
zero returns `ForgeError::InvalidInput`. An uninitialized or empty wallet
returns an empty vector for a positive limit. These views do not require
authorization and do not modify contract state.

## States

- `Pending` — Awaiting approvals
- `Approved` — Threshold reached, ready for execution
- `Executed` — Transaction completed
- `Rejected` — Revoked or expired
- `Expired` — Timed out
