#![no_std]

//! # Soroban Forge — Vesting contract
//!
//! A token-vesting contract that releases a beneficiary's tokens over time.
//! Two schedule shapes share one id space and one claim path:
//!
//! - **linear** ([`VestingSchedule`], created by `create_schedule`) — an
//!   optional cliff followed by a straight-line ramp to `duration`;
//! - **tranche** ([`TrancheSchedule`], created by `create_tranche_schedule`) —
//!   an explicit, ordered table of discrete unlocks.
//!
//! The two are deliberately **not** merged into one record: the linear
//! record's wire shape and its floor-division math stay exactly as they were,
//! and a tranche schedule lives under its own `DataKey` variant.
//!
//! Timings in both shapes are expressed as **durations in seconds measured
//! from the schedule start** (the ledger timestamp recorded at creation).
//!
//! ## Linear shape
//!
//! ```text
//! start ......... start+cliff ................... start+duration
//!   |             (claims become possible)        (fully vested)
//!   |  Locked    |            Vesting (linear)  |
//! ```
//!
//! The vested amount at ledger time `t` is:
//! - `0` when `t < start + cliff`,
//! - `total_amount` when `t >= start + duration`,
//! - otherwise `total_amount * (t - (start + cliff)) / (duration - cliff)`,
//!   using integer (floor) division so claims never round up.
//!
//! ## Tranche shape
//!
//! A grant agreement of the form "25% at TGE, 25% at +6 months, 50% at +12
//! months" is not expressible as a ramp, so the tranche kind takes the unlock
//! table verbatim: an ordered list of [`Tranche`]s, each an offset from
//! `start` plus the amount that unlocks at it.
//!
//! ```text
//!        t1            t2                t3
//! start   |             |                 |
//!   |     |   tranche 1  |   tranche 2     |   tranche 3
//!   | Locked  (a1)         (a2)              (a3)
//!   |     |  vested:      vested:           vested:
//!   |     |     0  ->     a1  ->            a1+a2  ->  total
//!   +-----+--------------+------------------+---------.
//! ```
//!
//! Offsets are strictly increasing, so the vested amount is a **step function**
//! of the sum of every tranche whose offset has elapsed: `0` before `t1`,
//! `a1` between `t1` and `t2`, `a1 + a2` between `t2` and `t3`, and the full
//! total from `t3` on. Nothing accrues between unlocks, and the vested amount
//! never decreases, so a claim at any moment pays exactly the difference
//! since the last one.
//!
//! Unlock progress is compared on the *elapsed offset* rather than on
//! `start + unlock_at`, so an offset of `u64::MAX` cannot overflow: that
//! tranche simply never unlocks.
//!
//! ## Status
//!
//! Both kinds derive status from the same two inputs — ledger time and the
//! claimed amount: `Locked` before the first unlock, `Vesting` from the first
//! unlock until the final amount is claimed, `Completed` once `claimed ==
//! total_amount`. `Revoked` is reserved (see below).
//!
//! Authorization model:
//! - `create_schedule` and `create_tranche_schedule` require the beneficiary.
//! - `claim` requires the beneficiary.
//! - `claimable`, `get_status`, and `get_tranche_schedule` are read-only
//!   views.
//!
//! ## Settlement (load-bearing)
//!
//! The contract custodies the configured SEP-41 token and `claim` settles
//! through it: the newly claimable amount is transferred from this contract
//! to the beneficiary, and the schedule is written only after that transfer
//! succeeds. The transfer-before-state ordering mirrors
//! `crates/escrow/src/lib.rs` — a failed transfer (empty contract balance,
//! undeployed token) returns [`ForgeError::TokenTransferFailed`] with
//! `claimed` and `status` untouched. A zero-claim call exits before the
//! transfer, so no empty transfers are ever issued. Soroban's frame rollback
//! is the outer atomicity guarantee: any `Err` returned from `claim` reverts
//! the whole invocation, including sub-invocations.
//!
//! The `Revoked` status is reserved for a revocation method that lands in a
//! follow-up; it is not reachable through the current public interface.

#[cfg(test)]
extern crate std;

use soroban_forge_shared_utils::ForgeError;
use soroban_sdk::{contract, contractclient, contractimpl, contracttype, token, Address, Env, Vec};

/// Maximum number of tranches a single schedule may carry.
///
/// Grant agreements express unlock tables with a handful of entries (a TGE
/// tranche plus quarterly or half-yearly ones), and every claimable/status
/// call scans the stored table, so the cap bounds the instruction use of a
/// claim. Creation rejects a longer table with [`ForgeError::InvalidInput`]
/// rather than truncating it. The cap is deliberately generous relative to
/// real agreements; raise it only with a reason.
pub const MAX_TRANCHES: u32 = 32;

/// Public interface for the Soroban Forge vesting contract.
///
/// Declared as a `contractclient` trait so SDK consumers (and the TypeScript
/// SDK generator) get a strongly-typed client without coupling to the
/// implementation crate.
#[contractclient(name = "SorobanForgeVestingClient")]
pub trait SorobanForgeVesting {
    /// Create a new vesting schedule for `beneficiary`.
    ///
    /// `cliff` and `duration` are seconds measured from creation
    /// (`cliff <= duration`, `duration > 0`, `total_amount > 0`). Returns the
    /// stable schedule id.
    fn create_schedule(
        env: Env,
        beneficiary: Address,
        token: Address,
        total_amount: i128,
        cliff: u64,
        duration: u64,
    ) -> Result<u64, soroban_forge_shared_utils::ForgeError>;

    /// Create a new tranche (discrete unlock) vesting schedule for
    /// `beneficiary`.
    ///
    /// `tranches` is the unlock table: an ordered list of [`Tranche`]s, each
    /// an offset in seconds from creation plus the amount that unlocks at it.
    /// It must be non-empty, hold at most [`MAX_TRANCHES`] entries with
    /// strictly increasing offsets and positive amounts, and sum to at most
    /// `i128::MAX`. The table is validated, stored once, and immutable
    /// afterwards. The id comes from the same counter as
    /// [`create_schedule`](Self::create_schedule), so both kinds share one id
    /// space.
    ///
    /// # Errors
    ///
    /// * [`ForgeError::InvalidInput`] — the table is empty, longer than
    ///   [`MAX_TRANCHES`], holds a non-positive amount, or an offset that does
    ///   not strictly increase.
    /// * [`ForgeError::ArithmeticOverflow`] — the table's amounts sum past
    ///   `i128::MAX`.
    fn create_tranche_schedule(
        env: Env,
        beneficiary: Address,
        token: Address,
        tranches: Vec<Tranche>,
    ) -> Result<u64, soroban_forge_shared_utils::ForgeError>;

    /// Read the full tranche schedule record, immutable unlock table included
    /// (read-only view).
    ///
    /// The record-view counterpart of `get_schedule` (proposed for the linear
    /// kind in issue #125); a linear id is `NotFound` here, and vice versa.
    fn get_tranche_schedule(
        env: Env,
        schedule_id: u64,
    ) -> Result<TrancheSchedule, soroban_forge_shared_utils::ForgeError>;

    /// Claim tokens that have vested as of the current ledger time.
    ///
    /// Requires the beneficiary. Works for both schedule kinds and transfers
    /// the exact vested-but-unclaimed amount from this contract to the
    /// beneficiary, then records the claim; returns `0` without issuing a
    /// transfer when nothing is claimable.
    ///
    /// # Errors
    ///
    /// * [`ForgeError::NotFound`] — no schedule with this id.
    /// * [`ForgeError::TokenTransferFailed`] — the token contract rejected
    ///   the payout (insufficient contract balance, undeployed token).
    /// * [`ForgeError::ArithmeticOverflow`] — the claimed total overflowed.
    fn claim(env: Env, schedule_id: u64) -> Result<i128, soroban_forge_shared_utils::ForgeError>;

    /// Return the amount currently claimable by `schedule_id` (read-only).
    ///
    /// Kind-aware: a linear id is measured against its cliff and duration, a
    /// tranche id against its unlock table.
    fn claimable(
        env: Env,
        schedule_id: u64,
    ) -> Result<i128, soroban_forge_shared_utils::ForgeError>;

    /// Read the current lifecycle status of `schedule_id` (read-only).
    fn get_status(
        env: Env,
        schedule_id: u64,
    ) -> Result<VestingStatus, soroban_forge_shared_utils::ForgeError>;
}

/// Lifecycle state of a vesting schedule.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VestingStatus {
    /// Before the cliff has been reached.
    Locked,
    /// Past the cliff; tokens are vesting linearly.
    Vesting,
    /// Fully vested and claimed.
    Completed,
    /// Schedule was terminated before completion (reserved).
    Revoked,
}

/// A single token-vesting schedule.
#[contracttype]
#[derive(Clone, Debug)]
pub struct VestingSchedule {
    /// Recipient of the vested tokens.
    pub beneficiary: Address,
    /// Token contract whose balance is drawn down.
    pub token: Address,
    /// Total amount to vest linearly between `cliff` and `duration`.
    pub total_amount: i128,
    /// Ledger timestamp at which vesting begins (creation time).
    pub start: u64,
    /// Seconds after `start` at which claims become possible.
    pub cliff: u64,
    /// Seconds after `start` at which the schedule is fully vested.
    pub duration: u64,
    /// Amount already claimed by the beneficiary.
    pub claimed: i128,
    /// Current lifecycle state.
    pub status: VestingStatus,
}

/// One entry of a tranche schedule's unlock table: an amount that becomes
/// claimable at a fixed offset from the schedule start.
///
/// A grant agreement's "25% at TGE, 25% at +6 months, 50% at +12 months"
/// is three of these, not a ramp.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Tranche {
    /// Seconds after `start` at which this tranche unlocks. Compared as an
    /// offset, so `u64::MAX` is a valid (never-reached) boundary rather than
    /// an overflow.
    pub unlock_at: u64,
    /// Amount that unlocks at `unlock_at`; must be positive.
    pub amount: i128,
}

/// A vesting schedule whose unlock table is an explicit list of tranches
/// instead of a cliff-plus-ramp.
///
/// A separate record from [`VestingSchedule`] on purpose: the linear record's
/// wire shape stays untouched, and the two kinds sit under distinct
/// `DataKey` variants while sharing one monotonic id counter.
#[contracttype]
#[derive(Clone, Debug)]
pub struct TrancheSchedule {
    /// Recipient of the unlocked tokens.
    pub beneficiary: Address,
    /// Token contract whose balance is drawn down.
    pub token: Address,
    /// Sum of every tranche amount; the amount unlocked at the last unlock.
    pub total_amount: i128,
    /// Ledger timestamp the tranche offsets are measured from (creation
    /// time).
    pub start: u64,
    /// The immutable unlock table, ordered by strictly increasing
    /// `unlock_at`. Non-empty and at most [`MAX_TRANCHES`] long.
    pub tranches: Vec<Tranche>,
    /// Amount already claimed by the beneficiary.
    pub claimed: i128,
    /// Current lifecycle state.
    pub status: VestingStatus,
}

/// Instance-storage keys.
///
/// Both schedule kinds are instance-only entries (see the persistent-storage
/// migration tracked in issue #55) keyed by the same id, so the variants
/// partition the id space: an id addresses a linear record or a tranche
/// record, never both.
#[contracttype]
enum DataKey {
    /// The linear vesting record for `u64` id.
    Schedule(u64),
    /// The tranche vesting record for `u64` id, unlock table included.
    TrancheSchedule(u64),
    /// Monotonic id counter, shared by both kinds.
    Count,
}

/// A stored schedule of either kind, as returned by [`Vesting::load`].
enum Stored {
    /// A linear (cliff + ramp) schedule.
    Linear(VestingSchedule),
    /// A tranche (discrete unlock table) schedule.
    Tranche(TrancheSchedule),
}

/// The deployable vesting contract.
#[contract]
pub struct Vesting;

#[contractimpl]
impl Vesting {
    /// Create a new vesting schedule and return its stable id.
    ///
    /// Requires `total_amount > 0`, `duration > 0`, and `cliff <= duration`.
    /// The beneficiary is authorized at creation time.
    pub fn create_schedule(
        env: Env,
        beneficiary: Address,
        token: Address,
        total_amount: i128,
        cliff: u64,
        duration: u64,
    ) -> Result<u64, ForgeError> {
        if total_amount <= 0 {
            return Err(ForgeError::InvalidInput);
        }
        if duration == 0 {
            return Err(ForgeError::InvalidInput);
        }
        if cliff > duration {
            return Err(ForgeError::InvalidInput);
        }
        beneficiary.require_auth();

        let id = Self::next_id(&env)?;
        let start = env.ledger().timestamp();
        let mut schedule = VestingSchedule {
            beneficiary,
            token,
            total_amount,
            start,
            cliff,
            duration,
            claimed: 0,
            status: VestingStatus::Locked,
        };
        // Derive the initial status from time (cliff == 0 starts `Vesting`).
        schedule.status = Self::current_status(&schedule, start)?;
        env.storage()
            .instance()
            .set(&DataKey::Schedule(id), &schedule);
        Ok(id)
    }

    /// Create a new tranche vesting schedule and return its stable id.
    ///
    /// `tranches` is the unlock table, ordered by strictly increasing
    /// `unlock_at` offsets in seconds from the creation timestamp. Requires a
    /// non-empty table of at most [`MAX_TRANCHES`] entries, every
    /// `amount > 0`, and a cumulative sum that fits in `i128`; the table is
    /// then stored once and treated as immutable. The beneficiary is
    /// authorized at creation time, mirroring `create_schedule`.
    ///
    /// A first tranche at `unlock_at == 0` is allowed: it unlocks at the
    /// creation timestamp, the "TGE tranche" of a typical grant agreement.
    pub fn create_tranche_schedule(
        env: Env,
        beneficiary: Address,
        token: Address,
        tranches: Vec<Tranche>,
    ) -> Result<u64, ForgeError> {
        let total_amount = Self::validate_tranches(&tranches)?;
        beneficiary.require_auth();

        let id = Self::next_id(&env)?;
        let start = env.ledger().timestamp();
        let mut schedule = TrancheSchedule {
            beneficiary,
            token,
            total_amount,
            start,
            tranches,
            claimed: 0,
            status: VestingStatus::Locked,
        };
        // Derive the initial status from time (a table starting at 0 begins
        // `Vesting`, mirroring the linear `cliff == 0` case).
        schedule.status = Self::tranche_status(&schedule, start)?;
        env.storage()
            .instance()
            .set(&DataKey::TrancheSchedule(id), &schedule);
        Ok(id)
    }

    /// Read the full tranche schedule record, unlock table included
    /// (read-only view; no state change).
    pub fn get_tranche_schedule(env: Env, schedule_id: u64) -> Result<TrancheSchedule, ForgeError> {
        env.storage()
            .instance()
            .get(&DataKey::TrancheSchedule(schedule_id))
            .ok_or(ForgeError::NotFound)
    }

    /// Claim the vested-but-unclaimed amount.
    ///
    /// Requires the beneficiary. Works for both schedule kinds — the id
    /// resolves to a linear or a tranche record and the matching math runs.
    /// Returns exactly what vested since the last claim (or `0` when nothing
    /// is claimable), so repeated claims can never overpay or underpay.
    ///
    /// Ordering: the SEP-41 transfer runs **before** the schedule write —
    /// see the module docs. A zero-claim call returns before either.
    pub fn claim(env: Env, schedule_id: u64) -> Result<i128, ForgeError> {
        let now = env.ledger().timestamp();
        match Self::load(&env, schedule_id)? {
            Stored::Linear(schedule) => Self::settle_linear(&env, schedule_id, schedule, now),
            Stored::Tranche(schedule) => Self::settle_tranche(&env, schedule_id, schedule, now),
        }
    }

    /// Amount currently claimable (read-only view; no state change).
    pub fn claimable(env: Env, schedule_id: u64) -> Result<i128, ForgeError> {
        let now = env.ledger().timestamp();
        match Self::load(&env, schedule_id)? {
            Stored::Linear(schedule) => Self::claimable_amount(&schedule, now),
            Stored::Tranche(schedule) => Self::tranche_claimable(&schedule, now),
        }
    }

    /// Read the current lifecycle status (read-only view).
    ///
    /// The status is derived from the ledger time and claimed amount rather
    /// than the stored field, so it is always current between claims.
    pub fn get_status(env: Env, schedule_id: u64) -> Result<VestingStatus, ForgeError> {
        let now = env.ledger().timestamp();
        match Self::load(&env, schedule_id)? {
            Stored::Linear(schedule) => Self::current_status(&schedule, now),
            Stored::Tranche(schedule) => Self::tranche_status(&schedule, now),
        }
    }

    /// Load a schedule of either kind by id.
    ///
    /// The two kinds occupy distinct [`DataKey`] variants under one id
    /// counter, so the linear read is attempted first and a tranche id simply
    /// falls through to the tranche read.
    fn load(env: &Env, schedule_id: u64) -> Result<Stored, ForgeError> {
        if let Some(schedule) = env
            .storage()
            .instance()
            .get(&DataKey::Schedule(schedule_id))
        {
            return Ok(Stored::Linear(schedule));
        }
        if let Some(schedule) = env
            .storage()
            .instance()
            .get(&DataKey::TrancheSchedule(schedule_id))
        {
            return Ok(Stored::Tranche(schedule));
        }
        Err(ForgeError::NotFound)
    }

    /// Settle a claim against a linear schedule: transfer-before-state, then
    /// record `claimed` and the derived status.
    fn settle_linear(
        env: &Env,
        schedule_id: u64,
        mut schedule: VestingSchedule,
        now: u64,
    ) -> Result<i128, ForgeError> {
        let amount = Self::claimable_amount(&schedule, now)?;
        if !Self::authorize_and_pay(env, &schedule.beneficiary, &schedule.token, amount)? {
            return Ok(0);
        }
        schedule.claimed = schedule
            .claimed
            .checked_add(amount)
            .ok_or(ForgeError::ArithmeticOverflow)?;
        schedule.status = Self::current_status(&schedule, now)?;
        env.storage()
            .instance()
            .set(&DataKey::Schedule(schedule_id), &schedule);
        Ok(amount)
    }

    /// Settle a claim against a tranche schedule: the same
    /// transfer-before-state discipline as [`Self::settle_linear`], over the
    /// unlock table's step function.
    fn settle_tranche(
        env: &Env,
        schedule_id: u64,
        mut schedule: TrancheSchedule,
        now: u64,
    ) -> Result<i128, ForgeError> {
        let amount = Self::tranche_claimable(&schedule, now)?;
        if !Self::authorize_and_pay(env, &schedule.beneficiary, &schedule.token, amount)? {
            return Ok(0);
        }
        schedule.claimed = schedule
            .claimed
            .checked_add(amount)
            .ok_or(ForgeError::ArithmeticOverflow)?;
        schedule.status = Self::tranche_status(&schedule, now)?;
        env.storage()
            .instance()
            .set(&DataKey::TrancheSchedule(schedule_id), &schedule);
        Ok(amount)
    }

    /// Authorize the beneficiary, then pay `amount` from this contract.
    ///
    /// Returns `Ok(true)` when a transfer ran and `Ok(false)` when `amount` is
    /// zero, so a zero claim exits without ever issuing an empty transfer.
    /// The payment happens here — before the caller's state write — so a
    /// failed transfer leaves the schedule's `claimed`/`status` untouched.
    fn authorize_and_pay(
        env: &Env,
        beneficiary: &Address,
        token: &Address,
        amount: i128,
    ) -> Result<bool, ForgeError> {
        // NOTE: when a revocation method lands, the claim paths must be
        // gated on `status != Revoked`; the status is currently unreachable.
        beneficiary.require_auth();
        if amount == 0 {
            return Ok(false);
        }
        transfer_from_contract(env, token, beneficiary, amount)?;
        Ok(true)
    }

    /// Validate an unlock table and return the sum of its amounts.
    ///
    /// Rejects an empty table, one longer than [`MAX_TRANCHES`], a
    /// non-positive amount, and an offset that does not strictly increase
    /// (a repeat or a rewind), and checks the cumulative sum so a table
    /// totalling past `i128::MAX` never reaches storage. The first offset is
    /// unconstrained; only the ordering between entries is.
    fn validate_tranches(tranches: &Vec<Tranche>) -> Result<i128, ForgeError> {
        if tranches.is_empty() || tranches.len() > MAX_TRANCHES {
            return Err(ForgeError::InvalidInput);
        }
        let mut total: i128 = 0;
        let mut previous_unlock: Option<u64> = None;
        for tranche in tranches.iter() {
            if tranche.amount <= 0 {
                return Err(ForgeError::InvalidInput);
            }
            if let Some(previous) = previous_unlock {
                if tranche.unlock_at <= previous {
                    return Err(ForgeError::InvalidInput);
                }
            }
            previous_unlock = Some(tranche.unlock_at);
            total = total
                .checked_add(tranche.amount)
                .ok_or(ForgeError::ArithmeticOverflow)?;
        }
        Ok(total)
    }

    /// Allocate the next monotonic schedule id.
    fn next_id(env: &Env) -> Result<u64, ForgeError> {
        let count: u64 = env.storage().instance().get(&DataKey::Count).unwrap_or(0);
        let id = count.checked_add(1).ok_or(ForgeError::ArithmeticOverflow)?;
        env.storage().instance().set(&DataKey::Count, &id);
        Ok(id)
    }

    /// Derive the lifecycle status from ledger time and claimed amount.
    fn current_status(schedule: &VestingSchedule, now: u64) -> Result<VestingStatus, ForgeError> {
        if schedule.claimed >= schedule.total_amount {
            return Ok(VestingStatus::Completed);
        }
        let cliff_time = schedule
            .start
            .checked_add(schedule.cliff)
            .ok_or(ForgeError::ArithmeticOverflow)?;
        if now < cliff_time {
            return Ok(VestingStatus::Locked);
        }
        Ok(VestingStatus::Vesting)
    }

    /// Vested amount at ledger time `now`, using floor division so claims
    /// never round up.
    fn vested_amount(schedule: &VestingSchedule, now: u64) -> Result<i128, ForgeError> {
        let cliff_time = schedule
            .start
            .checked_add(schedule.cliff)
            .ok_or(ForgeError::ArithmeticOverflow)?;
        if now < cliff_time {
            return Ok(0);
        }
        let end_time = schedule
            .start
            .checked_add(schedule.duration)
            .ok_or(ForgeError::ArithmeticOverflow)?;
        if now >= end_time {
            return Ok(schedule.total_amount);
        }

        // `cliff <= duration` is enforced at creation, so the period is
        // non-negative; a zero period (cliff == duration) means everything
        // vests at once, which the `now >= end_time` branch above already
        // returned. Guard defensively against division by zero.
        let period = end_time - cliff_time;
        if period == 0 {
            return Ok(schedule.total_amount);
        }
        let elapsed = now - cliff_time;
        let vested = schedule
            .total_amount
            .checked_mul(elapsed as i128)
            .ok_or(ForgeError::ArithmeticOverflow)?
            / period as i128;
        Ok(vested)
    }

    /// Claimable amount at ledger time `now` (vested minus claimed).
    fn claimable_amount(schedule: &VestingSchedule, now: u64) -> Result<i128, ForgeError> {
        let vested = Self::vested_amount(schedule, now)?;
        // By construction `claimed` never exceeds `vested`, so the subtraction
        // cannot underflow; use checked arithmetic to fail loudly if the
        // invariant is ever broken.
        vested
            .checked_sub(schedule.claimed)
            .ok_or(ForgeError::ArithmeticOverflow)
    }

    /// Seconds elapsed since `start`, saturating at zero.
    ///
    /// Tranche progress is compared on this offset instead of on
    /// `start + unlock_at`, which keeps a `u64::MAX` offset from overflowing:
    /// the tranche is simply never reached. A ledger timestamp before `start`
    /// yields `0`, i.e. nothing unlocked — the conservative direction.
    fn elapsed_since(start: u64, now: u64) -> u64 {
        now.saturating_sub(start)
    }

    /// Derive the lifecycle status of a tranche schedule.
    ///
    /// Same two inputs as the linear kind — ledger time and claimed amount —
    /// with the first unlock standing in for the cliff: `Locked` before it,
    /// `Vesting` from it until the final amount is claimed, `Completed` once
    /// `claimed == total_amount`.
    fn tranche_status(schedule: &TrancheSchedule, now: u64) -> Result<VestingStatus, ForgeError> {
        if schedule.claimed >= schedule.total_amount {
            return Ok(VestingStatus::Completed);
        }
        // Creation rejects an empty table, so the first tranche is always
        // there; the arm is defensive only.
        let Some(first) = schedule.tranches.get(0) else {
            return Ok(VestingStatus::Locked);
        };
        if Self::elapsed_since(schedule.start, now) < first.unlock_at {
            return Ok(VestingStatus::Locked);
        }
        Ok(VestingStatus::Vesting)
    }

    /// Unlocked amount of a tranche schedule at ledger time `now`: the sum of
    /// every tranche whose offset has elapsed.
    ///
    /// The scan stops at the first tranche that has not unlocked yet, which is
    /// exact because creation guarantees strictly increasing offsets (and
    /// bounded because the table is capped at [`MAX_TRANCHES`]). The
    /// cumulative sum is checked even though creation already proved it fits
    /// in `i128`, so a tampered record fails loudly instead of wrapping.
    fn tranche_unlocked(schedule: &TrancheSchedule, now: u64) -> Result<i128, ForgeError> {
        let elapsed = Self::elapsed_since(schedule.start, now);
        let mut total: i128 = 0;
        for tranche in schedule.tranches.iter() {
            if tranche.unlock_at > elapsed {
                break;
            }
            total = total
                .checked_add(tranche.amount)
                .ok_or(ForgeError::ArithmeticOverflow)?;
        }
        Ok(total)
    }

    /// Claimable amount of a tranche schedule at ledger time `now` (unlocked
    /// minus claimed).
    fn tranche_claimable(schedule: &TrancheSchedule, now: u64) -> Result<i128, ForgeError> {
        let unlocked = Self::tranche_unlocked(schedule, now)?;
        // `unlocked` is monotonic in `now` and `claimed` only ever rises to a
        // previously unlocked value, so the subtraction cannot underflow; use
        // checked arithmetic to fail loudly if the invariant is ever broken.
        unlocked
            .checked_sub(schedule.claimed)
            .ok_or(ForgeError::ArithmeticOverflow)
    }
}

/// Move `amount` of `token` from this contract to `to`.
///
/// Token failures are bucketed into [`ForgeError::TokenTransferFailed`]
/// rather than forwarded — the same policy as escrow: a client receiving
/// `Error(Contract, #N)` cannot know whether `N` came from the token or this
/// contract, and the root cause remains visible in the transaction's
/// diagnostic events.
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
        // Token returned a typed error (insufficient balance, custom token
        // logic) or the host aborted (most commonly an undeployed token
        // address). The raw discriminant is intentionally discarded.
        _ => Err(ForgeError::TokenTransferFailed),
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod authz;

#[cfg(test)]
mod props;
