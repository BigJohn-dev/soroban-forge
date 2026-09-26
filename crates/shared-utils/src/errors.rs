use soroban_sdk::contracterror;

/// Shared error type used across all Soroban Forge contracts.
///
/// Defining a single error enum in `shared-utils` keeps the on-chain error
/// space consistent and intelligible to SDK consumers, and avoids every
/// contract re-declaring the same failure modes. Contract crates may expose
/// their own domain-specific errors, but should prefer these where they fit.
///
/// Error codes start at 1; code 0 is reserved by the Soroban host.
#[contracterror]
#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum ForgeError {
    /// The caller is not permitted to perform this action.
    Unauthorized = 1,
    /// The requested entity (escrow, proposal, subscription, ...) does not exist.
    NotFound = 2,
    /// One or more arguments failed validation (e.g. zero amount, bad address).
    InvalidInput = 3,
    /// The contract does not hold enough balance to satisfy the operation.
    InsufficientFunds = 4,
    /// The entity was already initialised; re-initialisation is rejected.
    AlreadyInitialized = 5,
    /// The entity was expected to be initialised but was not.
    NotInitialized = 6,
    /// An operation was attempted after its deadline elapsed.
    DeadlineReached = 7,
    /// A required token allowance was lower than the amount being spent.
    InsufficientAllowance = 8,
    /// An arithmetic operation overflowed.
    ArithmeticOverflow = 9,
    /// Contract-specific error that does not map to the categories above.
    Custom = 10,
    /// A SEP-41 token invocation failed (insufficient balance, missing or
    /// deauthorized trustline, undeployed token contract, or token-logic
    /// rejection). The raw token error discriminant is intentionally not
    /// forwarded — callers cannot tell which contract produced a forwarded
    /// code, so it is bucketed; the root cause remains visible in the
    /// transaction's diagnostic events.
    TokenTransferFailed = 11,
    /// A cross-contract invocation failed (target reverted or host
    /// abort). The target reverts are surfaced here so the caller can
    /// distinguish them from token-transfer failures, and the invoking
    /// transaction is left un-executed.
    ContractInvocationFailed = 12,
    /// The requested withdrawal would push a token's rolling-window total
    /// past its configured withdrawal limit. Kept distinct from
    /// [`ForgeError::InvalidInput`] so a caller can tell a policy rejection
    /// (a valid withdrawal that is too large right now) from a malformed
    /// argument.
    WithdrawalLimitExceeded = 13,
}
