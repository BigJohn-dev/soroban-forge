# @soroban-forge/escrow-client

Generated TypeScript client for the **Soroban Forge escrow contract**, live on
Stellar testnet:

> `CC227UDF6WBLRTOKKVRIJN7BGSBK67ZGV6IDARJ2AMATGSQ7UZNBZHSB`

The client is generated from the deployed contract's ABI by the Stellar CLI, so
every method is fully typed and carries the doc comments from the contract
source.

## Install

```bash
npm install
```

## Build

Compile the TypeScript source to `dist/`:

```bash
npm run build
```

The compiled output is what consumers import.  The `dist/` directory is
published; the `src/` source is authoritative.

## Usage

```ts
import { Client, networks } from "@soroban-forge/escrow-client";

const client = new Client({
  ...networks.testnet,
  rpcUrl: "https://soroban-testnet.stellar.org", // or your own RPC
});

// Every contract method is available, typed, with `try*` variants:
const id = await client.create_escrow({
  buyer: "C…",
  seller: "C…",
  arbiter: "C…",
  token: "C…",
  amount: 500n,
  timeout: 86_400n,
});

await client.deposit({ escrow_id: id.result });
await client.release({ escrow_id: id.result });
await client.dispute({ escrow_id: id.result, claimant: seller });
await client.resolve({ escrow_id: id.result, in_favor_of_seller: true });
await client.get_status({ escrow_id: id.result });
await client.touch_ttl({ escrow_id: id.result }); // permissionless keeper
```

The `networks` export carries the embedded `contractId` and network passphrase;
pass your own `rpcUrl`.  Signers/wallets are supplied per call via
`MethodOptions` (`sign`, `simulate`, etc.) — see the
[stellar-sdk contract client docs](https://stellar.github.io/js-stellar-sdk/).

## Offline tests

The test suite runs completely offline.  It does **not** require:

- network access
- private keys
- funded accounts
- testnet credentials

```bash
cd packages/typescript-sdk
npm ci
npm test
```

The test script compiles TypeScript first (`tsconfig.test.json` → `dist-test/`)
and then runs the compiled JavaScript with Node's built-in test runner.

## Type-check

Run the TypeScript compiler in check-only mode (no output emitted):

```bash
npm run typecheck
```

## Client regeneration

The client (`src/index.ts`) is **generated** from the deployed contract's ABI
and must not be edited by hand.  Manual edits will be overwritten the next time
the contract interface changes.

To regenerate after a contract interface change, run:

```bash
stellar contract bindings typescript \
  --contract-id CC227UDF6WBLRTOKKVRIJN7BGSBK67ZGV6IDARJ2AMATGSQ7UZNBZHSB \
  --network testnet \
  --output-dir packages/typescript-sdk --overwrite
```

This requires the [Stellar CLI](https://developers.stellar.org/docs/tools/developer-tools/cli/stellar-cli)
and a live connection to Stellar testnet.  After regeneration:

1. Rebuild: `npm run build`
2. Run the offline tests: `npm test` — any ABI-breaking change will cause a
   test failure that makes the drift visible before merging.

The generation command, the deployed contract ID, and the testnet passphrase are
the authoritative source of truth.  If the contract is redeployed at a new
address, update the `--contract-id` flag above and the `networks.testnet`
configuration in the regenerated `src/index.ts`.

## Optional testnet verification

A separate, **opt-in** script performs one live read-only call (`get_status`)
against the deployed testnet contract to confirm end-to-end connectivity.

This is **not** part of `npm test`.  Run it only when you have network access
and a valid escrow ID:

```bash
# 1. Build (if not already done)
npm run build

# 2. Run the verification (ESCROW_ID must be a u64 that exists on-chain)
ESCROW_ID=1 npm run verify:testnet

# Optional: use a custom RPC endpoint
RPC_URL=https://soroban-testnet.stellar.org ESCROW_ID=1 npm run verify:testnet
```

The `verify:testnet` command builds the distribution automatically before running.

Requirements:

- `ESCROW_ID` — a valid escrow ID that exists on the deployed testnet contract
- `RPC_URL` — optional; defaults to `https://soroban-testnet.stellar.org`
- No private keys or funded accounts are needed (`get_status` is read-only)

The script exits with code `0` on success and non-zero on failure, making it
suitable as a post-deployment smoke check in a manual release workflow.

## Provenance

This package replaces the v0.1.0 console-log placeholder SDK.  The contract
itself, its testnet receipt rounds, and the conservation property are
documented in the [repository README](https://github.com/Meet-hybrid/soroban-forge).
