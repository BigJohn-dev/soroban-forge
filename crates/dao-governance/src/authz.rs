//! Negative authorization tests for the bond-bearing DAO entrypoints.
//!
//! The main suite (`tests` in `lib.rs`) runs under `mock_all_auths`, which
//! proves the *call graph* of authorizations — who the contract asks to
//! sign — but never that a wrong signer is rejected. This module covers the
//! other half for the paths that move (or demand) real tokens, in two
//! layers:
//!
//! 1. **Enforce mode** (`mock_auths` / `set_auths`): each test arms
//!    authorization for exactly the signer(s) a scenario names and asserts
//!    that anyone else — or the right signer over the wrong arguments — is
//!    rejected by the host, with proposal state and balances untouched.
//! 2. **Tree assertions** (`env.auths()` under `mock_all_auths`): pins the
//!    exact authorized-invocation tree each path demands, per the SDK's own
//!    recommendation for tests that would otherwise prove nothing about
//!    missing `require_auth` calls.
//!
//! The mechanics verified against soroban-sdk 27.0.6 (mirroring
//! `crates/escrow/src/authz.rs`, worth re-reading for the long form):
//!
//! - `mock_auths` sets the invocation envelope **and disables blanket
//!   mocking**; authorizations not matching a mocked auth fail. A fixture
//!   must mirror the auth tree exactly: the entrypoint frame *and* the
//!   nested bond-pull frame, with arguments as the client puts them on the
//!   wire.
//! - A missing/extra/mismatched auth **at the root entrypoint** aborts the
//!   whole invocation (`Err(Err(InvokeError::Abort))` through the `try_`
//!   client). A mismatch on the **nested** SAC `transfer` pull in `propose`
//!   surfaces as an error *returned by the token contract*, which the DAO
//!   buckets into [`ForgeError::TokenTransferFailed`] — the error-bucketing
//!   design observable end to end, with no proposal record written.
//! - `set_auths(&[])` is a blank envelope: every `require_auth` fails.
//! - **Contract self-authorization is implicit**: the host auto-approves
//!   `require_auth` for the currently-executing contract. That is why the
//!   bond *payouts* (`execute` refund/forfeit, `cancel_proposal` refund) run
//!   with no external signature at all, and why their recorded tree (if any)
//!   shows only the party's entrypoint frame.

use crate::{DaoGovernance, SorobanForgeDaoGovernanceClient};
use soroban_forge_shared_utils::ForgeError;
// The test harness links std even in a no_std crate; AuthorizedInvocation's
// sub_invocations field is a std Vec, so re-expose std here for `vec!`.
extern crate std;
use soroban_forge_test_utils::{MockTarget, TestAccounts};
use soroban_sdk::testutils::{
    Address as _, AuthorizedFunction, AuthorizedInvocation, Ledger as _, MockAuth, MockAuthInvoke,
};
use soroban_sdk::token::{Client as TokenClient, StellarAssetClient};
use soroban_sdk::{Address, Bytes, Env, IntoVal, InvokeError, Symbol};

const START: u64 = 1_000_000;
const DURATION: u64 = 86_400;
const BOND: i128 = 100;
const FUNDS: i128 = 10_000;

/// Fresh env with blanket mocking for *setup only* (configuration, funding,
/// proposal creation). The tested call re-arms the envelope afterwards.
///
/// Returns `(env, token, token_client, contract_id, client, accounts,
/// target_id)`.
macro_rules! setup {
    () => {{
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(START);

        let admin = Address::generate(&env);
        let sac = env.register_stellar_asset_contract_v2(admin);
        let token = sac.address();
        let token_admin = StellarAssetClient::new(&env, &token);
        let token_client = TokenClient::new(&env, &token);

        let contract_id = env.register(DaoGovernance, ());
        let client = SorobanForgeDaoGovernanceClient::new(&env, &contract_id);
        let accounts = TestAccounts::generate(&env);
        client.configure_bond(&token, &BOND, &accounts.deployer);
        token_admin.mint(&accounts.user1, &FUNDS);
        let target_id = env.register(MockTarget, ());

        (
            env,
            token,
            token_client,
            contract_id,
            client,
            accounts,
            target_id,
        )
    }};
}

fn payload(env: &Env) -> Bytes {
    Bytes::from_array(env, &[0xC0, 0xDE, 0x00, 0xFF])
}

/// Args of the nested SAC `transfer` frame the contract performs.
fn transfer_args(
    env: &Env,
    from: &Address,
    to: &Address,
    amount: i128,
) -> soroban_sdk::Vec<soroban_sdk::Val> {
    (from.clone(), to.clone(), amount).into_val(env)
}

/// Assert that a `try_` call aborted on authorization (the host aborts the
/// whole invocation when the armed envelope does not match).
macro_rules! assert_auth_abort {
    ($res:expr) => {
        assert!(
            matches!($res, Err(Err(InvokeError::Abort))),
            "expected auth abort, got {:?}",
            $res
        );
    };
}

// -----------------------------------------------------------------------
// propose
// -----------------------------------------------------------------------

#[test]
fn propose_accepts_the_proposer_signature_with_the_bond_pull() {
    let (env, token, tc, contract_id, client, accounts, target_id) = setup!();
    let proposer = &accounts.user1;

    // Two frames, both proposer-signed: the entrypoint and the bond pull
    // into the contract's custody address.
    env.mock_auths(&[MockAuth {
        address: proposer,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "propose",
            args: (proposer, &target_id, payload(&env), DURATION).into_val(&env),
            sub_invokes: &[MockAuthInvoke {
                contract: &token,
                fn_name: "transfer",
                args: transfer_args(&env, proposer, &contract_id, BOND),
                sub_invokes: &[],
            }],
        },
    }]);

    let id = client
        .try_propose(proposer, &target_id, &payload(&env), &DURATION)
        .expect("outer ok")
        .expect("contract ok");
    assert_eq!(client.get_proposal_count(), 1);
    assert_eq!(tc.balance(proposer), FUNDS - BOND);
    assert_eq!(tc.balance(&contract_id), BOND);
    assert_eq!(client.get_proposal(&id).bond_amount, BOND);
}

#[test]
fn propose_authorization_tree_is_proposer_over_bond_transfer() {
    let (env, token, _tc, contract_id, client, accounts, target_id) = setup!();
    let proposer = &accounts.user1;

    client.propose(proposer, &target_id, &payload(&env), &DURATION);

    // The tree is the proposer's entrypoint frame with the bond pull as its
    // only sub-invocation — no other signer is ever demanded.
    assert_eq!(
        env.auths(),
        [(
            proposer.clone(),
            AuthorizedInvocation {
                function: AuthorizedFunction::Contract((
                    contract_id.clone(),
                    Symbol::new(&env, "propose"),
                    (proposer.clone(), target_id.clone(), payload(&env), DURATION).into_val(&env),
                )),
                sub_invocations: std::vec![AuthorizedInvocation {
                    function: AuthorizedFunction::Contract((
                        token.clone(),
                        Symbol::new(&env, "transfer"),
                        transfer_args(&env, proposer, &contract_id, BOND),
                    )),
                    sub_invocations: std::vec![],
                }],
            },
        )],
    );
}

#[test]
fn propose_rejects_signature_from_a_non_proposer() {
    let (env, _token, tc, contract_id, client, accounts, target_id) = setup!();

    // A stranger signs the proposal; the contract demands `proposer`, so
    // the host must reject the call outright.
    env.mock_auths(&[MockAuth {
        address: &accounts.user2,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "propose",
            args: (&accounts.user1, &target_id, payload(&env), DURATION).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let res = client.try_propose(&accounts.user1, &target_id, &payload(&env), &DURATION);
    assert_auth_abort!(res);
    assert_eq!(client.get_proposal_count(), 0);
    assert_eq!(tc.balance(&accounts.user1), FUNDS);
    assert_eq!(tc.balance(&contract_id), 0);
}

#[test]
fn propose_rejects_signature_over_different_args() {
    let (env, _token, _tc, contract_id, client, accounts, target_id) = setup!();

    // Correct signer, but the armed authorization covers a *different
    // duration* than the invocation performs. A captured signature must not
    // be replayable over changed terms.
    env.mock_auths(&[MockAuth {
        address: &accounts.user1,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "propose",
            args: (&accounts.user1, &target_id, payload(&env), DURATION + 1).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let res = client.try_propose(&accounts.user1, &target_id, &payload(&env), &DURATION);
    assert_auth_abort!(res);
    assert_eq!(client.get_proposal_count(), 0);
}

#[test]
fn propose_without_nested_token_authorization_buckets_the_token_error() {
    let (env, _token, tc, contract_id, client, accounts, target_id) = setup!();

    // Only the entrypoint frame is armed; the nested SAC bond pull has no
    // authorization. The token rejects the pull, the DAO buckets the error —
    // and transfer-before-state means no proposal is ever written.
    env.mock_auths(&[MockAuth {
        address: &accounts.user1,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "propose",
            args: (&accounts.user1, &target_id, payload(&env), DURATION).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let res = client.try_propose(&accounts.user1, &target_id, &payload(&env), &DURATION);
    assert!(matches!(res, Err(Ok(ForgeError::TokenTransferFailed))));
    assert_eq!(client.get_proposal_count(), 0);
    assert_eq!(
        client.try_get_proposal(&1).unwrap_err().unwrap(),
        ForgeError::NotFound
    );
    assert_eq!(tc.balance(&accounts.user1), FUNDS);
    assert_eq!(tc.balance(&contract_id), 0);
}

#[test]
fn propose_rejects_token_authorization_over_a_different_bond_amount() {
    let (env, token, tc, contract_id, client, accounts, target_id) = setup!();

    // The nested token authorization names a smaller bond than the contract
    // actually pulls — the host rejects the mismatch and the DAO buckets it.
    env.mock_auths(&[MockAuth {
        address: &accounts.user1,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "propose",
            args: (&accounts.user1, &target_id, payload(&env), DURATION).into_val(&env),
            sub_invokes: &[MockAuthInvoke {
                contract: &token,
                fn_name: "transfer",
                args: transfer_args(&env, &accounts.user1, &contract_id, BOND - 1),
                sub_invokes: &[],
            }],
        },
    }]);

    let res = client.try_propose(&accounts.user1, &target_id, &payload(&env), &DURATION);
    assert!(matches!(res, Err(Ok(ForgeError::TokenTransferFailed))));
    assert_eq!(client.get_proposal_count(), 0);
    assert_eq!(tc.balance(&accounts.user1), FUNDS);
    assert_eq!(tc.balance(&contract_id), 0);
}

#[test]
fn blank_envelope_aborts_propose_and_writes_nothing() {
    let (env, _token, _tc, _contract_id, client, accounts, target_id) = setup!();

    // No authorization entries whatsoever: the proposer's require_auth
    // fails before anything is pulled or written.
    env.set_auths(&[]);

    let res = client.try_propose(&accounts.user1, &target_id, &payload(&env), &DURATION);
    assert_auth_abort!(res);
    // The id counter must not have advanced: the next proposal is id 1.
    env.mock_all_auths();
    let id = client.propose(&accounts.user1, &target_id, &payload(&env), &DURATION);
    assert_eq!(id, 1);
}

// -----------------------------------------------------------------------
// cancel_proposal — proposer refund
// -----------------------------------------------------------------------

#[test]
fn cancel_accepts_the_proposer_signature_and_pays_the_refund() {
    let (env, _token, tc, contract_id, client, accounts, target_id) = setup!();
    let proposer = &accounts.user1;
    let id = client.propose(proposer, &target_id, &payload(&env), &DURATION);
    assert_eq!(tc.balance(&contract_id), BOND);

    // The proposer's entrypoint signature alone completes the cancellation:
    // the refund out of the contract needs no external signer (contract
    // self-authorization is implicit).
    env.mock_auths(&[MockAuth {
        address: proposer,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "cancel_proposal",
            args: (id, proposer).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    client
        .try_cancel_proposal(&id, proposer)
        .expect("outer ok")
        .unwrap();
    assert_eq!(tc.balance(proposer), FUNDS);
    assert_eq!(tc.balance(&contract_id), 0);
    assert_eq!(
        client.get_proposal(&id).bond_state,
        crate::BondState::Refunded
    );
}

#[test]
fn cancel_rejects_a_non_proposer_signature_even_when_armed() {
    let (env, _token, tc, contract_id, client, accounts, target_id) = setup!();
    let id = client.propose(&accounts.user1, &target_id, &payload(&env), &DURATION);

    // A third party signs a call whose `proposer` argument claims to be
    // them: the armed signature exists, but it is not the proposal's
    // proposer. The identity check runs before require_auth, so this is a
    // typed rejection — not a host abort — and custody is untouched.
    env.mock_auths(&[MockAuth {
        address: &accounts.user2,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "cancel_proposal",
            args: (id, &accounts.user2).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let res = client.try_cancel_proposal(&id, &accounts.user2);
    assert!(matches!(res, Err(Ok(ForgeError::Unauthorized))));
    assert_eq!(tc.balance(&contract_id), BOND);
    assert_eq!(tc.balance(&accounts.user2), 0);
    assert_eq!(
        client.get_proposal(&id).bond_state,
        crate::BondState::Posted
    );
}

#[test]
fn cancel_authorization_tree_is_the_proposer_entrypoint_frame() {
    let (env, _token, _tc, contract_id, client, accounts, target_id) = setup!();
    let proposer = &accounts.user1;
    let id = client.propose(proposer, &target_id, &payload(&env), &DURATION);

    client.cancel_proposal(&id, proposer);

    // The refund transfer out of the contract is contract self-auth and is
    // therefore NOT recorded: the tree is the proposer's frame only.
    assert_eq!(
        env.auths(),
        [(
            proposer.clone(),
            AuthorizedInvocation {
                function: AuthorizedFunction::Contract((
                    contract_id.clone(),
                    Symbol::new(&env, "cancel_proposal"),
                    (id, proposer.clone()).into_val(&env),
                )),
                sub_invocations: std::vec![],
            },
        )],
    );
}

// -----------------------------------------------------------------------
// execute — permissionless finalisation, including bond payouts
// -----------------------------------------------------------------------

#[test]
fn execute_refunds_the_bond_under_a_blank_envelope() {
    let (env, _token, tc, contract_id, client, accounts, target_id) = setup!();
    let proposer = &accounts.user1;
    let id = client.propose(proposer, &target_id, &payload(&env), &DURATION);
    client.vote(&id, &accounts.user2, &true);
    env.ledger().set_timestamp(START + DURATION + 1);
    client.execute(&id); // Active -> Succeeded

    // Blank envelope: no signatures anywhere. Finalisation and the bond
    // refund must still complete — no external signer is ever demanded, and
    // the recorded authorization list stays empty.
    env.set_auths(&[]);
    client.execute(&id); // Succeeded -> Executed, refund paid

    assert_eq!(
        client.get_proposal(&id).bond_state,
        crate::BondState::Refunded
    );
    assert_eq!(tc.balance(proposer), FUNDS);
    assert_eq!(tc.balance(&contract_id), 0);
    assert_eq!(env.auths().len(), 0);
}

#[test]
fn execute_forfeits_the_bond_under_a_blank_envelope() {
    let (env, _token, tc, _contract_id, client, accounts, target_id) = setup!();
    let id = client.propose(&accounts.user1, &target_id, &payload(&env), &DURATION);
    client.vote(&id, &accounts.user2, &false);
    env.ledger().set_timestamp(START + DURATION + 1);

    // Same for the defeat path: the forfeit to the treasury is permissionless.
    env.set_auths(&[]);
    client.execute(&id);

    assert_eq!(
        client.get_proposal(&id).bond_state,
        crate::BondState::Forfeited
    );
    assert_eq!(tc.balance(&accounts.user1), FUNDS - BOND);
    assert_eq!(tc.balance(&accounts.deployer), BOND);
}

// -----------------------------------------------------------------------
// vote — voter authorization
// -----------------------------------------------------------------------

#[test]
fn vote_accepts_the_voter_signature() {
    let (env, _token, _tc, contract_id, client, accounts, target_id) = setup!();
    let id = client.propose(&accounts.user1, &target_id, &payload(&env), &DURATION);

    // The voter's entrypoint signature alone completes the vote — no token
    // transfer occurs, so there is no nested sub-invocation.
    env.mock_auths(&[MockAuth {
        address: &accounts.user2,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "vote",
            args: (id, &accounts.user2, true).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    client
        .try_vote(&id, &accounts.user2, &true)
        .expect("outer ok")
        .expect("contract ok");

    let proposal = client.get_proposal(&id);
    assert_eq!(proposal.for_votes, 1);
    assert_eq!(proposal.against_votes, 0);
}

#[test]
fn vote_rejects_signature_from_a_different_address() {
    let (env, _token, _tc, contract_id, client, accounts, target_id) = setup!();
    let id = client.propose(&accounts.user1, &target_id, &payload(&env), &DURATION);

    // A stranger (user3) arms their own signature for a vote whose `voter`
    // argument names user2. The contract calls `voter.require_auth()`, so the
    // host must reject the mismatch outright.
    env.mock_auths(&[MockAuth {
        address: &accounts.user3,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "vote",
            args: (id, &accounts.user2, true).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let res = client.try_vote(&id, &accounts.user2, &true);
    assert_auth_abort!(res);

    // The tally must be untouched.
    let proposal = client.get_proposal(&id);
    assert_eq!(proposal.for_votes, 0);
    assert_eq!(proposal.against_votes, 0);
}

#[test]
fn vote_rejects_signature_over_different_args() {
    let (env, _token, _tc, contract_id, client, accounts, target_id) = setup!();
    let id = client.propose(&accounts.user1, &target_id, &payload(&env), &DURATION);

    // Correct signer, but the armed authorization covers a *different
    // support value* than the invocation performs. A captured signature
    // must not be replayable with flipped intent.
    env.mock_auths(&[MockAuth {
        address: &accounts.user2,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "vote",
            args: (id, &accounts.user2, false).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    // The actual call casts `true` (for), but the armed auth covers `false` (against).
    let res = client.try_vote(&id, &accounts.user2, &true);
    assert_auth_abort!(res);

    let proposal = client.get_proposal(&id);
    assert_eq!(proposal.for_votes, 0);
    assert_eq!(proposal.against_votes, 0);
}

#[test]
fn vote_authorization_tree_is_the_voter_entrypoint_frame() {
    let (env, _token, _tc, contract_id, client, accounts, target_id) = setup!();
    let id = client.propose(&accounts.user1, &target_id, &payload(&env), &DURATION);

    // Under mock_all_auths, cast a vote and pin the recorded auth tree.
    client.vote(&id, &accounts.user2, &true);

    // No token transfer occurs, so the tree is the voter's entrypoint
    // frame only — no sub-invocations.
    assert_eq!(
        env.auths(),
        [(
            accounts.user2.clone(),
            soroban_sdk::testutils::AuthorizedInvocation {
                function: soroban_sdk::testutils::AuthorizedFunction::Contract((
                    contract_id.clone(),
                    Symbol::new(&env, "vote"),
                    (id, accounts.user2.clone(), true).into_val(&env),
                )),
                sub_invocations: std::vec![],
            },
        )],
    );
}

#[test]
fn blank_envelope_aborts_vote_and_preserves_tally() {
    let (env, _token, _tc, _contract_id, client, accounts, target_id) = setup!();
    let id = client.propose(&accounts.user1, &target_id, &payload(&env), &DURATION);

    // No authorization entries: every require_auth fails.
    env.set_auths(&[]);

    let res = client.try_vote(&id, &accounts.user2, &true);
    assert_auth_abort!(res);

    // Under a blank envelope the tally must be untouched.
    env.mock_all_auths();
    let proposal = client.get_proposal(&id);
    assert_eq!(proposal.for_votes, 0);
    assert_eq!(proposal.against_votes, 0);
}
