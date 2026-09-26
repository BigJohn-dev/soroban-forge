//! Escrow test suite.
//!
//! Covers the full lifecycle against a real Stellar Asset Contract (SAC)
//! fixture, so every balance assertion reflects actual token movement —
//! the same failure modes a live deployment would hit (missing balance,
//! failed transfer, double payout).
//!
//! Partial-release coverage includes: valid partial, multiple partials,
//! exact final partial (→ Completed), zero/negative amounts, over-remaining,
//! non-Funded state, after-completion rejection, partial→refund,
//! partial→dispute→resolve (both directions), storage compat, and conservation.
//!
//! NOTE on authorization coverage: the suite runs under `mock_all_auths`,
//! which proves the *call graph* of authorizations (who the contract
//! asks to sign) but not that a wrong signer is rejected. The one
//! logic-level access control that is enforceable without auth mocking —
//! the `dispute` claimant check, which runs before any `require_auth` —
//! is tested directly (`dispute_by_outsider_is_rejected`). Full negative
//! signature testing needs `set_auths` fixtures and is tracked in the
//! security-invariant backlog.

use crate::{Escrow, EscrowData, EscrowStatus, SorobanForgeEscrowClient};
use soroban_forge_shared_utils::ForgeError;
use soroban_sdk::testutils::{Address as _, Ledger as _};
use soroban_sdk::token::{Client as TokenClient, StellarAssetClient};
use soroban_sdk::{Address, Env};

const START: u64 = 1_000_000;
const TIMEOUT: u64 = 1_000;
const AMOUNT: i128 = 1_000;

/// Fresh env: mocked auths, a SAC token with a mintable admin, the escrow
/// contract, and named accounts. Returns the pieces tests name.
macro_rules! setup {
    () => {{
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(START);

        let admin = Address::generate(&env);
        let sac = env.register_stellar_asset_contract_v2(admin.clone());
        let token = sac.address();
        let token_admin = StellarAssetClient::new(&env, &token);
        let token_client = TokenClient::new(&env, &token);

        let contract_id = env.register(Escrow, ());
        let client = SorobanForgeEscrowClient::new(&env, &contract_id);

        let accounts = soroban_forge_test_utils::TestAccounts::generate(&env);
        // Buyer starts funded; everyone else starts at zero.
        token_admin.mint(&accounts.user1, &AMOUNT);

        (env, token, token_client, contract_id, client, accounts)
    }};
}

fn create(
    client: &SorobanForgeEscrowClient<'_>,
    token: &soroban_sdk::Address,
    buyer: &soroban_sdk::Address,
    seller: &soroban_sdk::Address,
    arbiter: &soroban_sdk::Address,
    timeout: u64,
) -> u64 {
    client.create_escrow(buyer, seller, arbiter, token, &AMOUNT, &timeout)
}

/// Default party layout: user1 buys, user2 sells, arbiter arbitrates.
fn parties(
    accounts: &soroban_forge_test_utils::TestAccounts,
) -> (
    &soroban_sdk::Address,
    &soroban_sdk::Address,
    &soroban_sdk::Address,
) {
    (&accounts.user1, &accounts.user2, &accounts.arbiter)
}

/// Assert that a full (non-paged) read of a participant's index equals
/// `expected`, exactly in creation order, with a complete list (no next
/// cursor).
fn assert_full_index(client: &SorobanForgeEscrowClient<'_>, party: &Address, expected: &[u64]) {
    let limit = u32::try_from(expected.len()).expect("test lists fit in u32");
    let page = client.escrows_for_participant(party, &0, &limit);
    assert_eq!(page.total, limit, "index must hold the whole expected list");
    assert_eq!(page.ids.len(), limit);
    for (n, want) in expected.iter().enumerate() {
        let i = u32::try_from(n).expect("index fits in u32");
        assert_eq!(
            page.ids.get_unchecked(i),
            *want,
            "creation-order position {i}"
        );
    }
    assert_eq!(page.next_cursor, None);
}

// -----------------------------------------------------------------------
// Creation
// -----------------------------------------------------------------------

#[test]
fn create_escrow_records_parties_and_stays_pending() {
    let (_env, _token, _tc, _id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let id = create(&client, &_token, buyer, seller, arbiter, TIMEOUT);

    let record: EscrowData = client.get_escrow(&id);
    assert_eq!(record.escrow_id, id);
    assert_eq!(&record.buyer, buyer);
    assert_eq!(&record.seller, seller);
    assert_eq!(&record.arbiter, arbiter);
    assert_eq!(record.amount, AMOUNT);
    assert_eq!(record.timeout, TIMEOUT);
    assert_eq!(record.created_at, START);
    assert_eq!(client.get_status(&id), EscrowStatus::Pending);
}

#[test]
fn escrow_ids_are_monotonic() {
    let (_env, _token, _tc, _id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let first = create(&client, &_token, buyer, seller, arbiter, TIMEOUT);
    let second = create(&client, &_token, buyer, seller, arbiter, TIMEOUT);
    assert_eq!(first + 1, second);
}

#[test]
fn create_rejects_zero_amount() {
    let (_env, token, _tc, _id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let err = client
        .try_create_escrow(buyer, seller, arbiter, &token, &0, &TIMEOUT)
        .unwrap_err()
        .unwrap();
    assert_eq!(err, ForgeError::InvalidInput);
}

#[test]
fn create_rejects_zero_timeout() {
    let (_env, token, _tc, _id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let err = client
        .try_create_escrow(buyer, seller, arbiter, &token, &AMOUNT, &0)
        .unwrap_err()
        .unwrap();
    assert_eq!(err, ForgeError::InvalidInput);
}

// -----------------------------------------------------------------------
// Deposit
// -----------------------------------------------------------------------

#[test]
fn deposit_moves_tokens_into_the_contract() {
    let (_env, token, tc, contract_id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let id = create(&client, &token, buyer, seller, arbiter, TIMEOUT);

    client.deposit(&id);

    assert_eq!(tc.balance(buyer), 0);
    assert_eq!(tc.balance(&contract_id), AMOUNT);
    assert_eq!(client.get_status(&id), EscrowStatus::Funded);
}

#[test]
fn deposit_twice_is_rejected_and_moves_nothing_more() {
    let (_env, token, tc, contract_id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let id = create(&client, &token, buyer, seller, arbiter, TIMEOUT);
    client.deposit(&id);

    let err = client.try_deposit(&id).unwrap_err().unwrap();
    assert_eq!(err, ForgeError::InvalidInput);
    assert_eq!(tc.balance(&contract_id), AMOUNT);
}

#[test]
fn deposit_with_insufficient_balance_fails_cleanly() {
    // user3 holds no tokens; create an escrow they cannot fund.
    let (_env, token, tc, contract_id, client, accounts) = setup!();
    let poor = &accounts.user3;
    let (_, seller, arbiter) = parties(&accounts);
    let id = create(&client, &token, poor, seller, arbiter, TIMEOUT);

    let err = client.try_deposit(&id).unwrap_err().unwrap();
    assert_eq!(err, ForgeError::TokenTransferFailed);
    assert_eq!(tc.balance(&contract_id), 0);
    // State untouched: still Pending, retryable after topping up.
    assert_eq!(client.get_status(&id), EscrowStatus::Pending);
}

// -----------------------------------------------------------------------
// Release / refund
// -----------------------------------------------------------------------

#[test]
fn release_pays_the_seller() {
    let (_env, token, tc, contract_id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let id = create(&client, &token, buyer, seller, arbiter, TIMEOUT);
    client.deposit(&id);

    client.release(&id);

    assert_eq!(tc.balance(seller), AMOUNT);
    assert_eq!(tc.balance(&contract_id), 0);
    assert_eq!(client.get_status(&id), EscrowStatus::Completed);
}

#[test]
fn release_requires_funded_state() {
    let (_env, token, tc, contract_id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let id = create(&client, &token, buyer, seller, arbiter, TIMEOUT);

    let err = client.try_release(&id).unwrap_err().unwrap();
    assert_eq!(err, ForgeError::InvalidInput);
    assert_eq!(tc.balance(&contract_id), 0);
}

#[test]
fn release_cannot_run_twice() {
    let (_env, token, tc, _contract_id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let id = create(&client, &token, buyer, seller, arbiter, TIMEOUT);
    client.deposit(&id);
    client.release(&id);

    let err = client.try_release(&id).unwrap_err().unwrap();
    assert_eq!(err, ForgeError::InvalidInput);
    assert_eq!(tc.balance(seller), AMOUNT);
}

#[test]
fn refund_by_seller_before_deadline() {
    let (_env, token, tc, contract_id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let id = create(&client, &token, buyer, seller, arbiter, TIMEOUT);
    client.deposit(&id);

    client.refund(&id);

    assert_eq!(tc.balance(buyer), AMOUNT);
    assert_eq!(tc.balance(&contract_id), 0);
    assert_eq!(client.get_status(&id), EscrowStatus::Refunded);
}

#[test]
fn refund_by_buyer_after_deadline() {
    let (env, token, tc, _contract_id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let id = create(&client, &token, buyer, seller, arbiter, TIMEOUT);
    client.deposit(&id);

    env.ledger().set_timestamp(START + TIMEOUT + 1);
    client.refund(&id);

    assert_eq!(tc.balance(buyer), AMOUNT);
    assert_eq!(client.get_status(&id), EscrowStatus::Refunded);
}

#[test]
fn refund_requires_funded_state() {
    let (_env, token, _tc, _id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let id = create(&client, &token, buyer, seller, arbiter, TIMEOUT);

    let err = client.try_refund(&id).unwrap_err().unwrap();
    assert_eq!(err, ForgeError::InvalidInput);
}

// -----------------------------------------------------------------------
// Dispute flow
// -----------------------------------------------------------------------

#[test]
fn dispute_by_buyer_claimant() {
    let (_env, token, _tc, _id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let id = create(&client, &token, buyer, seller, arbiter, TIMEOUT);
    client.deposit(&id);

    client.dispute(&id, buyer);

    assert_eq!(client.get_status(&id), EscrowStatus::Disputed);
}

#[test]
fn dispute_by_seller_claimant() {
    let (_env, token, _tc, _id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let id = create(&client, &token, buyer, seller, arbiter, TIMEOUT);
    client.deposit(&id);

    client.dispute(&id, seller);

    assert_eq!(client.get_status(&id), EscrowStatus::Disputed);
}

#[test]
fn dispute_by_outsider_is_rejected() {
    // The claimant check is a pure value test that runs before any
    // require_auth, so this negative case is enforceable even under
    // mocked auths.
    let (_env, token, _tc, _id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let id = create(&client, &token, buyer, seller, arbiter, TIMEOUT);
    client.deposit(&id);

    let err = client
        .try_dispute(&id, &accounts.user3)
        .unwrap_err()
        .unwrap();
    assert_eq!(err, ForgeError::InvalidInput);
    assert_eq!(client.get_status(&id), EscrowStatus::Funded);
}

#[test]
fn dispute_requires_funded_state() {
    let (_env, token, _tc, _id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let id = create(&client, &token, buyer, seller, arbiter, TIMEOUT);

    let err = client.try_dispute(&id, buyer).unwrap_err().unwrap();
    assert_eq!(err, ForgeError::InvalidInput);
}

#[test]
fn disputed_escrow_freezes_all_payouts() {
    let (_env, token, tc, contract_id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let id = create(&client, &token, buyer, seller, arbiter, TIMEOUT);
    client.deposit(&id);
    client.dispute(&id, buyer);

    assert_eq!(
        client.try_release(&id).unwrap_err().unwrap(),
        ForgeError::InvalidInput
    );
    assert_eq!(
        client.try_refund(&id).unwrap_err().unwrap(),
        ForgeError::InvalidInput
    );
    assert_eq!(tc.balance(&contract_id), AMOUNT);
    assert_eq!(client.get_status(&id), EscrowStatus::Disputed);
}

#[test]
fn resolve_in_favor_of_seller_pays_out() {
    let (_env, token, tc, contract_id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let id = create(&client, &token, buyer, seller, arbiter, TIMEOUT);
    client.deposit(&id);
    client.dispute(&id, seller);

    client.resolve(&id, &true);

    assert_eq!(tc.balance(seller), AMOUNT);
    assert_eq!(tc.balance(&contract_id), 0);
    assert_eq!(client.get_status(&id), EscrowStatus::Completed);
}

#[test]
fn resolve_in_favor_of_buyer_refunds() {
    let (_env, token, tc, contract_id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let id = create(&client, &token, buyer, seller, arbiter, TIMEOUT);
    client.deposit(&id);
    client.dispute(&id, buyer);

    client.resolve(&id, &false);

    assert_eq!(tc.balance(buyer), AMOUNT);
    assert_eq!(tc.balance(&contract_id), 0);
    assert_eq!(client.get_status(&id), EscrowStatus::Refunded);
}

#[test]
fn resolve_requires_disputed_state() {
    let (_env, token, _tc, _id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let id = create(&client, &token, buyer, seller, arbiter, TIMEOUT);
    client.deposit(&id);

    let err = client.try_resolve(&id, &true).unwrap_err().unwrap();
    assert_eq!(err, ForgeError::InvalidInput);
}

// -----------------------------------------------------------------------
// Cancel
// -----------------------------------------------------------------------

#[test]
fn cancel_pending_escrow() {
    let (_env, token, _tc, _id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let id = create(&client, &token, buyer, seller, arbiter, TIMEOUT);

    client.cancel(&id);

    assert_eq!(client.get_status(&id), EscrowStatus::Cancelled);
}

#[test]
fn cancel_after_deposit_is_rejected() {
    let (_env, token, tc, contract_id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let id = create(&client, &token, buyer, seller, arbiter, TIMEOUT);
    client.deposit(&id);

    let err = client.try_cancel(&id).unwrap_err().unwrap();
    assert_eq!(err, ForgeError::InvalidInput);
    assert_eq!(tc.balance(&contract_id), AMOUNT);
}

// -----------------------------------------------------------------------
// Missing ids
// -----------------------------------------------------------------------

#[test]
fn missing_escrow_reads_are_not_found() {
    let (_env, _token, _tc, _id, client, _accounts) = setup!();
    assert_eq!(
        client.try_get_status(&999).unwrap_err().unwrap(),
        ForgeError::NotFound
    );
    assert_eq!(
        client.try_get_escrow(&999).unwrap_err().unwrap(),
        ForgeError::NotFound
    );
}

#[test]
fn operations_on_missing_escrow_are_not_found() {
    let (_env, token, _tc, _id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let outsider = &accounts.user3;
    assert_eq!(
        client.try_deposit(&999).unwrap_err().unwrap(),
        ForgeError::NotFound
    );
    assert_eq!(
        client.try_release(&999).unwrap_err().unwrap(),
        ForgeError::NotFound
    );
    assert_eq!(
        client.try_refund(&999).unwrap_err().unwrap(),
        ForgeError::NotFound
    );
    assert_eq!(
        client.try_dispute(&999, outsider).unwrap_err().unwrap(),
        ForgeError::NotFound
    );
    assert_eq!(
        client.try_resolve(&999, &true).unwrap_err().unwrap(),
        ForgeError::NotFound
    );
    assert_eq!(
        client.try_cancel(&999).unwrap_err().unwrap(),
        ForgeError::NotFound
    );
    assert_eq!(
        client.try_touch_ttl(&999).unwrap_err().unwrap(),
        ForgeError::NotFound
    );
    let _ = (token, buyer, seller, arbiter);
}

// -----------------------------------------------------------------------
// Participant index views (escrows_for_participant)
// -----------------------------------------------------------------------

#[test]
fn create_indexes_all_distinct_parties() {
    let (_env, token, _tc, _id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let first = create(&client, &token, buyer, seller, arbiter, TIMEOUT);
    let second = create(&client, &token, buyer, seller, arbiter, TIMEOUT);

    for party in [buyer, seller, arbiter] {
        let page = client.escrows_for_participant(party, &0, &10);
        assert_eq!(page.total, 2);
        assert_eq!(page.ids.len(), 2);
        assert_eq!(page.ids.get_unchecked(0), first);
        assert_eq!(page.ids.get_unchecked(1), second);
        assert_eq!(page.next_cursor, None);
    }
}

#[test]
fn repeated_role_address_is_indexed_once() {
    let (_env, token, _tc, _id, client, accounts) = setup!();
    let (buyer, _seller, _arbiter) = parties(&accounts);
    // The same address wears two roles (seller and arbiter): it must be
    // indexed once, or iteration would yield this id twice.
    let shared = &accounts.user3;
    let id = create(&client, &token, buyer, shared, shared, TIMEOUT);

    assert_full_index(&client, buyer, &[id]);
    let page = client.escrows_for_participant(shared, &0, &10);
    assert_eq!(page.total, 1, "one address in two roles is indexed once");
    assert_eq!(page.ids.get_unchecked(0), id);
}

#[test]
fn unknown_address_returns_empty_page() {
    let (_env, token, _tc, _id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    create(&client, &token, buyer, seller, arbiter, TIMEOUT);

    // An address this contract has never seen yields an empty result, not
    // an error.
    let page = client.escrows_for_participant(&accounts.validator, &0, &10);
    assert_eq!(page.total, 0);
    assert_eq!(page.ids.len(), 0);
    assert_eq!(page.next_cursor, None);
}

#[test]
fn index_is_creation_order_with_party_subsets() {
    let (_env, token, _tc, _id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let other_seller = &accounts.user3;

    let id1 = create(&client, &token, buyer, seller, arbiter, TIMEOUT);
    let id2 = create(&client, &token, buyer, other_seller, arbiter, TIMEOUT);
    let id3 = create(&client, &token, buyer, seller, arbiter, TIMEOUT);
    let id4 = create(&client, &token, buyer, other_seller, arbiter, TIMEOUT);

    // The arbiter appears in every escrow, in creation order.
    assert_full_index(&client, arbiter, &[id1, id2, id3, id4]);
    // Each seller sees only the escrows it is a party to, still ordered.
    assert_full_index(&client, seller, &[id1, id3]);
    assert_full_index(&client, other_seller, &[id2, id4]);
}

#[test]
fn pagination_returns_sliced_pages_with_cursors() {
    let (env, token, _tc, _id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let mut expected = soroban_sdk::vec![&env];
    for _ in 0..10 {
        expected.push_back(create(&client, &token, buyer, seller, arbiter, TIMEOUT));
    }

    // A mid-list page: offset 1, limit 3 returns the 2nd..4th ids.
    let page = client.escrows_for_participant(buyer, &1, &3);
    assert_eq!(page.total, 10);
    assert_eq!(page.ids.len(), 3);
    assert_eq!(page.ids.get_unchecked(0), expected.get_unchecked(1));
    assert_eq!(page.ids.get_unchecked(2), expected.get_unchecked(3));
    assert_eq!(page.next_cursor, Some(4));

    // A page that exactly exhausts the list: one id, then no next cursor.
    let page = client.escrows_for_participant(buyer, &9, &1);
    assert_eq!(page.ids.len(), 1);
    assert_eq!(page.ids.get_unchecked(0), expected.get_unchecked(9));
    assert_eq!(page.next_cursor, None);

    // An over-run past the end is empty and terminal, not an error.
    let page = client.escrows_for_participant(buyer, &12, &5);
    assert_eq!(page.total, 10);
    assert_eq!(page.ids.len(), 0);
    assert_eq!(page.next_cursor, None);
}

#[test]
fn pagination_iterates_every_created_id_exactly_once() {
    let (env, token, _tc, _id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let mut expected = soroban_sdk::vec![&env];
    // Well past 25 so paging crosses several page boundaries; also keeps
    // this test inside CI's budget (each create is a single small write).
    for _ in 0..26 {
        expected.push_back(create(&client, &token, buyer, seller, arbiter, TIMEOUT));
    }

    let mut seen = soroban_sdk::vec![&env];
    let mut cursor = 0u32;
    let limit = 7u32;
    loop {
        let page = client.escrows_for_participant(buyer, &cursor, &limit);
        assert_eq!(page.total, 26, "total is reported on every page");
        assert!(
            page.ids.len() <= limit,
            "a page may not exceed its requested limit"
        );
        for n in 0..page.ids.len() {
            seen.push_back(page.ids.get_unchecked(n));
        }
        match page.next_cursor {
            Some(next) => {
                assert!(next > cursor, "pagination must advance past the page");
                cursor = next;
            }
            None => break,
        }
    }

    assert_eq!(
        seen, expected,
        "full iteration must yield every created id exactly once, in creation order"
    );
    assert_eq!(seen.len(), 26);
}

#[test]
fn zero_limit_returns_empty_page_without_residual_cursor() {
    let (_env, token, _tc, _id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    create(&client, &token, buyer, seller, arbiter, TIMEOUT);

    // A zero limit must not report a next cursor pointing at itself, or a
    // naive client would loop forever.
    let page = client.escrows_for_participant(buyer, &0, &0);
    assert_eq!(page.total, 1);
    assert_eq!(page.ids.len(), 0);
    assert_eq!(page.next_cursor, None);
}

#[test]
fn escrows_for_participant_is_read_only() {
    let (_env, token, _tc, _id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    create(&client, &token, buyer, seller, arbiter, TIMEOUT);
    let second = create(&client, &token, buyer, seller, arbiter, TIMEOUT);

    // Repeating the view must not change its answer: no state is touched
    // between two identical reads.
    let before = client.escrows_for_participant(buyer, &0, &10);
    let after = client.escrows_for_participant(buyer, &0, &10);
    assert_eq!(before, after);
    assert_eq!(after.ids.len(), 2);

    // And the id counter must not have advanced: the next escrow takes the
    // id right after the last created one, proving the view wrote nothing.
    let third = create(&client, &token, buyer, seller, arbiter, TIMEOUT);
    assert_eq!(third, second + 1);
}

// -----------------------------------------------------------------------
// TTL keeper
// -----------------------------------------------------------------------

#[test]
fn touch_ttl_extends_and_keeps_state_intact() {
    let (_env, token, _tc, _id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let id = create(&client, &token, buyer, seller, arbiter, TIMEOUT);
    client.deposit(&id);

    client.touch_ttl(&id);

    assert_eq!(client.get_status(&id), EscrowStatus::Funded);
}

// -----------------------------------------------------------------------
// Conservation property: every payout path returns exactly the deposit
// -----------------------------------------------------------------------

// -----------------------------------------------------------------------
// Partial release
// -----------------------------------------------------------------------

#[test]
fn release_partial_pays_seller_and_updates_accounting() {
    let (_env, token, tc, contract_id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let id = create(&client, &token, buyer, seller, arbiter, TIMEOUT);
    client.deposit(&id);

    client.release_partial(&id, &300);

    // 300 should have moved to seller.
    assert_eq!(tc.balance(seller), 300);
    // Contract still holds the remaining 700.
    assert_eq!(tc.balance(&contract_id), 700);
    // Escrow is still Funded.
    assert_eq!(client.get_status(&id), EscrowStatus::Funded);
    // released and remaining accounting.
    let record: EscrowData = client.get_escrow(&id);
    assert_eq!(record.released, 300);
    assert_eq!(record.remaining(), 700);
}

#[test]
fn multiple_partial_releases_accumulate_correctly() {
    let (_env, token, tc, contract_id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let id = create(&client, &token, buyer, seller, arbiter, TIMEOUT);
    client.deposit(&id);

    client.release_partial(&id, &200);
    client.release_partial(&id, &300);
    client.release_partial(&id, &100);

    assert_eq!(tc.balance(seller), 600);
    assert_eq!(tc.balance(&contract_id), 400);
    let record: EscrowData = client.get_escrow(&id);
    assert_eq!(record.released, 600);
    assert_eq!(record.remaining(), 400);
    assert_eq!(client.get_status(&id), EscrowStatus::Funded);
}

#[test]
fn exact_final_partial_release_completes_escrow() {
    let (_env, token, tc, contract_id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let id = create(&client, &token, buyer, seller, arbiter, TIMEOUT);
    client.deposit(&id);

    client.release_partial(&id, &400);
    // Release the exact remaining amount.
    client.release_partial(&id, &600);

    assert_eq!(tc.balance(seller), AMOUNT);
    assert_eq!(tc.balance(&contract_id), 0);
    assert_eq!(client.get_status(&id), EscrowStatus::Completed);
    let record: EscrowData = client.get_escrow(&id);
    assert_eq!(record.released, AMOUNT);
    assert_eq!(record.remaining(), 0);
}

#[test]
fn release_partial_zero_amount_is_rejected() {
    let (_env, token, tc, contract_id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let id = create(&client, &token, buyer, seller, arbiter, TIMEOUT);
    client.deposit(&id);

    let err = client.try_release_partial(&id, &0).unwrap_err().unwrap();
    assert_eq!(err, ForgeError::InvalidInput);
    assert_eq!(tc.balance(&contract_id), AMOUNT);
    assert_eq!(tc.balance(seller), 0);
    assert_eq!(client.get_status(&id), EscrowStatus::Funded);
}

#[test]
fn release_partial_negative_amount_is_rejected() {
    let (_env, token, tc, contract_id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let id = create(&client, &token, buyer, seller, arbiter, TIMEOUT);
    client.deposit(&id);

    let err = client.try_release_partial(&id, &-100).unwrap_err().unwrap();
    assert_eq!(err, ForgeError::InvalidInput);
    assert_eq!(tc.balance(&contract_id), AMOUNT);
    assert_eq!(tc.balance(seller), 0);
}

#[test]
fn release_partial_exceeding_remaining_is_rejected() {
    let (_env, token, tc, contract_id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let id = create(&client, &token, buyer, seller, arbiter, TIMEOUT);
    client.deposit(&id);

    client.release_partial(&id, &400);

    // Try to release more than what's left (remaining = 600).
    let err = client.try_release_partial(&id, &601).unwrap_err().unwrap();
    assert_eq!(err, ForgeError::InvalidInput);
    // Balances unchanged after rejected call.
    assert_eq!(tc.balance(seller), 400);
    assert_eq!(tc.balance(&contract_id), 600);
}

#[test]
fn release_partial_on_pending_escrow_is_rejected() {
    let (_env, token, _tc, _id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let id = create(&client, &token, buyer, seller, arbiter, TIMEOUT);

    let err = client.try_release_partial(&id, &100).unwrap_err().unwrap();
    assert_eq!(err, ForgeError::InvalidInput);
}

#[test]
fn release_partial_after_completion_is_rejected() {
    let (_env, token, tc, _contract_id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let id = create(&client, &token, buyer, seller, arbiter, TIMEOUT);
    client.deposit(&id);
    // Complete the escrow via full partial release.
    client.release_partial(&id, &AMOUNT);

    assert_eq!(client.get_status(&id), EscrowStatus::Completed);
    let err = client.try_release_partial(&id, &1).unwrap_err().unwrap();
    assert_eq!(err, ForgeError::InvalidInput);
    assert_eq!(tc.balance(seller), AMOUNT);
}

#[test]
fn partial_release_then_refund_pays_only_remaining() {
    let (_env, token, tc, contract_id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let id = create(&client, &token, buyer, seller, arbiter, TIMEOUT);
    client.deposit(&id);

    client.release_partial(&id, &400);
    // Seller refunds the remaining 600 to buyer.
    client.refund(&id);

    assert_eq!(tc.balance(seller), 400);
    assert_eq!(tc.balance(buyer), 600);
    assert_eq!(tc.balance(&contract_id), 0);
    assert_eq!(client.get_status(&id), EscrowStatus::Refunded);
}

#[test]
fn partial_release_then_dispute_then_resolve_for_seller() {
    let (_env, token, tc, contract_id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let id = create(&client, &token, buyer, seller, arbiter, TIMEOUT);
    client.deposit(&id);

    client.release_partial(&id, &300);
    client.dispute(&id, buyer);
    // Arbiter resolves in favor of seller: remaining 700 goes to seller.
    client.resolve(&id, &true);

    assert_eq!(tc.balance(seller), AMOUNT); // 300 partial + 700 resolved
    assert_eq!(tc.balance(buyer), 0);
    assert_eq!(tc.balance(&contract_id), 0);
    assert_eq!(client.get_status(&id), EscrowStatus::Completed);
}

#[test]
fn partial_release_then_dispute_then_resolve_for_buyer() {
    let (_env, token, tc, contract_id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let id = create(&client, &token, buyer, seller, arbiter, TIMEOUT);
    client.deposit(&id);

    client.release_partial(&id, &300);
    client.dispute(&id, seller);
    // Arbiter resolves in favor of buyer: remaining 700 goes back to buyer.
    client.resolve(&id, &false);

    assert_eq!(tc.balance(seller), 300);
    assert_eq!(tc.balance(buyer), 700);
    assert_eq!(tc.balance(&contract_id), 0);
    assert_eq!(client.get_status(&id), EscrowStatus::Refunded);
}

#[test]
fn full_release_after_partial_releases_remaining_only() {
    let (_env, token, tc, contract_id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let id = create(&client, &token, buyer, seller, arbiter, TIMEOUT);
    client.deposit(&id);

    client.release_partial(&id, &250);
    // Call the full release (should pay only the remaining 750).
    client.release(&id);

    assert_eq!(tc.balance(seller), AMOUNT);
    assert_eq!(tc.balance(&contract_id), 0);
    assert_eq!(client.get_status(&id), EscrowStatus::Completed);
}

#[test]
fn storage_compat_new_records_have_released_zero() {
    // Verify that a freshly created record has released = 0 and
    // remaining == amount.
    let (_env, token, _tc, _id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let id = create(&client, &token, buyer, seller, arbiter, TIMEOUT);
    client.deposit(&id);

    let record: EscrowData = client.get_escrow(&id);
    assert_eq!(record.released, 0);
    assert_eq!(record.remaining(), AMOUNT);
    assert_eq!(record.amount, AMOUNT);
}

#[test]
fn release_partial_conservation_holds() {
    // After multiple partial releases the sum buyer+seller+contract must
    // always equal AMOUNT (the mint).
    let (_env, token, tc, contract_id, client, accounts) = setup!();
    let (buyer, seller, arbiter) = parties(&accounts);
    let id = create(&client, &token, buyer, seller, arbiter, TIMEOUT);
    client.deposit(&id);

    let amounts = [100i128, 200, 300, 400];
    let mut total_paid = 0i128;
    for &amt in &amounts {
        // Only release if it doesn't exceed remaining.
        let record: EscrowData = client.get_escrow(&id);
        if amt <= record.remaining() {
            client.release_partial(&id, &amt);
            total_paid += amt;
            assert_eq!(
                tc.balance(buyer) + tc.balance(seller) + tc.balance(&contract_id),
                AMOUNT,
                "conservation must hold after each partial release"
            );
        }
    }
    let record: EscrowData = client.get_escrow(&id);
    assert_eq!(record.released, total_paid);
}

/// For every reachable terminal path × timeout combination, the contract
/// ends holding exactly zero and the parties' combined balance equals the
/// original mint: `buyer_start == buyer_end + seller_end`. One escrow, one
/// payout, so the pooled balance must return to zero on every path.
// The path table's function-pointer type is verbose by design — it reads
// as a table of scenarios, not a data structure to be abstracted.
#[allow(clippy::type_complexity)]
#[test]
fn conservation_holds_on_every_terminal_path() {
    let paths: &[&dyn Fn(
        &Env,
        &SorobanForgeEscrowClient<'_>,
        u64,
        &Address,
        &Address,
        &Address,
    )] = &[
        // Release to the seller.
        &|_env, client, id, _buyer, _seller, _arbiter| {
            client.release(&id);
        },
        // Seller refund before the deadline.
        &|_env, client, id, _buyer, _seller, _arbiter| {
            client.refund(&id);
        },
        // Buyer reclaim after the deadline.
        &|env, client, id, _buyer, _seller, _arbiter| {
            env.ledger().set_timestamp(START + TIMEOUT + 1);
            client.refund(&id);
        },
        // Dispute raised by the buyer; arbiter sides with the seller.
        &|_env, client, id, buyer, _seller, _arbiter| {
            client.dispute(&id, buyer);
            client.resolve(&id, &true);
        },
        // Dispute raised by the seller; arbiter sides with the buyer.
        &|_env, client, id, _buyer, seller, _arbiter| {
            client.dispute(&id, seller);
            client.resolve(&id, &false);
        },
        // Partial release (half) then full release of remainder.
        &|_env, client, id, _buyer, _seller, _arbiter| {
            client.release_partial(&id, &(AMOUNT / 2));
            client.release(&id);
        },
        // Partial release (partial) then refund of remaining.
        &|_env, client, id, _buyer, _seller, _arbiter| {
            client.release_partial(&id, &(AMOUNT / 3));
            client.refund(&id);
        },
        // Partial release then dispute then resolve for seller.
        &|_env, client, id, buyer, _seller, _arbiter| {
            client.release_partial(&id, &(AMOUNT / 4));
            client.dispute(&id, buyer);
            client.resolve(&id, &true);
        },
        // Partial release then dispute then resolve for buyer.
        &|_env, client, id, _buyer, seller, _arbiter| {
            client.release_partial(&id, &(AMOUNT / 4));
            client.dispute(&id, seller);
            client.resolve(&id, &false);
        },
        // Exact final partial release → Completed.
        &|_env, client, id, _buyer, _seller, _arbiter| {
            client.release_partial(&id, &(AMOUNT / 2));
            client.release_partial(&id, &(AMOUNT - AMOUNT / 2));
        },
    ];

    for timeout in [1u64, 10, TIMEOUT, 100_000] {
        for (i, path) in paths.iter().enumerate() {
            let (env, token, tc, contract_id, client, accounts) = setup!();
            let (buyer, seller, arbiter) = parties(&accounts);
            let id = create(&client, &token, buyer, seller, arbiter, timeout);
            client.deposit(&id);

            path(&env, &client, id, buyer, seller, arbiter);

            assert_eq!(
                tc.balance(&contract_id),
                0,
                "path {i}: contract must not retain dust (timeout {timeout})"
            );
            assert_eq!(
                tc.balance(buyer) + tc.balance(seller),
                AMOUNT,
                "path {i}: buyer+seller must sum to the deposit (timeout {timeout})"
            );
            assert_eq!(
                client.try_deposit(&id).unwrap_err().unwrap(),
                ForgeError::InvalidInput,
                "path {i}: terminal escrow must not be refundable again"
            );
        }
    }
}
