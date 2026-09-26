//! Negative authorization tests for the wallet's state-changing entrypoints.
//!
//! The main suite (`tests` in lib.rs) runs under `mock_all_auths`, which
//! proves the *call graph* of authorizations — who the contract asks to
//! sign — but never that a wrong signer is rejected. This module covers the
//! other half for every state-changing entrypoint (`initialize`, `submit`,
//! `confirm`, `execute`), in two layers:
//!
//! 1. **Enforce mode** (`mock_auths` / `set_auths`): each test arms
//!    authorization for exactly the signer a scenario names and asserts
//!    that anyone else — or the right signer over the wrong arguments — is
//!    rejected by the host, with wallet state untouched.
//! 2. **Tree assertions** (`env.auths()` under `mock_all_auths`): pins the
//!    exact authorized-invocation tree each path demands, per the SDK's own
//!    recommendation for tests that would otherwise prove nothing about
//!    missing `require_auth` calls.
//!
//! The mechanics (mirroring `crates/escrow/src/authz.rs` and
//! `crates/dao-governance/src/authz.rs`, worth re-reading for the long
//! form):
//!
//! - `mock_auths` sets the invocation envelope **and disables blanket
//!   mocking**; authorizations not matching a mocked auth fail.
//! - A missing/extra/mismatched auth aborts the whole invocation
//!   (`Err(Err(InvokeError::Abort))` through the `try_` client).
//! - `set_auths(&[])` is a blank envelope: every `require_auth` fails.
//! - `initialize` performs **no** `require_auth` — the deployer-initializer
//!   pattern trusts the deployment transaction itself — so its negative
//!   coverage below proves the *validation* guards instead (single init,
//!   threshold bounds, duplicate owners), which are the only rejection
//!   paths that entrypoint has.
//!
//! **Soroban host limitation note.** Under the test host the *absence* of a
//! `require_auth` cannot be distinguished from a satisfied one while
//! `mock_all_auths` is armed — the host auto-approves. That is exactly what
//! the tree assertions catch: if the `require_auth(signer)` call were
//! removed from `submit` or `confirm`, the recorded tree would shrink to
//! nothing and `submit_authorization_tree_...` /
//! `confirm_authorization_tree_...` would fail. The mutation drill in the
//! PR description removes that call and observes precisely those failures.
//!
//! Wallet entrypoints carry no token pulls inside the tested paths, so no
//! nested sub-invocations appear in any tree (unlike the DAO's bond pull).

use crate::{MultiSigWallet, SorobanForgeMultiSigWalletClient};
use soroban_forge_test_utils::{MockTarget, TestAccounts};
// The test harness links std even in a no_std crate; AuthorizedInvocation's
// sub_invocations field is a std Vec, so re-expose std here for `vec!`.
extern crate std;
use soroban_sdk::testutils::{AuthorizedFunction, AuthorizedInvocation, MockAuth, MockAuthInvoke};
use soroban_sdk::{Bytes, Env, IntoVal, InvokeError, Symbol};

/// Fresh env with blanket mocking for *setup only*. The tested call re-arms
/// the envelope afterwards.
///
/// Returns `(env, contract_id, client, accounts)`.
macro_rules! setup {
    () => {{
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(MultiSigWallet, ());
        let client = SorobanForgeMultiSigWalletClient::new(&env, &contract_id);
        let accounts = TestAccounts::generate(&env);
        (env, contract_id, client, accounts)
    }};
}

fn payload(env: &Env) -> Bytes {
    Bytes::from_array(env, &[0x01, 0x02, 0x03])
}

/// The standard 3-owner set (user1, user2, user3) at threshold 2.
fn owner_vec(env: &Env, accounts: &TestAccounts) -> soroban_sdk::Vec<soroban_sdk::Address> {
    soroban_sdk::vec![
        env,
        accounts.user1.clone(),
        accounts.user2.clone(),
        accounts.user3.clone()
    ]
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
// initialize — deployer-trusted, so negative coverage proves the
// validation guards (the only rejection paths it has)
// -----------------------------------------------------------------------

#[test]
fn initialize_rejects_a_second_initialization() {
    let (_env, _contract_id, client, accounts) = setup!();
    client.initialize(&owner_vec(&_env, &accounts), &2_u32);

    // Re-initialization must be rejected: the wallet's owner set and
    // threshold must never be replaceable after deployment.
    let err = client
        .try_initialize(&owner_vec(&_env, &accounts), &1_u32)
        .unwrap_err()
        .unwrap();
    assert_eq!(
        err,
        soroban_forge_shared_utils::ForgeError::AlreadyInitialized
    );
    assert_eq!(client.try_get_threshold().unwrap().unwrap(), 2_u32);
}

#[test]
fn initialize_rejects_threshold_above_owner_count() {
    let (_env, _contract_id, client, accounts) = setup!();

    // A threshold above the owner count would make the wallet permanently
    // unable to execute — rejected as InvalidInput.
    let err = client
        .try_initialize(&owner_vec(&_env, &accounts), &4_u32)
        .unwrap_err()
        .unwrap();
    assert_eq!(err, soroban_forge_shared_utils::ForgeError::InvalidInput);
}

#[test]
fn initialize_rejects_duplicate_owners() {
    let (env, _contract_id, client, accounts) = setup!();

    // A duplicated owner address would let one party cast two
    // confirmations toward the threshold.
    let dup = soroban_sdk::vec![
        &env,
        accounts.user1.clone(),
        accounts.user1.clone(),
        accounts.user2.clone()
    ];
    let err = client.try_initialize(&dup, &2_u32).unwrap_err().unwrap();
    assert_eq!(err, soroban_forge_shared_utils::ForgeError::InvalidInput);
}

// -----------------------------------------------------------------------
// submit — owner-only
// -----------------------------------------------------------------------

#[test]
fn submit_accepts_the_owner_signature() {
    let (env, contract_id, client, accounts) = setup!();
    client.initialize(&owner_vec(&env, &accounts), &2_u32);
    let target = env.register(MockTarget, ());

    env.mock_auths(&[MockAuth {
        address: &accounts.user1,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "submit",
            args: (&accounts.user1, &target, payload(&env)).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let tx_id = client
        .try_submit(&accounts.user1, &target, &payload(&env))
        .expect("outer ok")
        .expect("contract ok");
    assert_eq!(tx_id, 1);
    assert_eq!(client.try_get_tx_count().unwrap().unwrap(), 1_u64);
}

#[test]
fn submit_rejects_signature_from_a_non_owner() {
    let (env, contract_id, client, accounts) = setup!();
    client.initialize(&owner_vec(&env, &accounts), &2_u32);
    let target = env.register(MockTarget, ());

    // A stranger (deployer is not an owner) arms their own signature for a
    // submit whose `submitter` argument claims to be user1. The contract
    // demands `submitter.require_auth()`, so the host must reject.
    env.mock_auths(&[MockAuth {
        address: &accounts.deployer,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "submit",
            args: (&accounts.user1, &target, payload(&env)).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let res = client.try_submit(&accounts.user1, &target, &payload(&env));
    assert_auth_abort!(res);
    assert_eq!(client.try_get_tx_count().unwrap().unwrap(), 0_u64);
}

#[test]
fn submit_without_authorization_aborts_and_writes_nothing() {
    let (env, _contract_id, client, accounts) = setup!();
    client.initialize(&owner_vec(&env, &accounts), &2_u32);

    // Blank envelope: every require_auth fails.
    env.set_auths(&[]);

    let target = env.register(MockTarget, ());
    let res = client.try_submit(&accounts.user1, &target, &payload(&env));
    assert_auth_abort!(res);
    assert_eq!(client.try_get_tx_count().unwrap().unwrap(), 0_u64);
}

#[test]
fn submit_authorization_tree_is_the_owner_entrypoint_frame() {
    let (env, contract_id, client, accounts) = setup!();
    client.initialize(&owner_vec(&env, &accounts), &2_u32);
    let target = env.register(MockTarget, ());

    client.submit(&accounts.user1, &target, &payload(&env));

    // The tree is the submitter's entrypoint frame only — a removed
    // `require_auth(submitter)` shrinks this to empty and fails the test.
    assert_eq!(
        env.auths(),
        [(
            accounts.user1.clone(),
            AuthorizedInvocation {
                function: AuthorizedFunction::Contract((
                    contract_id.clone(),
                    Symbol::new(&env, "submit"),
                    (accounts.user1.clone(), target.clone(), payload(&env)).into_val(&env),
                )),
                sub_invocations: std::vec![],
            },
        )],
    );
}

#[test]
fn submit_by_a_non_owner_is_unauthorized_even_under_mocked_auths() {
    let (env, _contract_id, client, accounts) = setup!();
    client.initialize(&owner_vec(&env, &accounts), &2_u32);
    let target = env.register(MockTarget, ());

    // Under blanket mocking the identity check is what rejects: the
    // non-owner deployer gets the typed error, not a host abort.
    let err = client
        .try_submit(&accounts.deployer, &target, &payload(&env))
        .unwrap_err()
        .unwrap();
    assert_eq!(err, soroban_forge_shared_utils::ForgeError::Unauthorized);
}

// -----------------------------------------------------------------------
// confirm — owner-only, once, pending-only
// -----------------------------------------------------------------------

#[test]
fn confirm_accepts_the_owner_signature() {
    let (env, contract_id, client, accounts) = setup!();
    client.initialize(&owner_vec(&env, &accounts), &2_u32);
    let target = env.register(MockTarget, ());
    let tx_id = client.submit(&accounts.user1, &target, &payload(&env));

    env.mock_auths(&[MockAuth {
        address: &accounts.user2,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "confirm",
            args: (tx_id, &accounts.user2).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    client
        .try_confirm(&tx_id, &accounts.user2)
        .expect("outer ok")
        .expect("contract ok");

    let tx = client
        .try_get_tx(&tx_id)
        .expect("outer ok")
        .expect("contract ok");
    assert_eq!(tx.confirmations.len(), 1_u32);
}

#[test]
fn confirm_rejects_signature_from_a_non_owner() {
    let (env, contract_id, client, accounts) = setup!();
    client.initialize(&owner_vec(&env, &accounts), &2_u32);
    let target = env.register(MockTarget, ());
    let tx_id = client.submit(&accounts.user1, &target, &payload(&env));

    // A stranger (deployer) signs a confirm whose `signer` argument names
    // user2. The contract calls `signer.require_auth()`, so the mismatch
    // aborts the invocation.
    env.mock_auths(&[MockAuth {
        address: &accounts.deployer,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "confirm",
            args: (tx_id, &accounts.user2).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let res = client.try_confirm(&tx_id, &accounts.user2);
    assert_auth_abort!(res);

    let tx = client
        .try_get_tx(&tx_id)
        .expect("outer ok")
        .expect("contract ok");
    assert_eq!(tx.confirmations.len(), 0_u32);
}

#[test]
fn confirm_rejects_signature_over_a_different_signer_argument() {
    let (env, contract_id, client, accounts) = setup!();
    client.initialize(&owner_vec(&env, &accounts), &2_u32);
    let target = env.register(MockTarget, ());
    let tx_id = client.submit(&accounts.user1, &target, &payload(&env));

    // user2 arms a signature for *their own* confirm, but the invocation
    // runs with user3 as the signer — a captured signature must not be
    // replayable for a different owner.
    env.mock_auths(&[MockAuth {
        address: &accounts.user2,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "confirm",
            args: (tx_id, &accounts.user2).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let res = client.try_confirm(&tx_id, &accounts.user3);
    assert_auth_abort!(res);

    let tx = client
        .try_get_tx(&tx_id)
        .expect("outer ok")
        .expect("contract ok");
    assert_eq!(tx.confirmations.len(), 0_u32);
}

#[test]
fn confirm_authorization_tree_is_the_owner_entrypoint_frame() {
    let (env, contract_id, client, accounts) = setup!();
    client.initialize(&owner_vec(&env, &accounts), &2_u32);
    let target = env.register(MockTarget, ());
    let tx_id = client.submit(&accounts.user1, &target, &payload(&env));

    client.confirm(&tx_id, &accounts.user2);

    // The tree is the confirming owner's frame only — a removed
    // `require_auth(signer)` shrinks this to empty and fails the test.
    assert_eq!(
        env.auths(),
        [(
            accounts.user2.clone(),
            AuthorizedInvocation {
                function: AuthorizedFunction::Contract((
                    contract_id.clone(),
                    Symbol::new(&env, "confirm"),
                    (tx_id, accounts.user2.clone()).into_val(&env),
                )),
                sub_invocations: std::vec![],
            },
        )],
    );
}

#[test]
fn blank_envelope_aborts_confirm_and_preserves_state() {
    let (env, _contract_id, client, accounts) = setup!();
    client.initialize(&owner_vec(&env, &accounts), &2_u32);
    let target = env.register(MockTarget, ());
    let tx_id = client.submit(&accounts.user1, &target, &payload(&env));

    env.set_auths(&[]);
    let res = client.try_confirm(&tx_id, &accounts.user2);
    assert_auth_abort!(res);

    // The record must be untouched.
    let tx = client
        .try_get_tx(&tx_id)
        .expect("outer ok")
        .expect("contract ok");
    assert_eq!(tx.confirmations.len(), 0_u32);
    assert_eq!(tx.status, crate::TxStatus::Pending);
}

// -----------------------------------------------------------------------
// execute — permissionless once the threshold is met
// -----------------------------------------------------------------------

#[test]
fn execute_is_permissionless_at_threshold_under_a_blank_envelope() {
    let (env, _contract_id, client, accounts) = setup!();
    client.initialize(&owner_vec(&env, &accounts), &2_u32);
    let target = env.register(MockTarget, ());
    let tx_id = client.submit(&accounts.user1, &target, &payload(&env));
    client.confirm(&tx_id, &accounts.user1);
    client.confirm(&tx_id, &accounts.user2);

    // Execute demands no signature: with the threshold met a blank
    // envelope still completes the state transition.
    env.set_auths(&[]);
    client.execute(&tx_id);

    let tx = client
        .try_get_tx(&tx_id)
        .expect("outer ok")
        .expect("contract ok");
    assert_eq!(tx.status, crate::TxStatus::Executed);
    assert_eq!(env.auths().len(), 0);
}

#[test]
fn execute_below_threshold_is_invalid_input_even_for_owners() {
    let (env, _contract_id, client, accounts) = setup!();
    client.initialize(&owner_vec(&env, &accounts), &2_u32);
    let target = env.register(MockTarget, ());
    let tx_id = client.submit(&accounts.user1, &target, &payload(&env));
    client.confirm(&tx_id, &accounts.user1);

    // A single confirmation below the threshold of 2 must never execute —
    // this is the invariant props.rs P1 also covers, pinned deterministically.
    let err = client.try_execute(&tx_id).unwrap_err().unwrap();
    assert_eq!(err, soroban_forge_shared_utils::ForgeError::InvalidInput);
    let tx = client
        .try_get_tx(&tx_id)
        .expect("outer ok")
        .expect("contract ok");
    assert_eq!(tx.status, crate::TxStatus::Pending);
}

#[test]
fn execute_of_an_already_executed_tx_is_rejected() {
    let (env, _contract_id, client, accounts) = setup!();
    client.initialize(&owner_vec(&env, &accounts), &2_u32);
    let target = env.register(MockTarget, ());
    let tx_id = client.submit(&accounts.user1, &target, &payload(&env));
    client.confirm(&tx_id, &accounts.user1);
    client.confirm(&tx_id, &accounts.user2);
    client.execute(&tx_id);

    // Terminal state: replay must be rejected (the once-only invariant,
    // pinned deterministically; props.rs P2 covers it over random inputs).
    let err = client.try_execute(&tx_id).unwrap_err().unwrap();
    assert_eq!(err, soroban_forge_shared_utils::ForgeError::InvalidInput);
}
