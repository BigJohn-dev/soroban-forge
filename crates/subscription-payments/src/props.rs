//! Randomized invariant suite (proptest) for subscription payments.
//!
//! Exercised properties:
//! 1. Monotonicity: `last_charged` is monotonic and advances by exactly one period
//!    per successful charge.
//! 2. No early bill: `charge` returns `0` and changes nothing before a full period
//!    has elapsed.
//! 3. Terminal safety: a `Cancelled` subscription never bills.

use crate::{SorobanForgeSubscriptionPaymentsClient, SubscriptionPayments, SubscriptionStatus};
use proptest::prelude::*;
use soroban_forge_shared_utils::ForgeError;
use soroban_sdk::testutils::{Address as _, Ledger as _};
use soroban_sdk::token::StellarAssetClient;
use soroban_sdk::{Address, Env};

const START: u64 = 1_000_000;
const MAX_AMOUNT: i128 = 1_000_000_000_000;
const MAX_PERIOD: u64 = 10 * 365 * 24 * 3600;

struct World {
    env: Env,
    token: Address,
    contract_id: Address,
    subscriber: Address,
    provider: Address,
}

fn setup_world(mint_amount: i128) -> World {
    let env = Env::default();
    env.mock_all_auths_allowing_non_root_auth();
    env.ledger().set_timestamp(START);

    let admin = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(admin);
    let token = sac.address();

    let subscriber = Address::generate(&env);
    let provider = Address::generate(&env);

    StellarAssetClient::new(&env, &token).mint(&subscriber, &mint_amount);

    let contract_id = env.register(SubscriptionPayments, ());

    World {
        env,
        token,
        contract_id,
        subscriber,
        provider,
    }
}

impl World {
    fn client(&self) -> SorobanForgeSubscriptionPaymentsClient<'_> {
        SorobanForgeSubscriptionPaymentsClient::new(&self.env, &self.contract_id)
    }

    fn subscribe(&self, amount: i128, period: u64) -> u64 {
        self.client().subscribe(
            &self.subscriber,
            &self.provider,
            &self.token,
            &amount,
            &period,
        )
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn prop_last_charged_is_monotonic_and_advances_by_one_period(
        amount in 1i128..=100_000_i128,
        period in 1u64..=MAX_PERIOD,
        charges_count in 1u32..=10_u32,
    ) {
        let mint_total = amount.saturating_mul(charges_count as i128);
        let w = setup_world(mint_total);
        let id = w.subscribe(amount, period);

        let mut prev_last_charged = START;

        for i in 1..=charges_count {
            let target_time = START + period * (i as u64);
            w.env.ledger().set_timestamp(target_time);

            let billed = w.client().charge(&id);
            prop_assert_eq!(billed, amount);

            let sub = w.client().get_subscription(&id);
            prop_assert!(sub.last_charged > prev_last_charged, "last_charged must be strictly monotonic");
            prop_assert_eq!(sub.last_charged, START + period * (i as u64), "last_charged must advance by one period per charge");
            prev_last_charged = sub.last_charged;
        }
    }

    #[test]
    fn prop_charge_returns_zero_before_period_elapses(
        amount in 1i128..=MAX_AMOUNT,
        period in 2u64..=MAX_PERIOD,
        elapsed_delta in 1u64..=MAX_PERIOD,
    ) {
        let delta = elapsed_delta % period;
        if delta == 0 {
            return Ok(());
        }

        let w = setup_world(amount * 10);
        let id = w.subscribe(amount, period);

        w.env.ledger().set_timestamp(START + delta);

        let billed = w.client().charge(&id);
        prop_assert_eq!(billed, 0, "charge must return 0 before period elapses");

        let sub = w.client().get_subscription(&id);
        prop_assert_eq!(sub.last_charged, START, "last_charged must remain unchanged when charging early");
    }

    #[test]
    fn prop_cancelled_subscription_never_bills(
        amount in 1i128..=MAX_AMOUNT,
        period in 1u64..=MAX_PERIOD,
        cancel_delay in 0u64..=MAX_PERIOD,
        charge_delay in 1u64..=MAX_PERIOD,
    ) {
        let w = setup_world(amount * 10);
        let id = w.subscribe(amount, period);

        w.env.ledger().set_timestamp(START + cancel_delay);
        w.client().cancel(&id);

        let sub_after_cancel = w.client().get_subscription(&id);
        prop_assert_eq!(sub_after_cancel.status, SubscriptionStatus::Cancelled);

        w.env.ledger().set_timestamp(START + cancel_delay + charge_delay);
        let res = w.client().try_charge(&id);
        prop_assert!(res.is_err(), "charge on a cancelled subscription must return an error");
        let err = res.unwrap_err().unwrap();
        prop_assert_eq!(err, ForgeError::InvalidInput, "cancelled subscription must fail with InvalidInput");

        let sub_after_failed_charge = w.client().get_subscription(&id);
        prop_assert_eq!(sub_after_failed_charge.status, SubscriptionStatus::Cancelled);
    }
}
