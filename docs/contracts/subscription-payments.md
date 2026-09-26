# Subscription Payments Contract

Recurring, on-chain subscription billing: a subscriber authorizes a provider to
pull a fixed `amount` per `period` (seconds) through SEP-41 token transfers.

## Interface

```rust
fn subscribe(subscriber, provider, token, amount, period) -> Result<u64, ForgeError>
fn charge(subscription_id) -> Result<i128, ForgeError>
fn charge_catchup(subscription_id, max_periods: u32) -> Result<i128, ForgeError>
fn cancel(subscription_id) -> Result<(), ForgeError>
fn get_subscription(subscription_id) -> Result<Subscription, ForgeError>
fn get_subscription_count() -> u64
fn subscriptions_for_subscriber(subscriber, offset, limit) -> Result<Vec<Subscription>, ForgeError>
fn subscriptions_for_provider(provider, offset, limit) -> Result<Vec<Subscription>, ForgeError>
```

- `subscribe` requires the subscriber and returns a stable, monotonic id.
- `charge` requires the provider and settles at most one full period per call.
- `charge_catchup` requires the provider and settles `min(max_periods,
elapsed_periods)` periods in one atomic invocation. `max_periods == 0` is a
  no-op; values above the hard cap of 32 are rejected. The cap bounds Soroban
  instruction use and token outlay for keeper calls.
- `charge_catchup` refuses `PastDue`; call `charge` to use the existing retry
  policy. A failed catch-up transfer returns `TokenTransferFailed` and rolls
  back all transfers and `last_charged` through Soroban frame rollback.
- `cancel` requires the subscriber and prevents further charges.

## Subscription States

- `Active` — chargeable
- `Cancelled` — no further charges
- `PastDue` — a failed single-period payment requiring `charge` retry semantics

## Secondary Indices

Each subscription is indexed in two secondary lists, written on the `subscribe`
success path:

- `SubscriberSubscriptions(subscriber)` — creation-order subscription ids for
  which the address is the subscriber.
- `ProviderSubscriptions(provider)` — creation-order subscription ids for which
  the address is the provider.

The indices store lightweight `Vec<u64>` id lists; full `Subscription` records
are only loaded for the requested page slice.

## View Methods

- `get_subscription_count()` returns the total number of subscriptions created
  (the monotonic id counter; cancellation never lowers it).
- `subscriptions_for_subscriber(subscriber, offset, limit)` returns a page of
  `Subscription` records for the subscriber, in creation order.
- `subscriptions_for_provider(provider, offset, limit)` returns a page of
  `Subscription` records for the provider, in creation order.

Both enumeration methods are read-only (no authorization, no storage writes).
`offset` skips the first `offset` subscriptions and `limit` caps the page size:
an empty index or an out-of-bounds offset yields an empty `Vec`, and a `limit`
of `0` fails with `ForgeError::InvalidInput`.

## Storage

- `Subscription(id)` — the record for id `u64`.
- `Count` — monotonic subscription id counter.
- `SubscriberSubscriptions(address)` — subscriber id index.
- `ProviderSubscriptions(address)` — provider id index.

## Payment Semantics

`charge` returns the billed amount, or `0` when no full period has elapsed since
the last charge. `charge_catchup` returns the total billed amount and advances
`last_charged` once per successfully settled period. Period and total arithmetic
uses checked operations.

## Events

The contract emits typed on-chain lifecycle events for indexers and off-chain monitoring:

- `Subscribed` (topic: `subscription_id: u64`) — emitted when a subscription is created via `subscribe`. Contains `subscriber`, `provider`, `token`, `amount`, and `period`.
- `Charged` (topic: `subscription_id: u64`) — emitted on successful billing via `charge` or `charge_catchup`. Contains `amount`, `last_charged`, and `next_charge_at`.
- `Cancelled` (topic: `subscription_id: u64`) — emitted when a subscription is cancelled via `cancel`. Contains `subscriber`.

