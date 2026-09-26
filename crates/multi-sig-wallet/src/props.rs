//! Randomized invariant suite (proptest).
//!
//! The hand-written suite in `tests` (lib.rs) pins behaviour on known values;
//! this module tries to *falsify* the wallet's threshold and execute-once
//! claims over generated inputs. Three properties are exercised:
//!
//! **P1 — Threshold enforcement.** For a generated bounded sequence of
//! confirm actions over the fixed owner pool:
//!
//! ```text
//! execute succeeds  ⟺  distinct confirmations >= threshold  (and no rejections)
//! ```
//!
//! The generator can produce repeated pool indices, which the contract
//! rejects as duplicate signals with `ForgeError::InvalidInput` (not a host
//! abort). The test counts only the actions that actually landed and asserts
//! `execute` is rejected with `ForgeError::InvalidInput` below the threshold
//! and that the tx record still reads `Pending` afterwards — proving a
//! removed threshold check or miscounted confirmations would fail loudly.
//!
//! **P2 — Execute once.** Once `execute` succeeds the tx is terminal: a
//! second `execute` is rejected with `ForgeError::InvalidInput`, and the
//! observed side effects (cross-contract dispatch count against a mock
//! target) do not grow. This holds for every generated owner subset that
//! meets the threshold, so no replay path exists regardless of who confirms.
//!
//! **P3 — Distinct-owner confirmation counting.** For random (possibly
//! duplicate) confirmation sequences: the stored `confirmations` vector
//! contains each owner at most once, duplicates are rejected with
//! `ForgeError::InvalidInput` without mutating the record, and the
//! confirmation count equals the number of distinct owners who confirmed.
//! A regression in the dedup check (or a `contains` removed from
//! `confirm`) is caught here.
//!
//! **In-process limitation note.** Real Soroban authorization (`require_auth`)
//! is enforced by the host, which the test env stubs; the *authorization*
//! side of these invariants is covered separately by `authz.rs` (negative
//! suite) and by the mutation check documented there. These properties
//! deliberately run under `mock_all_auths` so they isolate the counting and
//! state-machine logic from authorization mechanics.
//!
//! Runs are deterministic (fixed strategy bounds, proptest's default seed);
//! a failure prints its case seed for replay. Override the case count with
//! `PROPTEST_CASES=n cargo test -p soroban-forge-multi-sig-wallet props`.

use crate::{MultiSigWallet, SorobanForgeMultiSigWalletClient, TxStatus};
use proptest::prelude::*;
use soroban_forge_shared_utils::ForgeError;
use soroban_forge_test_utils::TestAccounts;
use soroban_sdk::{contract, contractimpl, contracttype, Bytes, Env};

// The fixed pool of owner slots available for property tests. A signer is
// selected by index, so the generator can produce both unique and repeated
// picks — proving the count is exact regardless of how many duplicates
// occur. The wallet is initialized with exactly these owners so every pool
// index is a valid owner.
const OWNER_POOL_SIZE: usize = 5;

// -----------------------------------------------------------------------
// World: one fresh env, wallet (threshold 2), and a counting mock target
// -----------------------------------------------------------------------

struct World {
    env: Env,
    client: SorobanForgeMultiSigWalletClient<'static>,
    accounts: TestAccounts,
    /// Fixed owner pool; index into this selects the confirming owner.
    owners: std::vec::Vec<soroban_sdk::Address>,
    target: soroban_sdk::Address,
    target_id: soroban_sdk::Address,
}

/// A mock target that counts how many times it was dispatched, so P2 can
/// prove `execute` performs its cross-contract invocation at most once.
#[contract]
pub struct CountingTarget;

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
enum CountKey {
    Dispatches,
}

#[contractimpl]
impl CountingTarget {
    /// Record one dispatch. The wallet invokes `execute` with the payload.
    pub fn execute(env: Env, _payload: Bytes) {
        let key = CountKey::Dispatches;
        let current: u32 = env.storage().instance().get(&key).unwrap_or(0);
        env.storage().instance().set(&key, &(current + 1));
    }

    /// Number of dispatches recorded so far.
    pub fn dispatches(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&CountKey::Dispatches)
            .unwrap_or(0)
    }
}

fn setup_world() -> World {
    let env = Env::default();
    env.mock_all_auths();

    let contract_id = env.register(MultiSigWallet, ());
    let client = SorobanForgeMultiSigWalletClient::new(&env, &contract_id);
    let accounts = TestAccounts::generate(&env);

    // The owner pool IS the owner set: threshold 2 of 5, every pool index
    // is an owner, so rejection of a pick can only ever be a duplicate-
    // signal rejection (never a non-owner rejection). The pool is drawn
    // from TestAccounts (user1..arbiter) so `accounts.user1` — used as the
    // submitter — is always an owner.
    assert_eq!(OWNER_POOL_SIZE, 5, "pool must match the TestAccounts size");
    let owners = std::vec::Vec::from([
        accounts.user1.clone(),
        accounts.user2.clone(),
        accounts.user3.clone(),
        accounts.validator.clone(),
        accounts.arbiter.clone(),
    ]);
    let mut owner_vec = soroban_sdk::Vec::new(&env);
    for addr in &owners {
        owner_vec.push_back(addr.clone());
    }
    client.initialize(&owner_vec, &2_u32);

    let target = env.register(CountingTarget, ());
    let target_id = target.clone();

    World {
        env,
        client,
        accounts,
        owners,
        target,
        target_id,
    }
}

impl World {
    fn owner(&self, idx: usize) -> &soroban_sdk::Address {
        &self.owners[idx % OWNER_POOL_SIZE]
    }

    fn payload(&self) -> Bytes {
        Bytes::from_array(&self.env, &[0x0B, 0xAD, 0xC0, 0xDE])
    }

    /// Submit an opaque tx against the counting target; returns its id.
    fn submit(&self) -> u64 {
        self.client
            .submit(&self.accounts.user1, &self.target, &self.payload())
    }

    /// Dispatch count recorded by the counting target.
    fn dispatches(&self) -> u32 {
        let client = CountingTargetClient::new(&self.env, &self.target_id);
        client.dispatches()
    }
}

// -----------------------------------------------------------------------
// Strategies
// -----------------------------------------------------------------------

/// A single confirm action: (owner_pool_index).
fn confirm_action() -> impl Strategy<Value = usize> {
    0usize..OWNER_POOL_SIZE
}

/// A bounded sequence of up to 8 confirm actions (unique and repeated picks).
fn confirm_sequence() -> impl Strategy<Value = std::vec::Vec<usize>> {
    prop::collection::vec(confirm_action(), 0..=8)
}

// -----------------------------------------------------------------------
// P1 — Threshold enforcement
// -----------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// For any bounded sequence of confirm actions:
    ///
    /// ```text
    /// execute is rejected with InvalidInput while distinct confirmations
    /// are below the threshold, and the tx stays Pending afterwards
    /// ```
    ///
    /// Duplicate picks are rejected by the contract with
    /// `ForgeError::InvalidInput` and do not count toward the threshold.
    /// The test tracks the distinct owners who actually confirmed and
    /// asserts the execute outcome matches the threshold comparison
    /// exactly — a removed `confirmations.len() < threshold` check, an
    /// off-by-one in the comparison, or a double-counted duplicate
    /// confirmation all falsify this property.
    #[test]
    fn p1_execute_rejected_while_below_threshold(
        actions in confirm_sequence(),
    ) {
        let w = setup_world();
        let tx_id = w.submit();

        let mut confirmed: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();

        for idx in &actions {
            let owner = w.owner(*idx).clone();
            let res = w.client.try_confirm(&tx_id, &owner);
            match res {
                Ok(Ok(())) => {
                    // First signal from this pool slot — must land.
                    prop_assert!(
                        confirmed.insert(*idx),
                        "contract accepted a second confirmation from owner slot {idx}"
                    );
                }
                Err(Ok(ForgeError::InvalidInput)) => {
                    // Duplicate signal: must already be recorded.
                    prop_assert!(
                        confirmed.contains(idx),
                        "contract rejected a first-time confirmation from owner slot {idx}"
                    );
                }
                other => {
                    prop_assert!(false, "unexpected confirm result: {:?}", other);
                }
            }

            // Invariant while below threshold: execute must be rejected and
            // the tx must stay Pending. (Checked after every action so a
            // mid-sequence breach cannot hide between confirms.)
            if confirmed.len() < 2 {
                let exec = w.client.try_execute(&tx_id);
                prop_assert!(
                    matches!(exec, Err(Ok(ForgeError::InvalidInput))),
                    "execute must be rejected below threshold (confirmed={}), got {:?}",
                    confirmed.len(),
                    exec
                );
                let tx = w.client.try_get_tx(&tx_id).expect("outer ok").expect("contract ok");
                prop_assert_eq!(tx.status, TxStatus::Pending);
                prop_assert_eq!(tx.confirmations.len(), confirmed.len() as u32);
            }
        }

        // At the boundary (threshold == 2): execute must succeed if and
        // only if the distinct confirmation count reached the threshold.
        let exec = w.client.try_execute(&tx_id);
        if confirmed.len() >= 2 {
            prop_assert!(
                matches!(exec, Ok(Ok(()))),
                "execute must succeed at threshold (confirmed={}), got {:?}",
                confirmed.len(),
                exec
            );
        } else {
            prop_assert!(
                matches!(exec, Err(Ok(ForgeError::InvalidInput))),
                "execute must stay rejected below threshold, got {:?}",
                exec
            );
        }
    }
}

// -----------------------------------------------------------------------
// P2 — Execute once
// -----------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Once `execute` succeeds, the tx is terminal: a second `execute` is
    /// rejected with `ForgeError::InvalidInput` and the mock target's
    /// dispatch count does not grow — no replay path exists regardless of
    /// which owners confirmed (any threshold-meeting subset generated by
    /// `actions`).
    #[test]
    fn p2_transaction_executes_at_most_once(
        actions in confirm_sequence(),
    ) {
        let w = setup_world();
        let tx_id = w.submit();

        // Confirm with distinct owners until the threshold is met. The
        // random sequence seeds the order; when it does not reach the
        // threshold on its own, the remaining pool owners are added in a
        // deterministic order so every case executes at the threshold.
        let mut distinct = 0usize;
        for idx in &actions {
            if distinct >= 2 {
                break;
            }
            if confirmed_contains(&w, &w.owners[*idx % OWNER_POOL_SIZE]) {
                continue;
            }
            let owner = w.owner(*idx).clone();
            if w.client.try_confirm(&tx_id, &owner).is_ok() {
                distinct += 1;
            }
        }
        for slot in 0..OWNER_POOL_SIZE {
            if distinct >= 2 {
                break;
            }
            if confirmed_contains(&w, &w.owners[slot]) {
                continue;
            }
            if w.client.try_confirm(&tx_id, &w.owners[slot]).is_ok() {
                distinct += 1;
            }
        }
        prop_assert!(distinct >= 2, "generator must reach the threshold");

        w.client.execute(&tx_id);
        let dispatches_after_first = w.dispatches();

        // Replay attempts (from any account — execute is permissionless
        // once the threshold is met) must be rejected without re-dispatch.
        let replay = w.client.try_execute(&tx_id);
        prop_assert!(
            matches!(replay, Err(Ok(ForgeError::InvalidInput))),
            "second execute must be rejected, got {:?}",
            replay
        );
        prop_assert_eq!(w.dispatches(), dispatches_after_first, "target must not be re-invoked");

        let tx = w.client.try_get_tx(&tx_id).expect("outer ok").expect("contract ok");
        prop_assert_eq!(tx.status, TxStatus::Executed);
    }
}

/// Whether `owner` already appears in the stored confirmations vector.
fn confirmed_contains(w: &World, owner: &soroban_sdk::Address) -> bool {
    // The last submitted tx is the one under test; its id is the current
    // count. (Each property test uses exactly one tx.)
    let count = w
        .client
        .try_get_tx_count()
        .expect("outer ok")
        .expect("count ok");
    if count == 0 {
        return false;
    }
    let tx = w
        .client
        .try_get_tx(&count)
        .expect("outer ok")
        .expect("contract ok");
    tx.confirmations.contains(owner)
}

// -----------------------------------------------------------------------
// P3 — Distinct-owner confirmation counting
// -----------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// For any (possibly duplicate) confirmation sequence:
    ///
    /// ```text
    /// stored confirmations == distinct owners whose confirms landed
    /// ```
    ///
    /// Duplicate confirms are rejected with `ForgeError::InvalidInput`
    /// and leave the record untouched, so the vector can never contain an
    /// address twice. A removed `contains` dedup check, a confirmation
    /// written before the check, or an off-by-one in the push would all
    /// falsify this property.
    #[test]
    fn p3_confirmations_count_distinct_owners_exactly(
        actions in confirm_sequence(),
    ) {
        let w = setup_world();
        let tx_id = w.submit();

        let mut confirmed: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();

        for idx in &actions {
            let owner = w.owner(*idx).clone();
            let res = w.client.try_confirm(&tx_id, &owner);
            match res {
                Ok(Ok(())) => {
                    prop_assert!(
                        confirmed.insert(*idx),
                        "duplicate confirmation accepted for owner slot {idx}"
                    );
                }
                Err(Ok(ForgeError::InvalidInput)) => {
                    prop_assert!(
                        confirmed.contains(idx),
                        "first-time confirmation rejected for owner slot {idx}"
                    );
                }
                other => {
                    prop_assert!(false, "unexpected confirm result: {:?}", other);
                }
            }

            // After every action the stored vector must equal the distinct
            // set, in insertion order, with no duplicates.
            let tx = w.client.try_get_tx(&tx_id).expect("outer ok").expect("contract ok");
            prop_assert_eq!(
                tx.confirmations.len(),
                confirmed.len() as u32,
                "confirmation count must equal distinct successful confirmers"
            );
            for stored in tx.confirmations.iter() {
                let pos = w
                    .owners
                    .iter()
                    .position(|a| *a == stored)
                    .expect("confirmation from a non-owner is impossible in this world");
                prop_assert!(
                    confirmed.contains(&pos),
                    "stored confirmation from owner slot {pos} was never successfully confirmed"
                );
            }
        }
    }
}
