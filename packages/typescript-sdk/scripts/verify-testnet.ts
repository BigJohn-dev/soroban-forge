/**
 * Optional testnet verification for the @soroban-forge/escrow-client.
 *
 * Performs a single read-only call (get_status) against the deployed escrow
 * contract on Stellar testnet to confirm the client can reach the network and
 * decode a real response.
 *
 * This script is NOT part of the normal offline test suite (npm test).
 * Run it explicitly when you need to verify live connectivity:
 *
 *   # 1. Compile (from packages/typescript-sdk/):
 *   npm run build
 *   npx tsc --project tsconfig.test.json
 *
 *   # 2. Run (requires a valid escrow_id that exists on testnet):
 *   ESCROW_ID=<u64> node dist-test/scripts/verify-testnet.js
 *
 *   # Optional: override the RPC endpoint (defaults to the public testnet URL):
 *   RPC_URL=https://soroban-testnet.stellar.org ESCROW_ID=1 node dist-test/scripts/verify-testnet.js
 *
 * Requirements:
 *   - Network access to Stellar testnet RPC
 *   - A valid ESCROW_ID environment variable (a u64 that exists on-chain)
 *   - No private keys or funded accounts are required (get_status is read-only)
 *
 * The script exits with code 0 on success and non-zero on any failure.
 */

import { Client, networks } from "../dist/index.js";

const DEFAULT_RPC_URL = "https://soroban-testnet.stellar.org";

function fail(message: string): never {
  console.error(`\n[verify-testnet] FAIL: ${message}\n`);
  process.exit(1);
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

const rpcUrl = process.env["RPC_URL"] ?? DEFAULT_RPC_URL;
const escrowIdStr = process.env["ESCROW_ID"];

if (!escrowIdStr) {
  fail(
    "ESCROW_ID environment variable is required.\n" +
      "  Example: ESCROW_ID=1 node dist-test/scripts/verify-testnet.js\n" +
      "  Provide a u64 escrow ID that exists on the deployed testnet contract.",
  );
}

let escrowId: bigint;
try {
  escrowId = BigInt(escrowIdStr);
} catch {
  fail(`ESCROW_ID "${escrowIdStr}" is not a valid integer.`);
}

console.log("[verify-testnet] Configuration:");
console.log(`  Contract ID : ${networks.testnet.contractId}`);
console.log(`  Network     : ${networks.testnet.networkPassphrase}`);
console.log(`  RPC URL     : ${rpcUrl}`);
console.log(`  Escrow ID   : ${escrowId}`);
console.log();

// ---------------------------------------------------------------------------
// Live read-only call
// ---------------------------------------------------------------------------

const client = new Client({
  ...networks.testnet,
  rpcUrl,
});

console.log("[verify-testnet] Calling get_status on testnet…");

try {
  const tx = await client.get_status({ escrow_id: escrowId });

  // tx.result is the decoded return value (a Result<EscrowStatus>)
  const result = tx.result;

  if (result.isErr()) {
    const errCode = result.unwrapErr();
    fail(
      `get_status returned a contract error: ${JSON.stringify(errCode)}\n` +
        "  The escrow may not exist — supply a valid ESCROW_ID.",
    );
  }

  const status = result.unwrap();
  console.log("[verify-testnet] SUCCESS");
  console.log(`  get_status(${escrowId}) → ${JSON.stringify(status)}`);
  console.log();
  console.log(
    "[verify-testnet] The generated client reached the testnet contract and decoded the response correctly.",
  );
} catch (err) {
  fail(
    `Unexpected error during get_status call:\n  ${String(err)}\n\n` +
      "  Possible causes:\n" +
      "    • No network access to the RPC endpoint\n" +
      "    • The escrow ID does not exist on-chain\n" +
      "    • The contract has been redeployed at a different ID\n" +
      "  Run `npm run build` first if the client is not compiled.",
  );
}
