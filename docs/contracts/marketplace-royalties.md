# Marketplace Royalties Contract

NFT / digital asset sales with configurable royalty distribution across
secondary sales — computed by `distribute` and settled atomically in real
SEP-41 tokens by `settle_sale` (one sale) or `settle_sales` (a capped,
all-or-nothing batch).

## Interface

```rust
fn set_royalty(collection, recipient, bps) -> Result<(), ForgeError>
fn distribute(collection, seller, amount) -> Result<i128, ForgeError>
fn settle_sale(collection, token, payer, seller, amount) -> Result<Settlement, ForgeError>
fn settle_sales(collection, token, payer, sales: Vec<(seller, amount)>) -> Result<Vec<Settlement>, ForgeError>
fn get_royalty(collection) -> Result<Royalty, ForgeError>
fn get_settlement_summary(collection) -> Result<SettlementSummary, ForgeError>
fn touch_ttl(collection) -> Result<(), ForgeError>
```

## Concepts

- **Creators** receive a split of every sale, paid to the configured
  `recipient` on settlement.
- **Sellers** receive the net of every sale.
- **Payers** fund both transfers from their `token` balance in a single
  authorized call.

## Settlement

`settle_sale` moves one sale's proceeds with escrow's transfer-before-state
ordering:

1. Load the collection's configuration (`NotFound` if unregistered), validate
   `amount > 0` (`InvalidInput`), and require the collection's and the payer's
   authorization.
2. Compute `royalty_share = amount * bps / 10_000` (floored) and
   `seller_net = amount - royalty_share` with checked arithmetic
   (`ArithmeticOverflow`), and stage the updated settlement totals. The two
   parts always sum exactly to `amount` — rounding dust stays with the
   seller.
3. Transfer `seller_net` from `payer` to `seller`, then `royalty_share` from
   `payer` to the configured `recipient` — the recipient is paid last.
4. Only after both transfers succeed, commit the collection's cumulative
   settlement totals and return the `Settlement`.

A `Disabled` or zero-bps configuration skips the recipient transfer and
settles the full amount to the seller. Token failures (insufficient balance,
missing trustline, undeployed token) surface as
`ForgeError::TokenTransferFailed`, and any returned error rolls back the
whole invocation — a failed settlement can never leave the recipient
partially paid and never commits totals.

`distribute` stays a pure computation for callers that only need the net; it
moves no tokens.

### Batch settlement

`settle_sales` settles a batch of sales of one collection in one invocation
against one payer authorization, with per-sale semantics identical to
`settle_sale`:

1. Load the configuration (`NotFound` if unregistered), then validate the
   whole batch before any token moves: `sales` must be non-empty and at
   most `MAX_SETTLE_SALES` (20) long, and every `amount` must be positive
   (`InvalidInput` for either). The cap bounds one transaction's worst case
   to at most `2 * MAX_SETTLE_SALES` nested token transfers, keeping a
   batched settlement inside Soroban's per-transaction instruction budget
   and ledger bandwidth; larger sets issue several calls, each still
   atomic.
2. Require the collection's and the payer's authorization once — the
   payer's single `require_auth` covers every nested token transfer in the
   batch.
3. Compute each sale's split and the batch aggregate with checked
   arithmetic, then stage the updated settlement totals by adding the
   aggregate to the stored summary (`ArithmeticOverflow` — checked against
   the existing totals, still before any transfer).
4. Transfer each sale in order, seller first and royalty recipient last,
   skipping the recipient transfer when the share floors to zero — exactly
   the `settle_sale` order, sale after sale.
5. Only after every transfer succeeds, commit the summary once with the
   batch's aggregate deltas (`sales`, `gross_volume`, `royalties_paid`) and
   return the per-sale `Settlement`s in sale order.

Atomicity is all-or-nothing for the batch: a failure in any sale —
including a later sale's transfer after earlier sales fully succeeded —
rolls the whole invocation back, so balances and the summary are exactly as
they were before the call (no sale is half-settled).

## Compatibility

`set_royalty`, `distribute`, and `get_royalty` are unchanged. `settle_sale`,
`settle_sales`, and `get_settlement_summary` are additive; the generated
`SorobanForgeMarketplaceRoyaltiesClient` gains all three automatically, and
`Settlement`/`SettlementSummary` are shared by both settlement
entrypoints.

## Storage & TTL Maintenance

Persistent storage: `Royalty` configuration records (`DataKey::Royalty(Address)`) and `SettlementSummary` records (`DataKey::Summary(Address)`).

`set_royalty`, `settle_sale`, and `settle_sales` extend persistent storage TTL on every write to a 30-day horizon (`30 * DAY_IN_LEDGERS = 518,400` ledgers).

A permissionless public keeper entrypoint `touch_ttl(collection)` allows anyone to bump persistent storage TTL for a collection's `Royalty` and `Summary` records. If no royalty configuration exists for `collection`, `touch_ttl` returns `ForgeError::NotFound`.

## Events

The contract emits typed on-chain lifecycle events for indexers and off-chain monitoring:

- `RoyaltyConfigured` (topic: `collection: Address`) — emitted when a royalty configuration is registered or updated via `set_royalty`. Contains `recipient` and `bps`.
- `SaleSettled` (topic: `collection: Address`) — emitted on sale settlement via `settle_sale` or `settle_sales`. Contains `token`, `payer`, `seller`, `royalty_recipient`, `gross_amount`, `seller_net`, and `royalty_share`.

