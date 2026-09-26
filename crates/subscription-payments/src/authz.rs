//! Negative authorization tests for subscription payments.
//!
//! Authorization model:
//! - `subscribe` requires the subscriber.
//! - `charge` requires the provider.
//! - `cancel` requires the subscriber.

use crate::{SorobanForgeSubscriptionPaymentsClient, SubscriptionPayments, SubscriptionStatus};
use soroban_sdk::testutils::{Address as _, Ledger as _, MockAuth, MockAuthInvoke};
use soroban_sdk::token::StellarAssetClient;
use soroban_sdk::{Address, Env, IntoVal, InvokeError};

const START: u64 = 1_000_000;
const PERIOD: u64 = 1_000;
const AMOUNT: i128 = 250;

macro_rules! setup {
    () => {{
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(START);

        let admin = Address::generate(&env);
        let sac = env.register_stellar_asset_contract_v2(admin);
        let token = sac.address();
        let token_admin = StellarAssetClient::new(&env, &token);

        let contract_id = env.register(SubscriptionPayments, ());
        let client = SorobanForgeSubscriptionPaymentsClient::new(&env, &contract_id);
        let accounts = soroban_forge_test_utils::TestAccounts::generate(&env);
        token_admin.mint(&accounts.user1, &10_000_i128);

        (env, token, contract_id, client, accounts)
    }};
}

macro_rules! assert_auth_abort {
    ($res:expr) => {
        assert!(
            matches!($res, Err(Err(InvokeError::Abort))),
            "expected auth abort, got {:?}",
            $res
        );
    };
}

#[test]
fn subscribe_accepts_subscriber_signature() {
    let (env, token, contract_id, client, accounts) = setup!();
    let subscriber = &accounts.user1;
    let provider = &accounts.user2;

    env.mock_auths(&[MockAuth {
        address: subscriber,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "subscribe",
            args: (subscriber, provider, &token, AMOUNT, PERIOD).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let id = client
        .try_subscribe(subscriber, provider, &token, &AMOUNT, &PERIOD)
        .expect("outer ok")
        .expect("contract ok");

    let sub = client.get_subscription(&id);
    assert_eq!(sub.subscriber, *subscriber);
    assert_eq!(sub.provider, *provider);
}

#[test]
fn subscribe_rejects_signature_from_non_subscriber() {
    let (env, token, contract_id, client, accounts) = setup!();
    let subscriber = &accounts.user1;
    let provider = &accounts.user2;

    // Armed with provider auth instead of subscriber
    env.mock_auths(&[MockAuth {
        address: provider,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "subscribe",
            args: (subscriber, provider, &token, AMOUNT, PERIOD).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let res = client.try_subscribe(subscriber, provider, &token, &AMOUNT, &PERIOD);
    assert_auth_abort!(res);
}

#[test]
fn charge_accepts_provider_signature_with_subscriber_token_auth() {
    let (env, token, contract_id, client, accounts) = setup!();
    let subscriber = &accounts.user1;
    let provider = &accounts.user2;

    let id = client.subscribe(subscriber, provider, &token, &AMOUNT, &PERIOD);

    env.ledger().set_timestamp(START + PERIOD);

    env.mock_auths(&[
        MockAuth {
            address: provider,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "charge",
                args: (id,).into_val(&env),
                sub_invokes: &[],
            },
        },
        MockAuth {
            address: subscriber,
            invoke: &MockAuthInvoke {
                contract: &token,
                fn_name: "transfer",
                args: (subscriber, provider, AMOUNT).into_val(&env),
                sub_invokes: &[],
            },
        },
    ]);

    let billed = client.try_charge(&id).expect("outer ok").unwrap();
    assert_eq!(billed, AMOUNT);
}

#[test]
fn charge_rejects_subscriber_signature() {
    let (env, token, contract_id, client, accounts) = setup!();
    let subscriber = &accounts.user1;
    let provider = &accounts.user2;

    let id = client.subscribe(subscriber, provider, &token, &AMOUNT, &PERIOD);

    env.ledger().set_timestamp(START + PERIOD);

    // Subscriber trying to call charge instead of provider
    env.mock_auths(&[MockAuth {
        address: subscriber,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "charge",
            args: (id,).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let res = client.try_charge(&id);
    assert_auth_abort!(res);
}

#[test]
fn cancel_accepts_subscriber_signature() {
    let (env, token, contract_id, client, accounts) = setup!();
    let subscriber = &accounts.user1;
    let provider = &accounts.user2;

    let id = client.subscribe(subscriber, provider, &token, &AMOUNT, &PERIOD);

    env.mock_auths(&[MockAuth {
        address: subscriber,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "cancel",
            args: (id,).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    client.try_cancel(&id).expect("outer ok").unwrap();
    let sub = client.get_subscription(&id);
    assert_eq!(sub.status, SubscriptionStatus::Cancelled);
}

#[test]
fn cancel_rejects_provider_signature() {
    let (env, token, contract_id, client, accounts) = setup!();
    let subscriber = &accounts.user1;
    let provider = &accounts.user2;

    let id = client.subscribe(subscriber, provider, &token, &AMOUNT, &PERIOD);

    // Provider trying to cancel
    env.mock_auths(&[MockAuth {
        address: provider,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "cancel",
            args: (id,).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let res = client.try_cancel(&id);
    assert_auth_abort!(res);

    let sub = client.get_subscription(&id);
    assert_eq!(sub.status, SubscriptionStatus::Active);
}
