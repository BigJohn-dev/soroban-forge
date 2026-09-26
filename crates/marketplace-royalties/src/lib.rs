#![no_std]

//! # Soroban Forge — Marketplace Royalties contract
//!
//! Enforces creator royalty splits on secondary sales: when an NFT changes
//! hands, the sale proceeds are split between the seller and one or more
//! royalty recipients according to configured basis-point rates. This
//! iteration stores one royalty configuration per collection, with a single
//! recipient; `settle_sale` moves the computed split in real SEP-41 tokens.
//!
//! Flow:
//!
//! ```text
//! set_royalty(collection, recipient, bps)   -> Active config
//! distribute(collection, seller, amount)    -> computes `amount * bps / 10_000`
//!                                              for the recipient and returns
//!                                              the net owed to the seller
//! settle_sale(collection, token, payer,
//!             seller, amount)               -> transfers the seller net, then
//!                                              the royalty share, then commits
//!                                              the settlement totals
//! settle_sales(collection, token, payer,
//!              sales: Vec<(seller, amount)>) -> validates the whole batch
//!                                              (config, cap, every amount,
//!                                              aggregate split math) before
//!                                              any token moves, runs each
//!                                              sale's seller-then-recipient
//!                                              transfers in sale order, and
//!                                              commits the aggregate totals
//!                                              exactly once
//! ```
//!
//! Authorization model:
//! - `set_royalty` requires the collection (the contract whose config this
//!   is), and `bps` must not exceed 100% (10_000 bps).
//! - `distribute` requires the collection and returns the seller's net after
//!   the configured royalty split; a `Disabled` configuration settles in full
//!   to the seller. It computes only and moves no tokens.
//! - `settle_sale` requires the collection (as `distribute` does) and the
//!   `payer`, whose balance funds both transfers; the payer's authorization
//!   covers the nested token invocations exactly as escrow's does.
//! - `settle_sales` has the same trust model as `settle_sale`: one
//!   collection authorization and one payer authorization cover every
//!   nested token transfer in the batch — no per-sale re-authorization.
//! - `get_royalty` and `get_settlement_summary` are read-only views.
//!
//! Settlement follows escrow's transfer-before-state ordering: both token
//! transfers run before any settlement state is committed, the royalty
//! recipient is paid last so a failed transfer can never leave it partially
//! paid, and token failures are bucketed into `TokenTransferFailed`. Any
//! returned error rolls the whole invocation back. `settle_sales` extends
//! that discipline to the batch: every fallible step (configuration load,
//! the [`MAX_SETTLE_SALES`] cap, per-sale `amount > 0`, the per-sale split
//! math, and the aggregate check against the stored summary) runs before
//! the first transfer, the summary is written once per call, and a failure
//! in any sale — including a later sale's transfer — reverts the entire
//! invocation, so no sale in the batch is ever half-settled. Multiple
//! recipients per collection and per-token royalties remain out of scope
//! for this iteration.

#[cfg(test)]
extern crate std;

use soroban_forge_shared_utils::ForgeError;
use soroban_sdk::{
    contract, contractclient, contractevent, contractimpl, contracttype, token, Address, Env,
};

/// Maximum number of sales one `settle_sales` invocation may settle,
/// checked **before any transfer** and reported as
/// [`ForgeError::InvalidInput`].
///
/// Rationale: each sale issues up to two cross-contract token transfers, so
/// the cap bounds one transaction's worst case to
/// `2 * MAX_SETTLE_SALES` nested token invocations plus the batch's own
/// contract work. That keeps a batched settlement comfortably inside
/// Soroban's per-transaction instruction budget and ledger bandwidth
/// (host metering, entry accesses, and event size) while still covering a
/// realistic order batch or payout sweep in a single transaction. Callers
/// with larger sets simply issue several calls; each remains atomic on its
/// own.
pub const MAX_SETTLE_SALES: u32 = 20;

/// Public interface for the Soroban Forge marketplace royalties contract.
#[contractclient(name = "SorobanForgeMarketplaceRoyaltiesClient")]
pub trait SorobanForgeMarketplaceRoyalties {
    /// Register or update the royalty recipient and basis-point rate for
    /// `collection`.
    fn set_royalty(
        env: Env,
        collection: Address,
        recipient: Address,
        bps: u32,
    ) -> Result<(), soroban_forge_shared_utils::ForgeError>;

    /// Distribute `amount` from a sale of `collection`, returning the net to
    /// the seller after royalties. Pure computation: no tokens move.
    fn distribute(
        env: Env,
        collection: Address,
        seller: Address,
        amount: i128,
    ) -> Result<i128, soroban_forge_shared_utils::ForgeError>;

    /// Settle a sale of `collection` atomically: transfer the seller's net
    /// and the royalty share from `payer` in `token`, then commit the
    /// collection's cumulative settlement totals.
    fn settle_sale(
        env: Env,
        collection: Address,
        token: Address,
        payer: Address,
        seller: Address,
        amount: i128,
    ) -> Result<Settlement, soroban_forge_shared_utils::ForgeError>;

    /// Settle up to [`MAX_SETTLE_SALES`] sales of `collection` in a single
    /// invocation against one payer authorization.
    ///
    /// Each element of `sales` is `(seller, amount)` and settles exactly as
    /// `settle_sale` settles it — identical split math, identical
    /// seller-then-recipient transfer order — with the per-sale
    /// [`Settlement`]s returned in sale order, so clients can share the
    /// `Settlement` type between both entrypoints.
    ///
    /// Atomicity is all-or-nothing for the whole batch: configuration load,
    /// the cap, every `amount > 0`, and the aggregate split math (checked
    /// against the stored summary) all run **before the first transfer**,
    /// and any later failure — including a token failure in a later sale —
    /// rolls the entire invocation back through frame rollback, leaving
    /// every balance and the summary untouched. The single payer
    /// authorization covers all nested token transfers in the batch, the
    /// same trust model as `settle_sale`. The summary is committed exactly
    /// once per call with the batch's aggregate deltas.
    fn settle_sales(
        env: Env,
        collection: Address,
        token: Address,
        payer: Address,
        sales: soroban_sdk::Vec<(Address, i128)>,
    ) -> Result<soroban_sdk::Vec<Settlement>, soroban_forge_shared_utils::ForgeError>;

    /// Read the stored royalty configuration for `collection` (read-only view).
    fn get_royalty(
        env: Env,
        collection: Address,
    ) -> Result<Royalty, soroban_forge_shared_utils::ForgeError>;

    /// Read the cumulative settlement totals for `collection` (read-only
    /// view); `NotFound` until the first successful settlement.
    fn get_settlement_summary(
        env: Env,
        collection: Address,
    ) -> Result<SettlementSummary, soroban_forge_shared_utils::ForgeError>;

    /// Permissionless keeper entrypoint: extend the persistent storage TTL of a collection's royalty configuration and settlement summary.
    ///
    /// # Errors
    ///
    /// * [`ForgeError::NotFound`] — no royalty configuration for this collection.
    fn touch_ttl(
        env: Env,
        collection: Address,
    ) -> Result<(), soroban_forge_shared_utils::ForgeError>;
}

/// Lifecycle state of a registered royalty configuration.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RoyaltyStatus {
    /// Active and applied to sales.
    Active,
    /// Disabled; sales settle to the seller in full.
    Disabled,
}

/// A royalty configuration for a single collection.
#[contracttype]
#[derive(Clone, Debug)]
pub struct Royalty {
    /// Collection (NFT contract) this configuration applies to.
    pub collection: Address,
    /// Address entitled to royalty payments.
    pub recipient: Address,
    /// Royalty rate in basis points (100 bps = 1%).
    pub bps: u32,
    /// Whether the configuration is currently enforced.
    pub status: RoyaltyStatus,
}

/// The two amounts one atomic settlement transferred: one `settle_sale`
/// invocation, or one sale of a `settle_sales` batch. Shared by both
/// entrypoints so generated clients can reuse the type.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Settlement {
    /// Amount transferred to the configured royalty recipient.
    pub royalty_share: i128,
    /// Amount transferred to the seller.
    pub seller_net: i128,
}

/// Cumulative settlement totals for one collection.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SettlementSummary {
    /// Number of sales settled so far.
    pub sales: u32,
    /// Sum of every settled sale amount.
    pub gross_volume: i128,
    /// Sum of every royalty share transferred to the recipient.
    pub royalties_paid: i128,
}

/// Persistent storage TTL constants.
///
/// One ledger closes roughly every 5 seconds, so 17,280 ledgers ≈ 1 day.
/// `BUMP_AMOUNT` is the lifetime written on every touch; `BUMP_THRESHOLD`
/// is how close to expiry an entry must be before a bump applies. The
/// 30-day horizon comfortably covers a royalty configuration between keeper
/// touches.
mod ttl {
    pub const DAY_IN_LEDGERS: u32 = 17_280;
    /// Lifetime applied on every TTL touch.
    pub const BUMP_AMOUNT: u32 = 30 * DAY_IN_LEDGERS;
    /// Bump only when the entry is within this window of expiring.
    pub const BUMP_THRESHOLD: u32 = BUMP_AMOUNT - DAY_IN_LEDGERS;
}

/// Bump a persistent entry's TTL to the [`ttl::BUMP_AMOUNT`] horizon when
/// it falls inside [`ttl::BUMP_THRESHOLD`].
fn bump_entry(env: &Env, key: &DataKey) {
    env.storage()
        .persistent()
        .extend_ttl(key, ttl::BUMP_THRESHOLD, ttl::BUMP_AMOUNT);
}

/// Persistent-storage keys.
#[contracttype]
enum DataKey {
    /// The royalty configuration for `Address` collection (persistent storage).
    Royalty(Address),
    /// The cumulative settlement totals for `Address` collection (persistent storage).
    Summary(Address),
}

/// The deployable marketplace royalties contract.
#[contract]
pub struct MarketplaceRoyalties;

#[contractimpl]
impl MarketplaceRoyalties {
    /// Register or update a royalty configuration for `collection`.
    ///
    /// Requires the collection's authorization and `bps <= 10_000`
    /// (100%). Re-registration updates the existing configuration in place.
    pub fn set_royalty(
        env: Env,
        collection: Address,
        recipient: Address,
        bps: u32,
    ) -> Result<(), ForgeError> {
        if bps > 10_000 {
            return Err(ForgeError::InvalidInput);
        }
        collection.require_auth();

        let royalty = Royalty {
            collection,
            recipient,
            bps,
            status: RoyaltyStatus::Active,
        };
        let key = DataKey::Royalty(royalty.collection.clone());
        env.storage().persistent().set(&key, &royalty);
        bump_entry(&env, &key);
        events::royalty_configured(&env, &royalty);
        Ok(())
    }

    /// Compute the royalty split for a sale.
    ///
    /// Requires the collection's authorization and `amount > 0`. Returns the
    /// net owed to `seller` after reserving `amount * bps / 10_000` for the
    /// configured recipient. A `Disabled` configuration settles in full.
    /// This entrypoint is a pure computation and moves no tokens; use
    /// `settle_sale` to transfer the split in real SEP-41 tokens.
    pub fn distribute(
        env: Env,
        collection: Address,
        seller: Address,
        amount: i128,
    ) -> Result<i128, ForgeError> {
        let royalty: Royalty = env
            .storage()
            .persistent()
            .get(&DataKey::Royalty(collection.clone()))
            .ok_or(ForgeError::NotFound)?;
        if amount <= 0 {
            return Err(ForgeError::InvalidInput);
        }
        collection.require_auth();

        let _ = &seller;
        let (_, seller_net) = split(amount, effective_bps(&royalty))?;
        Ok(seller_net)
    }

    /// Settle a sale of `collection` atomically in `token`.
    ///
    /// Requires the collection's authorization (as `distribute` does)
    /// and the `payer`'s, which covers both nested token transfers. Requires
    /// `amount > 0` and a stored configuration. The split is computed with
    /// checked arithmetic, and both transfers run **before any settlement
    /// state is committed**: the seller's net first, the royalty recipient
    /// last, so a failed transfer can never leave the royalty recipient
    /// partially paid. A `Disabled` or zero-bps configuration settles the
    /// full amount to the seller in a single transfer. Token failures are
    /// bucketed into [`ForgeError::TokenTransferFailed`], and any returned
    /// error rolls the whole invocation back — including an earlier
    /// successful transfer — so retrying after a failure never double-pays.
    /// Use `settle_sales` to settle a batch of sales in one invocation.
    pub fn settle_sale(
        env: Env,
        collection: Address,
        token: Address,
        payer: Address,
        seller: Address,
        amount: i128,
    ) -> Result<Settlement, ForgeError> {
        let royalty: Royalty = env
            .storage()
            .persistent()
            .get(&DataKey::Royalty(collection.clone()))
            .ok_or(ForgeError::NotFound)?;
        if amount <= 0 {
            return Err(ForgeError::InvalidInput);
        }
        collection.require_auth();
        payer.require_auth();

        // Every fallible computation runs before the first transfer, so an
        // arithmetic failure can never strand funds mid-settlement.
        let (royalty_share, seller_net) = split(amount, effective_bps(&royalty))?;
        let summary = next_summary(&env, &collection, 1, amount, royalty_share)?;

        // Transfer-before-state (escrow pattern): the seller is paid first
        // and the royalty recipient last, so the protected party is only
        // ever paid when everything before it already succeeded.
        if seller_net > 0 {
            transfer(&env, &token, &payer, &seller, seller_net)?;
        }
        if royalty_share > 0 {
            transfer(&env, &token, &payer, &royalty.recipient, royalty_share)?;
        }

        // Both transfers succeeded; only now commit settlement state.
        let summary_key = DataKey::Summary(collection.clone());
        let royalty_key = DataKey::Royalty(collection.clone());
        env.storage().persistent().set(&summary_key, &summary);
        bump_entry(&env, &royalty_key);
        bump_entry(&env, &summary_key);
        events::sale_settled(
            &env,
            &collection,
            &token,
            &payer,
            &seller,
            &royalty.recipient,
            amount,
            seller_net,
            royalty_share,
        );

        Ok(Settlement {
            royalty_share,
            seller_net,
        })
    }

    /// Settle a batch of sales of `collection` atomically in `token`.
    ///
    /// Same trust model as `settle_sale`: the collection's authorization and
    /// the `payer`'s are each required once, and the payer's single
    /// authorization covers **every** nested token transfer in the batch —
    /// there is no per-sale re-authorization.
    ///
    /// Validation runs to completion before the first transfer: the
    /// configuration must exist ([`ForgeError::NotFound`]), `sales` must be
    /// non-empty and at most [`MAX_SETTLE_SALES`] long
    /// ([`ForgeError::InvalidInput`]), every `amount` must be positive
    /// ([`ForgeError::InvalidInput`]), and every split plus the batch
    /// aggregate — including the checked add against the stored summary —
    /// must be overflow-free ([`ForgeError::ArithmeticOverflow`]). Only
    /// then do transfers run, in sale order, each sale seller-then-recipient
    /// with the recipient skipped when its share is zero, exactly as
    /// `settle_sale` does. A failure in any sale rolls the whole invocation
    /// back: no sale is half-settled, no balance moves, and the summary is
    /// untouched. On success the summary is committed exactly once with the
    /// batch's aggregate deltas and the per-sale [`Settlement`]s come back
    /// in sale order.
    pub fn settle_sales(
        env: Env,
        collection: Address,
        token: Address,
        payer: Address,
        sales: soroban_sdk::Vec<(Address, i128)>,
    ) -> Result<soroban_sdk::Vec<Settlement>, ForgeError> {
        let royalty: Royalty = env
            .storage()
            .persistent()
            .get(&DataKey::Royalty(collection.clone()))
            .ok_or(ForgeError::NotFound)?;
        let count = sales.len();
        if count == 0 || count > MAX_SETTLE_SALES {
            return Err(ForgeError::InvalidInput);
        }
        for (_, amount) in sales.iter() {
            if amount <= 0 {
                return Err(ForgeError::InvalidInput);
            }
        }
        collection.require_auth();
        payer.require_auth();

        // Every fallible computation for the whole batch runs before the
        // first transfer: per-sale split math, the aggregate deltas, and the
        // checked add against the stored summary — so an arithmetic failure
        // anywhere in the batch can never strand funds mid-settlement.
        let bps = effective_bps(&royalty);
        let mut settlements = soroban_sdk::Vec::new(&env);
        let mut gross_volume: i128 = 0;
        let mut royalties_paid: i128 = 0;
        for (_, amount) in sales.iter() {
            let (royalty_share, seller_net) = split(amount, bps)?;
            gross_volume = gross_volume
                .checked_add(amount)
                .ok_or(ForgeError::ArithmeticOverflow)?;
            royalties_paid = royalties_paid
                .checked_add(royalty_share)
                .ok_or(ForgeError::ArithmeticOverflow)?;
            settlements.push_back(Settlement {
                royalty_share,
                seller_net,
            });
        }
        let summary = next_summary(&env, &collection, count, gross_volume, royalties_paid)?;

        // Transfer-before-state (escrow pattern), per sale in sale order:
        // each sale pays its seller first and the royalty recipient last,
        // and the summary is still not committed — a failure in any sale,
        // including a later one, rolls the entire invocation back.
        for i in 0..count {
            // Both `get`s are in range by construction: `sales` has `count`
            // entries and `settlements` was built one-for-one from it.
            let (seller, _) = sales.get(i).ok_or(ForgeError::InvalidInput)?;
            let settlement = settlements.get(i).ok_or(ForgeError::InvalidInput)?;
            if settlement.seller_net > 0 {
                transfer(&env, &token, &payer, &seller, settlement.seller_net)?;
            }
            if settlement.royalty_share > 0 {
                transfer(
                    &env,
                    &token,
                    &payer,
                    &royalty.recipient,
                    settlement.royalty_share,
                )?;
            }
        }

        // Every transfer succeeded; only now commit settlement state, once.
        let summary_key = DataKey::Summary(collection.clone());
        let royalty_key = DataKey::Royalty(collection);
        env.storage().persistent().set(&summary_key, &summary);
        bump_entry(&env, &royalty_key);
        bump_entry(&env, &summary_key);

        Ok(settlements)
    }

    /// Read the stored royalty configuration for `collection` (read-only view).
    pub fn get_royalty(env: Env, collection: Address) -> Result<Royalty, ForgeError> {
        env.storage()
            .persistent()
            .get(&DataKey::Royalty(collection))
            .ok_or(ForgeError::NotFound)
    }

    /// Read the cumulative settlement totals for `collection` (read-only
    /// view). `NotFound` until the collection settles its first sale.
    pub fn get_settlement_summary(
        env: Env,
        collection: Address,
    ) -> Result<SettlementSummary, ForgeError> {
        env.storage()
            .persistent()
            .get(&DataKey::Summary(collection))
            .ok_or(ForgeError::NotFound)
    }

    /// Permissionless keeper: bump the royalty and summary entries' TTL without changing
    /// any state.
    ///
    /// Returns `ForgeError::NotFound` if no royalty configuration exists for `collection`.
    pub fn touch_ttl(env: Env, collection: Address) -> Result<(), ForgeError> {
        let royalty_key = DataKey::Royalty(collection.clone());
        if !env.storage().persistent().has(&royalty_key) {
            return Err(ForgeError::NotFound);
        }
        bump_entry(&env, &royalty_key);
        let summary_key = DataKey::Summary(collection);
        if env.storage().persistent().has(&summary_key) {
            bump_entry(&env, &summary_key);
        }
        Ok(())
    }
}

/// The rate actually applied to sales under `royalty`: zero once the
/// configuration is disabled, so disabled collections settle in full.
fn effective_bps(royalty: &Royalty) -> u32 {
    match royalty.status {
        RoyaltyStatus::Active => royalty.bps,
        RoyaltyStatus::Disabled => 0,
    }
}

/// Split `amount` into `(royalty_share, seller_net)` at `bps` using checked
/// arithmetic. The share floors (`amount * bps / 10_000`) and the remainder
/// stays with the seller, so `royalty_share + seller_net == amount` exactly;
/// `bps <= 10_000` guarantees the share never exceeds the amount. Overflow
/// surfaces as [`ForgeError::ArithmeticOverflow`] before any transfer runs.
fn split(amount: i128, bps: u32) -> Result<(i128, i128), ForgeError> {
    let royalty_share = amount
        .checked_mul(bps as i128)
        .ok_or(ForgeError::ArithmeticOverflow)?
        / 10_000;
    let seller_net = amount
        .checked_sub(royalty_share)
        .ok_or(ForgeError::ArithmeticOverflow)?;
    Ok((royalty_share, seller_net))
}

/// Read the collection's current totals with a batch delta added checked,
/// returning the record **without** writing it. The delta is the aggregate
/// of a whole call: `sales` sales summing to `gross_volume` with
/// `royalties_paid` reserved for the recipient (`1`, `amount`, and
/// `royalty_share` for a single `settle_sale`). Runs before any transfer so
/// an overflow aborts the settlement while no funds moved; the caller
/// commits the returned record exactly once, after every transfer
/// succeeded.
fn next_summary(
    env: &Env,
    collection: &Address,
    sales: u32,
    gross_volume: i128,
    royalties_paid: i128,
) -> Result<SettlementSummary, ForgeError> {
    let current: Option<SettlementSummary> = env
        .storage()
        .persistent()
        .get(&DataKey::Summary(collection.clone()));
    let summary = match current {
        None => SettlementSummary {
            sales,
            gross_volume,
            royalties_paid,
        },
        Some(totals) => SettlementSummary {
            sales: totals
                .sales
                .checked_add(sales)
                .ok_or(ForgeError::ArithmeticOverflow)?,
            gross_volume: totals
                .gross_volume
                .checked_add(gross_volume)
                .ok_or(ForgeError::ArithmeticOverflow)?,
            royalties_paid: totals
                .royalties_paid
                .checked_add(royalties_paid)
                .ok_or(ForgeError::ArithmeticOverflow)?,
        },
    };
    Ok(summary)
}

/// Move `amount` of `token` from `from` to `to`.
///
/// Same typed-error bucketing as escrow: a client receiving
/// `Error(Contract, #N)` cannot know whether `N` came from the token or this
/// contract, so every token-side failure collapses into
/// [`ForgeError::TokenTransferFailed`] and the raw discriminant is
/// discarded; the root cause remains visible in the transaction's
/// diagnostic events. The payer's authorization on the calling entrypoint
/// covers the nested token invocation — no allowance is needed for a
/// `transfer` pull when the holder authorizes the call.
fn transfer(
    env: &Env,
    token: &Address,
    from: &Address,
    to: &Address,
    amount: i128,
) -> Result<(), ForgeError> {
    match token::TokenClient::new(env, token).try_transfer(from, to, &amount) {
        Ok(Ok(())) => Ok(()),
        // Token returned a typed error (insufficient balance, missing
        // trustline, custom token logic) or the host aborted (most commonly
        // an undeployed token address).
        _ => Err(ForgeError::TokenTransferFailed),
    }
}

/// Lifecycle events emitted by the marketplace royalties contract.
mod events {
    use super::*;

    #[contractevent]
    pub struct RoyaltyConfigured {
        #[topic]
        pub collection: Address,
        pub recipient: Address,
        pub bps: u32,
    }

    #[contractevent]
    pub struct SaleSettled {
        #[topic]
        pub collection: Address,
        pub token: Address,
        pub payer: Address,
        pub seller: Address,
        pub royalty_recipient: Address,
        pub gross_amount: i128,
        pub seller_net: i128,
        pub royalty_share: i128,
    }

    pub fn royalty_configured(env: &Env, royalty: &Royalty) {
        RoyaltyConfigured {
            collection: royalty.collection.clone(),
            recipient: royalty.recipient.clone(),
            bps: royalty.bps,
        }
        .publish(env);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn sale_settled(
        env: &Env,
        collection: &Address,
        token: &Address,
        payer: &Address,
        seller: &Address,
        royalty_recipient: &Address,
        gross_amount: i128,
        seller_net: i128,
        royalty_share: i128,
    ) {
        SaleSettled {
            collection: collection.clone(),
            token: token.clone(),
            payer: payer.clone(),
            seller: seller.clone(),
            royalty_recipient: royalty_recipient.clone(),
            gross_amount,
            seller_net,
            royalty_share,
        }
        .publish(env);
    }
}

#[cfg(test)]
mod authz;
#[cfg(test)]
mod props;

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_forge_test_utils::TestAccounts;
    use soroban_sdk::testutils::{Address as _, Events as _};
    use soroban_sdk::token::{Client as TokenClient, StellarAssetClient};
    use soroban_sdk::Env;

    /// Build a fresh env with mocked auths, a registered contract, a registered
    /// royalty config (500 bps = 5%), and named accounts.
    macro_rules! setup {
        () => {{
            let env = Env::default();
            env.mock_all_auths();
            let contract_id = env.register(MarketplaceRoyalties, ());
            let client = SorobanForgeMarketplaceRoyaltiesClient::new(&env, &contract_id);
            let accounts = TestAccounts::generate(&env);
            client.set_royalty(&accounts.arbiter, &accounts.user2, &500_u32);
            (env, client, accounts)
        }};
    }

    /// Settlement variant of `setup!`: additionally registers a real SEP-41
    /// (Stellar Asset Contract) token and mints 1_000 units to the payer, so
    /// every settlement test asserts actual balance movement. Layout:
    /// `arbiter` is the collection, `user2` the royalty recipient (500 bps),
    /// `user3` the seller, and `user1` the payer.
    macro_rules! setup_settlement {
        () => {{
            let env = Env::default();
            env.mock_all_auths();

            let admin = Address::generate(&env);
            let sac = env.register_stellar_asset_contract_v2(admin);
            let token = sac.address();
            let token_admin = StellarAssetClient::new(&env, &token);
            let token_client = TokenClient::new(&env, &token);

            let contract_id = env.register(MarketplaceRoyalties, ());
            let client = SorobanForgeMarketplaceRoyaltiesClient::new(&env, &contract_id);
            let accounts = TestAccounts::generate(&env);
            client.set_royalty(&accounts.arbiter, &accounts.user2, &500_u32);
            token_admin.mint(&accounts.user1, &1_000_i128);

            (env, token, token_client, contract_id, client, accounts)
        }};
    }

    #[test]
    fn set_royalty_stores_config() {
        let (_env, client, accounts) = setup!();
        let royalty = client.get_royalty(&accounts.arbiter);
        assert_eq!(royalty.recipient, accounts.user2);
        assert_eq!(royalty.bps, 500);
        assert_eq!(royalty.status, RoyaltyStatus::Active);
    }

    #[test]
    fn set_royalty_update_in_place() {
        let (_env, client, accounts) = setup!();
        client.set_royalty(&accounts.arbiter, &accounts.user3, &1_000_u32);
        let royalty = client.get_royalty(&accounts.arbiter);
        assert_eq!(royalty.recipient, accounts.user3);
        assert_eq!(royalty.bps, 1_000);
    }

    #[test]
    fn set_royalty_rejects_bps_over_100_percent() {
        let (_env, client, accounts) = setup!();
        let err = client
            .try_set_royalty(&accounts.arbiter, &accounts.user2, &10_001_u32)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn distribute_pays_royalty_and_returns_net() {
        let (_env, client, accounts) = setup!();
        // 1000 units sold with a 5% royalty -> 50 to the recipient, 950 net.
        let net = client.distribute(&accounts.arbiter, &accounts.user1, &1_000_i128);
        assert_eq!(net, 950);
    }

    #[test]
    fn distribute_zero_bps_returns_full_amount() {
        let (_env, client, accounts) = setup!();
        client.set_royalty(&accounts.arbiter, &accounts.user2, &0_u32);
        let net = client.distribute(&accounts.arbiter, &accounts.user1, &1_000_i128);
        assert_eq!(net, 1_000);
    }

    #[test]
    fn distribute_100_percent_returns_zero_net() {
        let (_env, client, accounts) = setup!();
        client.set_royalty(&accounts.arbiter, &accounts.user2, &10_000_u32);
        let net = client.distribute(&accounts.arbiter, &accounts.user1, &1_000_i128);
        assert_eq!(net, 0);
    }

    #[test]
    fn distribute_rejects_non_positive_amount() {
        let (_env, client, accounts) = setup!();
        let err = client
            .try_distribute(&accounts.arbiter, &accounts.user1, &0_i128)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn distribute_missing_config_is_not_found() {
        let (_env, client, accounts) = setup!();
        // An unregistered collection has no config.
        let err = client
            .try_distribute(&accounts.validator, &accounts.user1, &1_000_i128)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::NotFound);
    }

    #[test]
    fn get_royalty_missing_is_not_found() {
        let (_env, client, accounts) = setup!();
        let err = client
            .try_get_royalty(&accounts.validator)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::NotFound);
    }

    #[test]
    fn distribute_disabled_collection_settles_in_full() {
        let (_env, client, accounts) = setup!();
        // A re-registration with bps 0 keeps the config `Active`; emulate a
        // disabled state by checking that a zero-bps config settles in full.
        client.set_royalty(&accounts.arbiter, &accounts.user2, &0_u32);
        assert_eq!(
            client.get_royalty(&accounts.arbiter).status,
            RoyaltyStatus::Active
        );
        let net = client.distribute(&accounts.arbiter, &accounts.user1, &1_000_i128);
        assert_eq!(net, 1_000);
    }

    // -------------------------------------------------------------------
    // settle_sale — atomic SEP-41 settlement against a real token
    // -------------------------------------------------------------------

    #[test]
    fn settle_sale_pays_seller_and_recipient_and_records_totals() {
        let (_env, token, tc, contract_id, client, accounts) = setup_settlement!();
        let payer = &accounts.user1;
        let recipient = &accounts.user2;
        let seller = &accounts.user3;
        let collection = &accounts.arbiter;

        let settled = client.settle_sale(collection, &token, payer, seller, &1_000_i128);

        // 5% of 1000 = 50 for the recipient, 950 for the seller — exact.
        assert_eq!(settled.royalty_share, 50);
        assert_eq!(settled.seller_net, 950);
        assert_eq!(settled.royalty_share + settled.seller_net, 1_000);
        assert_eq!(tc.balance(payer), 0);
        assert_eq!(tc.balance(seller), 950);
        assert_eq!(tc.balance(recipient), 50);
        // The contract settles through and never retains funds.
        assert_eq!(tc.balance(&contract_id), 0);

        let summary = client.get_settlement_summary(collection);
        assert_eq!(summary.sales, 1);
        assert_eq!(summary.gross_volume, 1_000);
        assert_eq!(summary.royalties_paid, 50);
    }

    #[test]
    fn settle_accumulates_totals_across_sales() {
        let (env, token, _tc, _contract_id, client, accounts) = setup_settlement!();
        let payer = &accounts.user1;
        let seller = &accounts.user3;
        let collection = &accounts.arbiter;

        client.settle_sale(collection, &token, payer, seller, &1_000_i128);
        StellarAssetClient::new(&env, &token).mint(payer, &1_000_i128);
        client.settle_sale(collection, &token, payer, seller, &1_000_i128);

        let summary = client.get_settlement_summary(collection);
        assert_eq!(summary.sales, 2);
        assert_eq!(summary.gross_volume, 2_000);
        assert_eq!(summary.royalties_paid, 100);
    }

    #[test]
    fn settle_zero_bps_settles_in_full_to_seller() {
        let (_env, token, tc, _contract_id, client, accounts) = setup_settlement!();
        let payer = &accounts.user1;
        let recipient = &accounts.user2;
        let seller = &accounts.user3;
        let collection = &accounts.arbiter;
        client.set_royalty(collection, recipient, &0_u32);

        let settled = client.settle_sale(collection, &token, payer, seller, &1_000_i128);

        assert_eq!(settled.royalty_share, 0);
        assert_eq!(settled.seller_net, 1_000);
        assert_eq!(tc.balance(seller), 1_000);
        assert_eq!(tc.balance(recipient), 0);
        assert_eq!(client.get_settlement_summary(collection).royalties_paid, 0);
    }

    #[test]
    fn settle_disabled_config_settles_in_full_to_seller() {
        let (env, token, tc, contract_id, client, accounts) = setup_settlement!();
        let payer = &accounts.user1;
        let recipient = &accounts.user2;
        let seller = &accounts.user3;
        let collection = &accounts.arbiter;

        // `set_royalty` has no public "disable" switch (it always stores
        // `Active`), so write the `Disabled` record directly — the same gap
        // the compute-only `distribute` suite documents.
        let disabled = Royalty {
            collection: collection.clone(),
            recipient: recipient.clone(),
            bps: 500,
            status: RoyaltyStatus::Disabled,
        };
        env.as_contract(&contract_id, || {
            env.storage()
                .persistent()
                .set(&DataKey::Royalty(collection.clone()), &disabled);
        });

        let settled = client.settle_sale(collection, &token, payer, seller, &1_000_i128);

        assert_eq!(settled.royalty_share, 0);
        assert_eq!(settled.seller_net, 1_000);
        assert_eq!(tc.balance(seller), 1_000);
        assert_eq!(tc.balance(recipient), 0);
    }

    #[test]
    fn settle_100_percent_settles_in_full_to_recipient() {
        let (_env, token, tc, _contract_id, client, accounts) = setup_settlement!();
        let payer = &accounts.user1;
        let recipient = &accounts.user2;
        let seller = &accounts.user3;
        let collection = &accounts.arbiter;
        client.set_royalty(collection, recipient, &10_000_u32);

        let settled = client.settle_sale(collection, &token, payer, seller, &1_000_i128);

        assert_eq!(settled.royalty_share, 1_000);
        assert_eq!(settled.seller_net, 0);
        assert_eq!(tc.balance(recipient), 1_000);
        assert_eq!(tc.balance(seller), 0);
    }

    #[test]
    fn settle_rejects_non_positive_amount() {
        let (_env, token, tc, _contract_id, client, accounts) = setup_settlement!();
        let payer = &accounts.user1;
        let seller = &accounts.user3;
        let collection = &accounts.arbiter;

        for amount in [0_i128, -100] {
            let err = client
                .try_settle_sale(collection, &token, payer, seller, &amount)
                .unwrap_err()
                .unwrap();
            assert_eq!(err, ForgeError::InvalidInput);
        }
        assert_eq!(tc.balance(payer), 1_000);
    }

    #[test]
    fn settle_missing_config_is_not_found() {
        let (_env, token, tc, _contract_id, client, accounts) = setup_settlement!();
        let payer = &accounts.user1;
        let seller = &accounts.user3;
        let unregistered = &accounts.validator; // never configured

        let err = client
            .try_settle_sale(unregistered, &token, payer, seller, &1_000_i128)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::NotFound);
        assert_eq!(tc.balance(payer), 1_000);
        assert_eq!(tc.balance(seller), 0);
    }

    #[test]
    fn settle_overflow_fails_before_any_transfer() {
        let (_env, token, tc, _contract_id, client, accounts) = setup_settlement!();
        let payer = &accounts.user1;
        let recipient = &accounts.user2;
        let seller = &accounts.user3;
        let collection = &accounts.arbiter;

        // i128::MAX * 500 bps overflows the checked multiply.
        let err = client
            .try_settle_sale(collection, &token, payer, seller, &i128::MAX)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::ArithmeticOverflow);
        assert_eq!(tc.balance(payer), 1_000);
        assert_eq!(tc.balance(seller), 0);
        assert_eq!(tc.balance(recipient), 0);
        assert_eq!(
            client
                .try_get_settlement_summary(collection)
                .unwrap_err()
                .unwrap(),
            ForgeError::NotFound
        );
    }

    #[test]
    fn settle_with_insufficient_balance_pays_nobody() {
        let (_env, token, tc, _contract_id, client, accounts) = setup_settlement!();
        let payer = &accounts.user1;
        let recipient = &accounts.user2;
        let seller = &accounts.user3;
        let collection = &accounts.arbiter;

        // The payer holds 1_000; a 10_000 sale needs 9_500 for the seller
        // first, so the very first transfer already fails at the token.
        let err = client
            .try_settle_sale(collection, &token, payer, seller, &10_000_i128)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::TokenTransferFailed);
        assert_eq!(tc.balance(payer), 1_000);
        assert_eq!(tc.balance(seller), 0);
        assert_eq!(tc.balance(recipient), 0);
        assert_eq!(
            client
                .try_get_settlement_summary(collection)
                .unwrap_err()
                .unwrap(),
            ForgeError::NotFound
        );
    }

    #[test]
    fn settle_with_undeployed_token_fails_and_moves_nothing() {
        let (env, _token, tc, _contract_id, client, accounts) = setup_settlement!();
        let payer = &accounts.user1;
        let recipient = &accounts.user2;
        let seller = &accounts.user3;
        let collection = &accounts.arbiter;
        let not_a_token = Address::generate(&env);

        // The host aborts inside `try_transfer`; the failure is bucketed
        // exactly like escrow's undeployed-token path.
        let err = client
            .try_settle_sale(collection, &not_a_token, payer, seller, &1_000_i128)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::TokenTransferFailed);
        // The real token's balances are untouched.
        assert_eq!(tc.balance(payer), 1_000);
        assert_eq!(tc.balance(seller), 0);
        assert_eq!(tc.balance(recipient), 0);
        assert_eq!(
            client
                .try_get_settlement_summary(collection)
                .unwrap_err()
                .unwrap(),
            ForgeError::NotFound
        );
    }

    #[test]
    fn settle_never_leaves_the_recipient_partially_paid() {
        let (_env, token, tc, _contract_id, client, accounts) = setup_settlement!();
        let payer = &accounts.user1;
        let recipient = &accounts.user2;
        let seller = &accounts.user3;
        let collection = &accounts.arbiter;

        // amount 1_030 -> share 51, net 979. The payer's 1_000 covers the
        // seller's transfer but leaves only 21 — not enough for the
        // recipient's 51 — so the SECOND transfer fails after the first
        // succeeded. The recipient must end up with nothing: no partial
        // payment, no settlement state, and the invocation rollback
        // restores the payer's and seller's balances.
        let err = client
            .try_settle_sale(collection, &token, payer, seller, &1_030_i128)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::TokenTransferFailed);
        assert_eq!(
            tc.balance(recipient),
            0,
            "recipient must never be partially paid"
        );
        assert_eq!(tc.balance(payer), 1_000, "rollback restores the payer");
        assert_eq!(tc.balance(seller), 0, "rollback restores the seller");
        assert_eq!(
            client
                .try_get_settlement_summary(collection)
                .unwrap_err()
                .unwrap(),
            ForgeError::NotFound,
            "no settlement state is committed on failure"
        );
    }

    // -------------------------------------------------------------------
    // settle_sales — atomic batch settlement against a real token
    // -------------------------------------------------------------------

    /// Build a `settle_sales` batch from `(seller, amount)` pairs.
    fn sales_of(env: &Env, entries: &[(Address, i128)]) -> soroban_sdk::Vec<(Address, i128)> {
        let mut batch = soroban_sdk::Vec::new(env);
        for (seller, amount) in entries {
            batch.push_back((seller.clone(), *amount));
        }
        batch
    }

    #[test]
    fn settle_sales_pays_every_sale_and_commits_the_summary_once() {
        let (env, token, tc, contract_id, client, accounts) = setup_settlement!();
        let payer = &accounts.user1;
        let recipient = &accounts.user2;
        let seller_a = &accounts.user3;
        let seller_b = &accounts.validator;
        let seller_c = &accounts.deployer;
        let collection = &accounts.arbiter;

        // 5% royalty. The middle sale (19) floors to a zero royalty share,
        // so a zero-share sale rides along inside a paid batch.
        let batch = sales_of(
            &env,
            &[
                (seller_a.clone(), 400),
                (seller_b.clone(), 19),
                (seller_c.clone(), 300),
            ],
        );
        let settled = client.settle_sales(collection, &token, payer, &batch);

        assert_eq!(settled.len(), 3);
        assert_eq!(
            settled.get(0).unwrap(),
            Settlement {
                royalty_share: 20,
                seller_net: 380
            }
        );
        assert_eq!(
            settled.get(1).unwrap(),
            Settlement {
                royalty_share: 0,
                seller_net: 19
            }
        );
        assert_eq!(
            settled.get(2).unwrap(),
            Settlement {
                royalty_share: 15,
                seller_net: 285
            }
        );
        // Per-sale conservation, exactly as `settle_sale` guarantees it.
        for (i, amount) in [400_i128, 19, 300].into_iter().enumerate() {
            let sale = settled.get(i as u32).unwrap();
            assert_eq!(sale.royalty_share + sale.seller_net, amount);
        }

        assert_eq!(tc.balance(payer), 1_000 - 719, "payer funds every sale");
        assert_eq!(tc.balance(seller_a), 380);
        assert_eq!(tc.balance(seller_b), 19, "zero-share sale pays seller only");
        assert_eq!(tc.balance(seller_c), 285);
        assert_eq!(tc.balance(recipient), 35, "20 + 0 + 15");
        // The contract settles through and never retains funds.
        assert_eq!(tc.balance(&contract_id), 0);

        // Committed exactly once, as the aggregate of the per-sale deltas.
        let summary = client.get_settlement_summary(collection);
        assert_eq!(summary.sales, 3);
        assert_eq!(summary.gross_volume, 719);
        assert_eq!(summary.royalties_paid, 35);
    }

    #[test]
    fn settle_sales_matches_single_sale_splits_and_totals() {
        let (env_a, token_a, tc_a, _contract_id_a, client_a, accounts_a) = setup_settlement!();
        let (env_b, token_b, tc_b, _contract_id_b, client_b, accounts_b) = setup_settlement!();
        StellarAssetClient::new(&env_a, &token_a).mint(&accounts_a.user1, &10_000_i128);
        StellarAssetClient::new(&env_b, &token_b).mint(&accounts_b.user1, &10_000_i128);

        let amounts = [977_i128, 31, 651];

        // Same inputs, one sale per invocation...
        let mut single = soroban_sdk::Vec::new(&env_a);
        for (i, amount) in amounts.iter().enumerate() {
            let seller = match i {
                0 => &accounts_a.user3,
                1 => &accounts_a.validator,
                _ => &accounts_a.deployer,
            };
            single.push_back(client_a.settle_sale(
                &accounts_a.arbiter,
                &token_a,
                &accounts_a.user1,
                seller,
                amount,
            ));
        }

        // ...and the same inputs as one batch.
        let batch = sales_of(
            &env_b,
            &[
                (accounts_b.user3.clone(), amounts[0]),
                (accounts_b.validator.clone(), amounts[1]),
                (accounts_b.deployer.clone(), amounts[2]),
            ],
        );
        let settled =
            client_b.settle_sales(&accounts_b.arbiter, &token_b, &accounts_b.user1, &batch);

        // Per-sale split math is identical for identical inputs.
        assert_eq!(settled, single);
        // So are the cumulative totals...
        assert_eq!(
            client_b.get_settlement_summary(&accounts_b.arbiter),
            client_a.get_settlement_summary(&accounts_a.arbiter),
        );
        // ...and every balance.
        for (a, b) in [
            (&accounts_a.user1, &accounts_b.user1),
            (&accounts_a.user2, &accounts_b.user2),
            (&accounts_a.user3, &accounts_b.user3),
            (&accounts_a.validator, &accounts_b.validator),
            (&accounts_a.deployer, &accounts_b.deployer),
        ] {
            assert_eq!(tc_a.balance(a), tc_b.balance(b));
        }
    }

    #[test]
    fn settle_sales_accumulates_totals_across_calls() {
        let (env, token, tc, _contract_id, client, accounts) = setup_settlement!();
        let payer = &accounts.user1;
        let recipient = &accounts.user2;
        let seller_a = &accounts.user3;
        let seller_b = &accounts.validator;
        let collection = &accounts.arbiter;

        let batch = sales_of(&env, &[(seller_a.clone(), 200), (seller_b.clone(), 100)]);
        client.settle_sales(collection, &token, payer, &batch);
        client.settle_sale(collection, &token, payer, seller_a, &400_i128);

        let summary = client.get_settlement_summary(collection);
        assert_eq!(summary.sales, 3);
        assert_eq!(summary.gross_volume, 700);
        assert_eq!(summary.royalties_paid, 35); // 10 + 5 + 20

        assert_eq!(tc.balance(payer), 1_000 - 700);
        assert_eq!(tc.balance(seller_a), 190 + 380);
        assert_eq!(tc.balance(seller_b), 95);
        assert_eq!(tc.balance(recipient), 35);
    }

    #[test]
    fn settle_sales_at_cap_settles_the_whole_batch() {
        let (env, token, tc, _contract_id, client, accounts) = setup_settlement!();
        let payer = &accounts.user1;
        let recipient = &accounts.user2;
        let seller_a = &accounts.user3;
        let seller_b = &accounts.validator;
        let collection = &accounts.arbiter;

        // Exactly `MAX_SETTLE_SALES` sales of 40: share 2, net 38 each.
        let entries: std::vec::Vec<(Address, i128)> = (0..MAX_SETTLE_SALES)
            .map(|i| {
                let seller = if i % 2 == 0 {
                    seller_a.clone()
                } else {
                    seller_b.clone()
                };
                (seller, 40_i128)
            })
            .collect();
        let batch = sales_of(&env, &entries);
        assert_eq!(batch.len(), MAX_SETTLE_SALES);

        let settled = client.settle_sales(collection, &token, payer, &batch);

        assert_eq!(settled.len(), MAX_SETTLE_SALES);
        assert_eq!(tc.balance(payer), 1_000 - 800);
        assert_eq!(tc.balance(seller_a), 10 * 38, "even-indexed sales");
        assert_eq!(tc.balance(seller_b), 10 * 38, "odd-indexed sales");
        assert_eq!(tc.balance(recipient), 40, "2 per sale");
        let summary = client.get_settlement_summary(collection);
        assert_eq!(summary.sales, MAX_SETTLE_SALES);
        assert_eq!(summary.gross_volume, 800);
        assert_eq!(summary.royalties_paid, 40);
    }

    #[test]
    fn settle_sales_over_cap_rejects_before_any_transfer() {
        let (env, token, tc, _contract_id, client, accounts) = setup_settlement!();
        let payer = &accounts.user1;
        let recipient = &accounts.user2;
        let seller = &accounts.user3;
        let collection = &accounts.arbiter;

        let entries: std::vec::Vec<(Address, i128)> = (0..=MAX_SETTLE_SALES)
            .map(|_| (seller.clone(), 10_i128))
            .collect();
        let batch = sales_of(&env, &entries);
        assert_eq!(batch.len(), MAX_SETTLE_SALES + 1);

        let err = client
            .try_settle_sales(collection, &token, payer, &batch)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
        // The cap is checked before any token moves.
        assert_eq!(tc.balance(payer), 1_000);
        assert_eq!(tc.balance(seller), 0);
        assert_eq!(tc.balance(recipient), 0);
        assert_eq!(
            client
                .try_get_settlement_summary(collection)
                .unwrap_err()
                .unwrap(),
            ForgeError::NotFound
        );
    }

    #[test]
    fn settle_sales_rejects_empty_batch() {
        let (env, token, tc, _contract_id, client, accounts) = setup_settlement!();
        let payer = &accounts.user1;
        let collection = &accounts.arbiter;

        let err = client
            .try_settle_sales(collection, &token, payer, &soroban_sdk::Vec::new(&env))
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
        assert_eq!(tc.balance(payer), 1_000);
        assert_eq!(
            client
                .try_get_settlement_summary(collection)
                .unwrap_err()
                .unwrap(),
            ForgeError::NotFound
        );
    }

    #[test]
    fn settle_sales_rejects_non_positive_amount_mid_batch() {
        let (env, token, tc, _contract_id, client, accounts) = setup_settlement!();
        let payer = &accounts.user1;
        let recipient = &accounts.user2;
        let seller_a = &accounts.user3;
        let seller_b = &accounts.validator;
        let collection = &accounts.arbiter;

        // The first sale is valid on its own; the invalid second sale must
        // abort the batch before the first sale ever transfers.
        for bad in [0_i128, -50] {
            let batch = sales_of(&env, &[(seller_a.clone(), 100), (seller_b.clone(), bad)]);
            let err = client
                .try_settle_sales(collection, &token, payer, &batch)
                .unwrap_err()
                .unwrap();
            assert_eq!(err, ForgeError::InvalidInput);
            assert_eq!(tc.balance(payer), 1_000);
            assert_eq!(tc.balance(seller_a), 0);
            assert_eq!(tc.balance(seller_b), 0);
            assert_eq!(tc.balance(recipient), 0);
            assert_eq!(
                client
                    .try_get_settlement_summary(collection)
                    .unwrap_err()
                    .unwrap(),
                ForgeError::NotFound
            );
        }
    }

    #[test]
    fn settle_sales_missing_config_is_not_found() {
        let (env, token, tc, _contract_id, client, accounts) = setup_settlement!();
        let payer = &accounts.user1;
        let seller = &accounts.user3;
        let unregistered = &accounts.validator; // never configured

        let batch = sales_of(&env, &[(seller.clone(), 1_000)]);
        let err = client
            .try_settle_sales(unregistered, &token, payer, &batch)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::NotFound);
        assert_eq!(tc.balance(payer), 1_000);
        assert_eq!(tc.balance(seller), 0);
    }

    #[test]
    fn settle_sales_overflow_fails_before_any_transfer() {
        let (env, token, tc, _contract_id, client, accounts) = setup_settlement!();
        let payer = &accounts.user1;
        let recipient = &accounts.user2;
        let seller_a = &accounts.user3;
        let seller_b = &accounts.validator;
        let collection = &accounts.arbiter;

        // The first sale splits fine; the second overflows the checked
        // multiply — and neither sale may move a token.
        let batch = sales_of(
            &env,
            &[(seller_a.clone(), 500), (seller_b.clone(), i128::MAX)],
        );
        let err = client
            .try_settle_sales(collection, &token, payer, &batch)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::ArithmeticOverflow);
        assert_eq!(tc.balance(payer), 1_000);
        assert_eq!(tc.balance(seller_a), 0);
        assert_eq!(tc.balance(seller_b), 0);
        assert_eq!(tc.balance(recipient), 0);
        assert_eq!(
            client
                .try_get_settlement_summary(collection)
                .unwrap_err()
                .unwrap(),
            ForgeError::NotFound
        );
    }

    #[test]
    fn settle_sales_aggregate_overflow_keeps_the_prior_summary() {
        let (env, token, tc, _contract_id, client, accounts) = setup_settlement!();
        let payer = &accounts.user1;
        let recipient = &accounts.user2;
        let seller = &accounts.user3;
        let collection = &accounts.arbiter;
        client.set_royalty(collection, recipient, &0_u32);

        client.settle_sale(collection, &token, payer, seller, &100_i128);

        // At 0 bps the split of `i128::MAX` is exact (share 0), so the
        // failure can only come from the aggregate check against the stored
        // summary — which must run before any transfer and leave the
        // existing totals untouched.
        let batch = sales_of(&env, &[(seller.clone(), i128::MAX)]);
        let err = client
            .try_settle_sales(collection, &token, payer, &batch)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::ArithmeticOverflow);
        assert_eq!(tc.balance(payer), 1_000 - 100);
        assert_eq!(tc.balance(seller), 100, "prior sale is untouched");
        let summary = client.get_settlement_summary(collection);
        assert_eq!(summary.sales, 1);
        assert_eq!(summary.gross_volume, 100);
        assert_eq!(summary.royalties_paid, 0);
    }

    #[test]
    fn settle_sales_never_leaves_the_recipient_partially_paid() {
        let (env, token, tc, contract_id, client, accounts) = setup_settlement!();
        let payer = &accounts.user1;
        let recipient = &accounts.user2;
        let seller_a = &accounts.user3;
        let seller_b = &accounts.validator;
        let collection = &accounts.arbiter;

        // Sale 1: 400 -> 20 / 380 (both transfers succeed, payer left 620).
        // Sale 2: 630 -> 31 / 599. The seller transfer succeeds (payer left
        // 21) but the recipient's 31 does not fit — the SECOND sale fails
        // after the FIRST sale fully succeeded. Frame rollback must undo
        // everything: no sale half-settled, no summary committed.
        let batch = sales_of(&env, &[(seller_a.clone(), 400), (seller_b.clone(), 630)]);
        let err = client
            .try_settle_sales(collection, &token, payer, &batch)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::TokenTransferFailed);
        assert_eq!(
            tc.balance(recipient),
            0,
            "recipient must never be partially paid"
        );
        assert_eq!(tc.balance(payer), 1_000, "rollback restores the payer");
        assert_eq!(tc.balance(seller_a), 0, "rollback restores seller 1");
        assert_eq!(tc.balance(seller_b), 0, "rollback restores seller 2");
        assert_eq!(tc.balance(&contract_id), 0);
        assert_eq!(
            client
                .try_get_settlement_summary(collection)
                .unwrap_err()
                .unwrap(),
            ForgeError::NotFound,
            "no settlement state is committed on failure"
        );
    }

    #[test]
    fn settle_sales_with_undeployed_token_fails_and_moves_nothing() {
        let (env, _token, tc, _contract_id, client, accounts) = setup_settlement!();
        let payer = &accounts.user1;
        let recipient = &accounts.user2;
        let seller = &accounts.user3;
        let collection = &accounts.arbiter;
        let not_a_token = Address::generate(&env);

        let batch = sales_of(&env, &[(seller.clone(), 400), (seller.clone(), 600)]);
        let err = client
            .try_settle_sales(collection, &not_a_token, payer, &batch)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::TokenTransferFailed);
        assert_eq!(tc.balance(payer), 1_000);
        assert_eq!(tc.balance(seller), 0);
        assert_eq!(tc.balance(recipient), 0);
        assert_eq!(
            client
                .try_get_settlement_summary(collection)
                .unwrap_err()
                .unwrap(),
            ForgeError::NotFound
        );
    }

    #[test]
    fn settle_sales_zero_bps_batch_skips_the_recipient() {
        let (env, token, tc, _contract_id, client, accounts) = setup_settlement!();
        let payer = &accounts.user1;
        let recipient = &accounts.user2;
        let seller_a = &accounts.user3;
        let seller_b = &accounts.validator;
        let collection = &accounts.arbiter;
        client.set_royalty(collection, recipient, &0_u32);

        let batch = sales_of(&env, &[(seller_a.clone(), 500), (seller_b.clone(), 500)]);
        let settled = client.settle_sales(collection, &token, payer, &batch);

        for sale in settled.iter() {
            assert_eq!(sale.royalty_share, 0);
        }
        assert_eq!(tc.balance(seller_a), 500);
        assert_eq!(tc.balance(seller_b), 500);
        assert_eq!(tc.balance(recipient), 0, "recipient transfer is skipped");
        let summary = client.get_settlement_summary(collection);
        assert_eq!(summary.sales, 2);
        assert_eq!(summary.gross_volume, 1_000);
        assert_eq!(summary.royalties_paid, 0);
    }

    #[test]
    fn settle_sales_disabled_config_settles_in_full_to_sellers() {
        let (env, token, tc, contract_id, client, accounts) = setup_settlement!();
        let payer = &accounts.user1;
        let recipient = &accounts.user2;
        let seller = &accounts.user3;
        let collection = &accounts.arbiter;

        // Same gap as the single-sale test: `set_royalty` always stores
        // `Active`, so write the `Disabled` record directly.
        let disabled = Royalty {
            collection: collection.clone(),
            recipient: recipient.clone(),
            bps: 500,
            status: RoyaltyStatus::Disabled,
        };
        env.as_contract(&contract_id, || {
            env.storage()
                .persistent()
                .set(&DataKey::Royalty(collection.clone()), &disabled);
        });

        let batch = sales_of(&env, &[(seller.clone(), 600), (seller.clone(), 400)]);
        let settled = client.settle_sales(collection, &token, payer, &batch);

        assert_eq!(settled.len(), 2);
        assert_eq!(tc.balance(seller), 1_000);
        assert_eq!(tc.balance(recipient), 0);
        assert_eq!(client.get_settlement_summary(collection).royalties_paid, 0);
    }

    #[test]
    fn get_settlement_summary_missing_is_not_found() {
        let (_env, client, accounts) = setup!();
        let err = client
            .try_get_settlement_summary(&accounts.arbiter)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::NotFound);
    }

    #[test]
    fn touch_ttl_extends_and_keeps_state_intact() {
        let (_env, token, _tc, _contract_id, client, accounts) = setup_settlement!();
        let collection = &accounts.arbiter;
        client.settle_sale(
            collection,
            &token,
            &accounts.user1,
            &accounts.user3,
            &1_000_i128,
        );

        assert_eq!(client.touch_ttl(collection), ());
        let royalty = client.get_royalty(collection);
        assert_eq!(royalty.bps, 500);
        let summary = client.get_settlement_summary(collection);
        assert_eq!(summary.sales, 1);
    }

    #[test]
    fn touch_ttl_unknown_collection_returns_not_found() {
        let (env, client, _accounts) = setup!();
        let unknown = Address::generate(&env);
        let err = client.try_touch_ttl(&unknown).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::NotFound);
    }

    #[test]
    fn events_emitted_on_set_royalty_and_settle() {
        let (env, token, _tc, _contract_id, client, accounts) = setup_settlement!();
        let collection = &accounts.arbiter;
        client.set_royalty(collection, &accounts.user2, &500_u32);
        client.settle_sale(
            collection,
            &token,
            &accounts.user1,
            &accounts.user3,
            &1_000_i128,
        );

        let events = env.events().all();
        assert!(!events.events().is_empty());
    }
}
