//! Negative authorization tests for marketplace royalties.
//!
//! Authorization model:
//! - `set_royalty` requires the collection.
//! - `distribute` requires the collection.
//! - `settle_sale` requires the collection and the payer.

use crate::{MarketplaceRoyalties, SorobanForgeMarketplaceRoyaltiesClient};
use soroban_sdk::testutils::{Address as _, MockAuth, MockAuthInvoke};
use soroban_sdk::token::StellarAssetClient;
use soroban_sdk::{Address, Env, IntoVal, InvokeError};

const BPS: u32 = 500;
const AMOUNT: i128 = 1_000;

macro_rules! setup {
    () => {{
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let sac = env.register_stellar_asset_contract_v2(admin);
        let token = sac.address();
        let token_admin = StellarAssetClient::new(&env, &token);

        let collection = Address::generate(&env);
        let recipient = Address::generate(&env);
        let seller = Address::generate(&env);
        let payer = Address::generate(&env);

        token_admin.mint(&payer, &10_000_i128);

        let contract_id = env.register(MarketplaceRoyalties, ());
        let client = SorobanForgeMarketplaceRoyaltiesClient::new(&env, &contract_id);

        (
            env,
            token,
            contract_id,
            client,
            collection,
            recipient,
            seller,
            payer,
        )
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
fn set_royalty_accepts_collection_signature() {
    let (env, _token, contract_id, client, collection, recipient, _seller, _payer) = setup!();

    env.mock_auths(&[MockAuth {
        address: &collection,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "set_royalty",
            args: (&collection, &recipient, BPS).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    client
        .try_set_royalty(&collection, &recipient, &BPS)
        .expect("outer ok")
        .expect("contract ok");

    let royalty = client.get_royalty(&collection);
    assert_eq!(royalty.recipient, recipient);
    assert_eq!(royalty.bps, BPS);
}

#[test]
fn set_royalty_rejects_signature_from_non_collection() {
    let (env, _token, contract_id, client, collection, recipient, _seller, _payer) = setup!();

    // Recipient trying to set royalty instead of collection
    env.mock_auths(&[MockAuth {
        address: &recipient,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "set_royalty",
            args: (&collection, &recipient, BPS).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let res = client.try_set_royalty(&collection, &recipient, &BPS);
    assert_auth_abort!(res);
}

#[test]
fn distribute_accepts_collection_signature() {
    let (env, _token, contract_id, client, collection, recipient, seller, _payer) = setup!();

    client.set_royalty(&collection, &recipient, &BPS);

    env.mock_auths(&[MockAuth {
        address: &collection,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "distribute",
            args: (&collection, &seller, AMOUNT).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let net = client
        .try_distribute(&collection, &seller, &AMOUNT)
        .expect("outer ok")
        .unwrap();
    assert_eq!(net, 950);
}

#[test]
fn distribute_rejects_seller_signature() {
    let (env, _token, contract_id, client, collection, recipient, seller, _payer) = setup!();

    client.set_royalty(&collection, &recipient, &BPS);

    // Seller trying to authorize distribute instead of collection
    env.mock_auths(&[MockAuth {
        address: &seller,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "distribute",
            args: (&collection, &seller, AMOUNT).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let res = client.try_distribute(&collection, &seller, &AMOUNT);
    assert_auth_abort!(res);
}
