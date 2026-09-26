//! Negative authorization tests for the vesting contract.
//!
//! The main unit suite (`tests.rs`) runs under `mock_all_auths`, which proves
//! the *call graph* of authorizations — who the contract asks to sign —
//! but never that a wrong signer or tampered arguments are rejected by the host.
//! This module covers the negative authorization domain across two layers:
//!
//! 1. **Enforce mode** (`mock_auths` / `set_auths`): each test arms
//!    authorization for exactly the signer(s) a scenario names and asserts
//!    that an unauthorized caller — or the correct signer signing over different
//!    arguments/schedule IDs — is rejected by the host (`InvokeError::Abort`),
//!    leaving lifecycle state, balances, and monotonic counters untouched.
//! 2. **Tree assertions** (`env.auths()` under `mock_all_auths`): pins the
//!    exact authorized-invocation tree the contract demands on creation and
//!    settlement claim paths.
//!
//! ## Host Mechanics (soroban-sdk 27.0.6)
//!
//! - `mock_auths` sets the invocation envelope and disables blanket mocking.
//!   Authorizations not matching a mocked auth fail and abort the transaction.
//! - `set_auths(&[])` is an empty envelope: every `require_auth` fails immediately.
//! - **Contract self-authorization is implicit**: the host auto-approves
//!   `require_auth` for the currently-executing contract during token payout.
//!   `env.auths()` records only the beneficiary's entrypoint frame with no
//!   sub-invocations, and a claim payout legitimately completes on the beneficiary's
//!   signature alone.

use crate::{SorobanForgeVestingClient, Vesting, VestingStatus};

extern crate std;
use soroban_sdk::testutils::{
    Address as _, AuthorizedFunction, AuthorizedInvocation, Ledger as _, MockAuth, MockAuthInvoke,
};
use soroban_sdk::token::{Client as TokenClient, StellarAssetClient};
use soroban_sdk::{Address, Env, IntoVal, InvokeError, Symbol};

const START: u64 = 1_000_000;
const CLIFF: u64 = 1_000;
const DURATION: u64 = 4_000;
const TOTAL: i128 = 10_000;

/// Fresh env with blanket mocking for setup only (minting, contract registration).
/// The tested call re-arms the authorization envelope explicitly.
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

        let contract_id = env.register(Vesting, ());
        let client = SorobanForgeVestingClient::new(&env, &contract_id);

        let accounts = soroban_forge_test_utils::TestAccounts::generate(&env);
        // Mint tokens to vesting contract to back schedules
        token_admin.mint(&contract_id, &TOTAL);

        (env, token, token_client, contract_id, client, accounts)
    }};
}

/// Assert that a `try_` call aborted on authorization failure at the host level.
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
// create_schedule authorization & negative-auth tests
// -----------------------------------------------------------------------

#[test]
fn create_schedule_accepts_beneficiary_signature_with_matching_args() {
    let (env, token, _tc, contract_id, client, accounts) = setup!();
    let beneficiary = &accounts.user1;

    env.mock_auths(&[MockAuth {
        address: beneficiary,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "create_schedule",
            args: (beneficiary, &token, TOTAL, CLIFF, DURATION).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let id = client
        .try_create_schedule(beneficiary, &token, &TOTAL, &CLIFF, &DURATION)
        .expect("outer ok")
        .expect("contract ok");
    assert_eq!(id, 1);
    assert_eq!(client.get_status(&id), VestingStatus::Locked);
}

#[test]
fn create_schedule_rejects_signature_from_non_beneficiary() {
    let (env, token, _tc, contract_id, client, accounts) = setup!();
    let beneficiary = &accounts.user1;
    let attacker = &accounts.user2;

    // Attacker signs, but beneficiary is named in the call
    env.mock_auths(&[MockAuth {
        address: attacker,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "create_schedule",
            args: (beneficiary, &token, TOTAL, CLIFF, DURATION).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let res = client.try_create_schedule(beneficiary, &token, &TOTAL, &CLIFF, &DURATION);
    assert_auth_abort!(res);
}

#[test]
fn create_schedule_rejects_signature_over_different_amount() {
    let (env, token, _tc, contract_id, client, accounts) = setup!();
    let beneficiary = &accounts.user1;

    // Beneficiary signed for TOTAL, but transaction attempts TOTAL + 5000
    env.mock_auths(&[MockAuth {
        address: beneficiary,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "create_schedule",
            args: (beneficiary, &token, TOTAL, CLIFF, DURATION).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let res = client.try_create_schedule(beneficiary, &token, &(TOTAL + 5000), &CLIFF, &DURATION);
    assert_auth_abort!(res);
}

#[test]
fn create_schedule_rejects_signature_over_different_cliff_or_duration() {
    let (env, token, _tc, contract_id, client, accounts) = setup!();
    let beneficiary = &accounts.user1;

    // Beneficiary signed for CLIFF=1000, DURATION=4000
    env.mock_auths(&[MockAuth {
        address: beneficiary,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "create_schedule",
            args: (beneficiary, &token, TOTAL, CLIFF, DURATION).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    // Attempted call alters duration to 2000
    let res = client.try_create_schedule(beneficiary, &token, &TOTAL, &CLIFF, &2000u64);
    assert_auth_abort!(res);
}

#[test]
fn create_schedule_rejects_signature_over_different_token() {
    let (env, token, _tc, contract_id, client, accounts) = setup!();
    let beneficiary = &accounts.user1;
    let other_token = &accounts.validator;

    // Beneficiary signed for token, but call uses other_token
    env.mock_auths(&[MockAuth {
        address: beneficiary,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "create_schedule",
            args: (beneficiary, &token, TOTAL, CLIFF, DURATION).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let res = client.try_create_schedule(beneficiary, other_token, &TOTAL, &CLIFF, &DURATION);
    assert_auth_abort!(res);
}

#[test]
fn create_schedule_authorization_tree_is_beneficiary_root() {
    let (env, token, _tc, contract_id, client, accounts) = setup!();
    let beneficiary = &accounts.user1;

    let id = client.create_schedule(beneficiary, &token, &TOTAL, &CLIFF, &DURATION);

    assert_eq!(
        env.auths(),
        [(
            beneficiary.clone(),
            AuthorizedInvocation {
                function: AuthorizedFunction::Contract((
                    contract_id.clone(),
                    Symbol::new(&env, "create_schedule"),
                    (beneficiary, &token, TOTAL, CLIFF, DURATION).into_val(&env),
                )),
                sub_invocations: std::vec![],
            },
        )],
    );
    assert_eq!(id, 1);
}

// -----------------------------------------------------------------------
// claim authorization & negative-auth tests
// -----------------------------------------------------------------------

#[test]
fn claim_accepts_beneficiary_signature_alone() {
    let (env, token, tc, contract_id, client, accounts) = setup!();
    let beneficiary = &accounts.user1;
    let id = client.create_schedule(beneficiary, &token, &TOTAL, &CLIFF, &DURATION);

    env.ledger().set_timestamp(START + DURATION);

    // Enforce mode: arm exact beneficiary claim authorization
    env.mock_auths(&[MockAuth {
        address: beneficiary,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "claim",
            args: (id,).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let paid = client.try_claim(&id).expect("outer ok").expect("claim ok");
    assert_eq!(paid, TOTAL);
    assert_eq!(tc.balance(beneficiary), TOTAL);
    assert_eq!(tc.balance(&contract_id), 0);
    assert_eq!(client.get_status(&id), VestingStatus::Completed);
}

#[test]
fn claim_rejects_signature_from_non_beneficiary() {
    let (env, token, tc, contract_id, client, accounts) = setup!();
    let beneficiary = &accounts.user1;
    let attacker = &accounts.user2;
    let id = client.create_schedule(beneficiary, &token, &TOTAL, &CLIFF, &DURATION);

    env.ledger().set_timestamp(START + DURATION);

    // Attacker attempts to claim beneficiary's schedule
    env.mock_auths(&[MockAuth {
        address: attacker,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "claim",
            args: (id,).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let res = client.try_claim(&id);
    assert_auth_abort!(res);
    assert_eq!(tc.balance(beneficiary), 0);
    assert_eq!(tc.balance(&contract_id), TOTAL);
    assert_eq!(client.get_status(&id), VestingStatus::Vesting);
}

#[test]
fn claim_rejects_signature_replayed_for_different_schedule_id() {
    let (env, token, tc, contract_id, client, accounts) = setup!();
    let beneficiary = &accounts.user1;
    let id1 = client.create_schedule(beneficiary, &token, &TOTAL, &CLIFF, &DURATION);
    let id2 = client.create_schedule(beneficiary, &token, &TOTAL, &CLIFF, &DURATION);

    env.ledger().set_timestamp(START + DURATION);

    // Signature armed specifically for id1
    env.mock_auths(&[MockAuth {
        address: beneficiary,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "claim",
            args: (id1,).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    // Attempting to use id1 auth on id2 must fail
    let res = client.try_claim(&id2);
    assert_auth_abort!(res);
    assert_eq!(tc.balance(beneficiary), 0);
}

#[test]
fn claim_authorization_tree_is_beneficiary_root_entrypoint() {
    let (env, token, _tc, contract_id, client, accounts) = setup!();
    let beneficiary = &accounts.user1;
    let id = client.create_schedule(beneficiary, &token, &TOTAL, &CLIFF, &DURATION);

    env.ledger().set_timestamp(START + DURATION);
    client.claim(&id);

    // Contract self-authorization is implicit in the Soroban host; the recorded
    // tree contains the beneficiary's entrypoint frame only with 0 sub-invocations.
    assert_eq!(
        env.auths(),
        [(
            beneficiary.clone(),
            AuthorizedInvocation {
                function: AuthorizedFunction::Contract((
                    contract_id.clone(),
                    Symbol::new(&env, "claim"),
                    (id,).into_val(&env),
                )),
                sub_invocations: std::vec![],
            },
        )],
    );
}

// -----------------------------------------------------------------------
// Blank envelope tests (no authorizations armed)
// -----------------------------------------------------------------------

#[test]
fn blank_envelope_aborts_create_schedule_and_writes_nothing() {
    let (env, token, _tc, _contract_id, client, accounts) = setup!();
    let beneficiary = &accounts.user1;

    // Empty envelope
    env.set_auths(&[]);

    let res = client.try_create_schedule(beneficiary, &token, &TOTAL, &CLIFF, &DURATION);
    assert_auth_abort!(res);

    // Re-arming proves no ID counter was consumed
    env.mock_all_auths();
    let id = client.create_schedule(beneficiary, &token, &TOTAL, &CLIFF, &DURATION);
    assert_eq!(id, 1);
}

#[test]
fn blank_envelope_aborts_claim_and_preserves_custody() {
    let (env, token, tc, contract_id, client, accounts) = setup!();
    let beneficiary = &accounts.user1;
    let id = client.create_schedule(beneficiary, &token, &TOTAL, &CLIFF, &DURATION);

    env.ledger().set_timestamp(START + DURATION);

    // Blank envelope
    env.set_auths(&[]);

    let res = client.try_claim(&id);
    assert_auth_abort!(res);
    assert_eq!(tc.balance(beneficiary), 0);
    assert_eq!(tc.balance(&contract_id), TOTAL);
    assert_eq!(client.get_status(&id), VestingStatus::Vesting);
}
