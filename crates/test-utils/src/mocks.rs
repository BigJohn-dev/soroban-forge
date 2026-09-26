use soroban_sdk::testutils::Address as _;
use soroban_sdk::{contracttype, Address, Env, Vec};

/// A fixed set of distinct mock accounts for use in tests.
///
/// Addresses are generated from the provided [`Env`] and are stable for the
/// lifetime of that environment, giving tests readable, named participants
/// (deployer, users, validator, arbiter) without hard-coding addresses.
#[contracttype]
pub struct TestAccounts {
    /// Account that deploys contracts and funds operations.
    pub deployer: Address,
    /// First regular user.
    pub user1: Address,
    /// Second regular user.
    pub user2: Address,
    /// Third regular user.
    pub user3: Address,
    /// A validator / signer role.
    pub validator: Address,
    /// A neutral arbiter role.
    pub arbiter: Address,
}

impl TestAccounts {
    /// Generate a fresh set of distinct mock addresses from `env`.
    pub fn generate(env: &Env) -> Self {
        TestAccounts {
            deployer: Address::generate(env),
            user1: Address::generate(env),
            user2: Address::generate(env),
            user3: Address::generate(env),
            validator: Address::generate(env),
            arbiter: Address::generate(env),
        }
    }

    /// All six accounts as a [`Vec`], useful for multi-party setup.
    pub fn all(&self, env: &Env) -> Vec<Address> {
        let mut v = Vec::new(env);
        v.push_back(self.deployer.clone());
        v.push_back(self.user1.clone());
        v.push_back(self.user2.clone());
        v.push_back(self.user3.clone());
        v.push_back(self.validator.clone());
        v.push_back(self.arbiter.clone());
        v
    }
}

/// Mock target contract for testing cross-contract invocations.
///
/// Records the execution count and last dispatched payload in instance storage.
#[soroban_sdk::contract]
pub struct MockTarget;

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MockTargetKey {
    Count,
    LastPayload,
}

#[soroban_sdk::contractimpl]
impl MockTarget {
    /// Dispatched by contracts executing actions via cross-contract calls.
    pub fn execute(env: Env, payload: soroban_sdk::Bytes) {
        let count: u32 = env
            .storage()
            .instance()
            .get(&MockTargetKey::Count)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&MockTargetKey::Count, &(count + 1));
        env.storage()
            .instance()
            .set(&MockTargetKey::LastPayload, &payload);
    }

    /// Read the number of times `execute` was called.
    pub fn count(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&MockTargetKey::Count)
            .unwrap_or(0)
    }

    /// Read the last payload dispatched to `execute`.
    pub fn last_payload(env: Env) -> Option<soroban_sdk::Bytes> {
        env.storage().instance().get(&MockTargetKey::LastPayload)
    }
}

/// A mock target whose `execute` panics, simulating a target revert.
#[soroban_sdk::contract]
pub struct RevertingTarget;

#[soroban_sdk::contractimpl]
impl RevertingTarget {
    /// Panics on invocation, simulating a target revert.
    pub fn execute(_env: Env, _payload: soroban_sdk::Bytes) {
        panic!("target reverted");
    }
}
