//! Ground-truth fixtures for the reference event indexer.
//!
//! This unit test drives the real escrow contract through every lifecycle
//! path in a fresh [`Env`], captures the contract events the contract
//! actually emits, and serialises them into
//! `indexer/fixtures/escrow-events.json` — the fixture consumed by the
//! read-only indexer in `packages/typescript-sdk` (see
//! `indexer/docs/event-schema.md`).
//!
//! The file is written before the assertion runs, so the committed fixture
//! is always kept fresh. The test then fails if the regeneration changed
//! anything, forcing a review of any event-schema drift: run
//! `cargo test -p soroban-forge-escrow indexer_fixtures` after touching the
//! contract and commit the updated fixture.
//!
//! Generation runs every scenario **twice** and asserts byte equality, which
//! pins the fixtures as deterministic — a property the indexer relies on for
//! restartable, idempotent re-syncs.

extern crate std;

use crate::{Escrow, SorobanForgeEscrowClient};
use soroban_sdk::address_payload::AddressPayload;
use soroban_sdk::testutils::{Address as _, Events as _, Ledger as _};
use soroban_sdk::token::StellarAssetClient;
use soroban_sdk::xdr::{self, ReadXdr, WriteXdr};
use soroban_sdk::{Address, Env};
use std::format;
use std::path::Path;
use std::string::{String, ToString};
use std::vec::Vec;

const START: u64 = 1_720_000_000;
const TIMEOUT: u64 = 1_000;
const AMOUNT: i128 = 1_000;

/// Freely-allocated r/w limits for serialising leaf `ScVal`s; the values the
/// host produced are smaller than this by construction.
const XDR_LIMITS: xdr::Limits = xdr::Limits {
    depth: 500,
    len: 0x1_000_000,
};

const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../indexer/fixtures");
const FIXTURE_FILE: &str = "escrow-events.json";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The 32-byte contract hash of a deployed contract address. Panics on an
/// account address, which callers never pass here.
fn contract_hash(addr: &Address) -> [u8; 32] {
    match addr.to_payload() {
        Some(AddressPayload::ContractIdHash(hash)) => hash.to_array(),
        _ => panic!("expected a contract address, got {}", addr.to_string()),
    }
}

/// A synthetic, deterministic ledger-close timestamp. The indexer only uses
/// it as an opaque string; a fixed date keeps the fixtures stable across any
/// wall-clock at generation time.
fn ledger_closed_at(ledger: u64) -> String {
    format!(
        "2026-09-23T00:{:02}:{:02}.000Z",
        (ledger / 60) % 60,
        ledger % 60
    )
}

/// Accumulates raw events in the same shape the Soroban RPC
/// [`getEvents`](https://developers.stellar.org/docs/data/events) endpoint
/// returns: base64 XDR for every topic and the value, plus ledger/position
/// metadata the indexer uses for ordering, deduplication and cursor work.
struct Builder {
    events: Vec<serde_json::Value>,
    /// Contract-id hash -> C-address strkey for the contracts the scenarios
    /// touch (escrow + SAC token).
    contracts: Vec<([u8; 32], String)>,
    escrow_label: String,
    /// Next intra-ledger slot, so ordering tests have a deterministic key.
    ledger_slots: std::collections::HashMap<u64, u64>,
}

impl Builder {
    fn new() -> Self {
        Builder {
            events: Vec::new(),
            contracts: Vec::new(),
            escrow_label: String::new(),
            ledger_slots: std::collections::HashMap::new(),
        }
    }

    fn set_escrow(&mut self, addr: &Address) {
        self.escrow_label = addr.to_string().to_string();
    }

    fn register_contract(&mut self, addr: &Address) {
        self.contracts
            .push((contract_hash(addr), addr.to_string().to_string()));
    }

    fn label_for(&self, id: Option<&xdr::ContractId>) -> String {
        id.and_then(|hash| {
            self.contracts
                .iter()
                .find(|(h, _)| h == &hash.0 .0)
                .map(|(_, label)| label.clone())
        })
        .unwrap_or_else(|| {
            id.map_or_else(
                || "C-----NONE".into(),
                |hash| format!("C-----UNKNOWN-{}", hex(&hash.0 .0)),
            )
        })
    }

    /// Run `f` (one contract invocation), capture every event that invocation
    /// emitted, and stamp each with the given ledger and transaction hash.
    ///
    /// Testutils host semantics: `env.events().all()` is the event record of
    /// the *most recently completed* invocation, so the value read after `f`
    /// runs is exactly that call's events — no cumulative diffing needed.
    /// Returns `f`'s value so callers can thread return values (escrow ids)
    /// through.
    fn step<T>(
        &mut self,
        env: &Env,
        escrow: &Address,
        ledger: u64,
        transaction_hash: &str,
        f: impl FnOnce() -> T,
    ) -> T {
        let escrow_hash = contract_hash(escrow);
        let result = f();
        let events = env.events().all().events().to_vec();
        let closed_at = ledger_closed_at(ledger);
        for (i, event) in events.iter().enumerate() {
            let xdr::ContractEventBody::V0(body) = &event.body;
            let slot = self.ledger_slots.entry(ledger).or_insert(0);
            let event_index = *slot;
            *slot += 1;
            let topics = body
                .topics
                .iter()
                .map(|t| t.to_xdr_base64(XDR_LIMITS).unwrap())
                .collect::<Vec<_>>();
            let value = body.data.to_xdr_base64(XDR_LIMITS).unwrap();
            let is_escrow = matches!(&event.contract_id, Some(id) if id.0 .0 == escrow_hash);
            let contract_id = if is_escrow {
                self.escrow_label.clone()
            } else {
                self.label_for(event.contract_id.as_ref())
            };
            self.events.push(serde_json::json!({
                "type": "contract",
                "ledger": ledger,
                "ledgerClosedAt": closed_at,
                "contractId": contract_id,
                "id": format!("{ledger}-{event_index}"),
                "pagingToken": format!("{ledger}-{i}"),
                "inSuccessfulContractCall": true,
                "eventIndex": event_index,
                "transactionHash": transaction_hash,
                "operationIndex": 0,
                "topic": topics,
                "value": value,
            }));
        }
        result
    }
}

/// A fully wired scenario: a mock-auth env containing the escrow contract
/// and a Stellar Asset Contract token. The buyer starts zeroed; each scenario
/// funds it inside its own first `step`.
fn rig() -> (Env, Address, Address) {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(START);

    let admin = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    let token = sac.address();

    let escrow = env.register(Escrow, ());
    (env, escrow, token)
}

/// Build the full fixture payload. Runs every scenario twice and asserts the
/// two generations are byte-identical (determinism guarantee).
fn build_fixture() -> serde_json::Value {
    let first = generate();
    let second = generate();
    assert_eq!(first, second, "fixture generation is not deterministic");
    first
}

fn generate() -> serde_json::Value {
    let mut b = Builder::new();
    let parties = |env: &Env| {
        (
            Address::generate(env),
            Address::generate(env),
            Address::generate(env),
        )
    };

    // Scenario A — create -> deposit -> dispute (buyer) -> resolve (seller wins).
    {
        let (env, escrow, token) = rig();
        let client = SorobanForgeEscrowClient::new(&env, &escrow);
        b.set_escrow(&escrow);
        b.register_contract(&escrow);
        b.register_contract(&token);
        let (buyer, seller, arbiter) = parties(&env);
        b.step(&env, &escrow, 1000, "0000000000001000", || {
            StellarAssetClient::new(&env, &token).mint(&buyer, &AMOUNT);
        });
        let id = b.step(&env, &escrow, 1001, "0000000000001001", || {
            client.create_escrow(&buyer, &seller, &arbiter, &token, &AMOUNT, &TIMEOUT)
        });
        b.step(&env, &escrow, 1002, "0000000000001002", || {
            client.deposit(&id);
        });
        b.step(&env, &escrow, 1003, "0000000000001003", || {
            client.dispute(&id, &buyer);
        });
        b.step(&env, &escrow, 1004, "0000000000001004", || {
            client.resolve(&id, &true);
        });
    }

    // Scenario B — create -> cancel (before funding).
    {
        let (env, escrow, token) = rig();
        let client = SorobanForgeEscrowClient::new(&env, &escrow);
        b.register_contract(&escrow);
        b.register_contract(&token);
        let (buyer, seller, arbiter) = parties(&env);
        b.step(&env, &escrow, 2000, "0000000000002000", || {
            StellarAssetClient::new(&env, &token).mint(&buyer, &AMOUNT);
        });
        let id = b.step(&env, &escrow, 2001, "0000000000002001", || {
            client.create_escrow(&buyer, &seller, &arbiter, &token, &AMOUNT, &TIMEOUT)
        });
        b.step(&env, &escrow, 2002, "0000000000002002", || {
            client.cancel(&id);
        });
    }

    // Scenario C — create -> deposit -> refund (seller, before the deadline).
    {
        let (env, escrow, token) = rig();
        let client = SorobanForgeEscrowClient::new(&env, &escrow);
        b.register_contract(&escrow);
        b.register_contract(&token);
        let (buyer, seller, arbiter) = parties(&env);
        b.step(&env, &escrow, 3000, "0000000000003000", || {
            StellarAssetClient::new(&env, &token).mint(&buyer, &AMOUNT);
        });
        let id = b.step(&env, &escrow, 3001, "0000000000003001", || {
            client.create_escrow(&buyer, &seller, &arbiter, &token, &AMOUNT, &TIMEOUT)
        });
        b.step(&env, &escrow, 3002, "0000000000003002", || {
            client.deposit(&id);
        });
        b.step(&env, &escrow, 3003, "0000000000003003", || {
            client.refund(&id);
        });
    }

    // Scenario D — create -> deposit -> dispute (seller) -> resolve (buyer wins).
    {
        let (env, escrow, token) = rig();
        let client = SorobanForgeEscrowClient::new(&env, &escrow);
        b.register_contract(&escrow);
        b.register_contract(&token);
        let (buyer, seller, arbiter) = parties(&env);
        b.step(&env, &escrow, 4000, "0000000000004000", || {
            StellarAssetClient::new(&env, &token).mint(&buyer, &AMOUNT);
        });
        let id = b.step(&env, &escrow, 4001, "0000000000004001", || {
            client.create_escrow(&buyer, &seller, &arbiter, &token, &AMOUNT, &TIMEOUT)
        });
        b.step(&env, &escrow, 4002, "0000000000004002", || {
            client.deposit(&id);
        });
        b.step(&env, &escrow, 4003, "0000000000004003", || {
            client.dispute(&id, &seller);
        });
        b.step(&env, &escrow, 4004, "0000000000004004", || {
            client.resolve(&id, &false);
        });
    }

    // Scenario E — create -> deposit -> release.
    {
        let (env, escrow, token) = rig();
        let client = SorobanForgeEscrowClient::new(&env, &escrow);
        b.register_contract(&escrow);
        b.register_contract(&token);
        let (buyer, seller, arbiter) = parties(&env);
        b.step(&env, &escrow, 5000, "0000000000005000", || {
            StellarAssetClient::new(&env, &token).mint(&buyer, &AMOUNT);
        });
        let id = b.step(&env, &escrow, 5001, "0000000000005001", || {
            client.create_escrow(&buyer, &seller, &arbiter, &token, &AMOUNT, &TIMEOUT)
        });
        b.step(&env, &escrow, 5002, "0000000000005002", || {
            client.deposit(&id);
        });
        b.step(&env, &escrow, 5003, "0000000000005003", || {
            client.release(&id);
        });
    }

    // Scenario F — create -> deposit -> refund (buyer, after the deadline).
    {
        let (env, escrow, token) = rig();
        let client = SorobanForgeEscrowClient::new(&env, &escrow);
        b.register_contract(&escrow);
        b.register_contract(&token);
        let (buyer, seller, arbiter) = parties(&env);
        b.step(&env, &escrow, 6000, "0000000000006000", || {
            StellarAssetClient::new(&env, &token).mint(&buyer, &AMOUNT);
        });
        let id = b.step(&env, &escrow, 6001, "0000000000006001", || {
            client.create_escrow(&buyer, &seller, &arbiter, &token, &AMOUNT, &TIMEOUT)
        });
        b.step(&env, &escrow, 6002, "0000000000006002", || {
            client.deposit(&id);
        });
        env.ledger().set_timestamp(START + TIMEOUT + 1);
        b.step(&env, &escrow, 6003, "0000000000006003", || {
            client.refund(&id);
        });
    }

    // Scenario G — create -> deposit -> release_partial (×2) -> release.
    {
        let (env, escrow, token) = rig();
        let client = SorobanForgeEscrowClient::new(&env, &escrow);
        b.register_contract(&escrow);
        b.register_contract(&token);
        let (buyer, seller, arbiter) = parties(&env);
        b.step(&env, &escrow, 7000, "0000000000007000", || {
            StellarAssetClient::new(&env, &token).mint(&buyer, &AMOUNT);
        });
        let id = b.step(&env, &escrow, 7001, "0000000000007001", || {
            client.create_escrow(&buyer, &seller, &arbiter, &token, &AMOUNT, &TIMEOUT)
        });
        b.step(&env, &escrow, 7002, "0000000000007002", || {
            client.deposit(&id);
        });
        b.step(&env, &escrow, 7003, "0000000000007003", || {
            client.release_partial(&id, &300);
        });
        b.step(&env, &escrow, 7004, "0000000000007004", || {
            client.release_partial(&id, &200);
        });
        b.step(&env, &escrow, 7005, "0000000000007005", || {
            client.release(&id);
        });
    }

    serde_json::json!({
        "schema": 1,
        "description":
            "Deterministic lifecycle fixtures emitted by the Soroban Forge escrow \
             contract (soroban-sdk 27 testutils). Generated and validated by \
             crates/escrow/src/indexer_fixtures.rs; consumed by the reference \
             indexer tests in packages/typescript-sdk.",
        "contract": {
            "name": "soroban-forge-escrow",
            "id": b.escrow_label.clone(),
        },
        "events": b.events,
    })
}

/// Verify the fixture is stable across two generations, covers the complete
/// lifecycle surface, and matches the committed file.
#[test]
fn indexer_fixtures_regenerate_and_are_stable() {
    let fixture = build_fixture();

    let expected = [
        ("escrow_created", 7u64),
        ("deposited", 6),
        ("released", 2),
        ("partially_released", 2),
        ("refunded", 2),
        ("disputed", 2),
        ("resolved", 2),
        ("cancelled", 1),
    ];
    let mut counts = std::collections::BTreeMap::<String, u64>::new();
    let mut escrow_events = 0u64;
    for event in fixture["events"].as_array().unwrap() {
        let topics = event["topic"].as_array().unwrap();
        let Some(mut topic0) = topics
            .first()
            .and_then(|t| t.as_str())
            .and_then(|b64| xdr::ScVal::from_xdr_base64(b64, XDR_LIMITS).ok())
        else {
            continue;
        };
        let xdr::ScVal::Symbol(sym) = &mut topic0 else {
            continue;
        };
        let name = sym.to_utf8_string().unwrap();
        if [
            "escrow_created",
            "deposited",
            "released",
            "partially_released",
            "refunded",
            "disputed",
            "resolved",
            "cancelled",
        ]
        .contains(&name.as_str())
        {
            escrow_events += 1;
        }
        *counts.entry(name).or_insert(0) += 1;
    }
    // Only the seven lifecycle events carry the escrow's contract id; foreign
    // token events (mint/transfer) stay out of the counts below.
    for (name, want) in expected {
        assert_eq!(
            counts.get(name),
            Some(&want),
            "expected {want} `{name}` events in the fixture, found {}",
            counts.get(name).copied().unwrap_or(0)
        );
    }
    assert_eq!(
        escrow_events,
        expected.iter().map(|(_, n)| n).sum::<u64>(),
        "every lifecycle event on the escrow contract must be captured"
    );

    let path = Path::new(FIXTURE_DIR).join(FIXTURE_FILE);
    std::fs::create_dir_all(FIXTURE_DIR).unwrap();
    let pretty = format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap());
    if std::fs::read_to_string(&path).ok().as_deref() != Some(pretty.as_str()) {
        std::fs::write(&path, &pretty).unwrap();
        panic!(
            "indexer fixture changed and was rewritten to {}; \
             review the diff and commit the updated fixture",
            path.display()
        );
    }
}
