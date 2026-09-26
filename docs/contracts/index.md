# Contracts Overview

| Contract              | Path                         | Status                                                                                 |
| --------------------- | ---------------------------- | -------------------------------------------------------------------------------------- |
| Escrow                | crates/escrow                | **Flagship** — real SEP-41 settlement, disputes, events, persistent storage · 27 tests |
| Vesting               | crates/vesting               | State machine + tests · **real SEP-41 settlement** via `claim`                         |
| Multi-Sig Wallet      | crates/multi-sig-wallet      | State machine + tests · no execution dispatch                                          |
| DAO Governance        | crates/dao-governance        | State machine + tests · dispatches approved actions on-chain                           |
| Subscription Payments | crates/subscription-payments | State machine + tests · atomic SEP-41 settlement via `charge` and `charge_catchup`     |
| Marketplace Royalties | crates/marketplace-royalties | State machine + tests · **atomic SEP-41 settlement** via `settle_sale`                 |
| Contract | Path | Status |
|----------|------|--------|
| Escrow | crates/escrow | **Flagship** — real SEP-41 settlement, disputes, events, persistent storage · 27 tests |
| Vesting | crates/vesting | State machine + tests · **real SEP-41 settlement** via `claim` |
| Multi-Sig Wallet | crates/multi-sig-wallet | State machine + tests · no execution dispatch |
| DAO Governance | crates/dao-governance | State machine + tests · dispatches approved actions on-chain |
| Subscription Payments | crates/subscription-payments | State machine + tests · charges nothing |
| Marketplace Royalties | crates/marketplace-royalties | State machine + tests · **atomic SEP-41 settlement** via `settle_sale` and capped batches via `settle_sales` |

Per-entrypoint detail lives in the [Feature Status Matrix](../FEATURE-STATUS.md);
the aggregate gaps (token settlement, events, storage TTL, deployments) are
documented in [Known Limitations](../KNOWN-LIMITATIONS.md).

## Adding a New Contract

1. Add a new crate under `crates/`.
2. Register it in workspace `Cargo.toml`.
3. Add a document in `docs/contracts/`.
4. Add CI checks.
5. Tag a release.
