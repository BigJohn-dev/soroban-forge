#![no_std]

//! # Soroban Forge — DAO Governance contract
//!
//! A minimal on-chain governance primitive: members create proposals, cast one
//! vote each (`for`/`against`, tallied in governance-token units), and a
//! proposal is finalised once voting ends — passing when it has a strict
//! majority of `for` votes.
//!
//! Lifecycle:
//!
//! ```text
//! propose (voting_ends = now + duration)
//!   --> Active --vote × n--> voting ends
//!   --> execute: for > against ? Succeeded : Defeated
//!   --> cancel (proposer only): Cancelled (terminal)
//! ```
//!
//! `Cancelled` is terminal: a withdrawn proposal can never be voted on,
//! executed, or re-opened. Cancellation is permitted at any point before
//! execution — even after voting ends and quorum is met — so a proposer can
//! stop a proposal that is clearly failing or was created in error.
//!
//! Authorization model:
//! - `propose` requires the proposer.
//! - `vote` requires the voter and is one-vote-per-voter per proposal.
//! - `execute` may be called by anyone, but only once voting has ended.
//! - `cancel_proposal` requires the original proposer and freezes a
//!   not-yet-executed proposal.
//! - `get_proposal` is a read-only view.
//!
//! The `Queued` state is reserved for an optional timelock that lands in a
//! follow-up; it is not reachable through the current public interface.
//! Weighted voting by governance-token balance and action execution are
//! intentionally out of scope for this iteration: the contract tracks
//! proposals, votes, and timing, not balances.

#[cfg(test)]
extern crate std;

use soroban_forge_shared_utils::ForgeError;
use soroban_sdk::{contract, contractclient, contractimpl, contracttype, Address, Bytes, Env};

/// Public interface for the Soroban Forge DAO governance contract.
#[contractclient(name = "SorobanForgeDaoGovernanceClient")]
pub trait SorobanForgeDaoGovernance {
    /// Create a new proposal with an encoded action payload.
    ///
    /// `duration` (seconds) defines how long voting stays open. Returns the
    /// stable proposal id.
    fn propose(
        env: Env,
        proposer: Address,
        action: Bytes,
        duration: u64,
    ) -> Result<u64, soroban_forge_shared_utils::ForgeError>;

    /// Cast `voter`'s vote (for/against) on `proposal_id`. One vote per voter.
    fn vote(
        env: Env,
        proposal_id: u64,
        voter: Address,
        support: bool,
    ) -> Result<(), soroban_forge_shared_utils::ForgeError>;

    /// Finalise a proposal once voting has ended.
    fn execute(env: Env, proposal_id: u64) -> Result<(), soroban_forge_shared_utils::ForgeError>;

    /// Withdraw a proposal that has not yet been executed.
    ///
    /// Only the original proposer may cancel. Once cancelled the proposal is
    /// frozen against further votes and cannot be executed.
    fn cancel_proposal(
        env: Env,
        proposal_id: u64,
        proposer: Address,
    ) -> Result<(), soroban_forge_shared_utils::ForgeError>;

    /// Read a stored proposal by id (read-only view).
    fn get_proposal(
        env: Env,
        proposal_id: u64,
    ) -> Result<Proposal, soroban_forge_shared_utils::ForgeError>;
}

/// Lifecycle state of a governance proposal.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProposalState {
    /// Open for voting.
    Active,
    /// Approved and executed.
    Succeeded,
    /// Rejected or expired.
    Defeated,
    /// Queued for delayed execution (optional timelock).
    Queued,
    /// Withdrawn by the proposer before execution; terminal and immutable.
    Cancelled,
}

/// A single governance proposal.
#[contracttype]
#[derive(Clone, Debug)]
pub struct Proposal {
    /// Stable identifier assigned at creation.
    pub proposal_id: u64,
    /// Address that created the proposal.
    pub proposer: Address,
    /// Encoded action to execute on success.
    pub action: Bytes,
    /// Tally of "for" votes (in governance-token units).
    pub for_votes: i128,
    /// Tally of "against" votes (in governance-token units).
    pub against_votes: i128,
    /// Ledger timestamp at which voting closes.
    pub voting_ends: u64,
    /// Current state.
    pub state: ProposalState,
}

/// Instance-storage keys.
#[contracttype]
enum DataKey {
    /// The proposal record for `u64` id.
    Proposal(u64),
    /// Marks that `voter` has already voted on `proposal_id`.
    Vote(u64, Address),
    /// Monotonic proposal id counter.
    Count,
}

/// The deployable DAO governance contract.
#[contract]
pub struct DaoGovernance;

#[contractimpl]
impl DaoGovernance {
    /// Create a new proposal and return its stable id.
    ///
    /// Requires `duration > 0`. The proposer is authorized at creation time.
    pub fn propose(
        env: Env,
        proposer: Address,
        action: Bytes,
        duration: u64,
    ) -> Result<u64, ForgeError> {
        if duration == 0 {
            return Err(ForgeError::InvalidInput);
        }
        proposer.require_auth();

        let proposal_id = Self::next_id(&env)?;
        let voting_ends = env
            .ledger()
            .timestamp()
            .checked_add(duration)
            .ok_or(ForgeError::ArithmeticOverflow)?;
        let proposal = Proposal {
            proposal_id,
            proposer,
            action,
            for_votes: 0,
            against_votes: 0,
            voting_ends,
            state: ProposalState::Active,
        };
        env.storage()
            .instance()
            .set(&DataKey::Proposal(proposal_id), &proposal);
        Ok(proposal_id)
    }

    /// Cast a vote on an active proposal.
    ///
    /// Requires the voter. Each voter may vote exactly once; voting is closed
    /// once the deadline (`voting_ends`) passes.
    pub fn vote(
        env: Env,
        proposal_id: u64,
        voter: Address,
        support: bool,
    ) -> Result<(), ForgeError> {
        let mut proposal = Self::get_proposal_impl(&env, proposal_id)?;
        if proposal.state != ProposalState::Active {
            return Err(ForgeError::InvalidInput);
        }
        if env.ledger().timestamp() >= proposal.voting_ends {
            return Err(ForgeError::DeadlineReached);
        }
        voter.require_auth();

        let vote_key = DataKey::Vote(proposal_id, voter);
        if env.storage().instance().has(&vote_key) {
            return Err(ForgeError::InvalidInput);
        }

        if support {
            proposal.for_votes = proposal
                .for_votes
                .checked_add(1)
                .ok_or(ForgeError::ArithmeticOverflow)?;
        } else {
            proposal.against_votes = proposal
                .against_votes
                .checked_add(1)
                .ok_or(ForgeError::ArithmeticOverflow)?;
        }
        env.storage().instance().set(&vote_key, &true);
        env.storage()
            .instance()
            .set(&DataKey::Proposal(proposal_id), &proposal);
        Ok(())
    }

    /// Finalise a proposal once voting has ended.
    ///
    /// Callable by anyone after the deadline. The proposal passes (`Succeeded`)
    /// on a strict majority of `for` votes, otherwise it is `Defeated`.
    pub fn execute(env: Env, proposal_id: u64) -> Result<(), ForgeError> {
        let mut proposal = Self::get_proposal_impl(&env, proposal_id)?;
        if proposal.state != ProposalState::Active {
            return Err(ForgeError::InvalidInput);
        }
        if env.ledger().timestamp() < proposal.voting_ends {
            return Err(ForgeError::InvalidInput);
        }

        proposal.state = if proposal.for_votes > proposal.against_votes {
            ProposalState::Succeeded
        } else {
            ProposalState::Defeated
        };
        env.storage()
            .instance()
            .set(&DataKey::Proposal(proposal_id), &proposal);
        Ok(())
    }

    /// Withdraw an active proposal before it is executed.
    ///
    /// Requires the original proposer. A proposal may be cancelled even after
    /// voting ends and quorum is met, as long as it has not been executed (or
    /// already cancelled). A cancelled proposal is terminal: further votes and
    /// execution are rejected.
    ///
    /// * [`ForgeError::NotFound`] — no proposal with this id.
    /// * [`ForgeError::Unauthorized`] — `proposer` is not the original proposer.
    /// * [`ForgeError::InvalidInput`] — the proposal is no longer `Active`.
    pub fn cancel_proposal(
        env: Env,
        proposal_id: u64,
        proposer: Address,
    ) -> Result<(), ForgeError> {
        let mut proposal = Self::get_proposal_impl(&env, proposal_id)?;
        if proposal.state != ProposalState::Active {
            return Err(ForgeError::InvalidInput);
        }
        if proposer != proposal.proposer {
            return Err(ForgeError::Unauthorized);
        }
        proposer.require_auth();

        proposal.state = ProposalState::Cancelled;
        env.storage()
            .instance()
            .set(&DataKey::Proposal(proposal_id), &proposal);
        Ok(())
    }

    /// Read a stored proposal by id (read-only view).
    pub fn get_proposal(env: Env, proposal_id: u64) -> Result<Proposal, ForgeError> {
        Self::get_proposal_impl(&env, proposal_id)
    }

    /// Allocate the next monotonic proposal id.
    fn next_id(env: &Env) -> Result<u64, ForgeError> {
        let count: u64 = env.storage().instance().get(&DataKey::Count).unwrap_or(0);
        let id = count.checked_add(1).ok_or(ForgeError::ArithmeticOverflow)?;
        env.storage().instance().set(&DataKey::Count, &id);
        Ok(id)
    }

    fn get_proposal_impl(env: &Env, proposal_id: u64) -> Result<Proposal, ForgeError> {
        env.storage()
            .instance()
            .get(&DataKey::Proposal(proposal_id))
            .ok_or(ForgeError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_forge_test_utils::TestAccounts;
    use soroban_sdk::testutils::Ledger as _;
    use soroban_sdk::{Bytes, Env};

    const START: u64 = 1_000_000;
    const DURATION: u64 = 86_400;

    /// Build a fresh env with mocked auths, a registered contract, a pending
    /// proposal, and named accounts.
    macro_rules! setup {
        () => {{
            let env = Env::default();
            env.mock_all_auths();
            env.ledger().set_timestamp(START);
            let contract_id = env.register(DaoGovernance, ());
            let client = SorobanForgeDaoGovernanceClient::new(&env, &contract_id);
            let accounts = TestAccounts::generate(&env);
            let proposal_id = client.propose(&accounts.user1, &payload(&env), &DURATION);
            (env, client, accounts, proposal_id)
        }};
    }

    fn payload(env: &Env) -> Bytes {
        Bytes::from_array(env, &[0xC0, 0xDE, 0x00, 0xFF])
    }

    #[test]
    fn propose_succeeds_and_is_active() {
        let (_env, client, accounts, proposal_id) = setup!();
        let proposal = client.get_proposal(&proposal_id);
        assert_eq!(proposal.proposer, accounts.user1);
        assert_eq!(proposal.state, ProposalState::Active);
        assert_eq!(proposal.for_votes, 0);
        assert_eq!(proposal.against_votes, 0);
    }

    #[test]
    fn propose_assigns_distinct_ids() {
        let (env, client, accounts, _id) = setup!();
        let id2 = client.propose(&accounts.user2, &payload(&env), &DURATION);
        let id3 = client.propose(&accounts.user3, &payload(&env), &DURATION);
        assert_ne!(id2, id3);
    }

    #[test]
    fn propose_rejects_zero_duration() {
        let (env, client, accounts, _id) = setup!();
        let err = client
            .try_propose(&accounts.user1, &payload(&env), &0_u64)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn vote_records_support() {
        let (_env, client, accounts, proposal_id) = setup!();
        client.vote(&proposal_id, &accounts.user2, &true);
        let proposal = client.get_proposal(&proposal_id);
        assert_eq!(proposal.for_votes, 1);
        assert_eq!(proposal.against_votes, 0);
    }

    #[test]
    fn vote_records_against() {
        let (_env, client, accounts, proposal_id) = setup!();
        client.vote(&proposal_id, &accounts.user2, &false);
        let proposal = client.get_proposal(&proposal_id);
        assert_eq!(proposal.for_votes, 0);
        assert_eq!(proposal.against_votes, 1);
    }

    #[test]
    fn vote_twice_is_invalid() {
        let (_env, client, accounts, proposal_id) = setup!();
        client.vote(&proposal_id, &accounts.user2, &true);
        let err = client
            .try_vote(&proposal_id, &accounts.user2, &true)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn vote_after_deadline_is_rejected() {
        let (env, client, accounts, proposal_id) = setup!();
        env.ledger().set_timestamp(START + DURATION);
        let err = client
            .try_vote(&proposal_id, &accounts.user2, &true)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::DeadlineReached);
    }

    #[test]
    fn vote_missing_proposal_is_not_found() {
        let (_env, client, accounts, _id) = setup!();
        let err = client
            .try_vote(&999, &accounts.user2, &true)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::NotFound);
    }

    #[test]
    fn execute_before_deadline_is_invalid() {
        let (_env, client, _accounts, proposal_id) = setup!();
        let err = client.try_execute(&proposal_id).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn execute_after_deadline_passes_majority() {
        let (env, client, accounts, proposal_id) = setup!();
        client.vote(&proposal_id, &accounts.user2, &true);
        env.ledger().set_timestamp(START + DURATION + 1);
        client.execute(&proposal_id);
        assert_eq!(
            client.get_proposal(&proposal_id).state,
            ProposalState::Succeeded
        );
    }

    #[test]
    fn execute_after_deadline_defeats_minority() {
        let (env, client, accounts, proposal_id) = setup!();
        client.vote(&proposal_id, &accounts.user2, &false);
        client.vote(&proposal_id, &accounts.user3, &true);
        client.vote(&proposal_id, &accounts.validator, &false);
        env.ledger().set_timestamp(START + DURATION + 1);
        client.execute(&proposal_id);
        assert_eq!(
            client.get_proposal(&proposal_id).state,
            ProposalState::Defeated
        );
    }

    #[test]
    fn execute_tie_is_defeated() {
        let (env, client, accounts, proposal_id) = setup!();
        client.vote(&proposal_id, &accounts.user2, &true);
        client.vote(&proposal_id, &accounts.user3, &false);
        env.ledger().set_timestamp(START + DURATION + 1);
        client.execute(&proposal_id);
        assert_eq!(
            client.get_proposal(&proposal_id).state,
            ProposalState::Defeated
        );
    }

    #[test]
    fn execute_no_votes_is_defeated() {
        let (env, client, _accounts, proposal_id) = setup!();
        env.ledger().set_timestamp(START + DURATION + 1);
        client.execute(&proposal_id);
        assert_eq!(
            client.get_proposal(&proposal_id).state,
            ProposalState::Defeated
        );
    }

    #[test]
    fn execute_twice_is_invalid() {
        let (env, client, accounts, proposal_id) = setup!();
        client.vote(&proposal_id, &accounts.user2, &true);
        env.ledger().set_timestamp(START + DURATION + 1);
        client.execute(&proposal_id);
        let err = client.try_execute(&proposal_id).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn execute_missing_proposal_is_not_found() {
        let (_env, client, _accounts, _id) = setup!();
        let err = client.try_execute(&999).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::NotFound);
    }

    #[test]
    fn get_proposal_missing_is_not_found() {
        let (_env, client, _accounts, _id) = setup!();
        let err = client.try_get_proposal(&999).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::NotFound);
    }

    #[test]
    fn cancel_active_proposal_succeeds() {
        let (_env, client, accounts, proposal_id) = setup!();
        client.cancel_proposal(&proposal_id, &accounts.user1);
        assert_eq!(
            client.get_proposal(&proposal_id).state,
            ProposalState::Cancelled
        );
    }

    #[test]
    fn cancel_immediately_after_propose_succeeds() {
        let (env, client, accounts, _id) = setup!();
        let fresh_id = client.propose(&accounts.user3, &payload(&env), &DURATION);
        client.cancel_proposal(&fresh_id, &accounts.user3);
        assert_eq!(
            client.get_proposal(&fresh_id).state,
            ProposalState::Cancelled
        );
    }

    #[test]
    fn cancel_after_quorum_before_execute_succeeds() {
        let (env, client, accounts, proposal_id) = setup!();
        client.vote(&proposal_id, &accounts.user2, &true);
        client.vote(&proposal_id, &accounts.user3, &true);
        env.ledger().set_timestamp(START + DURATION + 1);
        client.cancel_proposal(&proposal_id, &accounts.user1);
        assert_eq!(
            client.get_proposal(&proposal_id).state,
            ProposalState::Cancelled
        );
        assert_eq!(client.get_proposal(&proposal_id).for_votes, 2);
    }

    #[test]
    fn cancel_by_non_proposer_is_unauthorized() {
        let (_env, client, accounts, proposal_id) = setup!();
        let err = client
            .try_cancel_proposal(&proposal_id, &accounts.user2)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::Unauthorized);
    }

    #[test]
    fn cancel_missing_proposal_is_not_found() {
        let (_env, client, accounts, _id) = setup!();
        let err = client
            .try_cancel_proposal(&999, &accounts.user1)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::NotFound);
    }

    #[test]
    fn double_cancel_is_invalid() {
        let (_env, client, accounts, proposal_id) = setup!();
        client.cancel_proposal(&proposal_id, &accounts.user1);
        let err = client
            .try_cancel_proposal(&proposal_id, &accounts.user1)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn cancel_then_vote_is_invalid() {
        let (_env, client, accounts, proposal_id) = setup!();
        client.cancel_proposal(&proposal_id, &accounts.user1);
        let err = client
            .try_vote(&proposal_id, &accounts.user2, &true)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn cancel_then_execute_is_invalid() {
        let (_env, client, accounts, proposal_id) = setup!();
        client.cancel_proposal(&proposal_id, &accounts.user1);
        let err = client.try_execute(&proposal_id).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn cancel_after_execute_is_invalid() {
        let (env, client, accounts, proposal_id) = setup!();
        env.ledger().set_timestamp(START + DURATION + 1);
        client.execute(&proposal_id);
        let err = client
            .try_cancel_proposal(&proposal_id, &accounts.user1)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }
}
