//! Randomized invariant suite (proptest).
//!
//! The hand-written suite in `tests` (lib.rs) pins behaviour on known values;
//! this module tries to *falsify* the governance contract's vote-counting and
//! lifecycle claims over generated inputs. Three properties are exercised:
//!
//! **P1 — Vote tally.** For a generated bounded sequence of vote actions over
//! a fixed voter pool:
//!
//! ```text
//! for_votes + against_votes == number of distinct voters who successfully voted
//! ```
//!
//! The generator can produce repeated pool indices, which the contract rejects
//! as duplicate votes with `ForgeError::InvalidInput` (not a host abort).
//! The test counts only the actions that actually landed and asserts the tally
//! matches exactly. This proves the contract never double-counts, drops a
//! valid vote, or miscategorises for/against.
//!
//! **P2 — Deadline / lifecycle.** A proposal that has not yet passed its
//! `voting_ends` timestamp must remain `Active` and must not be finalisable:
//! `execute` before the deadline returns `ForgeError::InvalidInput` and the
//! proposal stays `Active`. The deadline is exercised using the actual
//! `Ledger::set_timestamp` mechanism used by the existing tests.
//!
//! **Soroban host limitation note.** The Soroban test environment has no
//! automatic time advancement; only explicit `set_timestamp` calls move the
//! clock. There is no way to exercise a "voting window that closes mid-
//! sequence" without controlling time from test code. The invariant therefore
//! probes the exact boundary: at `voting_ends - 1` the proposal is still
//! `Active` and `execute` is rejected; the post-deadline path is covered
//! comprehensively by P3. This mirrors the approach used by the existing
//! governance tests in lib.rs.
//!
//! **P3 — Finalization outcome.** After voting ends, `execute` must produce:
//!
//! ```text
//! for_votes > against_votes → Succeeded
//! for_votes <= against_votes → Defeated   (includes ties and zero-vote proposals)
//! ```
//!
//! Specifically:
//! - 0 for / 0 against → `Defeated`
//! - equal for / against → `Defeated`
//! - strict for majority → `Succeeded`
//! - strict against majority → `Defeated`
//!
//! Four deterministic companion unit tests pin each boundary case explicitly
//! (see end of file), complementing the randomized P3 suite.
//!
//! All properties run against a real Stellar Asset Contract for bond custody.
//! Runs are deterministic (fixed strategy bounds, proptest's default seed);
//! a failure prints its case seed for replay. Override the case count with
//! `PROPTEST_CASES=n cargo test -p soroban-forge-dao-governance props`.

use crate::{DaoGovernance, ProposalState, SorobanForgeDaoGovernanceClient};
use proptest::prelude::*;
use soroban_forge_shared_utils::ForgeError;
use soroban_forge_test_utils::{MockTarget, TestAccounts};
use soroban_sdk::testutils::{Address as _, Ledger as _};
use soroban_sdk::token::StellarAssetClient;
use soroban_sdk::{Address, Bytes, Env};

const START: u64 = 1_000_000;
const DURATION: u64 = 86_400;
const BOND: i128 = 100;
const FUNDS: i128 = 10_000;

// The fixed pool of voter slots available for property tests. A voter is
// selected by index, so the generator can produce both unique and repeated
// voter picks, proving the tally is exact regardless of how many duplicates
// occur.
const VOTER_POOL_SIZE: usize = 8;

// -----------------------------------------------------------------------
// World: one fresh env, SAC, DAO contract, target, and funded accounts
// -----------------------------------------------------------------------

struct World {
    env: Env,
    contract_id: Address,
    accounts: TestAccounts,
    target: Address,
    /// Fixed pool of extra voters beyond the TestAccounts set.
    voters: std::vec::Vec<Address>,
}

fn setup_world() -> World {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(START);

    let admin = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(admin);
    let token = sac.address();
    let token_admin = StellarAssetClient::new(&env, &token);

    let contract_id = env.register(DaoGovernance, ());
    let client = SorobanForgeDaoGovernanceClient::new(&env, &contract_id);

    let accounts = TestAccounts::generate(&env);
    client.configure_bond(&token, &BOND, &accounts.deployer);
    token_admin.mint(&accounts.user1, &FUNDS);

    let mut voters = std::vec::Vec::with_capacity(VOTER_POOL_SIZE);
    for _ in 0..VOTER_POOL_SIZE {
        voters.push(Address::generate(&env));
    }

    let target = env.register(MockTarget, ());

    World {
        env,
        contract_id,
        accounts,
        target,
        voters,
    }
}

impl World {
    fn client(&self) -> SorobanForgeDaoGovernanceClient<'_> {
        SorobanForgeDaoGovernanceClient::new(&self.env, &self.contract_id)
    }

    fn payload(&self) -> Bytes {
        Bytes::from_array(&self.env, &[0xC0, 0xDE, 0x00, 0xFF])
    }

    /// Create a fresh proposal from `user1` and return its id.
    fn propose(&self) -> u64 {
        self.client().propose(
            &self.accounts.user1,
            &self.target,
            &self.payload(),
            &DURATION,
        )
    }

    /// Return the voter at `idx % VOTER_POOL_SIZE` from the pool.
    fn voter(&self, idx: usize) -> &Address {
        &self.voters[idx % VOTER_POOL_SIZE]
    }
}

// -----------------------------------------------------------------------
// Strategies
// -----------------------------------------------------------------------

/// A single vote action: (voter_pool_index, support).
fn vote_action() -> impl Strategy<Value = (usize, bool)> {
    (0usize..VOTER_POOL_SIZE, prop::bool::ANY)
}

/// A bounded sequence of up to 16 vote actions (unique and repeated picks).
fn vote_sequence() -> impl Strategy<Value = std::vec::Vec<(usize, bool)>> {
    prop::collection::vec(vote_action(), 0..=16)
}

// -----------------------------------------------------------------------
// P1 — Vote tally invariant
// -----------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// For any bounded sequence of vote actions over a fixed voter pool:
    ///
    /// ```text
    /// for_votes + against_votes == distinct voters who successfully voted
    /// ```
    ///
    /// Duplicate picks return `ForgeError::InvalidInput` without touching the
    /// tally. The test tracks which pool slots have voted and asserts the
    /// counts match exactly — proving the contract never double-counts, drops
    /// a valid vote, or miscategorises for/against.
    #[test]
    fn p1_vote_tally_equals_distinct_successful_voters(
        actions in vote_sequence(),
    ) {
        let w = setup_world();
        let proposal_id = w.propose();

        let mut voted: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
        let mut expected_for: i128 = 0;
        let mut expected_against: i128 = 0;

        for (idx, support) in &actions {
            let voter = w.voter(*idx);
            let res = w.client().try_vote(&proposal_id, voter, support);

            match res {
                Ok(Ok(())) => {
                    // First vote from this pool slot — must count.
                    prop_assert!(
                        voted.insert(*idx),
                        "contract accepted a second vote from voter slot {idx}"
                    );
                    if *support {
                        expected_for += 1;
                    } else {
                        expected_against += 1;
                    }
                }
                Err(Ok(ForgeError::InvalidInput)) => {
                    // Duplicate vote: the contract correctly rejected it.
                    // The voter must already be in the voted set.
                    prop_assert!(
                        voted.contains(idx),
                        "contract rejected a first-time vote from voter slot {idx} with InvalidInput"
                    );
                }
                other => {
                    prop_assert!(
                        false,
                        "unexpected vote result: {:?}", other
                    );
                }
            }
        }

        let proposal = w.client().get_proposal(&proposal_id);
        prop_assert_eq!(
            proposal.for_votes,
            expected_for,
            "for_votes mismatch: expected {}",
            expected_for
        );
        prop_assert_eq!(
            proposal.against_votes,
            expected_against,
            "against_votes mismatch: expected {}",
            expected_against
        );
        prop_assert_eq!(
            proposal.for_votes + proposal.against_votes,
            voted.len() as i128,
            "total votes must equal distinct successful voters"
        );
    }
}

// -----------------------------------------------------------------------
// P2 — Deadline / lifecycle invariant
// -----------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// A proposal that has not yet reached its `voting_ends` timestamp must
    /// remain `Active` and must not be finalisable.
    ///
    /// The test sets the ledger timestamp to `voting_ends - 1` (one second
    /// before the deadline), confirms the state is still `Active`, and
    /// confirms that `execute` is rejected with `ForgeError::InvalidInput`.
    ///
    /// **Limitation (Soroban host):** the Soroban test environment has no
    /// automatic time advancement; only explicit `set_timestamp` calls move
    /// the clock. The test therefore probes the exact pre-deadline boundary
    /// rather than simulating mid-sequence expiry. The post-deadline path is
    /// covered comprehensively by P3.
    #[test]
    fn p2_proposal_stays_active_before_deadline(
        actions in vote_sequence(),
    ) {
        let w = setup_world();
        let proposal_id = w.propose();

        // Cast votes while the voting window is still open (timestamp is START,
        // well before START + DURATION).
        for (idx, support) in &actions {
            let voter = w.voter(*idx);
            // Duplicates return InvalidInput; that is expected and fine here.
            let _ = w.client().try_vote(&proposal_id, voter, support);
        }

        // Advance to one second before the deadline — still within the window.
        w.env.ledger().set_timestamp(START + DURATION - 1);
        let proposal = w.client().get_proposal(&proposal_id);
        prop_assert_eq!(
            proposal.state,
            ProposalState::Active,
            "proposal must remain Active before voting_ends"
        );

        // execute before the deadline must be rejected with InvalidInput.
        let res = w.client().try_execute(&proposal_id);
        prop_assert!(
            matches!(res, Err(Ok(ForgeError::InvalidInput))),
            "execute before deadline must return InvalidInput, got {:?}", res
        );

        // The state must be unchanged after the rejected execute.
        let proposal = w.client().get_proposal(&proposal_id);
        prop_assert_eq!(
            proposal.state,
            ProposalState::Active,
            "proposal must remain Active after a rejected pre-deadline execute"
        );
    }
}

// -----------------------------------------------------------------------
// P3 — Finalization outcome invariant
// -----------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// After voting ends, `execute` must transition the proposal to the
    /// correct terminal state:
    ///
    /// ```text
    /// for_votes > against_votes → Succeeded
    /// for_votes <= against_votes → Defeated   (includes ties and zero votes)
    /// ```
    ///
    /// The zero-vote case (0 for / 0 against → Defeated) and the tie case
    /// (equal for / equal against → Defeated) are both reachable by the
    /// generator and asserted explicitly in the deterministic supplement below.
    #[test]
    fn p3_finalization_outcome_matches_vote_tally(
        actions in vote_sequence(),
    ) {
        let w = setup_world();
        let proposal_id = w.propose();

        // Cast votes.
        for (idx, support) in &actions {
            let voter = w.voter(*idx);
            // Duplicates return InvalidInput; ignore them.
            let _ = w.client().try_vote(&proposal_id, voter, support);
        }

        // Snapshot the final tally before advancing time.
        let snapshot = w.client().get_proposal(&proposal_id);
        let for_votes = snapshot.for_votes;
        let against_votes = snapshot.against_votes;

        // Advance past the deadline so execute is allowed.
        w.env.ledger().set_timestamp(START + DURATION + 1);

        let res = w.client().try_execute(&proposal_id);
        prop_assert!(
            matches!(res, Ok(Ok(()))),
            "execute after deadline must succeed, got {:?}", res
        );

        let after = w.client().get_proposal(&proposal_id);
        let expected_state = if for_votes > against_votes {
            ProposalState::Succeeded
        } else {
            ProposalState::Defeated
        };
        prop_assert_eq!(
            after.state,
            expected_state,
            "finalization: for={} against={}",
            for_votes,
            against_votes
        );
    }
}

// -----------------------------------------------------------------------
// P3 supplement — exhaustive boundary cases (deterministic)
// -----------------------------------------------------------------------
// These four unit tests are deterministic complements to the P3 proptest
// suite, pinning each corner case the issue requires explicitly:
//
//   0 for / 0 against → Defeated
//   equal for / against → Defeated
//   strict for majority → Succeeded
//   strict against majority → Defeated

#[test]
fn p3_zero_votes_is_defeated() {
    let w = setup_world();
    let proposal_id = w.propose();
    // No votes cast.
    w.env.ledger().set_timestamp(START + DURATION + 1);
    w.client().execute(&proposal_id);
    assert_eq!(
        w.client().get_proposal(&proposal_id).state,
        ProposalState::Defeated,
        "0 for / 0 against must produce Defeated"
    );
}

#[test]
fn p3_tie_is_defeated() {
    let w = setup_world();
    let proposal_id = w.propose();
    w.client().vote(&proposal_id, w.voter(0), &true);
    w.client().vote(&proposal_id, w.voter(1), &false);
    w.env.ledger().set_timestamp(START + DURATION + 1);
    w.client().execute(&proposal_id);
    assert_eq!(
        w.client().get_proposal(&proposal_id).state,
        ProposalState::Defeated,
        "1 for / 1 against (tie) must produce Defeated"
    );
}

#[test]
fn p3_for_majority_is_succeeded() {
    let w = setup_world();
    let proposal_id = w.propose();
    w.client().vote(&proposal_id, w.voter(0), &true);
    w.client().vote(&proposal_id, w.voter(1), &true);
    w.client().vote(&proposal_id, w.voter(2), &false);
    w.env.ledger().set_timestamp(START + DURATION + 1);
    w.client().execute(&proposal_id);
    assert_eq!(
        w.client().get_proposal(&proposal_id).state,
        ProposalState::Succeeded,
        "2 for / 1 against must produce Succeeded"
    );
}

#[test]
fn p3_against_majority_is_defeated() {
    let w = setup_world();
    let proposal_id = w.propose();
    w.client().vote(&proposal_id, w.voter(0), &true);
    w.client().vote(&proposal_id, w.voter(1), &false);
    w.client().vote(&proposal_id, w.voter(2), &false);
    w.env.ledger().set_timestamp(START + DURATION + 1);
    w.client().execute(&proposal_id);
    assert_eq!(
        w.client().get_proposal(&proposal_id).state,
        ProposalState::Defeated,
        "1 for / 2 against must produce Defeated"
    );
}
