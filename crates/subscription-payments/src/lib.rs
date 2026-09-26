#![no_std]

//! # Soroban Forge — Subscription Payments contract
//!
//! Recurring, on-chain subscription billing: a subscriber authorises a
//! provider to pull a fixed `amount` per `period` (seconds) from a token
//! balance using SEP-41 tokens. The contract tracks subscription state,
//! billing cadence, token settlement, pause/resume, and arrears retry handling.
//!
//! Lifecycle:
//!
//! ```text
//! subscribe -> Active <-> Paused
//!                 \         /
//!                  v       v
//!                   cancel -> Cancelled (no further charges)
//! subscribe -> Active --(period elapses, charge succeeds)--> bills amount, advances
//!            -> Active --(period elapses, charge fails)---> PastDue (arrears retry)
//!            -> PastDue --(retry exceeds max retries)-----> Cancelled
//!            -> cancel -> Cancelled (no further charges)
//! ```
//!
//! Authorization model:
//! - `subscribe` requires the subscriber (who authorises the agreement).
//! - `charge` requires the provider (who pulls payment) and bills when a
//!   full period has elapsed since the last charge, executing a SEP-41 token
//!   transfer from subscriber to provider.
//! - `charge_catchup` requires the provider and atomically bills multiple
//!   elapsed periods, bounded by [`MAX_CATCHUP_PERIODS`]. It refuses
//!   `PastDue` subscriptions so arrears retry semantics remain owned by
//!   `charge`.
//! - `pause` requires the subscriber.
//! - `resume` requires the subscriber.
//! - `cancel` requires the subscriber (works from `Active`, `Paused`, or `PastDue`).
//! - `get_subscription` is a read-only view.

#[cfg(test)]
extern crate std;

use soroban_forge_shared_utils::ForgeError;
use soroban_sdk::{
    contract, contractclient, contractevent, contractimpl, contracttype, token, Address, Env, Vec,
};

/// Maximum consecutive failed payment attempts before transitioning to Cancelled.
const MAX_RETRIES: u32 = 3;

/// Hard upper bound for one catch-up invocation.
///
/// Keeping the loop bounded protects Soroban instruction limits while still
/// covering practical keeper downtime without requiring one transaction per
/// missed period.
const MAX_CATCHUP_PERIODS: u32 = 32;

/// Public interface for the Soroban Forge subscription payments contract.
#[contractclient(name = "SorobanForgeSubscriptionPaymentsClient")]
pub trait SorobanForgeSubscriptionPayments {
    /// Subscribe `subscriber` to `provider`'s service at `amount` per
    /// `period`. Returns the stable subscription id.
    fn subscribe(
        env: Env,
        subscriber: Address,
        provider: Address,
        token: Address,
        amount: i128,
        period: u64,
    ) -> Result<u64, soroban_forge_shared_utils::ForgeError>;

    /// Charge the next due payment for `subscription_id`.
    ///
    /// Returns the billed amount, or `0` when no full period has elapsed since
    /// the last charge.
    fn charge(
        env: Env,
        subscription_id: u64,
    ) -> Result<i128, soroban_forge_shared_utils::ForgeError>;

    /// Atomically charge up to `max_periods` elapsed periods.
    ///
    /// Returns the total billed amount. `max_periods` must not exceed the
    /// contract's hard catch-up bound. A transfer failure returns
    /// [`ForgeError::TokenTransferFailed`] and rolls back all transfers and
    /// subscription state. `PastDue` subscriptions must use `charge` to
    /// preserve the existing retry policy.
    fn charge_catchup(
        env: Env,
        subscription_id: u64,
        max_periods: u32,
    ) -> Result<i128, soroban_forge_shared_utils::ForgeError>;

    /// Pause an active subscription, preventing further charges while paused.
    ///
    /// Requires the subscriber. Only valid when `Active`.
    fn pause(env: Env, subscription_id: u64) -> Result<(), soroban_forge_shared_utils::ForgeError>;

    /// Resume a paused subscription, advancing the next due date by the
    /// elapsed paused duration.
    ///
    /// Requires the subscriber. Only valid when `Paused`.
    fn resume(env: Env, subscription_id: u64)
        -> Result<(), soroban_forge_shared_utils::ForgeError>;

    /// Cancel `subscription_id`, preventing further charges.
    ///
    /// Requires the subscriber. Valid when `Active`, `Paused`, or `PastDue`.
    fn cancel(env: Env, subscription_id: u64)
        -> Result<(), soroban_forge_shared_utils::ForgeError>;

    /// Read a stored subscription by id (read-only view).
    fn get_subscription(
        env: Env,
        subscription_id: u64,
    ) -> Result<Subscription, soroban_forge_shared_utils::ForgeError>;

    /// Total number of subscriptions created so far (read-only view).
    fn get_subscription_count(env: Env) -> u64;

    /// List `subscriber`'s subscriptions in creation order, one page at a
    /// time (read-only view).
    ///
    /// `offset` skips the first `offset` subscriptions and `limit` caps the
    /// page size. Returns a `Result` so a `limit` of `0` fails with
    /// [`ForgeError::InvalidInput`]; an empty index or an out-of-bounds
    /// offset yields an empty `Vec`, not an error.
    fn subscriptions_for_subscriber(
        env: Env,
        subscriber: Address,
        offset: u32,
        limit: u32,
    ) -> Result<Vec<Subscription>, soroban_forge_shared_utils::ForgeError>;

    /// List `provider`'s subscriptions in creation order, one page at a time
    /// (read-only view).
    ///
    /// `offset` skips the first `offset` subscriptions and `limit` caps the
    /// page size. Returns a `Result` so a `limit` of `0` fails with
    /// [`ForgeError::InvalidInput`]; an empty index or an out-of-bounds
    /// offset yields an empty `Vec`, not an error.
    fn subscriptions_for_provider(
        env: Env,
        provider: Address,
        offset: u32,
        limit: u32,
    ) -> Result<Vec<Subscription>, soroban_forge_shared_utils::ForgeError>;
}

/// Lifecycle state of a subscription.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubscriptionStatus {
    /// Active and chargeable.
    Active,
    /// Cancelled; no further charges.
    Cancelled,
    /// Payment failed and the subscription is in arrears.
    PastDue,
    /// Temporarily paused; no charges can be made until resumed.
    Paused,
}

/// A recurring payment agreement.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Subscription {
    /// Stable identifier assigned at creation.
    pub subscription_id: u64,
    /// Account being charged.
    pub subscriber: Address,
    /// Account receiving payments.
    pub provider: Address,
    /// Token contract used for settlement.
    pub token: Address,
    /// Amount charged per period.
    pub amount: i128,
    /// Length of one billing period, in seconds.
    pub period: u64,
    /// Ledger timestamp of the last successful charge.
    pub last_charged: u64,
    /// Current state.
    pub status: SubscriptionStatus,
    /// Ledger timestamp when paused, if currently paused.
    pub paused_at: Option<u64>,
    /// Number of consecutive failed billing attempts.
    pub failed_attempts: u32,
}

/// Instance-storage keys.
#[contracttype]
enum DataKey {
    /// The subscription record for `u64` id.
    Subscription(u64),
    /// Monotonic subscription id counter.
    Count,
    /// Creation-order subscription ids for which the `Address` is the
    /// subscriber, in id order. Written once per `subscribe`.
    SubscriberSubscriptions(Address),
    /// Creation-order subscription ids for which the `Address` is the
    /// provider, in id order. Written once per `subscribe`.
    ProviderSubscriptions(Address),
}

/// The deployable subscription payments contract.
#[contract]
pub struct SubscriptionPayments;

#[contractimpl]
impl SubscriptionPayments {
    /// Create a new subscription and return its stable id.
    ///
    /// Requires `amount > 0` and `period > 0`. The subscriber is authorized at
    /// creation time; billing starts from the moment of subscription.
    pub fn subscribe(
        env: Env,
        subscriber: Address,
        provider: Address,
        token: Address,
        amount: i128,
        period: u64,
    ) -> Result<u64, ForgeError> {
        if amount <= 0 {
            return Err(ForgeError::InvalidInput);
        }
        if period == 0 {
            return Err(ForgeError::InvalidInput);
        }
        subscriber.require_auth();

        let subscription_id = Self::next_id(&env)?;
        let subscription = Subscription {
            subscription_id,
            subscriber: subscriber.clone(),
            provider: provider.clone(),
            token,
            amount,
            period,
            last_charged: env.ledger().timestamp(),
            status: SubscriptionStatus::Active,
            paused_at: None,
            failed_attempts: 0,
        };
        env.storage()
            .instance()
            .set(&DataKey::Subscription(subscription_id), &subscription);
        // Index writes join the success path after every fallible step
        // (validation, `require_auth`, id allocation), so they cannot
        // observe or create partial state.
        Self::append_index(
            &env,
            &DataKey::SubscriberSubscriptions(subscriber),
            subscription_id,
        );
        Self::append_index(
            &env,
            &DataKey::ProviderSubscriptions(provider),
            subscription_id,
        );
        events::subscribed(&env, &subscription);
        Ok(subscription_id)
    }

    /// Bill one due period.
    ///
    /// Requires the provider. If a full period has not elapsed since the last
    /// charge, returns `0` and leaves the subscription untouched. Otherwise
    /// attempts to transfer `amount` of `token` from `subscriber` to `provider`.
    ///
    /// - On successful payment: advances `last_charged` by one period, resets
    ///   `failed_attempts` to 0, transitions status to `Active`, and returns `amount`.
    /// - On failed payment: `last_charged` is NOT advanced. Increments `failed_attempts`.
    ///   If `failed_attempts >= MAX_RETRIES` (3), status becomes `Cancelled`.
    ///   Otherwise status becomes `PastDue`. Returns `0`.
    pub fn charge(env: Env, subscription_id: u64) -> Result<i128, ForgeError> {
        let mut subscription = Self::get_subscription_impl(&env, subscription_id)?;
        if subscription.status == SubscriptionStatus::Cancelled
            || subscription.status == SubscriptionStatus::Paused
        {
            return Err(ForgeError::InvalidInput);
        }
        subscription.provider.require_auth();

        let next_due = subscription
            .last_charged
            .checked_add(subscription.period)
            .ok_or(ForgeError::ArithmeticOverflow)?;
        if env.ledger().timestamp() < next_due {
            return Ok(0);
        }

        // Execute SEP-41 token transfer from subscriber to provider
        let transfer_result = token::TokenClient::new(&env, &subscription.token).try_transfer(
            &subscription.subscriber,
            &subscription.provider,
            &subscription.amount,
        );

        match transfer_result {
            Ok(Ok(())) => {
                subscription.last_charged = next_due;
                subscription.failed_attempts = 0;
                subscription.status = SubscriptionStatus::Active;
                subscription.paused_at = None;
                env.storage()
                    .instance()
                    .set(&DataKey::Subscription(subscription_id), &subscription);
                events::charged(&env, &subscription);
                Ok(subscription.amount)
            }
            _ => {
                let failed = subscription.failed_attempts.saturating_add(1);
                subscription.failed_attempts = failed;
                if failed >= MAX_RETRIES {
                    subscription.status = SubscriptionStatus::Cancelled;
                } else {
                    subscription.status = SubscriptionStatus::PastDue;
                }
                env.storage()
                    .instance()
                    .set(&DataKey::Subscription(subscription_id), &subscription);
                Ok(0)
            }
        }
    }

    /// Atomically bill multiple elapsed periods.
    pub fn charge_catchup(
        env: Env,
        subscription_id: u64,
        max_periods: u32,
    ) -> Result<i128, ForgeError> {
        if max_periods > MAX_CATCHUP_PERIODS {
            return Err(ForgeError::InvalidInput);
        }

        let mut subscription = Self::get_subscription_impl(&env, subscription_id)?;
        if subscription.status != SubscriptionStatus::Active {
            return Err(ForgeError::InvalidInput);
        }
        subscription.provider.require_auth();

        if max_periods == 0 {
            return Ok(0);
        }

        let now = env.ledger().timestamp();
        let elapsed = now
            .checked_sub(subscription.last_charged)
            .ok_or(ForgeError::ArithmeticOverflow)?;
        let elapsed_periods = elapsed / subscription.period;
        let periods = core::cmp::min(max_periods as u64, elapsed_periods);

        let mut total = 0_i128;
        for _ in 0..periods {
            let next_due = subscription
                .last_charged
                .checked_add(subscription.period)
                .ok_or(ForgeError::ArithmeticOverflow)?;
            let transfer_result = token::TokenClient::new(&env, &subscription.token).try_transfer(
                &subscription.subscriber,
                &subscription.provider,
                &subscription.amount,
            );
            if !matches!(transfer_result, Ok(Ok(()))) {
                return Err(ForgeError::TokenTransferFailed);
            }
            total = total
                .checked_add(subscription.amount)
                .ok_or(ForgeError::ArithmeticOverflow)?;
            subscription.last_charged = next_due;
        }

        if periods > 0 {
            subscription.failed_attempts = 0;
            subscription.status = SubscriptionStatus::Active;
            subscription.paused_at = None;
            env.storage()
                .instance()
                .set(&DataKey::Subscription(subscription_id), &subscription);
        }
        Ok(total)
    }

    /// Pause an active subscription, preventing charges while paused.
    ///
    /// Requires the subscriber. Only valid when `Active`.
    pub fn pause(env: Env, subscription_id: u64) -> Result<(), ForgeError> {
        let mut subscription = Self::get_subscription_impl(&env, subscription_id)?;
        if subscription.status != SubscriptionStatus::Active {
            return Err(ForgeError::InvalidInput);
        }
        subscription.subscriber.require_auth();

        subscription.status = SubscriptionStatus::Paused;
        subscription.paused_at = Some(env.ledger().timestamp());
        env.storage()
            .instance()
            .set(&DataKey::Subscription(subscription_id), &subscription);
        Ok(())
    }

    /// Resume a paused subscription, advancing the next due date by the
    /// elapsed paused duration so that paused periods are not billed.
    ///
    /// Requires the subscriber. Only valid when `Paused`.
    pub fn resume(env: Env, subscription_id: u64) -> Result<(), ForgeError> {
        let mut subscription = Self::get_subscription_impl(&env, subscription_id)?;
        if subscription.status != SubscriptionStatus::Paused {
            return Err(ForgeError::InvalidInput);
        }
        subscription.subscriber.require_auth();

        let now = env.ledger().timestamp();
        let paused_at = subscription.paused_at.unwrap_or(now);
        let elapsed = now
            .checked_sub(paused_at)
            .ok_or(ForgeError::ArithmeticOverflow)?;

        subscription.last_charged = subscription
            .last_charged
            .checked_add(elapsed)
            .ok_or(ForgeError::ArithmeticOverflow)?;
        subscription.status = SubscriptionStatus::Active;
        subscription.paused_at = None;
        env.storage()
            .instance()
            .set(&DataKey::Subscription(subscription_id), &subscription);
        Ok(())
    }

    /// Cancel a subscription, preventing further charges.
    ///
    /// Requires the subscriber. Valid when `Active`, `Paused`, or `PastDue`. Cancelling
    /// an already-cancelled subscription is rejected.
    pub fn cancel(env: Env, subscription_id: u64) -> Result<(), ForgeError> {
        let mut subscription = Self::get_subscription_impl(&env, subscription_id)?;
        if subscription.status == SubscriptionStatus::Cancelled {
            return Err(ForgeError::InvalidInput);
        }
        subscription.subscriber.require_auth();

        subscription.status = SubscriptionStatus::Cancelled;
        subscription.paused_at = None;
        env.storage()
            .instance()
            .set(&DataKey::Subscription(subscription_id), &subscription);
        events::cancelled(&env, &subscription);
        Ok(())
    }

    /// Read a stored subscription by id (read-only view).
    pub fn get_subscription(env: Env, subscription_id: u64) -> Result<Subscription, ForgeError> {
        Self::get_subscription_impl(&env, subscription_id)
    }

    /// Total number of subscriptions created so far (read-only view).
    ///
    /// This is the monotonic id counter, which only `subscribe` advances, so
    /// it never decreases and equals the number of live ids returned by the
    /// enumeration views.
    pub fn get_subscription_count(env: Env) -> u64 {
        env.storage().instance().get(&DataKey::Count).unwrap_or(0)
    }

    /// List `subscriber`'s subscriptions in creation order, one page at a
    /// time (read-only view).
    ///
    /// Requires no authorization and never mutates storage. An address with
    /// no subscriptions — or an offset at or past the end of its list —
    /// returns an empty `Vec`, not an error, so clients can back "my
    /// subscriptions" views without an off-chain indexer.
    ///
    /// # Errors
    ///
    /// * [`ForgeError::InvalidInput`] — `limit` is zero.
    pub fn subscriptions_for_subscriber(
        env: Env,
        subscriber: Address,
        offset: u32,
        limit: u32,
    ) -> Result<Vec<Subscription>, ForgeError> {
        if limit == 0 {
            return Err(ForgeError::InvalidInput);
        }
        let ids = Self::index_ids(&env, &DataKey::SubscriberSubscriptions(subscriber));
        Self::resolve_page(&env, &ids, offset, limit)
    }

    /// List `provider`'s subscriptions in creation order, one page at a time
    /// (read-only view).
    ///
    /// Requires no authorization and never mutates storage. An address with
    /// no subscriptions — or an offset at or past the end of its list —
    /// returns an empty `Vec`, not an error, so clients can back "my
    /// subscribers" views without an off-chain indexer.
    ///
    /// # Errors
    ///
    /// * [`ForgeError::InvalidInput`] — `limit` is zero.
    pub fn subscriptions_for_provider(
        env: Env,
        provider: Address,
        offset: u32,
        limit: u32,
    ) -> Result<Vec<Subscription>, ForgeError> {
        if limit == 0 {
            return Err(ForgeError::InvalidInput);
        }
        let ids = Self::index_ids(&env, &DataKey::ProviderSubscriptions(provider));
        Self::resolve_page(&env, &ids, offset, limit)
    }

    /// Allocate the next monotonic subscription id.
    fn next_id(env: &Env) -> Result<u64, ForgeError> {
        let count: u64 = env.storage().instance().get(&DataKey::Count).unwrap_or(0);
        let id = count.checked_add(1).ok_or(ForgeError::ArithmeticOverflow)?;
        env.storage().instance().set(&DataKey::Count, &id);
        Ok(id)
    }

    fn get_subscription_impl(env: &Env, subscription_id: u64) -> Result<Subscription, ForgeError> {
        env.storage()
            .instance()
            .get(&DataKey::Subscription(subscription_id))
            .ok_or(ForgeError::NotFound)
    }

    /// Append `subscription_id` to an index, carrying the address the index
    /// belongs to in the key.
    fn append_index(env: &Env, key: &DataKey, subscription_id: u64) {
        let mut ids: Vec<u64> = env
            .storage()
            .instance()
            .get(key)
            .unwrap_or_else(|| Vec::new(env));
        ids.push_back(subscription_id);
        env.storage().instance().set(key, &ids);
    }

    /// Read an index's id list, defaulting to empty when the address has no
    /// entries.
    fn index_ids(env: &Env, key: &DataKey) -> Vec<u64> {
        env.storage()
            .instance()
            .get(key)
            .unwrap_or_else(|| Vec::new(env))
    }

    /// Resolve a contiguous slice of `ids` into `Subscription` records.
    ///
    /// `offset` is clamped to the list length and `end` saturates, so an
    /// out-of-bounds offset yields an empty `Vec` rather than a bounds panic.
    fn resolve_page(
        env: &Env,
        ids: &Vec<u64>,
        offset: u32,
        limit: u32,
    ) -> Result<Vec<Subscription>, ForgeError> {
        let total = ids.len();
        let (mut at, end) = (offset.min(total), offset.saturating_add(limit).min(total));
        let mut subscriptions = Vec::new(env);
        while at < end {
            subscriptions.push_back(Self::get_subscription_impl(env, ids.get_unchecked(at))?);
            at += 1;
        }
        Ok(subscriptions)
    }
}

/// Lifecycle events emitted by the subscription payments contract.
mod events {
    use super::*;

    #[contractevent]
    pub struct Subscribed {
        #[topic]
        pub subscription_id: u64,
        pub subscriber: Address,
        pub provider: Address,
        pub token: Address,
        pub amount: i128,
        pub period: u64,
    }

    #[contractevent]
    pub struct Charged {
        #[topic]
        pub subscription_id: u64,
        pub amount: i128,
        pub last_charged: u64,
        pub next_charge_at: u64,
    }

    #[contractevent]
    pub struct Cancelled {
        #[topic]
        pub subscription_id: u64,
        pub subscriber: Address,
    }

    pub fn subscribed(env: &Env, subscription: &Subscription) {
        Subscribed {
            subscription_id: subscription.subscription_id,
            subscriber: subscription.subscriber.clone(),
            provider: subscription.provider.clone(),
            token: subscription.token.clone(),
            amount: subscription.amount,
            period: subscription.period,
        }
        .publish(env);
    }

    pub fn charged(env: &Env, subscription: &Subscription) {
        let next_charge_at = subscription
            .last_charged
            .saturating_add(subscription.period);
        Charged {
            subscription_id: subscription.subscription_id,
            amount: subscription.amount,
            last_charged: subscription.last_charged,
            next_charge_at,
        }
        .publish(env);
    }

    pub fn cancelled(env: &Env, subscription: &Subscription) {
        Cancelled {
            subscription_id: subscription.subscription_id,
            subscriber: subscription.subscriber.clone(),
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
    use soroban_sdk::testutils::{Address as _, Events as _, Ledger as _};
    use soroban_sdk::token::{Client as TokenClient, StellarAssetClient};
    use soroban_sdk::{Address, Env};

    const START: u64 = 1_000_000;
    const PERIOD: u64 = 1_000;
    const AMOUNT: i128 = 250;

    /// Build a fresh env with mocked auths, a registered contract, a real SEP-41
    /// token with minted funds to subscriber, a subscription, and named accounts.
    macro_rules! setup {
        () => {{
            let env = Env::default();
            env.mock_all_auths_allowing_non_root_auth();
            env.ledger().set_timestamp(START);

            let admin = Address::generate(&env);
            let sac = env.register_stellar_asset_contract_v2(admin);
            let token = sac.address();
            let token_admin = StellarAssetClient::new(&env, &token);
            let token_client = TokenClient::new(&env, &token);

            let contract_id = env.register(SubscriptionPayments, ());
            let client = SorobanForgeSubscriptionPaymentsClient::new(&env, &contract_id);
            let accounts = TestAccounts::generate(&env);

            token_admin.mint(&accounts.user1, &10_000_i128);

            let subscription_id = client.subscribe(
                &accounts.user1,
                &accounts.validator,
                &token,
                &AMOUNT,
                &PERIOD,
            );
            (
                env,
                token,
                token_client,
                contract_id,
                client,
                accounts,
                subscription_id,
            )
        }};
    }

    #[test]
    fn subscribe_succeeds_and_is_active() {
        let (_env, _token, _tc, _contract_id, client, accounts, subscription_id) = setup!();
        let subscription = client.get_subscription(&subscription_id);
        assert_eq!(subscription.subscriber, accounts.user1);
        assert_eq!(subscription.provider, accounts.validator);
        assert_eq!(subscription.amount, AMOUNT);
        assert_eq!(subscription.status, SubscriptionStatus::Active);
        assert_eq!(subscription.last_charged, START);
        assert_eq!(subscription.paused_at, None);
        assert_eq!(subscription.failed_attempts, 0);
    }

    #[test]
    fn subscribe_assigns_distinct_ids() {
        let (_env, token, _tc, _contract_id, client, accounts, subscription_id) = setup!();
        let id2 = client.subscribe(
            &accounts.user2,
            &accounts.validator,
            &token,
            &AMOUNT,
            &PERIOD,
        );
        assert_ne!(subscription_id, id2);
    }

    #[test]
    fn subscribe_rejects_zero_amount() {
        let (_env, token, _tc, _contract_id, client, accounts, _id) = setup!();
        let err = client
            .try_subscribe(
                &accounts.user1,
                &accounts.validator,
                &token,
                &0_i128,
                &PERIOD,
            )
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn subscribe_rejects_zero_period() {
        let (_env, token, _tc, _contract_id, client, accounts, _id) = setup!();
        let err = client
            .try_subscribe(
                &accounts.user1,
                &accounts.validator,
                &token,
                &AMOUNT,
                &0_u64,
            )
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn charge_before_period_returns_zero() {
        let (env, _token, tc, _contract_id, client, accounts, subscription_id) = setup!();
        env.ledger().set_timestamp(START + PERIOD - 1);
        assert_eq!(client.charge(&subscription_id), 0);
        assert_eq!(
            client.get_subscription(&subscription_id).last_charged,
            START
        );
        assert_eq!(tc.balance(&accounts.user1), 10_000);
        assert_eq!(tc.balance(&accounts.validator), 0);
    }

    #[test]
    fn charge_at_period_bills_full_amount_and_transfers_tokens() {
        let (env, _token, tc, _contract_id, client, accounts, subscription_id) = setup!();
        env.ledger().set_timestamp(START + PERIOD);
        assert_eq!(client.charge(&subscription_id), AMOUNT);
        assert_eq!(
            client.get_subscription(&subscription_id).last_charged,
            START + PERIOD
        );
        assert_eq!(tc.balance(&accounts.user1), 10_000 - AMOUNT);
        assert_eq!(tc.balance(&accounts.validator), AMOUNT);
    }

    #[test]
    fn charge_catches_up_one_period_per_call() {
        let (env, _token, tc, _contract_id, client, accounts, subscription_id) = setup!();
        env.ledger().set_timestamp(START + PERIOD * 3);
        // First call bills one period
        assert_eq!(client.charge(&subscription_id), AMOUNT);
        assert_eq!(
            client.get_subscription(&subscription_id).last_charged,
            START + PERIOD
        );
        assert_eq!(tc.balance(&accounts.validator), AMOUNT);

        // Second call bills second period
        assert_eq!(client.charge(&subscription_id), AMOUNT);
        assert_eq!(
            client.get_subscription(&subscription_id).last_charged,
            START + PERIOD * 2
        );
        assert_eq!(tc.balance(&accounts.validator), AMOUNT * 2);
    }

    #[test]
    fn charge_catchup_bills_elapsed_periods_and_respects_max_periods() {
        let (env, _token, tc, _contract_id, client, accounts, subscription_id) = setup!();
        env.ledger().set_timestamp(START + PERIOD * 3);

        assert_eq!(client.charge_catchup(&subscription_id, &2), AMOUNT * 2);
        let subscription = client.get_subscription(&subscription_id);
        assert_eq!(subscription.last_charged, START + PERIOD * 2);
        assert_eq!(tc.balance(&accounts.validator), AMOUNT * 2);

        assert_eq!(client.charge_catchup(&subscription_id, &2), AMOUNT);
        assert_eq!(
            client.get_subscription(&subscription_id).last_charged,
            START + PERIOD * 3
        );
        assert_eq!(tc.balance(&accounts.validator), AMOUNT * 3);
    }

    #[test]
    fn charge_catchup_zero_and_before_due_are_noops() {
        let (env, _token, tc, _contract_id, client, accounts, subscription_id) = setup!();
        assert_eq!(client.charge_catchup(&subscription_id, &0), 0);
        env.ledger().set_timestamp(START + PERIOD - 1);
        assert_eq!(client.charge_catchup(&subscription_id, &2), 0);
        assert_eq!(
            client.get_subscription(&subscription_id).last_charged,
            START
        );
        assert_eq!(tc.balance(&accounts.validator), 0);
    }

    #[test]
    fn charge_catchup_rejects_values_over_hard_cap() {
        let (_env, _token, _tc, _contract_id, client, _accounts, subscription_id) = setup!();
        let err = client
            .try_charge_catchup(&subscription_id, &33)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn charge_catchup_handles_u64_max_timestamp_without_overflow() {
        let (env, token, tc, _contract_id, client, accounts, _id) = setup!();
        let subscription_id = client.subscribe(
            &accounts.user2,
            &accounts.validator,
            &token,
            &1_i128,
            &u64::MAX,
        );
        StellarAssetClient::new(&env, &token).mint(&accounts.user2, &1_i128);
        env.ledger().set_timestamp(u64::MAX);

        assert_eq!(client.charge_catchup(&subscription_id, &1), 0);
        assert_eq!(tc.balance(&accounts.validator), 0);
    }

    #[test]
    fn charge_catchup_refuses_past_due_and_preserves_retry_state() {
        let (env, token, _tc, _contract_id, client, accounts, _id) = setup!();
        let broke_user = Address::generate(&env);
        StellarAssetClient::new(&env, &token).mint(&broke_user, &10_i128);
        let sub_id = client.subscribe(&broke_user, &accounts.validator, &token, &AMOUNT, &PERIOD);
        env.ledger().set_timestamp(START + PERIOD);
        assert_eq!(client.charge(&sub_id), 0);

        let before = client.get_subscription(&sub_id);
        let err = client.try_charge_catchup(&sub_id, &2).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
        assert_eq!(client.get_subscription(&sub_id), before);
    }

    #[test]
    fn charge_catchup_transfer_failure_rolls_back_prior_periods() {
        let (env, token, tc, _contract_id, client, accounts, _id) = setup!();
        let broke_user = Address::generate(&env);
        StellarAssetClient::new(&env, &token).mint(&broke_user, &(AMOUNT + 1));
        let sub_id = client.subscribe(&broke_user, &accounts.validator, &token, &AMOUNT, &PERIOD);
        env.ledger().set_timestamp(START + PERIOD * 2);

        let err = client.try_charge_catchup(&sub_id, &2).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::TokenTransferFailed);
        assert_eq!(tc.balance(&broke_user), AMOUNT + 1);
        assert_eq!(tc.balance(&accounts.validator), 0);
        assert_eq!(client.get_subscription(&sub_id).last_charged, START);
    }

    #[test]
    fn charge_catchup_missing_subscription_is_not_found() {
        let (_env, _token, _tc, _contract_id, client, _accounts, _id) = setup!();
        let err = client.try_charge_catchup(&999, &1).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::NotFound);
    }

    #[test]
    fn charge_failure_transitions_to_past_due_and_retry_restores_active() {
        let (env, token, tc, _contract_id, client, accounts, _id) = setup!();
        let broke_user = Address::generate(&env);
        StellarAssetClient::new(&env, &token).mint(&broke_user, &10_i128);
        let sub_id = client.subscribe(&broke_user, &accounts.validator, &token, &AMOUNT, &PERIOD);

        env.ledger().set_timestamp(START + PERIOD);

        // First charge attempt fails due to insufficient balance in real SAC token
        let billed = client.charge(&sub_id);
        assert_eq!(billed, 0);

        let sub_past_due = client.get_subscription(&sub_id);
        assert_eq!(sub_past_due.status, SubscriptionStatus::PastDue);
        assert_eq!(sub_past_due.failed_attempts, 1);
        assert_eq!(sub_past_due.last_charged, START); // Not advanced!

        // Subscriber gets funds minted now
        StellarAssetClient::new(&env, &token).mint(&broke_user, &10_000_i128);

        // Retry charge succeeds!
        let billed = client.charge(&sub_id);
        assert_eq!(billed, AMOUNT);

        let sub_active = client.get_subscription(&sub_id);
        assert_eq!(sub_active.status, SubscriptionStatus::Active);
        assert_eq!(sub_active.failed_attempts, 0);
        assert_eq!(sub_active.last_charged, START + PERIOD);
        assert_eq!(tc.balance(&accounts.validator), AMOUNT);
    }

    #[test]
    fn max_retries_exceeded_transitions_to_cancelled() {
        let (env, token, _tc, _contract_id, client, accounts, _id) = setup!();
        let broke_user = Address::generate(&env);
        StellarAssetClient::new(&env, &token).mint(&broke_user, &10_i128);
        let sub_id = client.subscribe(&broke_user, &accounts.validator, &token, &AMOUNT, &PERIOD);

        env.ledger().set_timestamp(START + PERIOD);

        // Attempt 1 -> PastDue (failed_attempts = 1)
        assert_eq!(client.charge(&sub_id), 0);
        assert_eq!(
            client.get_subscription(&sub_id).status,
            SubscriptionStatus::PastDue
        );

        // Attempt 2 -> PastDue (failed_attempts = 2)
        assert_eq!(client.charge(&sub_id), 0);
        assert_eq!(
            client.get_subscription(&sub_id).status,
            SubscriptionStatus::PastDue
        );

        // Attempt 3 -> Cancelled (failed_attempts = 3)
        assert_eq!(client.charge(&sub_id), 0);
        assert_eq!(
            client.get_subscription(&sub_id).status,
            SubscriptionStatus::Cancelled
        );

        // Attempt 4 -> InvalidInput because status is now Cancelled
        assert_eq!(
            client.try_charge(&sub_id).unwrap_err().unwrap(),
            ForgeError::InvalidInput
        );
    }

    #[test]
    fn cancel_from_past_due_succeeds() {
        let (env, token, _tc, _contract_id, client, accounts, _id) = setup!();
        let broke_user = Address::generate(&env);
        StellarAssetClient::new(&env, &token).mint(&broke_user, &10_i128);
        let sub_id = client.subscribe(&broke_user, &accounts.validator, &token, &AMOUNT, &PERIOD);

        env.ledger().set_timestamp(START + PERIOD);
        let _ = client.charge(&sub_id);
        assert_eq!(
            client.get_subscription(&sub_id).status,
            SubscriptionStatus::PastDue
        );

        // Cancel from PastDue
        client.cancel(&sub_id);
        assert_eq!(
            client.get_subscription(&sub_id).status,
            SubscriptionStatus::Cancelled
        );
    }

    #[test]
    fn charge_missing_subscription_is_not_found() {
        let (_env, _token, _tc, _contract_id, client, _accounts, _id) = setup!();
        let err = client.try_charge(&999).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::NotFound);
    }

    #[test]
    fn cancel_prevents_further_charges() {
        let (env, _token, _tc, _contract_id, client, _accounts, subscription_id) = setup!();
        client.cancel(&subscription_id);
        assert_eq!(
            client.get_subscription(&subscription_id).status,
            SubscriptionStatus::Cancelled
        );
        env.ledger().set_timestamp(START + PERIOD);
        let err = client.try_charge(&subscription_id).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn cancel_twice_is_invalid() {
        let (_env, _token, _tc, _contract_id, client, _accounts, subscription_id) = setup!();
        client.cancel(&subscription_id);
        let err = client.try_cancel(&subscription_id).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn cancel_missing_subscription_is_not_found() {
        let (_env, _token, _tc, _contract_id, client, _accounts, _id) = setup!();
        let err = client.try_cancel(&999).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::NotFound);
    }

    #[test]
    fn get_subscription_missing_is_not_found() {
        let (_env, _token, _tc, _contract_id, client, _accounts, _id) = setup!();
        let err = client.try_get_subscription(&999).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::NotFound);
    }

    #[test]
    fn get_subscription_count_tracks_creations() {
        let (_env, _token, _tc, _contract_id, client, accounts, _id) = setup!();
        assert_eq!(client.get_subscription_count(), 1);
        let id2 = client.subscribe(
            &accounts.user2,
            &accounts.validator,
            &accounts.deployer,
            &AMOUNT,
            &PERIOD,
        );
        assert_eq!(client.get_subscription_count(), 2);
        // Cancelling does not lower the creation count.
        client.cancel(&id2);
        assert_eq!(client.get_subscription_count(), 2);
    }

    #[test]
    fn subscriber_index_tracks_multiple_providers() {
        let (_env, _token, _tc, _contract_id, client, accounts, _id) = setup!();
        let id2 = client.subscribe(
            &accounts.user1,
            &accounts.arbiter,
            &accounts.deployer,
            &AMOUNT,
            &PERIOD,
        );
        let page = client.subscriptions_for_subscriber(&accounts.user1, &0, &10);
        assert_eq!(page.len(), 2);
        assert_eq!(page.get_unchecked(0).subscription_id, 1);
        assert_eq!(page.get_unchecked(0).provider, accounts.validator);
        assert_eq!(page.get_unchecked(1).subscription_id, id2);
        assert_eq!(page.get_unchecked(1).provider, accounts.arbiter);
    }

    #[test]
    fn provider_index_tracks_multiple_subscribers() {
        let (_env, _token, _tc, _contract_id, client, accounts, _id) = setup!();
        let id2 = client.subscribe(
            &accounts.user2,
            &accounts.validator,
            &accounts.deployer,
            &AMOUNT,
            &PERIOD,
        );
        let page = client.subscriptions_for_provider(&accounts.validator, &0, &10);
        assert_eq!(page.len(), 2);
        assert_eq!(page.get_unchecked(0).subscriber, accounts.user1);
        assert_eq!(page.get_unchecked(0).subscription_id, 1);
        assert_eq!(page.get_unchecked(1).subscriber, accounts.user2);
        assert_eq!(page.get_unchecked(1).subscription_id, id2);
    }

    #[test]
    fn pagination_slices_across_pages() {
        let (_env, _token, _tc, _contract_id, client, accounts, _id) = setup!();
        // user1 adds four more subscriptions, for ids 2..=5.
        client.subscribe(
            &accounts.user1,
            &accounts.arbiter,
            &accounts.deployer,
            &AMOUNT,
            &PERIOD,
        );
        client.subscribe(
            &accounts.user1,
            &accounts.user3,
            &accounts.deployer,
            &AMOUNT,
            &PERIOD,
        );
        client.subscribe(
            &accounts.user1,
            &accounts.user2,
            &accounts.deployer,
            &AMOUNT,
            &PERIOD,
        );
        client.subscribe(
            &accounts.user1,
            &accounts.validator,
            &accounts.deployer,
            &AMOUNT,
            &PERIOD,
        );

        let first = client.subscriptions_for_subscriber(&accounts.user1, &0, &2);
        assert_eq!(first.len(), 2);
        assert_eq!(first.get_unchecked(0).subscription_id, 1);
        assert_eq!(first.get_unchecked(1).subscription_id, 2);

        let mid = client.subscriptions_for_subscriber(&accounts.user1, &2, &2);
        assert_eq!(mid.len(), 2);
        assert_eq!(mid.get_unchecked(0).subscription_id, 3);
        assert_eq!(mid.get_unchecked(1).subscription_id, 4);

        let last = client.subscriptions_for_subscriber(&accounts.user1, &4, &10);
        assert_eq!(last.len(), 1);
        assert_eq!(last.get_unchecked(0).subscription_id, 5);
    }

    #[test]
    fn subscriptions_for_unknown_address_are_empty() {
        let (_env, _token, _tc, _contract_id, client, accounts, _id) = setup!();
        assert_eq!(
            client
                .subscriptions_for_subscriber(&accounts.arbiter, &0, &10)
                .len(),
            0
        );
        assert_eq!(
            client
                .subscriptions_for_provider(&accounts.arbiter, &0, &10)
                .len(),
            0
        );
    }

    #[test]
    fn pause_active_subscription_succeeds() {
        let (env, _token, _tc, _contract_id, client, _accounts, subscription_id) = setup!();
        env.ledger().set_timestamp(START + 250);
        client.pause(&subscription_id);

        let sub = client.get_subscription(&subscription_id);
        assert_eq!(sub.status, SubscriptionStatus::Paused);
        assert_eq!(sub.paused_at, Some(START + 250));
    }

    #[test]
    fn pause_already_paused_is_invalid() {
        let (_env, _token, _tc, _contract_id, client, _accounts, subscription_id) = setup!();
        client.pause(&subscription_id);
        let err = client.try_pause(&subscription_id).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn pause_cancelled_is_invalid() {
        let (_env, _token, _tc, _contract_id, client, _accounts, subscription_id) = setup!();
        client.cancel(&subscription_id);
        let err = client.try_pause(&subscription_id).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn pause_missing_is_not_found() {
        let (_env, _token, _tc, _contract_id, client, _accounts, _id) = setup!();
        let err = client.try_pause(&999).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::NotFound);
    }

    #[test]
    fn charge_paused_subscription_fails_and_does_not_advance() {
        let (env, _token, _tc, _contract_id, client, _accounts, subscription_id) = setup!();
        env.ledger().set_timestamp(START + 100);
        client.pause(&subscription_id);

        env.ledger().set_timestamp(START + PERIOD * 2);
        let err = client.try_charge(&subscription_id).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::InvalidInput);

        let sub = client.get_subscription(&subscription_id);
        assert_eq!(sub.last_charged, START);
        assert_eq!(sub.status, SubscriptionStatus::Paused);
    }

    #[test]
    fn resume_paused_subscription_advances_due_date_by_elapsed() {
        let (env, _token, _tc, _contract_id, client, _accounts, subscription_id) = setup!();
        // Pause at START + 300 (300s into 1000s period)
        env.ledger().set_timestamp(START + 300);
        client.pause(&subscription_id);

        // Resume at START + 800 (elapsed pause = 500s)
        env.ledger().set_timestamp(START + 800);
        client.resume(&subscription_id);

        let sub = client.get_subscription(&subscription_id);
        assert_eq!(sub.status, SubscriptionStatus::Active);
        assert_eq!(sub.paused_at, None);
        // last_charged should be shifted by 500s: START + 500
        assert_eq!(sub.last_charged, START + 500);

        // At original due date (START + 1000), only 500s of active cycle elapsed: charge -> 0
        env.ledger().set_timestamp(START + 1000);
        assert_eq!(client.charge(&subscription_id), 0);

        // At new due date (START + 500 + 1000 = START + 1500), charge succeeds
        env.ledger().set_timestamp(START + 1500);
        assert_eq!(client.charge(&subscription_id), AMOUNT);
        assert_eq!(
            client.get_subscription(&subscription_id).last_charged,
            START + 1500
        );
    }

    #[test]
    fn out_of_bounds_offset_returns_empty() {
        let (_env, _token, _tc, _contract_id, client, accounts, _id) = setup!();
        assert_eq!(
            client
                .subscriptions_for_subscriber(&accounts.user1, &1, &10)
                .len(),
            0
        );
        assert_eq!(
            client
                .subscriptions_for_provider(&accounts.validator, &2, &10)
                .len(),
            0
        );
    }

    #[test]
    fn zero_limit_is_invalid_input() {
        let (_env, _token, _tc, _contract_id, client, accounts, _id) = setup!();
        let err = client
            .try_subscriptions_for_subscriber(&accounts.user1, &0, &0)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
        let err = client
            .try_subscriptions_for_provider(&accounts.validator, &0, &0)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn pause_resume_zero_length_no_drift() {
        let (env, _token, _tc, _contract_id, client, _accounts, subscription_id) = setup!();
        env.ledger().set_timestamp(START + 300);
        client.pause(&subscription_id);
        client.resume(&subscription_id);

        let sub = client.get_subscription(&subscription_id);
        assert_eq!(sub.status, SubscriptionStatus::Active);
        assert_eq!(sub.last_charged, START);
        assert_eq!(sub.paused_at, None);

        // Charge at exactly START + PERIOD succeeds as normal
        env.ledger().set_timestamp(START + PERIOD);
        assert_eq!(client.charge(&subscription_id), AMOUNT);
    }

    #[test]
    fn pause_across_multiple_periods_preserves_remaining_cycle() {
        let (env, _token, _tc, _contract_id, client, _accounts, subscription_id) = setup!();
        // 100s into cycle (900s remaining)
        env.ledger().set_timestamp(START + 100);
        client.pause(&subscription_id);

        // Stay paused across multiple periods: 5 periods (5000s) elapsed while paused
        env.ledger().set_timestamp(START + 5100);
        client.resume(&subscription_id);

        let sub = client.get_subscription(&subscription_id);
        // last_charged shifted by 5000: START + 5000
        assert_eq!(sub.last_charged, START + 5000);
        // next due is START + 6000 (which is resume_time + 900s remaining)

        env.ledger().set_timestamp(START + 5999);
        assert_eq!(client.charge(&subscription_id), 0);

        env.ledger().set_timestamp(START + 6000);
        assert_eq!(client.charge(&subscription_id), AMOUNT);
    }

    #[test]
    fn resume_active_subscription_is_invalid() {
        let (_env, _token, _tc, _contract_id, client, _accounts, subscription_id) = setup!();
        let err = client.try_resume(&subscription_id).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn resume_cancelled_subscription_is_invalid() {
        let (_env, _token, _tc, _contract_id, client, _accounts, subscription_id) = setup!();
        client.cancel(&subscription_id);
        let err = client.try_resume(&subscription_id).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn resume_missing_is_not_found() {
        let (_env, _token, _tc, _contract_id, client, _accounts, _id) = setup!();
        let err = client.try_resume(&999).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::NotFound);
    }

    #[test]
    fn cancel_paused_subscription_succeeds_and_cannot_resume() {
        let (env, _token, _tc, _contract_id, client, _accounts, subscription_id) = setup!();
        env.ledger().set_timestamp(START + 200);
        client.pause(&subscription_id);

        // Cancel while paused
        client.cancel(&subscription_id);
        let sub = client.get_subscription(&subscription_id);
        assert_eq!(sub.status, SubscriptionStatus::Cancelled);
        assert_eq!(sub.paused_at, None);

        // Cannot resume cancelled subscription
        let err = client.try_resume(&subscription_id).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::InvalidInput);

        // Cannot charge cancelled subscription
        env.ledger().set_timestamp(START + PERIOD * 2);
        let err = client.try_charge(&subscription_id).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::InvalidInput);

        // Cannot pause cancelled subscription
        let err = client.try_pause(&subscription_id).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::InvalidInput);

        // Cannot cancel already cancelled subscription
        let err = client.try_cancel(&subscription_id).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn events_emitted_on_lifecycle_actions() {
        let (env, _token, _tc, _contract_id, client, _accounts, subscription_id) = setup!();
        env.ledger().set_timestamp(START + PERIOD);
        client.charge(&subscription_id);
        client.cancel(&subscription_id);

        let events = env.events().all();
        assert!(!events.events().is_empty());
    }
}
