//! Randomized invariant suite (proptest).
//!
//! The hand-written unit suite pins behaviour on known, fixed values; this
//! module systematically tests the contract's fund-safety, arithmetic conservation,
//! and monotonicity invariants over large spaces of generated inputs.
//!
//! Four property invariants are exercised:
//!
//! **P1 — Total Conservation & Residue Safety.** For random amounts, cliffs,
//! and durations: across arbitrary sequences of timestamps and interleaved claims,
//! `claimed + remaining_claimable <= total_amount` holds at every point in time.
//! Rounding residue from floor-division never leaks tokens or causes payouts to
//! exceed `total_amount`. Upon full duration elapsed and final claim, the contract
//! retains 0 tokens and the beneficiary holds `total_amount` exactly.
//!
//! **P2 — Monotonicity.** For arbitrary monotonically non-decreasing timestamp
//! progressions: `vested(t)` is strictly monotonic non-decreasing ($t_a \le t_b \implies \text{vested}(t_a) \le \text{vested}(t_b)$),
//! cumulative `claimed` never decreases, and `claimable` increases monotonically
//! over time until `duration` (in the absence of claims).
//!
//! **P3 — Arbitrary Action Sequences.** For random sequences of time advancements
//! and claim attempts: `claim()` returns exactly the newly vested amount or `0`,
//! zero-amount claims make no token transfers, and total pool tokens (`beneficiary + contract`)
//! are conserved identically.
//!
//! **P4 — Tamper-Resilient Conservation.** An attacker attempting to mutate stored
//! schedule parameters (amount, claimed, start, cliff, duration) between calls can
//! never move value out of the pool beyond the token contract's balance.
//!
//! Runs are deterministic with reproducible seeds. Override the case count with
//! `PROPTEST_CASES=n cargo test -p soroban-forge-vesting props`.

use crate::{SorobanForgeVestingClient, Vesting, VestingSchedule, VestingStatus};
use proptest::prelude::*;
use soroban_forge_shared_utils::ForgeError;
use soroban_sdk::testutils::{Address as _, Ledger as _};
use soroban_sdk::token::{Client as TokenClient, StellarAssetClient};
use soroban_sdk::{Address, Env};

const START: u64 = 1_000_000;
const MAX_AMOUNT: i128 = 1_000_000_000_000;
const MAX_DURATION: u64 = 10 * 365 * 24 * 3600; // 10 years in seconds

// -----------------------------------------------------------------------
// World: one fresh env, SAC, vesting contract, and accounts per case
// -----------------------------------------------------------------------

struct World {
    env: Env,
    token: Address,
    vesting: Address,
    beneficiary: Address,
}

fn setup_world() -> World {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(START);

    let admin = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(admin);
    let token = sac.address();

    let vesting = env.register(Vesting, ());

    World {
        token,
        vesting,
        beneficiary: Address::generate(&env),
        env,
    }
}

impl World {
    fn vesting_client(&self) -> SorobanForgeVestingClient<'_> {
        SorobanForgeVestingClient::new(&self.env, &self.vesting)
    }

    fn token_client(&self) -> TokenClient<'_> {
        TokenClient::new(&self.env, &self.token)
    }

    fn mint_to_contract(&self, amount: i128) {
        StellarAssetClient::new(&self.env, &self.token).mint(&self.vesting, &amount);
    }

    fn create(&self, amount: i128, cliff: u64, duration: u64) -> u64 {
        self.vesting_client().create_schedule(
            &self.beneficiary,
            &self.token,
            &amount,
            &cliff,
            &duration,
        )
    }

    /// Balances of (beneficiary, contract) — the whole pool.
    fn pool(&self) -> (i128, i128) {
        let t = self.token_client();
        (t.balance(&self.beneficiary), t.balance(&self.vesting))
    }

    /// The conserved total: everything held by the contract and the beneficiary.
    fn pool_total(&self) -> i128 {
        let (b, c) = self.pool();
        b + c
    }
}

// -----------------------------------------------------------------------
// Strategies
// -----------------------------------------------------------------------

fn arb_schedule_params() -> impl Strategy<Value = (i128, u64, u64)> {
    (1i128..=MAX_AMOUNT, 1u64..=MAX_DURATION)
        .prop_flat_map(|(amount, duration)| (Just(amount), 0u64..=duration, Just(duration)))
}

// -----------------------------------------------------------------------
// P1 — Total Conservation & Residue Safety
// -----------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn p1_total_conservation_and_residue_safety(
        (total_amount, cliff, duration) in arb_schedule_params(),
        mut time_offsets in prop::collection::vec(0u64..=(MAX_DURATION * 2), 1..=8),
    ) {
        time_offsets.sort_unstable();

        let w = setup_world();
        w.mint_to_contract(total_amount);
        let id = w.create(total_amount, cliff, duration);

        let initial_pool = w.pool_total();
        prop_assert_eq!(initial_pool, total_amount, "initial pool must equal total_amount");

        let mut cumulative_claimed = 0i128;

        for offset in time_offsets {
            let current_time = START.saturating_add(offset);
            w.env.ledger().set_timestamp(current_time);

            let claimable = w.vesting_client().claimable(&id);
            prop_assert!(claimable >= 0, "claimable amount must never be negative");
            prop_assert!(
                cumulative_claimed + claimable <= total_amount,
                "claimed ({}) + claimable ({}) must not exceed total_amount ({})",
                cumulative_claimed,
                claimable,
                total_amount
            );

            let claimed_now = w.vesting_client().claim(&id);
            prop_assert_eq!(claimed_now, claimable, "claim() must return the exact claimable amount");
            cumulative_claimed += claimed_now;

            let (beneficiary_balance, contract_balance) = w.pool();
            prop_assert_eq!(
                beneficiary_balance,
                cumulative_claimed,
                "beneficiary balance must match cumulative claimed exactly"
            );
            prop_assert_eq!(
                contract_balance,
                total_amount - cumulative_claimed,
                "contract balance must match remaining unvested/unclaimed tokens exactly"
            );
            prop_assert_eq!(
                w.pool_total(),
                total_amount,
                "pool total must be invariant across claims"
            );
        }

        // Advance to full duration and perform final claim to verify residue safety
        w.env.ledger().set_timestamp(START.saturating_add(duration).saturating_add(1));
        let final_claimable = w.vesting_client().claimable(&id);
        let final_claimed = w.vesting_client().claim(&id);
        prop_assert_eq!(final_claimed, final_claimable);
        cumulative_claimed += final_claimed;

        let (final_b, final_c) = w.pool();
        prop_assert_eq!(cumulative_claimed, total_amount, "all tokens must be claimable at completion");
        prop_assert_eq!(final_b, total_amount, "beneficiary must receive exact total_amount with 0 token loss");
        prop_assert_eq!(final_c, 0, "contract balance must be drained to exactly 0");
        prop_assert_eq!(
            w.vesting_client().get_status(&id),
            VestingStatus::Completed,
            "status must be Completed after full claim"
        );
    }
}

// -----------------------------------------------------------------------
// P2 — Monotonicity Invariants
// -----------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn p2_monotonicity_over_time(
        (total_amount, cliff, duration) in arb_schedule_params(),
        mut offsets in prop::collection::vec(0u64..=(MAX_DURATION * 2), 2..=10),
    ) {
        offsets.sort_unstable();

        let w = setup_world();
        w.mint_to_contract(total_amount);
        let id = w.create(total_amount, cliff, duration);

        let mut prev_claimable = 0i128;
        let mut prev_time = 0u64;

        for offset in offsets {
            let current_time = START.saturating_add(offset);
            w.env.ledger().set_timestamp(current_time);

            let claimable = w.vesting_client().claimable(&id);

            if offset < cliff {
                prop_assert_eq!(claimable, 0, "before cliff, claimable must be 0");
                prop_assert_eq!(
                    w.vesting_client().get_status(&id),
                    VestingStatus::Locked,
                    "before cliff, status must be Locked"
                );
            } else {
                prop_assert!(
                    claimable >= prev_claimable,
                    "claimable ({}) at t={} must be >= previous claimable ({}) at t={}",
                    claimable,
                    current_time,
                    prev_claimable,
                    prev_time
                );
                if offset >= duration {
                    prop_assert_eq!(
                        claimable,
                        total_amount,
                        "at or after duration, claimable must equal total_amount"
                    );
                }
            }

            prev_claimable = claimable;
            prev_time = current_time;
        }
    }
}

// -----------------------------------------------------------------------
// P3 — Arbitrary Interleaved Action Sequences
// -----------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn p3_interleaved_action_sequences(
        (total_amount, cliff, duration) in arb_schedule_params(),
        mut actions in prop::collection::vec((0u64..=(MAX_DURATION * 2), prop::bool::ANY), 1..=12),
    ) {
        actions.sort_by_key(|(offset, _)| *offset);

        let w = setup_world();
        w.mint_to_contract(total_amount);
        let id = w.create(total_amount, cliff, duration);

        let mut cumulative_claimed = 0i128;

        for (offset, do_claim) in actions {
            let current_time = START.saturating_add(offset);
            w.env.ledger().set_timestamp(current_time);

            let pool_before = w.pool_total();
            let claimable = w.vesting_client().claimable(&id);

            if do_claim {
                let claimed = w.vesting_client().claim(&id);
                prop_assert_eq!(claimed, claimable, "claim() must pay exact claimable amount");
                cumulative_claimed += claimed;
            }

            let pool_after = w.pool_total();
            prop_assert_eq!(pool_before, pool_after, "pool total must be conserved across action");
            prop_assert_eq!(pool_after, total_amount, "pool total must equal total_amount");

            let (b_bal, c_bal) = w.pool();
            prop_assert_eq!(b_bal, cumulative_claimed);
            prop_assert_eq!(c_bal, total_amount - cumulative_claimed);
        }
    }
}

// -----------------------------------------------------------------------
// P4 — Tamper-Resilient Conservation
// -----------------------------------------------------------------------

fn mutate_vesting_record(w: &World, id: u64, kind: u8, delta: i128) {
    let key = crate::DataKey::Schedule(id);
    w.env.as_contract(&w.vesting, || {
        if let Some(mut rec) = w.env.storage().instance().get::<_, VestingSchedule>(&key) {
            match kind {
                // Fuzz total_amount
                0 => {
                    rec.total_amount = rec.total_amount.wrapping_add(delta).max(1);
                }
                // Fuzz claimed
                1 => {
                    rec.claimed = rec.claimed.wrapping_add(delta).max(0);
                }
                // Fuzz cliff
                2 => {
                    rec.cliff = (rec.cliff as i128).wrapping_add(delta).max(0) as u64;
                }
                // Fuzz duration
                3 => {
                    rec.duration = (rec.duration as i128).wrapping_add(delta).max(1) as u64;
                }
                _ => {}
            }
            w.env.storage().instance().set(&key, &rec);
        }
    });
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn p4_tampered_storage_never_violates_pool_conservation(
        (total_amount, cliff, duration) in arb_schedule_params(),
        mutations in prop::collection::vec((0u8..=3, -10_000i128..=10_000i128), 0..=4),
    ) {
        let w = setup_world();
        w.mint_to_contract(total_amount);
        let id = w.create(total_amount, cliff, duration);

        for (kind, delta) in mutations {
            mutate_vesting_record(&w, id, kind, delta);
        }

        w.env.ledger().set_timestamp(START.saturating_add(duration).saturating_add(10));

        let client = w.vesting_client();
        let pool_before = w.pool_total();

        match client.try_claim(&id) {
            Ok(Ok(_paid)) => {
                let (b, c) = w.pool();
                prop_assert!(c >= 0, "contract balance must not be negative");
                prop_assert!(b >= 0, "beneficiary balance must not be negative");
                prop_assert_eq!(b + c, pool_before, "pool total must be conserved on successful claim");
            }
            Err(Ok(ForgeError::TokenTransferFailed)) => {
                // Over-payment prevented by token balance check; pool unchanged
                prop_assert_eq!(w.pool_total(), pool_before);
            }
            Err(Ok(ForgeError::ArithmeticOverflow)) => {
                // Arithmetic overflow prevented; pool unchanged
                prop_assert_eq!(w.pool_total(), pool_before);
            }
            Err(Ok(ForgeError::NotFound)) => {}
            other => panic!("unexpected outcome under tampering: {:?}", other),
        }
    }
}
