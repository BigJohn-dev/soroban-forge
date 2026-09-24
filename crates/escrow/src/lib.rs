#![no_std]

//! # Soroban Forge — Escrow contract
//!
//! A three-party escrow that **holds real tokens**: a `buyer`, a `seller`,
//! and an `arbiter` agree on an `amount` of a single SEP-41 token and a
//! `timeout`. The buyer funds the escrow on-chain, and the contract
//! custodies the tokens until release, refund, or arbitration:
//!
//! ```text
//! Pending --deposit--> Funded --release--> Completed (seller paid)
//!                     |        --refund--> Refunded  (buyer back)
//!                     |        --dispute--> Disputed --resolve--> Completed | Refunded
//!          --cancel--> Cancelled (before funding only)
//! ```
//!
//! ## Custody model
//!
//! On `deposit`, the token contract moves `amount` from the buyer to this
//! contract's own address. From that moment the funds are inside the
//! contract and can only leave via `release` (to seller), `refund` (to
//! buyer), or `resolve` (either, by arbiter decision). There is no admin
//! key and no other exit.
//!
//! ## Ordering discipline (load-bearing)
//!
//! Every method that moves tokens performs the token transfer **first**
//! and writes state **after** the transfer succeeds. A failed transfer
//! reverts the whole invocation with storage untouched — there is no
//! state/ledger divergence window and no recovery path needed. The
//! inverse ordering (state first, transfer second) would strand funds
//! behind a failed transfer and is the classic escrow bug.
//!
//! ## Authorization model
//!
//! - `create_escrow` — **buyer only**. The seller is recorded but does
//!   not authorize creation: nothing of theirs is at risk before funding,
//!   their consent is expressed by the refund path (they can return funds
//!   at any time pre-deadline), and requiring their signature at creation
//!   forces a two-signer transaction that is hostile to wallets and CLI
//!   flows (this was learned live: a two-auth create produced `TxBadAuth`
//!   on every standard signing path during the testnet demo).
//! - `deposit` — buyer authorizes; their authorization covers the nested
//!   token pull.
//! - `release` — seller authorizes, confirming delivery. The buyer
//!   authorizing their own payout would make this a confirmation flow,
//!   not escrow.
//! - `refund` — seller before the deadline; buyer may reclaim after the
//!   deadline.
//! - `dispute` — the **claimant** (buyer or seller) is passed explicitly
//!   and must be one of the two parties; their `require_auth` proves the
//!   claim. Soroban has no "auth by A-or-B" primitive, so an explicit
//!   claimant parameter is the honest way to express either-party
//!   authorization.
//! - `resolve` — arbiter only; the decision is final.
//! - `cancel` — buyer, while `Pending`.
//!
//! ## Storage and TTL
//!
//! Each escrow is its own **persistent** entry (`DataKey::Escrow(id)`)
//! so the byte budget scales per record. Every participant's creation-order
//! index is likewise a **persistent** per-party entry
//! (`DataKey::ParticipantIndex(Address)`), written once per escrow creation:
//! a party's id list grows with their escrow count, so it cannot live in
//! instance storage (one party's growth would tax every shared instance
//! read). The only instance entry is the id counter. Every write bumps the
//! entry's TTL with the standard threshold/extend-to pattern, and
//! `touch_ttl` is a permissionless keeper entrypoint for escrows that sit
//! idle near expiry.

// WASM target guard: SDK 27 contracts must be built for wasm32v1-none.
// wasm32-unknown-unknown (os=unknown) can emit features the Soroban
// runtime rejects; wasm32v1-none (os=none) is the supported target.
#[cfg(all(target_family = "wasm", not(target_os = "none")))]
compile_error!(
    "build for wasm32v1-none (see rust-toolchain.toml); wasm32-unknown-unknown is not supported by the Soroban runtime"
);

use soroban_sdk::{
    contract, contractclient, contractevent, contractimpl, contracttype, token, Address, Env, Vec,
};

use soroban_forge_shared_utils::ForgeError;

/// Ledger-time constants for TTL bumps.
///
/// One ledger closes roughly every 5 seconds, so 17,280 ledgers ≈ 1 day.
/// `BUMP_AMOUNT` is the lifetime written on every touch; `BUMP_THRESHOLD`
/// is how close to expiry an entry must be before a bump applies. The
/// 30-day horizon comfortably covers a funded escrow between keeper
/// touches.
mod ttl {
    pub const DAY_IN_LEDGERS: u32 = 17_280;
    /// Lifetime applied on every TTL touch.
    pub const BUMP_AMOUNT: u32 = 30 * DAY_IN_LEDGERS;
    /// Bump only when the entry is within this window of expiring.
    pub const BUMP_THRESHOLD: u32 = BUMP_AMOUNT - DAY_IN_LEDGERS;
}

/// Public interface for the Soroban Forge escrow contract.
#[contractclient(name = "SorobanForgeEscrowClient")]
pub trait SorobanForgeEscrow {
    /// Create a new escrow and return its stable id.
    ///
    /// Requires `amount > 0`, `timeout > 0`. Only the buyer authorizes
    /// creation (see the authorization model in the module docs); the
    /// seller takes no risk until funding occurs.
    ///
    /// # Errors
    ///
    /// * [`ForgeError::InvalidInput`] — non-positive amount or zero timeout.
    /// * [`ForgeError::ArithmeticOverflow`] — the id counter overflowed.
    fn create_escrow(
        env: Env,
        buyer: Address,
        seller: Address,
        arbiter: Address,
        token: Address,
        amount: i128,
        timeout: u64,
    ) -> Result<u64, ForgeError>;

    /// Fund the escrow, pulling `amount` of the escrow's token from the
    /// buyer into this contract. Requires the buyer; only valid while
    /// `Pending`.
    ///
    /// The token transfer is performed **before** any state is written, so
    /// a failed transfer leaves no partial state (see module docs).
    ///
    /// # Errors
    ///
    /// * [`ForgeError::NotFound`] — no escrow with this id.
    /// * [`ForgeError::InvalidInput`] — escrow is not `Pending`.
    /// * [`ForgeError::TokenTransferFailed`] — the token contract rejected
    ///   the transfer (insufficient balance, missing trustline, deauthorized
    ///   token, or undeployed token contract).
    fn deposit(env: Env, escrow_id: u64) -> Result<(), ForgeError>;

    /// Release funds to the seller. Requires the seller (confirms
    /// delivery); only valid while `Funded`.
    ///
    /// # Errors
    ///
    /// * [`ForgeError::NotFound`] — no escrow with this id.
    /// * [`ForgeError::InvalidInput`] — escrow is not `Funded`.
    /// * [`ForgeError::TokenTransferFailed`] — the token contract rejected
    ///   the payout.
    fn release(env: Env, escrow_id: u64) -> Result<(), ForgeError>;

    /// Refund the buyer.
    ///
    /// Before the deadline the seller may refund; after the deadline the
    /// buyer may reclaim. Only valid while `Funded`.
    ///
    /// # Errors
    ///
    /// * [`ForgeError::NotFound`] — no escrow with this id.
    /// * [`ForgeError::InvalidInput`] — escrow is not `Funded`.
    /// * [`ForgeError::Unauthorized`] — wrong party for the current phase.
    /// * [`ForgeError::TokenTransferFailed`] — the token contract rejected
    ///   the payout.
    fn refund(env: Env, escrow_id: u64) -> Result<(), ForgeError>;

    /// Raise a dispute. `claimant` must be the buyer or the seller and
    /// must authorize the call; only valid while `Funded`. Freezes the
    /// escrow until the arbiter resolves it.
    ///
    /// # Errors
    ///
    /// * [`ForgeError::NotFound`] — no escrow with this id.
    /// * [`ForgeError::InvalidInput`] — escrow is not `Funded`, or the
    ///   claimant is neither buyer nor seller.
    /// * [`ForgeError::Unauthorized`] — the claimant did not authorize.
    fn dispute(env: Env, escrow_id: u64, claimant: Address) -> Result<(), ForgeError>;

    /// Resolve a dispute. Requires the arbiter; only valid while
    /// `Disputed`. Pays the full amount to the seller (`true`) or back to
    /// the buyer (`false`). The decision is final.
    ///
    /// # Errors
    ///
    /// * [`ForgeError::NotFound`] — no escrow with this id.
    /// * [`ForgeError::InvalidInput`] — escrow is not `Disputed`.
    /// * [`ForgeError::Unauthorized`] — caller is not the arbiter.
    /// * [`ForgeError::TokenTransferFailed`] — the token contract rejected
    ///   the payout.
    fn resolve(env: Env, escrow_id: u64, in_favor_of_seller: bool) -> Result<(), ForgeError>;

    /// Cancel a `Pending` escrow before it is funded. Requires the buyer.
    ///
    /// # Errors
    ///
    /// * [`ForgeError::NotFound`] — no escrow with this id.
    /// * [`ForgeError::InvalidInput`] — escrow is not `Pending`.
    fn cancel(env: Env, escrow_id: u64) -> Result<(), ForgeError>;

    /// Read the current lifecycle status.
    ///
    /// # Errors
    ///
    /// * [`ForgeError::NotFound`] — no escrow with this id.
    fn get_status(env: Env, escrow_id: u64) -> Result<EscrowStatus, ForgeError>;

    /// Read the full escrow record.
    ///
    /// # Errors
    ///
    /// * [`ForgeError::NotFound`] — no escrow with this id.
    fn get_escrow(env: Env, escrow_id: u64) -> Result<EscrowData, ForgeError>;

    /// List the escrow ids a participant is party to (buyer, seller, or
    /// arbiter), in creation order.
    ///
    /// Read-only: requires no authorization and never mutates storage. An
    /// address with no escrows — or an address this contract has never seen
    /// — returns an empty page, not an error. Good for building "my
    /// escrows" views without an off-chain indexer.
    ///
    /// Pagination is a simple offset/limit scheme. `cursor` is the offset
    /// of the first id to return and `limit` caps the page size. The
    /// returned [`ParticipantEscrowsPage::next_cursor`] continues the next
    /// page; `None` means the list is exhausted. A `limit` of `0` returns
    /// an empty page with no next cursor.
    ///
    /// Iterating by replaying `next_cursor` until it is `None` yields every
    /// escrow involving the participant exactly once, in creation order.
    ///
    /// # Errors
    ///
    /// This view never errors; unknown participants yield an empty page.
    fn escrows_for_participant(
        env: Env,
        participant: Address,
        cursor: u32,
        limit: u32,
    ) -> ParticipantEscrowsPage;

    /// Permissionless TTL keeper: bumps the escrow entry's TTL to the
    /// [`ttl::BUMP_AMOUNT`] horizon when it falls inside
    /// [`ttl::BUMP_THRESHOLD`]. Call periodically for escrows that must
    /// outlive their entry's current TTL. Costs fees; changes nothing
    /// else.
    ///
    /// # Errors
    ///
    /// * [`ForgeError::NotFound`] — no escrow with this id.
    fn touch_ttl(env: Env, escrow_id: u64) -> Result<(), ForgeError>;
}

/// Lifecycle state of an escrow.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EscrowStatus {
    /// Created but not funded.
    Pending,
    /// Tokens held by the contract.
    Funded,
    /// Released to the seller.
    Completed,
    /// Refunded to the buyer.
    Refunded,
    /// A party raised a dispute; frozen until the arbiter resolves.
    Disputed,
    /// Cancelled before funding.
    Cancelled,
}

/// A single three-party escrow record.
#[contracttype]
#[derive(Clone, Debug)]
pub struct EscrowData {
    /// Stable id, never reused.
    pub escrow_id: u64,
    /// Party funding the escrow and the default refund recipient.
    pub buyer: Address,
    /// Party paid on release.
    pub seller: Address,
    /// Neutral party deciding disputes. Recorded at creation; authorizes
    /// only `resolve`.
    pub arbiter: Address,
    /// SEP-41 token contract custodied by this escrow.
    pub token: Address,
    /// Amount of `token` held in custody.
    pub amount: i128,
    /// Seconds after `created_at` at which the buyer may self-refund.
    pub timeout: u64,
    /// Current lifecycle state.
    pub status: EscrowStatus,
    /// Unix timestamp of creation.
    pub created_at: u64,
}

/// A page of escrow ids involving a participant.
///
/// Returned by [`SorobanForgeEscrow::escrows_for_participant`]; powered by
/// the per-party persistent index written at creation.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParticipantEscrowsPage {
    /// Escrow ids on this page, in creation order.
    pub ids: Vec<u64>,
    /// Total escrows involving the participant across all pages.
    pub total: u32,
    /// Offset for the next page, or `None` when `ids` ends the participant's
    /// list. Replay it until `None` to iterate the whole list.
    pub next_cursor: Option<u32>,
}

/// Storage keys. Escrow records are per-id **persistent** entries so the
/// byte budget scales per record; only the id counter lives in instance
/// storage (one small entry, written once per creation).
#[contracttype]
enum DataKey {
    /// The escrow record for `u64` id.
    Escrow(u64),
    /// Monotonic id counter.
    Count,
    /// Creation-order ids of every escrow the `Address` participates in
    /// (as buyer, seller, or arbiter), written once per escrow creation.
    ///
    /// Persistent, not instance: the entry grows with that party's escrow
    /// count and is written only on the create path, so parking it in
    /// instance storage would bloat a shared hot entry with one party's
    /// growth (the same per-record-scaling argument that puts `Escrow(id)`
    /// in persistent storage rather than a single instance map). It also
    /// carries its own extensible TTL under the standard `bump_entry`
    /// threshold/extend pattern, mirroring every other persistent write.
    ///
    /// The value is a single `Vec<u64>` appended in creation order rather
    /// than sharded per-party keys (`ParticipantIndex(Address, u64)`).
    /// Tradeoff: one entry per party keeps reads cheap — a page is one
    /// entry read plus an in-memory slice — and appends are one read + one
    /// rewrite of that party's (id-sized, 8 bytes each) list. The cost is
    /// that a party's entry grows unboundedly and each append rewrites the
    /// whole list; for the overwhelming majority of parties the list stays
    /// tiny, and a party accumulating so many escrows that a single entry
    /// fills (Soroban's ~64 KB entry cap is tens of thousands of ids)
    /// would migrate to a sharded scheme — a compatible upgrade since the
    /// view only ever reads through this key class.
    ParticipantIndex(Address),
}

/// The deployable escrow contract.
#[contract]
pub struct Escrow;

#[contractimpl]
impl Escrow {
    // -------------------------------------------------------------------
    // Lifecycle
    // -------------------------------------------------------------------

    /// Create a new escrow and return its stable id.
    ///
    /// Only the buyer authorizes at creation. The arbiter does not
    /// authorize either: they must be able to `resolve` later even if
    /// they never participated in creation.
    pub fn create_escrow(
        env: Env,
        buyer: Address,
        seller: Address,
        arbiter: Address,
        token: Address,
        amount: i128,
        timeout: u64,
    ) -> Result<u64, ForgeError> {
        if amount <= 0 {
            return Err(ForgeError::InvalidInput);
        }
        if timeout == 0 {
            return Err(ForgeError::InvalidInput);
        }
        // Buyer-only authorization: a two-signer create (buyer + seller)
        // was tried live on testnet and failed on every standard signing
        // path (`TxBadAuth` from the CLI, `TxMalformed` from manually
        // chained signatures). The seller loses nothing by being recorded
        // without consenting — their protections are the refund and
        // dispute paths once funded.
        buyer.require_auth();

        let id = Self::next_id(&env)?;
        // Distinct parties only: one address in several roles (e.g. seller
        // == arbiter) is indexed once so iteration yields this id exactly
        // once, per the index's read contract.
        let mut participants = Vec::new(&env);
        participants.push_back(buyer.clone());
        if seller != buyer {
            participants.push_back(seller.clone());
        }
        if arbiter != buyer && arbiter != seller {
            participants.push_back(arbiter.clone());
        }
        let escrow = EscrowData {
            escrow_id: id,
            buyer,
            seller,
            arbiter,
            token,
            amount,
            timeout,
            status: EscrowStatus::Pending,
            created_at: env.ledger().timestamp(),
        };
        env.storage()
            .persistent()
            .set(&DataKey::Escrow(id), &escrow);
        bump_entry(&env, &DataKey::Escrow(id));
        // Index write joins the rest of the success-path writes: it runs
        // after every fallible step (validation, `require_auth`, id
        // allocation), so it cannot observe or create partial state.
        Self::index_participants(&env, id, &participants);
        events::escrow_created(&env, &escrow);
        Ok(id)
    }

    /// Fund the escrow, pulling tokens from the buyer into this contract.
    ///
    /// Ordering: transfer **first**, state write **second** — see the
    /// module docs for why the inverse would be a fund-safety bug.
    pub fn deposit(env: Env, escrow_id: u64) -> Result<(), ForgeError> {
        let escrow = Self::load_escrow(&env, escrow_id)?;
        escrow.buyer.require_auth();

        if escrow.status != EscrowStatus::Pending {
            return Err(ForgeError::InvalidInput);
        }

        // Pull the tokens before writing any state. If the buyer lacks
        // balance or a trustline the invocation reverts here with storage
        // untouched.
        transfer_to_contract(&env, &escrow.token, &escrow.buyer, escrow.amount)?;

        let mut funded = escrow;
        funded.status = EscrowStatus::Funded;
        env.storage()
            .persistent()
            .set(&DataKey::Escrow(escrow_id), &funded);
        bump_entry(&env, &DataKey::Escrow(escrow_id));
        events::deposited(&env, &funded);
        Ok(())
    }

    /// Release funds to the seller. Seller-authorized: delivery
    /// confirmation by the paid party, not the paying one.
    pub fn release(env: Env, escrow_id: u64) -> Result<(), ForgeError> {
        let escrow = Self::load_escrow(&env, escrow_id)?;
        escrow.seller.require_auth();

        if escrow.status != EscrowStatus::Funded {
            return Err(ForgeError::InvalidInput);
        }

        // Pay out before mutating state, mirroring `deposit`.
        transfer_from_contract(&env, &escrow.token, &escrow.seller, escrow.amount)?;

        let mut completed = escrow;
        completed.status = EscrowStatus::Completed;
        env.storage()
            .persistent()
            .set(&DataKey::Escrow(escrow_id), &completed);
        bump_entry(&env, &DataKey::Escrow(escrow_id));
        events::released(&env, &completed);
        Ok(())
    }

    /// Refund the buyer.
    ///
    /// Pre-deadline: seller authorizes (voluntary refund). Post-deadline:
    /// buyer authorizes (reclaim of unfulfilled funds).
    pub fn refund(env: Env, escrow_id: u64) -> Result<(), ForgeError> {
        let escrow = Self::load_escrow(&env, escrow_id)?;
        if escrow.status != EscrowStatus::Funded {
            return Err(ForgeError::InvalidInput);
        }

        let now = env.ledger().timestamp();
        let deadline = escrow
            .created_at
            .checked_add(escrow.timeout)
            .ok_or(ForgeError::ArithmeticOverflow)?;

        if now >= deadline {
            escrow.buyer.require_auth();
        } else {
            escrow.seller.require_auth();
        }

        transfer_from_contract(&env, &escrow.token, &escrow.buyer, escrow.amount)?;

        let mut refunded = escrow;
        refunded.status = EscrowStatus::Refunded;
        env.storage()
            .persistent()
            .set(&DataKey::Escrow(escrow_id), &refunded);
        bump_entry(&env, &DataKey::Escrow(escrow_id));
        events::refunded(&env, &refunded);
        Ok(())
    }

    /// Raise a dispute: the claimant (buyer or seller) authorizes, while
    /// `Funded`. Freezes all payout paths until the arbiter resolves.
    pub fn dispute(env: Env, escrow_id: u64, claimant: Address) -> Result<(), ForgeError> {
        let escrow = Self::load_escrow(&env, escrow_id)?;
        if escrow.status != EscrowStatus::Funded {
            return Err(ForgeError::InvalidInput);
        }
        // The claim must come from a party to the escrow, and the claimant
        // must have actually authorized this invocation. Requiring auth on
        // the claimant (not on buyer-then-seller) is the correct Soroban
        // idiom: the auth envelope is checked against exactly one address.
        if claimant != escrow.buyer && claimant != escrow.seller {
            return Err(ForgeError::InvalidInput);
        }
        claimant.require_auth();

        let mut disputed = escrow;
        disputed.status = EscrowStatus::Disputed;
        env.storage()
            .persistent()
            .set(&DataKey::Escrow(escrow_id), &disputed);
        bump_entry(&env, &DataKey::Escrow(escrow_id));
        events::disputed(&env, &disputed);
        Ok(())
    }

    /// Resolve a dispute: arbiter only, final. Pays the full amount to the
    /// seller (`true`) or refunds the buyer (`false`).
    pub fn resolve(env: Env, escrow_id: u64, in_favor_of_seller: bool) -> Result<(), ForgeError> {
        let escrow = Self::load_escrow(&env, escrow_id)?;
        if escrow.status != EscrowStatus::Disputed {
            return Err(ForgeError::InvalidInput);
        }
        // Arbiter auth is checked before any transfer: an unauthorized
        // resolve must fail without touching the token contract.
        escrow.arbiter.require_auth();

        let mut resolved = escrow;
        if in_favor_of_seller {
            transfer_from_contract(&env, &resolved.token, &resolved.seller, resolved.amount)?;
            resolved.status = EscrowStatus::Completed;
        } else {
            transfer_from_contract(&env, &resolved.token, &resolved.buyer, resolved.amount)?;
            resolved.status = EscrowStatus::Refunded;
        }
        env.storage()
            .persistent()
            .set(&DataKey::Escrow(escrow_id), &resolved);
        bump_entry(&env, &DataKey::Escrow(escrow_id));
        events::resolved(&env, &resolved, in_favor_of_seller);
        Ok(())
    }

    /// Cancel a `Pending` escrow. Requires the buyer. Nothing has moved,
    /// so no token transfer occurs.
    pub fn cancel(env: Env, escrow_id: u64) -> Result<(), ForgeError> {
        let escrow = Self::load_escrow(&env, escrow_id)?;
        if escrow.status != EscrowStatus::Pending {
            return Err(ForgeError::InvalidInput);
        }
        escrow.buyer.require_auth();

        let mut cancelled = escrow;
        cancelled.status = EscrowStatus::Cancelled;
        env.storage()
            .persistent()
            .set(&DataKey::Escrow(escrow_id), &cancelled);
        bump_entry(&env, &DataKey::Escrow(escrow_id));
        events::cancelled(&env, &cancelled);
        Ok(())
    }

    // -------------------------------------------------------------------
    // Views
    // -------------------------------------------------------------------

    /// Read the current lifecycle status.
    pub fn get_status(env: Env, escrow_id: u64) -> Result<EscrowStatus, ForgeError> {
        Ok(Self::load_escrow(&env, escrow_id)?.status)
    }

    /// Read the full escrow record.
    pub fn get_escrow(env: Env, escrow_id: u64) -> Result<EscrowData, ForgeError> {
        Self::load_escrow(&env, escrow_id)
    }

    /// Read the creation-order escrow ids one page at a time for a
    /// participant. Read-only: never mutates storage and never errors — an
    /// unknown address or one with no escrows simply yields an empty page.
    pub fn escrows_for_participant(
        env: Env,
        participant: Address,
        cursor: u32,
        limit: u32,
    ) -> ParticipantEscrowsPage {
        let ids = env
            .storage()
            .persistent()
            .get(&DataKey::ParticipantIndex(participant))
            .unwrap_or_else(|| Vec::new(&env));
        let total = ids.len();
        // `cursor` may exceed `total`; clamp so an over-run returns an
        // empty page rather than panicking on a missing index.
        let (mut at, end) = match limit {
            // A zero limit must not report a next cursor that points at
            // itself forever; treat it as "list not requested".
            0 => (total, total),
            _ => (cursor.min(total), cursor.saturating_add(limit).min(total)),
        };
        let mut page = Vec::new(&env);
        while at < end {
            page.push_back(ids.get_unchecked(at));
            at += 1;
        }
        let next_cursor = if end < total { Some(end) } else { None };
        ParticipantEscrowsPage {
            ids: page,
            total,
            next_cursor,
        }
    }

    /// Permissionless keeper: bump the escrow entry's TTL without changing
    /// any state. The existence check is deliberate — touching a missing
    /// id must fail loudly so a keeper can distinguish "extended" from
    /// "no such escrow".
    pub fn touch_ttl(env: Env, escrow_id: u64) -> Result<(), ForgeError> {
        Self::load_escrow(&env, escrow_id)?;
        bump_entry(&env, &DataKey::Escrow(escrow_id));
        Ok(())
    }

    // -------------------------------------------------------------------
    // Internals
    // -------------------------------------------------------------------

    /// Load an escrow record by id (internal, reference-taking form of
    /// [`Self::get_escrow`] for use inside entrypoints that own `Env`).
    fn load_escrow(env: &Env, escrow_id: u64) -> Result<EscrowData, ForgeError> {
        env.storage()
            .persistent()
            .get(&DataKey::Escrow(escrow_id))
            .ok_or(ForgeError::NotFound)
    }

    /// Allocate the next monotonic escrow id. Instance storage: one small
    /// entry, written once per creation.
    fn next_id(env: &Env) -> Result<u64, ForgeError> {
        let count: u64 = env.storage().instance().get(&DataKey::Count).unwrap_or(0);
        let id = count.checked_add(1).ok_or(ForgeError::ArithmeticOverflow)?;
        env.storage().instance().set(&DataKey::Count, &id);
        Ok(id)
    }

    /// Append `id` to the creation-order index of every distinct
    /// participant. Runs only on the create success path; each write bumps
    /// the entry TTL like any other persistent write.
    fn index_participants(env: &Env, id: u64, participants: &Vec<Address>) {
        for participant in participants.iter() {
            let key = DataKey::ParticipantIndex(participant.clone());
            let mut ids = env
                .storage()
                .persistent()
                .get(&key)
                .unwrap_or_else(|| Vec::new(env));
            ids.push_back(id);
            env.storage().persistent().set(&key, &ids);
            bump_entry(env, &key);
        }
    }
}

/// Move `amount` of `token` from `from` into this contract.
///
/// The buyer's `require_auth` on the calling entrypoint covers the nested
/// token authorization; no separate allowance is needed for a `transfer`
/// pull when the holder authorizes the invocation.
///
/// Token failures are bucketed into [`ForgeError::TokenTransferFailed`]
/// rather than forwarded: a client receiving `Error(Contract, #N)` cannot
/// know whether `N` came from the token or the escrow, and forwarding the
/// raw discriminant invites silent misinterpretation. The root cause
/// remains visible in the transaction's diagnostic events.
fn transfer_to_contract(
    env: &Env,
    token: &Address,
    from: &Address,
    amount: i128,
) -> Result<(), ForgeError> {
    match token::TokenClient::new(env, token).try_transfer(
        from,
        env.current_contract_address(),
        &amount,
    ) {
        Ok(Ok(())) => Ok(()),
        // Token returned a typed error (insufficient balance, missing
        // trustline, custom token logic) or the host aborted (most
        // commonly an undeployed token address). The raw discriminant is
        // intentionally discarded — see the bucketing note above.
        _ => Err(ForgeError::TokenTransferFailed),
    }
}

/// Move `amount` of `token` from this contract to `to`.
fn transfer_from_contract(
    env: &Env,
    token: &Address,
    to: &Address,
    amount: i128,
) -> Result<(), ForgeError> {
    match token::TokenClient::new(env, token).try_transfer(
        &env.current_contract_address(),
        to,
        &amount,
    ) {
        Ok(Ok(())) => Ok(()),
        _ => Err(ForgeError::TokenTransferFailed),
    }
}

/// Bump a persistent entry's TTL to the [`ttl::BUMP_AMOUNT`] horizon when
/// it falls inside [`ttl::BUMP_THRESHOLD`]. The standard threshold/extend
/// pattern: cheap no-op while the entry is fresh, decisive near expiry.
fn bump_entry(env: &Env, key: &DataKey) {
    env.storage()
        .persistent()
        .extend_ttl(key, ttl::BUMP_THRESHOLD, ttl::BUMP_AMOUNT);
}

/// Lifecycle events. The escrow id is a **topic** so indexers can filter
/// by escrow cheaply; the data payload carries the full record so no read
/// call is needed to reconstruct state.
mod events {
    use super::*;

    #[contractevent]
    pub struct EscrowCreated {
        #[topic]
        pub escrow_id: u64,
        pub data: EscrowData,
    }

    #[contractevent]
    pub struct Deposited {
        #[topic]
        pub escrow_id: u64,
        pub data: EscrowData,
    }

    #[contractevent]
    pub struct Released {
        #[topic]
        pub escrow_id: u64,
        pub data: EscrowData,
    }

    #[contractevent]
    pub struct Refunded {
        #[topic]
        pub escrow_id: u64,
        pub data: EscrowData,
    }

    #[contractevent]
    pub struct Disputed {
        #[topic]
        pub escrow_id: u64,
        pub data: EscrowData,
    }

    #[contractevent]
    pub struct Resolved {
        #[topic]
        pub escrow_id: u64,
        pub data: EscrowData,
        pub in_favor_of_seller: bool,
    }

    #[contractevent]
    pub struct Cancelled {
        #[topic]
        pub escrow_id: u64,
        pub data: EscrowData,
    }

    // Publishers: thin functions so call sites read as intent, not
    // mechanics, and so a future payload change touches one module.
    pub fn escrow_created(env: &Env, escrow: &EscrowData) {
        EscrowCreated {
            escrow_id: escrow.escrow_id,
            data: escrow.clone(),
        }
        .publish(env);
    }

    pub fn deposited(env: &Env, escrow: &EscrowData) {
        Deposited {
            escrow_id: escrow.escrow_id,
            data: escrow.clone(),
        }
        .publish(env);
    }

    pub fn released(env: &Env, escrow: &EscrowData) {
        Released {
            escrow_id: escrow.escrow_id,
            data: escrow.clone(),
        }
        .publish(env);
    }

    pub fn refunded(env: &Env, escrow: &EscrowData) {
        Refunded {
            escrow_id: escrow.escrow_id,
            data: escrow.clone(),
        }
        .publish(env);
    }

    pub fn disputed(env: &Env, escrow: &EscrowData) {
        Disputed {
            escrow_id: escrow.escrow_id,
            data: escrow.clone(),
        }
        .publish(env);
    }

    pub fn resolved(env: &Env, escrow: &EscrowData, in_favor_of_seller: bool) {
        Resolved {
            escrow_id: escrow.escrow_id,
            data: escrow.clone(),
            in_favor_of_seller,
        }
        .publish(env);
    }

    pub fn cancelled(env: &Env, escrow: &EscrowData) {
        Cancelled {
            escrow_id: escrow.escrow_id,
            data: escrow.clone(),
        }
        .publish(env);
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod authz;

#[cfg(test)]
mod props;
