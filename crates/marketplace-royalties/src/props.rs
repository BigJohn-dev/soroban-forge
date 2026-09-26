//! Randomized invariant suite (proptest) for marketplace royalties.
//!
//! Exercised invariants:
//! 1. Split conservation: `royalty_share + returned_net == amount` for every `amount > 0` and `bps <= 10_000`.
//! 2. Rate boundaries: `bps == 0` returns the full amount and `bps == 10_000` returns zero net.
//! 3. Bounded net: returned net is never negative and never exceeds the amount.

use crate::{MarketplaceRoyalties, SorobanForgeMarketplaceRoyaltiesClient};
use proptest::prelude::*;
use soroban_sdk::testutils::Address as _;
use soroban_sdk::token::StellarAssetClient;
use soroban_sdk::{Address, Env};

const MAX_AMOUNT: i128 = 1_000_000_000_000_000;

struct World {
    env: Env,
    token: Address,
    contract_id: Address,
    collection: Address,
    _recipient: Address,
    seller: Address,
    payer: Address,
}

fn setup_world(bps: u32, mint_amount: i128) -> World {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(admin);
    let token = sac.address();

    let collection = Address::generate(&env);
    let recipient = Address::generate(&env);
    let seller = Address::generate(&env);
    let payer = Address::generate(&env);

    StellarAssetClient::new(&env, &token).mint(&payer, &mint_amount);

    let contract_id = env.register(MarketplaceRoyalties, ());
    let client = SorobanForgeMarketplaceRoyaltiesClient::new(&env, &contract_id);
    client.set_royalty(&collection, &recipient, &bps);

    World {
        env,
        token,
        contract_id,
        collection,
        _recipient: recipient,
        seller,
        payer,
    }
}

impl World {
    fn client(&self) -> SorobanForgeMarketplaceRoyaltiesClient<'_> {
        SorobanForgeMarketplaceRoyaltiesClient::new(&self.env, &self.contract_id)
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn prop_split_conservation_and_bounds(
        amount in 1i128..=MAX_AMOUNT,
        bps in 0u32..=10_000_u32,
    ) {
        let w = setup_world(bps, amount);
        let net = w.client().distribute(&w.collection, &w.seller, &amount);

        // Calculate expected royalty share floor
        let expected_royalty = amount * (bps as i128) / 10_000;

        prop_assert_eq!(net + expected_royalty, amount, "split conservation: net + royalty must equal total amount");
        prop_assert!(net >= 0, "seller net must never be negative");
        prop_assert!(net <= amount, "seller net must never exceed gross amount");
        prop_assert!(expected_royalty >= 0, "royalty share must never be negative");
        prop_assert!(expected_royalty <= amount, "royalty share must never exceed gross amount");
    }

    #[test]
    fn prop_boundary_rates(
        amount in 1i128..=MAX_AMOUNT,
    ) {
        // Zero rate -> full net to seller
        let w_zero = setup_world(0, amount);
        let net_zero = w_zero.client().distribute(&w_zero.collection, &w_zero.seller, &amount);
        prop_assert_eq!(net_zero, amount, "bps == 0 must return full amount");

        // 100% rate -> 0 net to seller
        let w_full = setup_world(10_000, amount);
        let net_full = w_full.client().distribute(&w_full.collection, &w_full.seller, &amount);
        prop_assert_eq!(net_full, 0, "bps == 10_000 must return 0 net");
    }

    #[test]
    fn prop_atomic_settlement_conservation(
        amount in 1i128..=MAX_AMOUNT,
        bps in 0u32..=10_000_u32,
    ) {
        let w = setup_world(bps, amount);
        let settlement = w.client().settle_sale(&w.collection, &w.token, &w.payer, &w.seller, &amount);

        prop_assert_eq!(settlement.royalty_share + settlement.seller_net, amount, "settled shares must sum to amount");
        prop_assert!(settlement.seller_net >= 0);
        prop_assert!(settlement.royalty_share >= 0);
    }
}
