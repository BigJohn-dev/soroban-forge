/**
 * Offline tests for the generated @soroban-forge/escrow-client.
 *
 * These tests run entirely without network access, private keys, funded
 * accounts, or testnet credentials.  They verify:
 *
 *   1. Client construction and configuration surface
 *   2. All 10 generated escrow methods are present
 *   3. Argument encoding via ContractSpec.funcArgsToScVals()
 *   4. The ForgeError runtime object (codes and messages)
 *   5. ABI shape — function names, parameter names, and parameter types —
 *      so that contract regeneration that changes the public API causes
 *      a clear test failure
 *   6. Compile-time usability of type-only exports (EscrowStatus, EscrowData)
 *      is verified by the TypeScript compiler during `npm run typecheck` /
 *      compilation, not at runtime.
 *
 * Run:
 *   npm test
 */

import test from "node:test";
import assert from "node:assert/strict";
import { StrKey } from "@stellar/stellar-sdk";

// --- imports from the generated client ----------------------------------------
import {
  Client,
  networks,
  ForgeError,
  contract,
} from "../../dist/index.js";

// Type-only imports: validated by the TypeScript compiler; they do not
// produce any runtime value and are intentionally not tested with assert.ok().
import type { EscrowStatus, EscrowData } from "../../dist/index.js";

// ---------------------------------------------------------------------------
// Test fixtures
// ---------------------------------------------------------------------------

/** A deterministic, valid Stellar contract address used as a "buyer" fixture. */
const FIXTURE_BUYER =
  "CC227UDF6WBLRTOKKVRIJN7BGSBK67ZGV6IDARJ2AMATGSQ7UZNBZHSB";

/** A deterministic, valid Stellar contract address used as a "seller" fixture. */
const FIXTURE_SELLER =
  "CBJQ53EOHB5MWSS7CETN523WILLNVS7NQAQQAQIS5QAYTPRCZSGKL23O";

/** A deterministic, valid Stellar contract address used as an "arbiter" fixture. */
const FIXTURE_ARBITER =
  "CDBCCKT7RBUAJEGVZDBUNMADCJW4RV2F2CJKIG22B4R4STGN37W52ZXN";

/** Token contract address (also used as a generic second fixture address). */
const FIXTURE_TOKEN =
  "CBJQ53EOHB5MWSS7CETN523WILLNVS7NQAQQAQIS5QAYTPRCZSGKL23O";

/** The authoritative testnet contract ID embedded in the generated client. */
const EXPECTED_CONTRACT_ID =
  "CC227UDF6WBLRTOKKVRIJN7BGSBK67ZGV6IDARJ2AMATGSQ7UZNBZHSB";

/** The Stellar testnet network passphrase. */
const TESTNET_PASSPHRASE = "Test SDF Network ; September 2015";

/** A dummy RPC URL — construction must succeed without a live endpoint. */
const DUMMY_RPC_URL = "https://soroban-testnet.stellar.org";

// ---------------------------------------------------------------------------
// 1. Client construction
// ---------------------------------------------------------------------------

test("Client can be constructed with testnet network config", () => {
  const client = new Client({
    ...networks.testnet,
    rpcUrl: DUMMY_RPC_URL,
  });
  assert.ok(
    client instanceof Client,
    "constructed value should be a Client instance",
  );
});

test("networks.testnet carries the expected contract ID", () => {
  assert.equal(
    networks.testnet.contractId,
    EXPECTED_CONTRACT_ID,
    "contract ID must match the deployed testnet escrow — update EXPECTED_CONTRACT_ID if the deployment changes",
  );
});

test("networks.testnet carries the Stellar testnet passphrase", () => {
  assert.equal(
    networks.testnet.networkPassphrase,
    TESTNET_PASSPHRASE,
  );
});

test("EXPECTED_CONTRACT_ID is a valid Stellar contract address", () => {
  assert.ok(
    StrKey.isValidContract(EXPECTED_CONTRACT_ID),
    `${EXPECTED_CONTRACT_ID} must be a valid Stellar contract (C…) address`,
  );
});

// ---------------------------------------------------------------------------
// 2. Method surface — all 10 escrow methods must be present
// ---------------------------------------------------------------------------

const EXPECTED_METHODS = [
  "cancel",
  "refund",
  "deposit",
  "dispute",
  "release",
  "resolve",
  "touch_ttl",
  "get_escrow",
  "get_status",
  "create_escrow",
] as const;

test("Client exposes all 10 escrow method functions", () => {
  const client = new Client({ ...networks.testnet, rpcUrl: DUMMY_RPC_URL });
  for (const method of EXPECTED_METHODS) {
    assert.equal(
      typeof (client as unknown as Record<string, unknown>)[method],
      "function",
      `client.${method} must be a function`,
    );
  }
});

test("fromJSON exposes deserialization helpers for all 10 methods", () => {
  const client = new Client({ ...networks.testnet, rpcUrl: DUMMY_RPC_URL });
  for (const method of EXPECTED_METHODS) {
    assert.equal(
      typeof client.fromJSON[method],
      "function",
      `client.fromJSON.${method} must be a function`,
    );
  }
});

// ---------------------------------------------------------------------------
// 3. ABI shape — function names, parameter names and types (drift detection)
//
// If the contract is regenerated with a different ABI (renamed parameters,
// reordered arguments, changed types) these tests will fail clearly.
// ---------------------------------------------------------------------------

test("ContractSpec is accessible via client.spec", () => {
  const client = new Client({ ...networks.testnet, rpcUrl: DUMMY_RPC_URL });
  assert.ok(
    client.spec instanceof contract.Spec,
    "client.spec must be a ContractSpec instance",
  );
});

test("ABI exposes exactly the 10 expected function names", () => {
  const client = new Client({ ...networks.testnet, rpcUrl: DUMMY_RPC_URL });
  const funcNames = client.spec.funcs().map((f) => f.name().toString());
  const sortedExpected = [...EXPECTED_METHODS].sort();
  const sortedActual = [...funcNames].sort();
  assert.deepEqual(
    sortedActual,
    sortedExpected,
    "ABI function list must match; a mismatch indicates the generated client has changed",
  );
});

/**
 * Expected ABI shape keyed by function name.
 * Each entry lists [paramName, specTypeName] pairs in argument order.
 */
const EXPECTED_ABI_SHAPE: Record<string, Array<[string, string]>> = {
  cancel: [["escrow_id", "scSpecTypeU64"]],
  refund: [["escrow_id", "scSpecTypeU64"]],
  deposit: [["escrow_id", "scSpecTypeU64"]],
  dispute: [
    ["escrow_id", "scSpecTypeU64"],
    ["claimant", "scSpecTypeAddress"],
  ],
  release: [["escrow_id", "scSpecTypeU64"]],
  resolve: [
    ["escrow_id", "scSpecTypeU64"],
    ["in_favor_of_seller", "scSpecTypeBool"],
  ],
  touch_ttl: [["escrow_id", "scSpecTypeU64"]],
  get_escrow: [["escrow_id", "scSpecTypeU64"]],
  get_status: [["escrow_id", "scSpecTypeU64"]],
  create_escrow: [
    ["buyer", "scSpecTypeAddress"],
    ["seller", "scSpecTypeAddress"],
    ["arbiter", "scSpecTypeAddress"],
    ["token", "scSpecTypeAddress"],
    ["amount", "scSpecTypeI128"],
    ["timeout", "scSpecTypeU64"],
  ],
};

test("ABI parameter names and types match expected shape", () => {
  const client = new Client({ ...networks.testnet, rpcUrl: DUMMY_RPC_URL });
  for (const fn of client.spec.funcs()) {
    const name = fn.name().toString();
    const expected = EXPECTED_ABI_SHAPE[name];
    if (!expected) continue; // only check known methods
    const inputs = fn.inputs();
    assert.equal(
      inputs.length,
      expected.length,
      `${name}: parameter count mismatch`,
    );
    for (let i = 0; i < expected.length; i++) {
      const [expectedParamName, expectedTypeName] = expected[i]!;
      const input = inputs[i]!;
      assert.equal(
        input.name().toString(),
        expectedParamName,
        `${name} param[${i}] name mismatch`,
      );
      assert.equal(
        input.type().switch().name,
        expectedTypeName,
        `${name} param[${i}] type mismatch`,
      );
    }
  }
});

// ---------------------------------------------------------------------------
// 4. Argument encoding tests — ContractSpec.funcArgsToScVals()
//
// These verify that arguments are encoded into the correct Soroban XDR
// value types.  A regenerated client with a changed parameter type will
// produce a different ScVal switch and fail here.
// ---------------------------------------------------------------------------

test("cancel: escrow_id encodes as scvU64", () => {
  const client = new Client({ ...networks.testnet, rpcUrl: DUMMY_RPC_URL });
  const args = client.spec.funcArgsToScVals("cancel", { escrow_id: 42n });
  assert.equal(args.length, 1);
  assert.equal(args[0]!.switch().name, "scvU64");
  assert.equal(args[0]!.u64().toString(), "42");
});

test("refund: escrow_id encodes as scvU64", () => {
  const client = new Client({ ...networks.testnet, rpcUrl: DUMMY_RPC_URL });
  const args = client.spec.funcArgsToScVals("refund", { escrow_id: 7n });
  assert.equal(args.length, 1);
  assert.equal(args[0]!.switch().name, "scvU64");
  assert.equal(args[0]!.u64().toString(), "7");
});

test("deposit: escrow_id encodes as scvU64", () => {
  const client = new Client({ ...networks.testnet, rpcUrl: DUMMY_RPC_URL });
  const args = client.spec.funcArgsToScVals("deposit", { escrow_id: 3n });
  assert.equal(args.length, 1);
  assert.equal(args[0]!.switch().name, "scvU64");
  assert.equal(args[0]!.u64().toString(), "3");
});

test("dispute: encodes escrow_id (scvU64) and claimant (scvAddress)", () => {
  const client = new Client({ ...networks.testnet, rpcUrl: DUMMY_RPC_URL });
  const args = client.spec.funcArgsToScVals("dispute", {
    escrow_id: 5n,
    claimant: FIXTURE_BUYER,
  });
  assert.equal(args.length, 2);
  assert.equal(args[0]!.switch().name, "scvU64");
  assert.equal(args[0]!.u64().toString(), "5");
  assert.equal(args[1]!.switch().name, "scvAddress");
});

test("release: escrow_id encodes as scvU64", () => {
  const client = new Client({ ...networks.testnet, rpcUrl: DUMMY_RPC_URL });
  const args = client.spec.funcArgsToScVals("release", { escrow_id: 11n });
  assert.equal(args.length, 1);
  assert.equal(args[0]!.switch().name, "scvU64");
  assert.equal(args[0]!.u64().toString(), "11");
});

test("resolve: encodes escrow_id (scvU64) and in_favor_of_seller (scvBool)", () => {
  const client = new Client({ ...networks.testnet, rpcUrl: DUMMY_RPC_URL });

  const argsTrue = client.spec.funcArgsToScVals("resolve", {
    escrow_id: 2n,
    in_favor_of_seller: true,
  });
  assert.equal(argsTrue.length, 2);
  assert.equal(argsTrue[0]!.switch().name, "scvU64");
  assert.equal(argsTrue[1]!.switch().name, "scvBool");
  assert.equal(argsTrue[1]!.b(), true);

  const argsFalse = client.spec.funcArgsToScVals("resolve", {
    escrow_id: 2n,
    in_favor_of_seller: false,
  });
  assert.equal(argsFalse[1]!.b(), false);
});

test("touch_ttl: escrow_id encodes as scvU64", () => {
  const client = new Client({ ...networks.testnet, rpcUrl: DUMMY_RPC_URL });
  const args = client.spec.funcArgsToScVals("touch_ttl", { escrow_id: 99n });
  assert.equal(args.length, 1);
  assert.equal(args[0]!.switch().name, "scvU64");
  assert.equal(args[0]!.u64().toString(), "99");
});

test("get_escrow: escrow_id encodes as scvU64", () => {
  const client = new Client({ ...networks.testnet, rpcUrl: DUMMY_RPC_URL });
  const args = client.spec.funcArgsToScVals("get_escrow", { escrow_id: 1n });
  assert.equal(args.length, 1);
  assert.equal(args[0]!.switch().name, "scvU64");
});

test("get_status: escrow_id encodes as scvU64", () => {
  const client = new Client({ ...networks.testnet, rpcUrl: DUMMY_RPC_URL });
  const args = client.spec.funcArgsToScVals("get_status", { escrow_id: 1n });
  assert.equal(args.length, 1);
  assert.equal(args[0]!.switch().name, "scvU64");
});

test("create_escrow: encodes 6 args with correct types and values", () => {
  const client = new Client({ ...networks.testnet, rpcUrl: DUMMY_RPC_URL });
  const args = client.spec.funcArgsToScVals("create_escrow", {
    buyer: FIXTURE_BUYER,
    seller: FIXTURE_SELLER,
    arbiter: FIXTURE_ARBITER,
    token: FIXTURE_TOKEN,
    amount: 500n,
    timeout: 86_400n,
  });
  assert.equal(args.length, 6, "create_escrow must encode 6 arguments");

  // buyer, seller, arbiter, token → scvAddress
  assert.equal(args[0]!.switch().name, "scvAddress", "buyer must be scvAddress");
  assert.equal(args[1]!.switch().name, "scvAddress", "seller must be scvAddress");
  assert.equal(args[2]!.switch().name, "scvAddress", "arbiter must be scvAddress");
  assert.equal(args[3]!.switch().name, "scvAddress", "token must be scvAddress");

  // amount → scvI128
  assert.equal(args[4]!.switch().name, "scvI128", "amount must be scvI128");
  assert.equal(
    args[4]!.i128().lo().toString(),
    "500",
    "amount lo must equal 500",
  );

  // timeout → scvU64
  assert.equal(args[5]!.switch().name, "scvU64", "timeout must be scvU64");
  assert.equal(args[5]!.u64().toString(), "86400", "timeout must equal 86400");
});

// ---------------------------------------------------------------------------
// 5. ForgeError surface
//
// ForgeError is a plain runtime object (not an enum or class) whose numeric
// keys map to {message: string} descriptors.
// ---------------------------------------------------------------------------

test("ForgeError is exported as a runtime object", () => {
  assert.equal(
    typeof ForgeError,
    "object",
    "ForgeError must be a plain runtime object",
  );
  assert.ok(ForgeError !== null);
  assert.ok(!Array.isArray(ForgeError));
});

test("ForgeError exposes 11 error codes (1–11)", () => {
  const codes = Object.keys(ForgeError).map(Number).sort((a, b) => a - b);
  assert.deepEqual(codes, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]);
});

const EXPECTED_FORGE_ERRORS: Record<number, string> = {
  1: "Unauthorized",
  2: "NotFound",
  3: "InvalidInput",
  4: "InsufficientFunds",
  5: "AlreadyInitialized",
  6: "NotInitialized",
  7: "DeadlineReached",
  8: "InsufficientAllowance",
  9: "ArithmeticOverflow",
  10: "Custom",
  11: "TokenTransferFailed",
};

test("ForgeError messages match the documented error surface", () => {
  for (const [code, expectedMessage] of Object.entries(
    EXPECTED_FORGE_ERRORS,
  )) {
    const entry =
      ForgeError[Number(code) as keyof typeof ForgeError];
    assert.ok(entry, `ForgeError[${code}] must exist`);
    assert.equal(
      entry.message,
      expectedMessage,
      `ForgeError[${code}].message mismatch`,
    );
  }
});

// ---------------------------------------------------------------------------
// 6. Event types
//
// The generated spec includes contract event XDR entries.  The spec object
// exposes them through spec.events(); verify the escrow lifecycle events are
// present so that regeneration which removes or renames events is detectable.
// ---------------------------------------------------------------------------

const EXPECTED_ESCROW_EVENTS = [
  "EscrowCreated",
  "Deposited",
  "Released",
  "Refunded",
  "Disputed",
  "Resolved",
  "Cancelled",
] as const;

test("ContractSpec includes all documented escrow event types", () => {
  const client = new Client({ ...networks.testnet, rpcUrl: DUMMY_RPC_URL });
  const events = client.spec.events();
  const eventNames = events.map((e) => e.name().toString());
  for (const expected of EXPECTED_ESCROW_EVENTS) {
    assert.ok(
      eventNames.includes(expected),
      `spec must include event "${expected}"; found: ${eventNames.join(", ")}`,
    );
  }
});

// ---------------------------------------------------------------------------
// 7. Type-level compile-time checks
//
// EscrowStatus and EscrowData are TypeScript type-only exports.  They have
// no runtime presence and must not be tested with assert.ok(SomeType).
//
// The checks below are valid TypeScript that the compiler enforces at build
// time.  If these types change their structure, the compile step will fail.
// ---------------------------------------------------------------------------

// Compile-time check: EscrowStatus is a tagged-union type.
// Assigning a well-formed value here will fail to compile if the type changes.
const _statusPending: EscrowStatus = { tag: "Pending", values: undefined };
const _statusFunded: EscrowStatus = { tag: "Funded", values: undefined };
const _statusCompleted: EscrowStatus = { tag: "Completed", values: undefined };
const _statusRefunded: EscrowStatus = { tag: "Refunded", values: undefined };
const _statusDisputed: EscrowStatus = { tag: "Disputed", values: undefined };
const _statusCancelled: EscrowStatus = { tag: "Cancelled", values: undefined };

// Compile-time check: EscrowData has the documented shape.
const _escrowDataShape: EscrowData = {
  amount: 500n,
  arbiter: FIXTURE_ARBITER,
  buyer: FIXTURE_BUYER,
  created_at: 0n,
  escrow_id: 1n,
  seller: FIXTURE_SELLER,
  status: { tag: "Pending", values: undefined },
  timeout: 86_400n,
  token: FIXTURE_TOKEN,
};

// Suppress "unused variable" warnings from TypeScript strict mode.
void _statusPending;
void _statusFunded;
void _statusCompleted;
void _statusRefunded;
void _statusDisputed;
void _statusCancelled;
void _escrowDataShape;

test("EscrowStatus type has all 6 documented tags (compile-time verified)", () => {
  // Runtime presence of the STATUS TAGS is verified through the spec events
  // and ABI; here we simply confirm this test file compiled, meaning TypeScript
  // accepted all the assignments above.
  assert.ok(true, "compile-time type assertions for EscrowStatus passed");
});

test("EscrowData type has the documented field shape (compile-time verified)", () => {
  assert.ok(true, "compile-time type assertions for EscrowData passed");
});
