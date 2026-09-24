#![no_std]

//! # Soroban Forge — Multi-signature Wallet contract
//!
//! A wallet that requires `threshold` approvals from a set of owners before a
//! transaction executes. The wallet is configured once via
//! [`SorobanForgeMultiSigWallet::initialize`]; transactions submitted by an
//! owner then collect confirmations from other owners until the threshold is
//! met, at which point [`SorobanForgeMultiSigWallet::execute`] completes them.
//!
//! Lifecycle:
//!
//! ```text
//! submit --(confirm × n)--> threshold met --execute--> Executed
//! submit --(reject × n)--> rejection threshold met --> Rejected (terminal)
//! submit_withdrawal --(confirm × n)--> threshold met --execute--> Executed (+ tokens moved)
//! submit_withdrawal --(reject × n)--> rejection threshold met --> Rejected (terminal)
//! ```
//!
//! Authorization model:
//! - `initialize` sets the owner set and threshold once (guarded against
//!   re-initialisation).
//! - `submit` requires an owner and records a `target` contract address
//!   and an opaque `payload` for cross-contract invocation.
//! - `confirm` requires an owner that has not confirmed already.
//! - `reject` requires an owner that has not already signalled on the tx.
//! - `execute` may be called by anyone; it only succeeds once the threshold is
//!   met. For opaque-payload txs it performs a real cross-contract
//!   invocation to the recorded `target`. For typed withdrawal txs it moves
//!   real tokens.
//! - A target revert surfaces as [`ForgeError::ContractInvocationFailed`]
//!   and leaves the transaction un-executed (status stays `Pending`).
//!
//! ## Rejection policy
//!
//! An owner who spots a malicious or mistaken pending transaction records a
//! formal objection via `reject`. Rejections are stored on the tx record
//! (`WalletTx::rejections`) alongside confirmations. The policy:
//!
//! - **A signed owner may signal once, in one direction.** An owner who
//!   confirmed cannot also reject the same tx, and an owner who rejected
//!   cannot confirm it (`InvalidInput`). A combined signal would be
//!   ambiguous.
//! - **Existing confirmations do not block rejection.** The whole point of
//!   the entrypoint is to stop a tx that already reached (or is approaching)
//!   the threshold, so an owner can reject a tx others have confirmed.
//! - **Any rejection blocks execution.** `execute` refuses a tx that carries
//!   even one rejection, below or at the threshold — a single formal
//!   objection stalls the tx. That is load-bearing: without it, rejections
//!   below the threshold would be a no-op and a threshold-met malicious tx
//!   would still execute.
//! - **Reaching the threshold is terminal.** Once `rejections.len() >=
//!   threshold` the status flips to `Rejected`: no further confirms,
//!   executes, or rejects are accepted.
//! - **Executed txs can never be rejected** (there is nothing to stop), and
//!   rejections are never revoked (un-confirm is out of scope).
//!
//! ## Custody model
//!
//! The wallet custodies SEP-41 token balances. Anyone (an owner or a
//! third party) can `deposit` into the wallet by pull transfer; only a
//! threshold-approved withdrawal can move funds out. Open deposits are a
//! deliberate choice: a wallet that can only receive from its owners is a
//! strict subset of one that accepts direct funding, and custody risk is
//! unchanged because every exit is multisig-gated. The wallet must be
//! initialized before deposits are accepted — funding an uninitialized
//! wallet would strand the tokens behind a contract with no owners and no
//! exit.
//!
//! A withdrawal is a transaction like any other: `submit_withdrawal` records
//! a typed [`Withdrawal`] (token, destination, amount) as a `Pending` tx that
//! collects confirmations; `execute` moves the tokens to the destination and
//! flips the status only past the threshold. Withdrawal validity is checked
//! at execution time, not submission time — the balance can change between
//! the two, so a submitted withdrawal is an intent, not a reservation.
//!
//! Two transaction kinds coexist by design:
//! - **Opaque-payload txs** (`submit`) keep the `payload` `Bytes` untouched
//!   for arbitrary transaction bodies; dispatching those bytes is out of
//!   scope here.
//! - **Typed withdrawal txs** (`submit_withdrawal`) carry an empty `payload`
//!   and a [`TxKind::Withdrawal`] record instead. Encoding withdrawals into
//!   the opaque payload was rejected because the contract would need a
//!   payload codec to validate and execute them natively; a typed record
//!   lets the contract validate amount/destination semantics directly and
//!   keeps the `payload` free for genuinely arbitrary txs. The tradeoff is
//!   that withdrawals are a first-class tx kind rather than uninterpreted
//!   bytes.
//!
//! ## Ordering discipline (load-bearing)
//!
//! Every method that moves tokens performs the token transfer **first** and
//! writes state **after** the transfer succeeds. A failed transfer reverts
//! the whole invocation with balances and tx state untouched — there is no
//! state/ledger divergence window and no recovery path needed. The inverse
//! ordering (state first, transfer second) would strand funds behind a
//! failed transfer and is the classic custody bug.
//!
//! ## Token trust model
//!
//! `deposit` accepts a user-specified token address. The contract does not
//! verify that the address is a deployed token contract or enforce SEP-41
//! compliance; it assumes the supplied token follows the expected SEP-41
//! interface and behavior. Token transfer failures use the existing
//! `TokenTransferFailed` error path, so a malicious or non-compliant token
//! is an external trust assumption rather than something the wallet
//! validates.
//!
//! ## Storage and TTL
//!
//! Token balances live in **per-token persistent entries**
//! (`DataKey::Balance(token)`), not instance storage, mirroring the escrow
//! crate's rationale: persistent entries scale the byte budget per record
//! instead of taxing every instance read (the owner set, threshold, and tx
//! records) with custody data that grows with the number of custodied
//! tokens, and persistent entries carry their own extensible TTL so a
//! long-idle balance does not silently expire the way archived instance
//! storage would force a full-state restore. Every balance write bumps the
//! entry's TTL with the standard threshold/extend pattern, and `touch_ttl`
//! is a permissionless keeper entrypoint for balances that sit idle near
//! expiry. The `Owners`/`Threshold`/`Count`/`Tx` keys stay in instance
//! storage: they are small, hot, written by the existing entrypoints, and
//! their semantics are unchanged by this custody layer.

// WASM target guard: SDK 27 contracts must be built for wasm32v1-none.
// wasm32-unknown-unknown (os=unknown) can emit features the Soroban
// runtime rejects; wasm32v1-none (os=none) is the supported target.
#[cfg(all(target_family = "wasm", not(target_os = "none")))]
compile_error!(
    "build for wasm32v1-none (see rust-toolchain.toml); wasm32-unknown-unknown is not supported by the Soroban runtime"
);

#[cfg(test)]
extern crate std;

use soroban_forge_shared_utils::ForgeError;
use soroban_sdk::{
    contract, contractclient, contractevent, contractimpl, contracttype, token, Address, Bytes,
    Env, IntoVal, Symbol, Val, Vec,
};

/// Ledger-time constants for TTL bumps.
///
/// One ledger closes roughly every 5 seconds, so 17,280 ledgers ≈ 1 day.
/// `BUMP_AMOUNT` is the lifetime written on every touch; `BUMP_THRESHOLD`
/// is how close to expiry an entry must be before a bump applies. The
/// 30-day horizon comfortably covers an idle custody balance between keeper
/// touches.
mod ttl {
    pub const DAY_IN_LEDGERS: u32 = 17_280;
    /// Lifetime applied on every TTL touch.
    pub const BUMP_AMOUNT: u32 = 30 * DAY_IN_LEDGERS;
    /// Bump only when the entry is within this window of expiring.
    pub const BUMP_THRESHOLD: u32 = BUMP_AMOUNT - DAY_IN_LEDGERS;
}

/// Public interface for the Soroban Forge multi-signature wallet contract.
#[contractclient(name = "SorobanForgeMultiSigWalletClient")]
pub trait SorobanForgeMultiSigWallet {
    /// Configure the wallet with the given set of `owners` and an approval
    /// `threshold`. May only be called once.
    fn initialize(
        env: Env,
        owners: Vec<Address>,
        threshold: u32,
    ) -> Result<(), soroban_forge_shared_utils::ForgeError>;

    /// Add a pending transaction submitted by `submitter` and open it for
    /// owner approvals. Returns the stable transaction id.
    ///
    /// `target` is the contract address to invoke on execution;
    /// `tx` is the opaque payload passed to the target.
    fn submit(
        env: Env,
        submitter: Address,
        target: Address,
        tx: Bytes,
    ) -> Result<u64, soroban_forge_shared_utils::ForgeError>;

    /// Record `signer`'s approval of `tx_id`.
    fn confirm(
        env: Env,
        tx_id: u64,
        signer: Address,
    ) -> Result<(), soroban_forge_shared_utils::ForgeError>;

    /// Record `signer`'s formal objection to `tx_id`.
    ///
    /// Confirmations already on the tx do not block an objection. Each owner
    /// may signal once, in one direction (confirm **or** reject), and only
    /// while the tx is `Pending`. Once rejections reach the configured
    /// threshold the tx becomes `Rejected` and cannot be confirmed, executed,
    /// or rejected further. A tx carrying any rejection can never execute,
    /// even below the threshold (see the module docs).
    fn reject(
        env: Env,
        tx_id: u64,
        signer: Address,
    ) -> Result<(), soroban_forge_shared_utils::ForgeError>;

    /// Execute `tx_id` once approvals meet the configured threshold.
    ///
    /// For opaque-payload txs (see [`TxKind::Opaque`]), this performs
    /// a real cross-contract invocation to the recorded `target`
    /// contract with the stored `payload`, gated on the owner threshold.
    /// For typed withdrawal txs (see [`TxKind::Withdrawal`]), the
    /// recorded tokens are moved to the recorded destination **before**
    /// the status flips to `Executed` (transfer first, state second —
    /// see the module docs).
    ///
    /// Failure ordering: a target revert surfaces as
    /// [`ForgeError::ContractInvocationFailed`] and leaves the
    /// transaction un-executed (status stays `Pending`).
    fn execute(env: Env, tx_id: u64) -> Result<(), soroban_forge_shared_utils::ForgeError>;

    /// Deposit `amount` of `token` into the wallet's custody, pulling the
    /// tokens from `from` with their authorization.
    ///
    /// Open to owners and third parties alike (see the custody model in the
    /// module docs); the wallet must already be initialized so deposited
    /// funds always have a multisig-gated exit.
    ///
    /// # Errors
    ///
    /// * [`ForgeError::NotInitialized`] — the wallet has no owner set.
    /// * [`ForgeError::InvalidInput`] — non-positive amount.
    /// * [`ForgeError::TokenTransferFailed`] — the token contract rejected
    ///   the transfer (insufficient balance, missing trustline, deauthorized
    ///   token, or undeployed token contract).
    fn deposit(
        env: Env,
        token: Address,
        from: Address,
        amount: i128,
    ) -> Result<(), soroban_forge_shared_utils::ForgeError>;

    /// Submit a token withdrawal as a pending transaction like any other:
    /// it collects owner confirmations and executes only past the threshold.
    /// Returns the stable transaction id.
    ///
    /// The withdrawal is recorded as a typed [`TxKind::Withdrawal`] on the
    /// tx; the `payload` stays empty. Funding is validated at execution
    /// time, not submission time (see the custody model in the module docs).
    ///
    /// # Errors
    ///
    /// * [`ForgeError::NotInitialized`] — the wallet has no owner set.
    /// * [`ForgeError::Unauthorized`] — `submitter` is not an owner.
    /// * [`ForgeError::InvalidInput`] — non-positive amount.
    fn submit_withdrawal(
        env: Env,
        submitter: Address,
        token: Address,
        destination: Address,
        amount: i128,
    ) -> Result<u64, soroban_forge_shared_utils::ForgeError>;

    /// Read the wallet's custody balance of `token` (read-only view).
    ///
    /// Returns `0` for a token that has never been deposited.
    fn balance(env: Env, token: Address) -> i128;

    /// Permissionless TTL keeper: bumps the balance entry's TTL for `token`
    /// to the standard horizon without changing any state.
    ///
    /// # Errors
    ///
    /// * [`ForgeError::NotFound`] — the wallet holds no balance entry for
    ///   `token`.
    fn touch_ttl(env: Env, token: Address) -> Result<(), soroban_forge_shared_utils::ForgeError>;

    /// Read the current approval threshold (read-only view).
    fn get_threshold(env: Env) -> Result<u32, soroban_forge_shared_utils::ForgeError>;

    /// Read a stored transaction by id (read-only view).
    fn get_tx(env: Env, tx_id: u64) -> Result<WalletTx, soroban_forge_shared_utils::ForgeError>;

    /// Read the configured owner set, in initialization order (read-only view).
    ///
    /// # Errors
    ///
    /// * [`ForgeError::NotInitialized`] — the wallet has no owner set.
    fn get_owners(env: Env) -> Result<Vec<Address>, soroban_forge_shared_utils::ForgeError>;

    /// Check whether `address` is a member of the owner set (read-only view).
    ///
    /// Uninitialized wallets read as `false`.
    fn is_owner(env: Env, address: Address) -> bool;

    /// Read the confirmation list recorded for `tx_id`, in the order the
    /// confirmations were recorded (read-only view).
    ///
    /// # Errors
    ///
    /// * [`ForgeError::NotFound`] — no transaction with id `tx_id`.
    fn get_confirmations(
        env: Env,
        tx_id: u64,
    ) -> Result<Vec<Address>, soroban_forge_shared_utils::ForgeError>;

    /// Read the rejection list recorded for `tx_id`, in the order the
    /// rejections were recorded (read-only view).
    ///
    /// # Errors
    ///
    /// * [`ForgeError::NotFound`] — no transaction with id `tx_id`.
    fn get_rejections(
        env: Env,
        tx_id: u64,
    ) -> Result<Vec<Address>, soroban_forge_shared_utils::ForgeError>;

    /// Read the number of transactions submitted so far (read-only view).
    fn get_tx_count(env: Env) -> u64;
}

/// Lifecycle state of a submitted transaction.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TxStatus {
    /// Open for confirmations; threshold not yet met.
    Pending,
    /// Threshold met and executed successfully.
    Executed,
    /// Rejected by owners (reached the rejection threshold); terminal.
    Rejected,
}

/// What a submitted transaction carries.
///
/// The two kinds encode the custody split documented in the module docs:
/// opaque-payload transactions dispatch (out of scope here) through their
/// `payload` bytes, while withdrawal transactions move real tokens through
/// the typed record below.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TxKind {
    /// Opaque-payload transaction: `WalletTx::payload` carries the intent.
    Opaque,
    /// Typed token withdrawal.
    Withdrawal(Withdrawal),
}

/// A typed token withdrawal record (see [`TxKind::Withdrawal`] and the
/// custody model in the module docs for why this is a record rather than
/// payload bytes).
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Withdrawal {
    /// SEP-41 token to move out of custody.
    pub token: Address,
    /// Recipient of the tokens.
    pub destination: Address,
    /// Amount to move; must be positive.
    pub amount: i128,
}

/// A transaction awaiting multi-signature approval.
#[contracttype]
#[derive(Clone, Debug)]
pub struct WalletTx {
    /// Stable identifier assigned at submission time.
    pub tx_id: u64,
    /// Address that submitted the transaction.
    pub submitter: Address,
    /// The target contract to invoke on execution.
    pub target: Address,
    /// The encoded transaction payload to pass to the target.
    pub payload: Bytes,
    /// Owners that have confirmed so far.
    pub confirmations: soroban_sdk::Vec<Address>,
    /// Owners that have formally objected so far.
    pub rejections: soroban_sdk::Vec<Address>,
    /// Current state.
    pub status: TxStatus,
    /// What the transaction carries: an opaque payload or a typed token
    /// withdrawal (see [`TxKind`]).
    pub kind: TxKind,
}

/// Storage keys, split by class (see the storage-and-TTL notes in the
/// module docs). Config and tx records stay in **instance** storage: they
/// are small, hot, and their semantics predate the custody layer. Token
/// balances live in **per-token persistent** entries so the byte budget
/// scales with the number of custodied tokens instead of inflating the
/// shared instance entry, and so each balance carries its own extensible
/// TTL — instance storage is the wrong home for anything the wallet
/// custodies long-term.
#[contracttype]
enum DataKey {
    // --- instance storage: small, hot, bounded config/state ---
    /// The transaction record for `u64` id.
    Tx(u64),
    /// The wallet's owner set.
    Owners,
    /// The approval threshold required to execute.
    Threshold,
    /// Monotonic transaction id counter.
    Count,
    // --- persistent storage: per-token custody accounting ---
    /// The wallet's custody balance of the token at `Address`.
    Balance(Address),
}

/// The deployable multi-signature wallet contract.
#[contract]
pub struct MultiSigWallet;

#[contractimpl]
impl MultiSigWallet {
    /// Configure the wallet for the first and only time.
    ///
    /// Requires a non-empty owner set with no duplicates and
    /// `0 < threshold <= owners.len()`. The caller deploys the wallet and
    /// initialises it in the same transaction.
    pub fn initialize(env: Env, owners: Vec<Address>, threshold: u32) -> Result<(), ForgeError> {
        if env.storage().instance().has(&DataKey::Threshold) {
            return Err(ForgeError::AlreadyInitialized);
        }
        if owners.is_empty() {
            return Err(ForgeError::InvalidInput);
        }
        if threshold == 0 || threshold > owners.len() {
            return Err(ForgeError::InvalidInput);
        }
        // Reject duplicate owners so a single owner can never inflate their
        // personal approval count.
        for i in 0..owners.len() {
            let owner = owners.get_unchecked(i);
            for j in 0..i {
                if owners.get_unchecked(j) == owner {
                    return Err(ForgeError::InvalidInput);
                }
            }
        }

        env.storage().instance().set(&DataKey::Owners, &owners);
        env.storage()
            .instance()
            .set(&DataKey::Threshold, &threshold);
        Ok(())
    }

    /// Submit a new transaction for owner approval.
    ///
    /// Requires the submitter to be an owner. Returns the stable `tx_id` that
    /// confirmations reference.
    pub fn submit(
        env: Env,
        submitter: Address,
        target: Address,
        tx: Bytes,
    ) -> Result<u64, ForgeError> {
        if !Self::is_initialized(&env) {
            return Err(ForgeError::NotInitialized);
        }
        if !Self::is_owner_impl(&env, &submitter) {
            return Err(ForgeError::Unauthorized);
        }
        submitter.require_auth();

        let tx_id = Self::next_id(&env)?;
        let wallet_tx = WalletTx {
            tx_id,
            submitter,
            target,
            payload: tx,
            confirmations: Vec::new(&env),
            rejections: Vec::new(&env),
            status: TxStatus::Pending,
            kind: TxKind::Opaque,
        };
        env.storage()
            .instance()
            .set(&DataKey::Tx(tx_id), &wallet_tx);
        events::submitted(&env, &wallet_tx);
        Ok(tx_id)
    }

    /// Record an owner's approval of a pending transaction.
    ///
    /// An owner may confirm only once, and only while the transaction is
    /// `Pending`. An owner who has already rejected the transaction may not
    /// also confirm it (one signal per owner, in one direction — see the
    /// module docs for the rejection policy).
    pub fn confirm(env: Env, tx_id: u64, signer: Address) -> Result<(), ForgeError> {
        let mut wallet_tx = Self::get_tx_impl(&env, tx_id)?;
        if wallet_tx.status != TxStatus::Pending {
            return Err(ForgeError::InvalidInput);
        }
        if !Self::is_owner_impl(&env, &signer) {
            return Err(ForgeError::Unauthorized);
        }
        signer.require_auth();

        if wallet_tx.confirmations.contains(&signer) {
            return Err(ForgeError::InvalidInput);
        }
        if wallet_tx.rejections.contains(&signer) {
            return Err(ForgeError::InvalidInput);
        }
        wallet_tx.confirmations.push_back(signer);
        env.storage()
            .instance()
            .set(&DataKey::Tx(tx_id), &wallet_tx);
        events::confirmed(&env, &wallet_tx);
        Ok(())
    }

    /// Record an owner's formal objection to a pending transaction.
    ///
    /// An owner may reject only while the transaction is `Pending`, may not
    /// reject twice, and may not reject a transaction they have confirmed
    /// (one signal per owner, in one direction). Existing confirmations from
    /// other owners do not block a rejection, and a rejection can never be
    /// revoked.
    ///
    /// Once `rejections.len() >= threshold` the status flips to `Rejected`,
    /// which is terminal: no further confirms, executes, or rejects. Below
    /// the threshold the tx stays `Pending` but is already blocked from
    /// executing (see the module docs).
    pub fn reject(env: Env, tx_id: u64, signer: Address) -> Result<(), ForgeError> {
        let mut wallet_tx = Self::get_tx_impl(&env, tx_id)?;
        if wallet_tx.status != TxStatus::Pending {
            return Err(ForgeError::InvalidInput);
        }
        if !Self::is_owner_impl(&env, &signer) {
            return Err(ForgeError::Unauthorized);
        }
        signer.require_auth();

        if wallet_tx.rejections.contains(&signer) || wallet_tx.confirmations.contains(&signer) {
            return Err(ForgeError::InvalidInput);
        }
        wallet_tx.rejections.push_back(signer);
        let threshold: u32 = env
            .storage()
            .instance()
            .get(&DataKey::Threshold)
            .ok_or(ForgeError::NotInitialized)?;
        if wallet_tx.rejections.len() >= threshold {
            wallet_tx.status = TxStatus::Rejected;
        }
        env.storage()
            .instance()
            .set(&DataKey::Tx(tx_id), &wallet_tx);
        Ok(())
    }

    /// Execute a transaction once the owner approvals meet the threshold.
    ///
    /// Callable by anyone once the threshold is met; otherwise the state
    /// transition is rejected.
    ///
    /// For typed withdrawal txs this moves the recorded tokens to the
    /// recorded destination before the status flips (transfer first, state
    /// second — see the module docs). The wallet balance is validated
    /// against the recorded amount first (`InsufficientFunds`), so a
    /// withdrawal that exceeds custody fails with balances and tx state
    /// untouched.
    ///
    /// For opaque-payload txs this performs a real cross-contract
    /// invocation to the recorded `target` with the stored `payload`.
    /// The invocation is attempted **before** the status flips. A
    /// target revert surfaces as [`ForgeError::ContractInvocationFailed`]
    /// and leaves the transaction un-executed (status stays `Pending`).
    ///
    /// Execution is refused while the tx carries **any** rejection, even
    /// below the rejection threshold (see the module docs).
    pub fn execute(env: Env, tx_id: u64) -> Result<(), ForgeError> {
        let mut wallet_tx = Self::get_tx_impl(&env, tx_id)?;
        if wallet_tx.status != TxStatus::Pending {
            return Err(ForgeError::InvalidInput);
        }
        let threshold: u32 = env
            .storage()
            .instance()
            .get(&DataKey::Threshold)
            .ok_or(ForgeError::NotInitialized)?;
        if wallet_tx.confirmations.len() < threshold {
            return Err(ForgeError::InvalidInput);
        }
        // A single formal objection stalls the tx. Checked before any token
        // transfer or cross-contract invocation so a sub-threshold rejection
        // can never be bypassed by executing.
        if !wallet_tx.rejections.is_empty() {
            return Err(ForgeError::InvalidInput);
        }

        // Withdrawal txs move real tokens before the status flip. Any
        // failure below reverts the whole invocation: balances and tx state
        // stay exactly as they were.
        if let TxKind::Withdrawal(withdrawal) = &wallet_tx.kind {
            let balance = Self::balance_impl(&env, &withdrawal.token);
            if balance < withdrawal.amount {
                return Err(ForgeError::InsufficientFunds);
            }
            transfer_from_contract(
                &env,
                &withdrawal.token,
                &withdrawal.destination,
                withdrawal.amount,
            )?;
            Self::sub_balance(&env, &withdrawal.token, withdrawal.amount)?;
        } else {
            // Opaque-payload txs perform a real cross-contract
            // invocation. Invoke first, write Executed only on success.
            let target = &wallet_tx.target;
            let payload_val: Val = wallet_tx.payload.clone().into_val(&env);
            let args = soroban_sdk::vec![&env, payload_val];
            let result = env.try_invoke_contract::<(), ForgeError>(
                target,
                &Symbol::new(&env, "execute"),
                args,
            );
            if let Err(_) | Ok(Err(_)) = result {
                return Err(ForgeError::ContractInvocationFailed);
            }
        }

        wallet_tx.status = TxStatus::Executed;
        env.storage()
            .instance()
            .set(&DataKey::Tx(tx_id), &wallet_tx);
        events::executed(&env, &wallet_tx);
        Ok(())
    }

    /// Deposit `amount` of `token` into custody, pulling from `from`.
    ///
    /// Ordering: transfer **first**, balance write **second** — see the
    /// module docs for why the inverse would be a fund-safety bug.
    pub fn deposit(
        env: Env,
        token: Address,
        from: Address,
        amount: i128,
    ) -> Result<(), ForgeError> {
        if !Self::is_initialized(&env) {
            return Err(ForgeError::NotInitialized);
        }
        if amount <= 0 {
            return Err(ForgeError::InvalidInput);
        }
        from.require_auth();

        // Pull the tokens before writing any state. If `from` lacks balance
        // or a trustline the invocation reverts here with storage untouched.
        transfer_to_contract(&env, &token, &from, amount)?;

        Self::add_balance(&env, &token, amount)?;
        Ok(())
    }

    /// Submit a token withdrawal as a pending tx (see the trait docs).
    pub fn submit_withdrawal(
        env: Env,
        submitter: Address,
        token: Address,
        destination: Address,
        amount: i128,
    ) -> Result<u64, ForgeError> {
        if !Self::is_initialized(&env) {
            return Err(ForgeError::NotInitialized);
        }
        if !Self::is_owner_impl(&env, &submitter) {
            return Err(ForgeError::Unauthorized);
        }
        if amount <= 0 {
            return Err(ForgeError::InvalidInput);
        }
        submitter.require_auth();

        let tx_id = Self::next_id(&env)?;
        let wallet_tx = WalletTx {
            tx_id,
            submitter,
            target: env.current_contract_address(),
            payload: Bytes::new(&env),
            confirmations: Vec::new(&env),
            rejections: Vec::new(&env),
            status: TxStatus::Pending,
            kind: TxKind::Withdrawal(Withdrawal {
                token,
                destination,
                amount,
            }),
        };
        env.storage()
            .instance()
            .set(&DataKey::Tx(tx_id), &wallet_tx);
        Ok(tx_id)
    }

    /// Read the wallet's custody balance of `token` (read-only view).
    ///
    /// Unknown tokens read as zero so the view has no error path.
    pub fn balance(env: Env, token: Address) -> i128 {
        Self::balance_impl(&env, &token)
    }

    /// Permissionless keeper: bump the balance entry's TTL without changing
    /// any state (see the trait docs).
    pub fn touch_ttl(env: Env, token: Address) -> Result<(), ForgeError> {
        let key = DataKey::Balance(token);
        if !env.storage().persistent().has(&key) {
            return Err(ForgeError::NotFound);
        }
        bump_entry(&env, &key);
        Ok(())
    }

    /// Read the configured approval threshold (read-only view).
    pub fn get_threshold(env: Env) -> Result<u32, ForgeError> {
        env.storage()
            .instance()
            .get(&DataKey::Threshold)
            .ok_or(ForgeError::NotInitialized)
    }

    /// Read a stored transaction by id (read-only view).
    pub fn get_tx(env: Env, tx_id: u64) -> Result<WalletTx, ForgeError> {
        Self::get_tx_impl(&env, tx_id)
    }

    /// Read the configured owner set, in initialization order (read-only view).
    ///
    /// # Errors
    ///
    /// * [`ForgeError::NotInitialized`] — the wallet has no owner set.
    pub fn get_owners(env: Env) -> Result<Vec<Address>, ForgeError> {
        env.storage()
            .instance()
            .get(&DataKey::Owners)
            .ok_or(ForgeError::NotInitialized)
    }

    /// Check whether `address` is a member of the owner set (read-only view).
    ///
    /// Uninitialized wallets read as `false`.
    pub fn is_owner(env: Env, address: Address) -> bool {
        Self::is_owner_impl(&env, &address)
    }

    /// Read the confirmation list recorded for `tx_id`, in the order the
    /// confirmations were recorded (read-only twin of
    /// `WalletTx::confirmations`).
    ///
    /// # Errors
    ///
    /// * [`ForgeError::NotFound`] — no transaction with id `tx_id`.
    pub fn get_confirmations(env: Env, tx_id: u64) -> Result<Vec<Address>, ForgeError> {
        let wallet_tx = Self::get_tx_impl(&env, tx_id)?;
        Ok(wallet_tx.confirmations)
    }

    /// Read the rejection list recorded for `tx_id`, in the order the
    /// rejections were recorded (read-only twin of `WalletTx::rejections`).
    ///
    /// # Errors
    ///
    /// * [`ForgeError::NotFound`] — no transaction with id `tx_id`.
    pub fn get_rejections(env: Env, tx_id: u64) -> Result<Vec<Address>, ForgeError> {
        let wallet_tx = Self::get_tx_impl(&env, tx_id)?;
        Ok(wallet_tx.rejections)
    }

    /// Read the number of transactions submitted so far (read-only view).
    ///
    /// The `Count` counter only advances on successful `submit`/
    /// `submit_withdrawal`, so this matches the number of recorded
    /// transactions. Uninitialized wallets read as `0`.
    pub fn get_tx_count(env: Env) -> u64 {
        env.storage().instance().get(&DataKey::Count).unwrap_or(0)
    }

    /// Credit the custody balance of `token` by `amount`, overflow-safe.
    fn add_balance(env: &Env, token: &Address, amount: i128) -> Result<(), ForgeError> {
        let current: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::Balance(token.clone()))
            .unwrap_or(0);
        let updated = current
            .checked_add(amount)
            .ok_or(ForgeError::ArithmeticOverflow)?;
        env.storage()
            .persistent()
            .set(&DataKey::Balance(token.clone()), &updated);
        bump_entry(env, &DataKey::Balance(token.clone()));
        Ok(())
    }

    /// Debit the custody balance of `token` by `amount`, overflow-safe.
    ///
    /// Callers validate funding before transferring, so the checked
    /// subtraction is an invariant backstop rather than the primary guard.
    fn sub_balance(env: &Env, token: &Address, amount: i128) -> Result<(), ForgeError> {
        let current: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::Balance(token.clone()))
            .unwrap_or(0);
        let updated = current
            .checked_sub(amount)
            .ok_or(ForgeError::ArithmeticOverflow)?;
        env.storage()
            .persistent()
            .set(&DataKey::Balance(token.clone()), &updated);
        bump_entry(env, &DataKey::Balance(token.clone()));
        Ok(())
    }

    /// Read the custody balance of `token`; unknown tokens read as zero.
    fn balance_impl(env: &Env, token: &Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::Balance(token.clone()))
            .unwrap_or(0)
    }

    /// Allocate the next monotonic transaction id.
    fn next_id(env: &Env) -> Result<u64, ForgeError> {
        let count: u64 = env.storage().instance().get(&DataKey::Count).unwrap_or(0);
        let id = count.checked_add(1).ok_or(ForgeError::ArithmeticOverflow)?;
        env.storage().instance().set(&DataKey::Count, &id);
        Ok(id)
    }

    fn is_initialized(env: &Env) -> bool {
        env.storage().instance().has(&DataKey::Threshold)
    }

    fn is_owner_impl(env: &Env, address: &Address) -> bool {
        let owners: Vec<Address> = match env.storage().instance().get(&DataKey::Owners) {
            Some(owners) => owners,
            None => return false,
        };
        owners.contains(address)
    }

    fn get_tx_impl(env: &Env, tx_id: u64) -> Result<WalletTx, ForgeError> {
        env.storage()
            .instance()
            .get(&DataKey::Tx(tx_id))
            .ok_or(ForgeError::NotFound)
    }
}

/// Move `amount` of `token` from `from` into this contract.
///
/// The depositor's `require_auth` on the calling entrypoint covers the
/// nested token authorization; no separate allowance is needed for a
/// `transfer` pull when the holder authorizes the invocation.
///
/// Token failures are bucketed into [`ForgeError::TokenTransferFailed`]
/// rather than forwarded: a client receiving `Error(Contract, #N)` cannot
/// know whether `N` came from the token or the wallet, and forwarding the
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

/// Lifecycle events. The tx id is a **topic** so indexers can filter
/// by tx cheaply; the data payload carries the full record so no read
/// call is needed to reconstruct state.
mod events {
    use super::*;

    #[contractevent]
    pub struct Submitted {
        #[topic]
        pub tx_id: u64,
        pub data: WalletTx,
    }

    #[contractevent]
    pub struct Confirmed {
        #[topic]
        pub tx_id: u64,
        pub data: WalletTx,
    }

    #[contractevent]
    pub struct Executed {
        #[topic]
        pub tx_id: u64,
        pub data: WalletTx,
    }

    pub fn submitted(env: &Env, tx: &WalletTx) {
        Submitted {
            tx_id: tx.tx_id,
            data: tx.clone(),
        }
        .publish(env);
    }

    pub fn confirmed(env: &Env, tx: &WalletTx) {
        Confirmed {
            tx_id: tx.tx_id,
            data: tx.clone(),
        }
        .publish(env);
    }

    pub fn executed(env: &Env, tx: &WalletTx) {
        Executed {
            tx_id: tx.tx_id,
            data: tx.clone(),
        }
        .publish(env);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_forge_test_utils::TestAccounts;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::token::{Client as TokenClient, StellarAssetClient};
    use soroban_sdk::{contract, contractimpl, Bytes, Env};

    /// A minimal mock target contract for testing cross-contract
    /// invocation. Its `execute` method is a no-op that accepts the
    /// payload without panicking, proving the invocation succeeded.
    #[contract]
    pub struct MockTarget;

    #[contracttype]
    enum MockTargetKey {
        /// Account balances (unused, but required for the contract).
        Balance(Address),
    }

    #[contractimpl]
    impl MockTarget {
        /// Accept the invocation. Does not access storage, so it
        /// works reliably through `try_invoke_contract` in the test
        /// host.
        pub fn execute(_env: Env, _payload: Bytes) {}
    }

    /// A mock target whose `execute` panics, simulating a target
    /// revert for testing the failure-ordering guarantee.
    #[contract]
    pub struct BlockingTarget;

    #[contracttype]
    enum BlockingTargetKey {
        /// Account balances (unused, but required for the contract).
        Balance(Address),
    }

    #[contractimpl]
    impl BlockingTarget {
        /// Panics on invocation, simulating a target revert.
        pub fn execute(_env: Env, _payload: Bytes) {
            panic!("target reverted");
        }
    }

    /// Build a fresh env with mocked auths, a registered contract, a configured
    /// wallet (threshold 2), and named accounts. The generated client exposes
    /// its env via the public `env` field, so it cannot be returned from a
    /// helper.
    macro_rules! setup {
        () => {{
            let env = Env::default();
            env.mock_all_auths();
            let contract_id = env.register(MultiSigWallet, ());
            let client = SorobanForgeMultiSigWalletClient::new(&env, &contract_id);
            let accounts = TestAccounts::generate(&env);
            let owners = owner_vec(&env, &accounts);
            client.initialize(&owners, &2_u32);
            (env, client, accounts)
        }};
    }

    /// Fresh env with a SAC token on top of the standard `setup!`: the wallet
    /// is configured (threshold 2), `user1` starts funded with [`DEPOSIT`],
    /// and everyone else starts at zero.
    macro_rules! custody {
        () => {{
            let env = Env::default();
            env.mock_all_auths();
            let contract_id = env.register(MultiSigWallet, ());
            let client = SorobanForgeMultiSigWalletClient::new(&env, &contract_id);
            let accounts = TestAccounts::generate(&env);
            let owners = owner_vec(&env, &accounts);
            client.initialize(&owners, &2_u32);

            let admin = Address::generate(&env);
            let sac = env.register_stellar_asset_contract_v2(admin.clone());
            let token = sac.address();
            let token_admin = StellarAssetClient::new(&env, &token);
            let token_client = TokenClient::new(&env, &token);
            token_admin.mint(&accounts.user1, &DEPOSIT);

            (env, client, accounts, token, token_client)
        }};
    }

    const DEPOSIT: i128 = 10_000;

    /// A minimal SEP-41-shaped token that can refuse transfers **to** a
    /// chosen recipient. Used to force the wallet's payout transfer to fail
    /// after custody has already been validated, without relying on SAC
    /// admin flags (not available on the test-host SAC).
    #[contract]
    pub struct BlockingToken;

    #[contracttype]
    enum BlockingKey {
        /// Recipient that rejects incoming transfers.
        Blocked,
        /// Account balances.
        Balance(Address),
    }

    #[contractimpl]
    impl BlockingToken {
        /// Mark `to` as a recipient that rejects incoming transfers.
        pub fn block(env: Env, to: Address) {
            env.storage().instance().set(&BlockingKey::Blocked, &to);
        }

        /// Mint test funds to `to`.
        pub fn mint(env: Env, to: Address, amount: i128) {
            let key = BlockingKey::Balance(to);
            let current: i128 = env.storage().instance().get(&key).unwrap_or(0);
            env.storage().instance().set(&key, &(current + amount));
        }

        /// SEP-41 balance view.
        pub fn balance(env: Env, id: Address) -> i128 {
            env.storage()
                .instance()
                .get(&BlockingKey::Balance(id))
                .unwrap_or(0)
        }

        /// SEP-41 transfer that aborts when `to` was blocked.
        pub fn transfer(env: Env, from: Address, to: Address, amount: i128) {
            if env.storage().instance().has(&BlockingKey::Blocked) {
                let blocked: Address = env.storage().instance().get(&BlockingKey::Blocked).unwrap();
                if to == blocked {
                    panic!("blocked recipient");
                }
            }
            let from_key = BlockingKey::Balance(from);
            let to_key = BlockingKey::Balance(to);
            let from_bal: i128 = env.storage().instance().get(&from_key).unwrap_or(0);
            env.storage()
                .instance()
                .set(&from_key, &(from_bal - amount));
            let to_bal: i128 = env.storage().instance().get(&to_key).unwrap_or(0);
            env.storage().instance().set(&to_key, &(to_bal + amount));
        }
    }

    fn owner_vec(env: &Env, accounts: &TestAccounts) -> soroban_sdk::Vec<Address> {
        soroban_sdk::vec![
            env,
            accounts.user1.clone(),
            accounts.user2.clone(),
            accounts.user3.clone()
        ]
    }

    fn payload(env: &Env) -> Bytes {
        Bytes::from_array(env, &[0x01, 0x02, 0x03])
    }

    fn target(env: &Env) -> Address {
        Address::generate(env)
    }

    #[test]
    fn initialize_sets_threshold() {
        let (_env, client, accounts) = setup!();
        assert_eq!(client.get_threshold(), 2_u32);
        let _ = owner_vec(&client.env, &accounts);
    }

    /// A fresh, uninitialized wallet for `initialize` validation tests. The
    /// standard `setup!` is intentionally avoided so `try_initialize` never
    /// returns `AlreadyInitialized`.
    macro_rules! fresh {
        () => {{
            let env = Env::default();
            env.mock_all_auths();
            let contract_id = env.register(MultiSigWallet, ());
            let client = SorobanForgeMultiSigWalletClient::new(&env, &contract_id);
            let accounts = TestAccounts::generate(&env);
            (env, client, accounts)
        }};
    }

    #[test]
    fn initialize_rejects_zero_threshold() {
        let (env, client, accounts) = fresh!();
        let err = client
            .try_initialize(&owner_vec(&env, &accounts), &0_u32)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn initialize_rejects_threshold_above_owner_count() {
        let (env, client, accounts) = fresh!();
        let err = client
            .try_initialize(&owner_vec(&env, &accounts), &99_u32)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn initialize_rejects_empty_owners() {
        let (env, client, _accounts) = fresh!();
        let err = client
            .try_initialize(&soroban_sdk::Vec::new(&env), &1_u32)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn initialize_rejects_duplicate_owners() {
        let (env, client, accounts) = fresh!();
        let dup = soroban_sdk::vec![
            &env,
            accounts.user1.clone(),
            accounts.user1.clone(),
            accounts.user2.clone()
        ];
        let err = client.try_initialize(&dup, &2_u32).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn initialize_rejects_reinitialization() {
        let (env, client, accounts) = setup!();
        let err = client
            .try_initialize(&owner_vec(&env, &accounts), &3_u32)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::AlreadyInitialized);
    }

    #[test]
    fn submit_creates_pending_tx() {
        let (env, client, accounts) = setup!();
        let tx_id = client.submit(&accounts.user1, &target(&env), &payload(&env));
        let tx = client.get_tx(&tx_id);
        assert_eq!(tx.submitter, accounts.user1);
        assert_eq!(tx.status, TxStatus::Pending);
        assert_eq!(tx.confirmations.len(), 0);
    }

    #[test]
    fn submit_assigns_distinct_ids() {
        let (env, client, accounts) = setup!();
        let id1 = client.submit(&accounts.user1, &target(&env), &payload(&env));
        let id2 = client.submit(&accounts.user1, &target(&env), &payload(&env));
        assert_ne!(id1, id2);
    }

    #[test]
    fn submit_rejects_non_owner() {
        let (env, client, accounts) = setup!();
        let err = client
            .try_submit(&accounts.arbiter, &accounts.arbiter, &payload(&env))
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::Unauthorized);
    }

    #[test]
    fn submit_before_initialize_is_not_initialized() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(MultiSigWallet, ());
        let client = SorobanForgeMultiSigWalletClient::new(&env, &contract_id);
        let accounts = TestAccounts::generate(&env);
        let err = client
            .try_submit(&accounts.user1, &accounts.user1, &payload(&env))
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::NotInitialized);
    }

    #[test]
    fn confirm_records_approval() {
        let (env, client, accounts) = setup!();
        let tx_id = client.submit(&accounts.user1, &target(&env), &payload(&env));
        client.confirm(&tx_id, &accounts.user2);
        let tx = client.get_tx(&tx_id);
        assert_eq!(tx.confirmations.len(), 1);
        assert_eq!(tx.confirmations.get_unchecked(0), accounts.user2);
    }

    #[test]
    fn confirm_twice_is_invalid() {
        let (env, client, accounts) = setup!();
        let tx_id = client.submit(&accounts.user1, &target(&env), &payload(&env));
        client.confirm(&tx_id, &accounts.user2);
        let err = client
            .try_confirm(&tx_id, &accounts.user2)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn confirm_non_owner_is_unauthorized() {
        let (env, client, accounts) = setup!();
        let tx_id = client.submit(&accounts.user1, &target(&env), &payload(&env));
        let err = client
            .try_confirm(&tx_id, &accounts.arbiter)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::Unauthorized);
    }

    #[test]
    fn confirm_missing_tx_is_not_found() {
        let (_env, client, accounts) = setup!();
        client
            .try_confirm(&999, &accounts.user1)
            .unwrap_err()
            .unwrap();
    }

    // -------------------------------------------------------------------
    // Rejection
    // -------------------------------------------------------------------

    #[test]
    fn reject_records_objection() {
        let (env, client, accounts) = setup!();
        let tx_id = client.submit(&accounts.user1, &target(&env), &payload(&env));
        client.reject(&tx_id, &accounts.user2);
        let tx = client.get_tx(&tx_id);
        assert_eq!(tx.rejections.len(), 1);
        assert_eq!(tx.rejections.get_unchecked(0), accounts.user2);
        // Below the rejection threshold: still Pending, but blocked from
        // executing.
        assert_eq!(tx.status, TxStatus::Pending);
    }

    #[test]
    fn reject_twice_is_invalid() {
        let (env, client, accounts) = setup!();
        let tx_id = client.submit(&accounts.user1, &target(&env), &payload(&env));
        client.reject(&tx_id, &accounts.user2);
        let err = client
            .try_reject(&tx_id, &accounts.user2)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn reject_non_owner_is_unauthorized() {
        let (env, client, accounts) = setup!();
        let tx_id = client.submit(&accounts.user1, &target(&env), &payload(&env));
        let err = client
            .try_reject(&tx_id, &accounts.arbiter)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::Unauthorized);
    }

    #[test]
    fn reject_missing_tx_is_not_found() {
        let (_env, client, accounts) = setup!();
        let err = client
            .try_reject(&999, &accounts.user1)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::NotFound);
    }

    #[test]
    fn reject_reaches_threshold_rejects_tx() {
        let (env, client, accounts) = setup!();
        let tx_id = client.submit(&accounts.user1, &target(&env), &payload(&env));
        client.reject(&tx_id, &accounts.user2);
        client.reject(&tx_id, &accounts.user3);
        assert_eq!(client.get_tx(&tx_id).status, TxStatus::Rejected);
    }

    #[test]
    fn threshold_one_single_rejection_rejects() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(MultiSigWallet, ());
        let client = SorobanForgeMultiSigWalletClient::new(&env, &contract_id);
        let accounts = TestAccounts::generate(&env);
        let owners = owner_vec(&env, &accounts);
        client.initialize(&owners, &1_u32);
        let tx_id = client.submit(&accounts.user1, &target(&env), &payload(&env));
        client.reject(&tx_id, &accounts.user2);
        assert_eq!(client.get_tx(&tx_id).status, TxStatus::Rejected);
    }

    #[test]
    fn rejected_tx_cannot_be_confirmed() {
        let (env, client, accounts) = setup!();
        let tx_id = client.submit(&accounts.user1, &target(&env), &payload(&env));
        client.reject(&tx_id, &accounts.user2);
        client.reject(&tx_id, &accounts.user3);
        let err = client
            .try_confirm(&tx_id, &accounts.user1)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn rejected_tx_cannot_be_executed() {
        let (env, client, accounts) = setup!();
        let tx_id = client.submit(&accounts.user1, &target(&env), &payload(&env));
        client.reject(&tx_id, &accounts.user2);
        client.reject(&tx_id, &accounts.user3);
        let err = client.try_execute(&tx_id).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn reject_rejected_tx_is_invalid() {
        let (env, client, accounts) = setup!();
        let tx_id = client.submit(&accounts.user1, &target(&env), &payload(&env));
        client.reject(&tx_id, &accounts.user2);
        client.reject(&tx_id, &accounts.user3);
        let err = client
            .try_reject(&tx_id, &accounts.user1)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn reject_executed_tx_is_invalid() {
        let (env, client, accounts) = setup!();
        let mock_target_id = Address::generate(&env);
        env.register_at(&mock_target_id, MockTarget, ());
        let tx_id = client.submit(&accounts.user1, &mock_target_id, &payload(&env));
        client.confirm(&tx_id, &accounts.user2);
        client.confirm(&tx_id, &accounts.user3);
        client.execute(&tx_id);
        assert_eq!(client.get_tx(&tx_id).status, TxStatus::Executed);
        let err = client
            .try_reject(&tx_id, &accounts.user1)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn confirm_then_reject_is_invalid() {
        let (env, client, accounts) = setup!();
        let tx_id = client.submit(&accounts.user1, &target(&env), &payload(&env));
        client.confirm(&tx_id, &accounts.user2);
        let err = client
            .try_reject(&tx_id, &accounts.user2)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn reject_then_confirm_is_invalid() {
        let (env, client, accounts) = setup!();
        let tx_id = client.submit(&accounts.user1, &target(&env), &payload(&env));
        client.reject(&tx_id, &accounts.user2);
        let err = client
            .try_confirm(&tx_id, &accounts.user2)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn reject_with_confirmations_is_allowed() {
        let (env, client, accounts) = setup!();
        let tx_id = client.submit(&accounts.user1, &target(&env), &payload(&env));
        client.confirm(&tx_id, &accounts.user2);
        client.reject(&tx_id, &accounts.user3);
        let tx = client.get_tx(&tx_id);
        assert_eq!(tx.confirmations.len(), 1);
        assert_eq!(tx.rejections.len(), 1);
        assert_eq!(tx.status, TxStatus::Pending);
    }

    #[test]
    fn sub_threshold_rejection_blocks_execute() {
        let (env, client, accounts) = setup!();
        let mock_target_id = Address::generate(&env);
        env.register_at(&mock_target_id, MockTarget, ());
        let tx_id = client.submit(&accounts.user1, &mock_target_id, &payload(&env));
        client.confirm(&tx_id, &accounts.user2);
        client.confirm(&tx_id, &accounts.user3);
        client.reject(&tx_id, &accounts.user1);
        // Confirmations meet the threshold, but the standing objection wins.
        let err = client.try_execute(&tx_id).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
        assert_eq!(client.get_tx(&tx_id).status, TxStatus::Pending);
    }

    #[test]
    fn execute_requires_threshold() {
        let (env, client, accounts) = setup!();
        let tx_id = client.submit(&accounts.user1, &target(&env), &payload(&env));
        // Below the threshold of 2: execution is still blocked.
        client.confirm(&tx_id, &accounts.user2);
        let err = client.try_execute(&tx_id).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn execute_after_threshold_succeeds() {
        let (env, client, accounts) = setup!();
        let mock_target_id = Address::generate(&env);
        env.register_at(&mock_target_id, MockTarget, ());
        let tx_id = client.submit(&accounts.user1, &mock_target_id, &payload(&env));
        client.confirm(&tx_id, &accounts.user2);
        client.confirm(&tx_id, &accounts.user3);
        client.execute(&tx_id);
        assert_eq!(client.get_tx(&tx_id).status, TxStatus::Executed);
    }

    #[test]
    fn execute_twice_is_invalid() {
        let (env, client, accounts) = setup!();
        let mock_target_id = Address::generate(&env);
        env.register_at(&mock_target_id, MockTarget, ());
        let tx_id = client.submit(&accounts.user1, &mock_target_id, &payload(&env));
        client.confirm(&tx_id, &accounts.user2);
        client.confirm(&tx_id, &accounts.user3);
        client.execute(&tx_id);
        let err = client.try_execute(&tx_id).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn missing_tx_is_not_found() {
        let (_env, client, _accounts) = setup!();
        let err = client.try_get_tx(&999).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::NotFound);
    }

    // -------------------------------------------------------------------
    // Read-only introspection views
    // -------------------------------------------------------------------

    #[test]
    fn get_owners_round_trips_initialization_order() {
        let (env, client, accounts) = setup!();
        let owners = client.get_owners();
        assert_eq!(owners, owner_vec(&env, &accounts));
        assert_eq!(owners.len(), 3);
        assert_eq!(owners.get_unchecked(0), accounts.user1);
        assert_eq!(owners.get_unchecked(1), accounts.user2);
        assert_eq!(owners.get_unchecked(2), accounts.user3);
    }

    #[test]
    fn get_owners_before_initialize_is_not_initialized() {
        let (_env, client, _accounts) = fresh!();
        let err = client.try_get_owners().unwrap_err().unwrap();
        assert_eq!(err, ForgeError::NotInitialized);
    }

    #[test]
    fn is_owner_recognizes_members_only() {
        let (_env, client, accounts) = setup!();
        assert!(client.is_owner(&accounts.user1));
        assert!(client.is_owner(&accounts.user2));
        assert!(client.is_owner(&accounts.user3));
        assert!(!client.is_owner(&accounts.arbiter));
    }

    #[test]
    fn is_owner_before_initialize_is_false() {
        let (_env, client, accounts) = fresh!();
        assert!(!client.is_owner(&accounts.user1));
        assert!(!client.is_owner(&accounts.arbiter));
    }

    #[test]
    fn get_confirmations_reflects_recorded_order() {
        let (env, client, accounts) = setup!();
        let tx_id = client.submit(&accounts.user1, &target(&env), &payload(&env));
        let empty = client.get_confirmations(&tx_id);
        assert_eq!(empty.len(), 0);

        client.confirm(&tx_id, &accounts.user2);
        client.confirm(&tx_id, &accounts.user3);
        let confirmations = client.get_confirmations(&tx_id);
        assert_eq!(confirmations.len(), 2);
        assert_eq!(confirmations.get_unchecked(0), accounts.user2);
        assert_eq!(confirmations.get_unchecked(1), accounts.user3);
    }

    #[test]
    fn get_confirmations_duplicate_confirm_leaves_list_unchanged() {
        let (env, client, accounts) = setup!();
        let tx_id = client.submit(&accounts.user1, &target(&env), &payload(&env));
        client.confirm(&tx_id, &accounts.user2);
        let err = client
            .try_confirm(&tx_id, &accounts.user2)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);

        let confirmations = client.get_confirmations(&tx_id);
        assert_eq!(confirmations.len(), 1);
        assert_eq!(confirmations.get_unchecked(0), accounts.user2);
    }

    #[test]
    fn get_confirmations_unknown_tx_is_not_found() {
        let (_env, client, _accounts) = setup!();
        let err = client.try_get_confirmations(&999).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::NotFound);
    }

    #[test]
    fn get_confirmations_before_initialize_is_not_found() {
        let (_env, client, _accounts) = fresh!();
        let err = client.try_get_confirmations(&1).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::NotFound);
    }

    #[test]
    fn get_rejections_reflects_recorded_order() {
        let (env, client, accounts) = setup!();
        let tx_id = client.submit(&accounts.user1, &target(&env), &payload(&env));
        let empty = client.get_rejections(&tx_id);
        assert_eq!(empty.len(), 0);

        client.reject(&tx_id, &accounts.user2);
        client.reject(&tx_id, &accounts.user3);
        let rejections = client.get_rejections(&tx_id);
        assert_eq!(rejections.len(), 2);
        assert_eq!(rejections.get_unchecked(0), accounts.user2);
        assert_eq!(rejections.get_unchecked(1), accounts.user3);
    }

    #[test]
    fn get_rejections_unknown_tx_is_not_found() {
        let (_env, client, _accounts) = setup!();
        let err = client.try_get_rejections(&999).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::NotFound);
    }

    #[test]
    fn get_rejections_before_initialize_is_not_found() {
        let (_env, client, _accounts) = fresh!();
        let err = client.try_get_rejections(&1).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::NotFound);
    }

    #[test]
    fn get_tx_count_tracks_submits() {
        let (env, client, accounts) = setup!();
        assert_eq!(client.get_tx_count(), 0);
        client.submit(&accounts.user1, &target(&env), &payload(&env));
        client.submit(&accounts.user2, &target(&env), &payload(&env));
        client.submit(&accounts.user1, &target(&env), &payload(&env));
        assert_eq!(client.get_tx_count(), 3);
    }

    #[test]
    fn get_tx_count_does_not_count_failed_submits() {
        let (env, client, accounts) = setup!();
        // A rejected submit (non-owner) never advances the counter.
        let err = client
            .try_submit(&accounts.arbiter, &target(&env), &payload(&env))
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::Unauthorized);
        assert_eq!(client.get_tx_count(), 0);

        client.submit(&accounts.user1, &target(&env), &payload(&env));
        assert_eq!(client.get_tx_count(), 1);
    }

    #[test]
    fn get_tx_count_before_initialize_is_zero() {
        let (_env, client, _accounts) = fresh!();
        assert_eq!(client.get_tx_count(), 0);
    }

    #[test]
    fn execute_invokes_target_contract() {
        let (env, client, accounts) = setup!();
        let mock_target_id = Address::generate(&env);
        env.register_at(&mock_target_id, MockTarget, ());
        let tx_id = client.submit(&accounts.user1, &mock_target_id, &payload(&env));
        client.confirm(&tx_id, &accounts.user2);
        client.confirm(&tx_id, &accounts.user3);
        client.execute(&tx_id);

        // The tx was executed successfully.
        assert_eq!(client.get_tx(&tx_id).status, TxStatus::Executed);
    }

    #[test]
    fn execute_target_revert_leaves_tx_unexecuted() {
        let (env, client, accounts) = setup!();
        let mock_target_id = Address::generate(&env);
        env.register_at(&mock_target_id, BlockingTarget, ());
        let tx_id = client.submit(&accounts.user1, &mock_target_id, &payload(&env));
        client.confirm(&tx_id, &accounts.user2);
        client.confirm(&tx_id, &accounts.user3);

        // Execution should fail because the target panics.
        let err = client.try_execute(&tx_id).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::ContractInvocationFailed);
        // The tx is NOT executed.
        assert_eq!(client.get_tx(&tx_id).status, TxStatus::Pending);
    }

    #[test]
    fn execute_below_threshold_does_not_invoke_target() {
        let (env, client, accounts) = setup!();
        let mock_target_id = Address::generate(&env);
        env.register_at(&mock_target_id, MockTarget, ());
        let tx_id = client.submit(&accounts.user1, &mock_target_id, &payload(&env));
        // Only one confirmation, threshold is 2.
        client.confirm(&tx_id, &accounts.user2);
        // Should fail before invoking the target.
        let err = client.try_execute(&tx_id).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    // -------------------------------------------------------------------
    // Custody: deposits
    // -------------------------------------------------------------------

    #[test]
    fn deposit_moves_tokens_and_updates_balance() {
        let (_env, client, accounts, token, token_client) = custody!();
        client.deposit(&token, &accounts.user1, &1_000);

        assert_eq!(client.balance(&token), 1_000);
        assert_eq!(token_client.balance(&accounts.user1), DEPOSIT - 1_000);
    }

    #[test]
    fn deposit_from_third_party_is_allowed() {
        let (env, client, accounts, token, token_client) = custody!();
        // `arbiter` is not an owner; open custody accepts direct funding.
        let token_admin = StellarAssetClient::new(&env, &token);
        token_admin.mint(&accounts.arbiter, &500);
        client.deposit(&token, &accounts.arbiter, &500);
        assert_eq!(client.balance(&token), 500);
        assert_eq!(token_client.balance(&accounts.arbiter), 0);
    }

    #[test]
    fn balance_defaults_to_zero_for_unknown_token() {
        let (env, client, _accounts, _token, _token_client) = custody!();
        let unknown = Address::generate(&env);
        assert_eq!(client.balance(&unknown), 0);
    }

    #[test]
    fn deposit_rejects_zero_and_negative_amounts() {
        let (_env, client, accounts, token, _token_client) = custody!();
        let err = client
            .try_deposit(&token, &accounts.user1, &0_i128)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);

        let err = client
            .try_deposit(&token, &accounts.user1, &-1_i128)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn deposit_before_initialize_is_not_initialized() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(MultiSigWallet, ());
        let client = SorobanForgeMultiSigWalletClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let sac = env.register_stellar_asset_contract_v2(admin);
        let token = sac.address();
        let err = client
            .try_deposit(&token, &Address::generate(&env), &1_i128)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::NotInitialized);
    }

    #[test]
    fn deposit_transfer_failure_leaves_balance_untouched() {
        let (_env, client, accounts, token, _token_client) = custody!();
        // `user2` holds nothing: the token pull fails at the token level.
        let err = client
            .try_deposit(&token, &accounts.user2, &1_000)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::TokenTransferFailed);
        assert_eq!(client.balance(&token), 0);
    }

    // -------------------------------------------------------------------
    // Custody: withdrawals
    // -------------------------------------------------------------------

    #[test]
    fn withdrawal_executes_at_threshold_exactly_once() {
        let (_env, client, accounts, token, token_client) = custody!();
        client.deposit(&token, &accounts.user1, &1_000);

        let tx_id = client.submit_withdrawal(&accounts.user1, &token, &accounts.arbiter, &400_i128);
        client.confirm(&tx_id, &accounts.user2);
        client.confirm(&tx_id, &accounts.user3);
        client.execute(&tx_id);

        // Destination paid, custody debited, tx executed — exactly once.
        assert_eq!(token_client.balance(&accounts.arbiter), 400);
        assert_eq!(client.balance(&token), 600);
        assert_eq!(client.get_tx(&tx_id).status, TxStatus::Executed);

        let err = client.try_execute(&tx_id).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
        assert_eq!(token_client.balance(&accounts.arbiter), 400);
        assert_eq!(client.balance(&token), 600);
    }

    #[test]
    fn below_threshold_withdrawal_cannot_execute() {
        let (_env, client, accounts, token, token_client) = custody!();
        client.deposit(&token, &accounts.user1, &1_000);

        let tx_id = client.submit_withdrawal(&accounts.user1, &token, &accounts.arbiter, &400_i128);
        // One confirmation, threshold is 2.
        client.confirm(&tx_id, &accounts.user2);

        let err = client.try_execute(&tx_id).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
        assert_eq!(token_client.balance(&accounts.arbiter), 0);
        assert_eq!(client.balance(&token), 1_000);
        assert_eq!(client.get_tx(&tx_id).status, TxStatus::Pending);
    }

    #[test]
    fn withdrawal_rejection_at_threshold_moves_no_tokens() {
        let (_env, client, accounts, token, token_client) = custody!();
        client.deposit(&token, &accounts.user1, &1_000);

        let tx_id = client.submit_withdrawal(&accounts.user1, &token, &accounts.arbiter, &400_i128);
        client.reject(&tx_id, &accounts.user2);
        client.reject(&tx_id, &accounts.user3);

        assert_eq!(client.get_tx(&tx_id).status, TxStatus::Rejected);
        assert_eq!(token_client.balance(&accounts.arbiter), 0);
        assert_eq!(client.balance(&token), 1_000);
    }

    #[test]
    fn sub_threshold_withdrawal_rejection_blocks_execute() {
        let (_env, client, accounts, token, _token_client) = custody!();
        client.deposit(&token, &accounts.user1, &1_000);

        let tx_id = client.submit_withdrawal(&accounts.user1, &token, &accounts.arbiter, &400_i128);
        client.confirm(&tx_id, &accounts.user2);
        client.confirm(&tx_id, &accounts.user3);
        // The submitter's standing objection stalls an otherwise
        // threshold-met withdrawal.
        client.reject(&tx_id, &accounts.user1);

        let err = client.try_execute(&tx_id).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
        assert_eq!(client.get_tx(&tx_id).status, TxStatus::Pending);
    }

    #[test]
    fn withdrawal_exceeding_balance_fails_and_changes_nothing() {
        let (_env, client, accounts, token, token_client) = custody!();
        client.deposit(&token, &accounts.user1, &100);

        let tx_id =
            client.submit_withdrawal(&accounts.user1, &token, &accounts.arbiter, &1_000_i128);
        client.confirm(&tx_id, &accounts.user2);
        client.confirm(&tx_id, &accounts.user3);

        let err = client.try_execute(&tx_id).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::InsufficientFunds);
        // Nothing changed: custody intact, destination unfunded, tx pending.
        assert_eq!(client.balance(&token), 100);
        assert_eq!(token_client.balance(&accounts.arbiter), 0);
        assert_eq!(client.get_tx(&tx_id).status, TxStatus::Pending);
    }

    #[test]
    fn withdrawal_failed_transfer_leaves_state_untouched() {
        let (env, client, accounts, token, _token_client) = custody!();
        client.deposit(&token, &accounts.user1, &1_000);

        // The destination is blocked on the token, so the wallet's payout
        // transfer fails even though custody covers the amount. This is the
        // load-bearing ordering check: transfer first, state second — a
        // failed transfer must leave balances and tx state untouched.
        let token_b = env.register(BlockingToken, ());
        let token_b_client = BlockingTokenClient::new(&env, &token_b);
        token_b_client.mint(&accounts.user1, &1_000);
        client.deposit(&token_b, &accounts.user1, &1_000);
        assert_eq!(client.balance(&token_b), 1_000);
        token_b_client.block(&accounts.arbiter);

        let tx_id =
            client.submit_withdrawal(&accounts.user1, &token_b, &accounts.arbiter, &400_i128);
        client.confirm(&tx_id, &accounts.user2);
        client.confirm(&tx_id, &accounts.user3);

        let err = client.try_execute(&tx_id).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::TokenTransferFailed);
        assert_eq!(client.balance(&token_b), 1_000);
        assert_eq!(token_b_client.balance(&accounts.arbiter), 0);
        assert_eq!(client.get_tx(&tx_id).status, TxStatus::Pending);
    }

    #[test]
    fn submit_withdrawal_rejects_non_owner() {
        let (_env, client, accounts, token, _token_client) = custody!();
        let err = client
            .try_submit_withdrawal(&accounts.arbiter, &token, &accounts.arbiter, &100_i128)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::Unauthorized);
    }

    #[test]
    fn submit_withdrawal_rejects_zero_amount() {
        let (_env, client, accounts, token, _token_client) = custody!();
        let err = client
            .try_submit_withdrawal(&accounts.user1, &token, &accounts.arbiter, &0_i128)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn submit_withdrawal_before_initialize_is_not_initialized() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(MultiSigWallet, ());
        let client = SorobanForgeMultiSigWalletClient::new(&env, &contract_id);
        let accounts = TestAccounts::generate(&env);
        let token = Address::generate(&env);
        let err = client
            .try_submit_withdrawal(&accounts.user1, &token, &accounts.arbiter, &100_i128)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::NotInitialized);
    }

    // -------------------------------------------------------------------
    // Custody: per-token accounting and TTL keeper
    // -------------------------------------------------------------------

    #[test]
    fn balances_are_tracked_per_token() {
        let (env, client, accounts, token, _token_client) = custody!();
        let admin2 = Address::generate(&env);
        let token_b = env.register_stellar_asset_contract_v2(admin2).address();
        let token_admin_b = StellarAssetClient::new(&env, &token_b);
        token_admin_b.mint(&accounts.user1, &DEPOSIT);

        client.deposit(&token, &accounts.user1, &1_000);
        client.deposit(&token_b, &accounts.user1, &250);

        assert_eq!(client.balance(&token), 1_000);
        assert_eq!(client.balance(&token_b), 250);

        // Withdrawing token A leaves token B's accounting untouched.
        let tx_id =
            client.submit_withdrawal(&accounts.user1, &token, &accounts.arbiter, &1_000_i128);
        client.confirm(&tx_id, &accounts.user2);
        client.confirm(&tx_id, &accounts.user3);
        client.execute(&tx_id);

        assert_eq!(client.balance(&token), 0);
        assert_eq!(client.balance(&token_b), 250);
    }

    #[test]
    fn touch_ttl_extends_and_keeps_balance_intact() {
        let (_env, client, accounts, token, _token_client) = custody!();
        client.deposit(&token, &accounts.user1, &1_000);

        client.touch_ttl(&token);

        assert_eq!(client.balance(&token), 1_000);
    }

    #[test]
    fn touch_ttl_unknown_token_is_not_found() {
        let (env, client, _accounts, _token, _token_client) = custody!();
        let unknown = Address::generate(&env);
        let err = client.try_touch_ttl(&unknown).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::NotFound);
    }
}
