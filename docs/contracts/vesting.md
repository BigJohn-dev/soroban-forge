# Vesting Contract

Time-locked token release in two shapes: a cliff plus linear release, or an
explicit table of unlock tranches.

## Interface

```rust
// linear (cliff + ramp)
fn create_schedule(beneficiary, token, total_amount, cliff, duration) -> Result<u64, ForgeError>
// tranche (discrete unlock table)
fn create_tranche_schedule(beneficiary, token, tranches: Vec<Tranche>) -> Result<u64, ForgeError>
fn get_tranche_schedule(schedule_id) -> Result<TrancheSchedule, ForgeError>
// both kinds
fn claim(schedule_id) -> Result<i128, ForgeError>
fn claimable(schedule_id) -> Result<i128, ForgeError>
fn get_status(schedule_id) -> Result<VestingStatus, ForgeError>
```

`Tranche { unlock_at: u64, amount: i128 }` is one entry of the unlock table:
`unlock_at` is an **offset in seconds from the schedule start** (the ledger
timestamp recorded at creation) and `amount` is what unlocks there.

Both kinds draw ids from one counter, so the id space is shared; the two
records live under distinct storage keys and a linear id is `NotFound` in
`get_tranche_schedule` (and vice versa).

## Timing

Timings in both shapes are **durations in seconds measured from the schedule
start**.

### Linear shape

```text
start ......... start+cliff ................... start+duration
  |             (claims become possible)        (fully vested)
  |  Locked    |            Vesting (linear)  |
```

- `start + cliff` — claims become possible;
- `start + duration` — the schedule is fully vested.

Validation at creation: `total_amount > 0`, `duration > 0`, `cliff <= duration`
(failures return `ForgeError::InvalidInput`).

### Tranche shape

```text
       t1            t2                t3
start  |             |                 |
  |    |   tranche 1  |   tranche 2     |   tranche 3
  | Locked  (a1)         (a2)              (a3)
  |    |  vested:      vested:           vested:
  |    |     0  ->     a1  ->            a1+a2  ->  total
  +----+--------------+------------------+--------->
```

A grant agreement of the form "25% at TGE, 25% at +6 months, 50% at +12
months" is a table, not a ramp, so this shape takes it verbatim. Validation at
creation:

- the table is non-empty and holds at most `MAX_TRANCHES` (32) entries;
- every `amount > 0`;
- `unlock_at` strictly increases (a repeat or a rewind is rejected);
- the amounts sum to at most `i128::MAX` (`ForgeError::ArithmeticOverflow`).

A first tranche at `unlock_at == 0` unlocks at the creation timestamp — the
"TGE tranche" of a typical agreement. The table is validated, stored once, and
immutable afterwards.

## Release Formula

Linear, at ledger time `t`:

```text
0                                            when t < start + cliff
total_amount * (t - (start + cliff)) / (duration - cliff)   otherwise, floored
total_amount                                 when t >= start + duration
```

Tranche, at ledger time `t`: the sum of every tranche whose offset has
elapsed, i.e. `0` before the first unlock, `a1` up to the second, `a1 + a2` up
to the third, and the full total from the last unlock on. Nothing accrues
between unlocks.

Integer (floor) division means a linear claim never rounds up, so repeated
claims can never overpay or underpay: for either kind `claim` returns exactly
`unlocked - claimed`, or `0` when nothing is claimable. Tranche progress is
compared on the elapsed offset rather than on `start + unlock_at`, so an offset
of `u64::MAX` cannot overflow — that tranche simply never unlocks.

## Status

Derived from ledger time and the claimed amount (always current between
claims): `Locked` before the first unlock (the cliff, for the linear kind),
`Vesting` from the first unlock until the last amount is claimed, `Completed`
once `claimed == total_amount`. `Revoked` is reserved for a follow-up
revocation method.

## Authorization

- `create_schedule` and `create_tranche_schedule` require the beneficiary.
- `claim` requires the beneficiary.
- `claimable`, `get_status`, and `get_tranche_schedule` are read-only views.

## Settlement

The contract custodies the SEP-41 token configured on the schedule, and
`claim` settles through it for both kinds:

- the newly claimable amount is transferred from the contract to the
  beneficiary **before** the schedule is written (escrow's
  transfer-before-state pattern); a failed transfer returns
  `ForgeError::TokenTransferFailed` with `claimed` and `status` unchanged;
- zero-claim calls (before the first unlock, or nothing newly unlocked) return
  `0` and issue **no** token transfer;
- floor-division residue on the linear kind is never lost: it stays claimable
  between claims and is paid out by the final claim.

Tests live in-crate (`crates/vesting/src/lib.rs`) and run with
`cargo test -p soroban-forge-vesting --all-targets --locked`.
