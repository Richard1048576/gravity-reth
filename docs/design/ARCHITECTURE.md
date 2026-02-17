# gravity-reth Architecture

This document describes the architecture of gravity-reth, a multi-chain EVM execution client forked from [reth](https://github.com/paradigmxyz/reth) v1.11.0.

## Overview

gravity-reth extends upstream reth with three pillars:

1. **RocksDB storage backend** — replacing MDBX for better write throughput
2. **Skip-merklization mode** — bypass state root computation for faster sync
3. **Multi-chain support** — BSC and OP Stack (Base/Optimism) alongside Ethereum

All three features are additive and do not break the upstream MDBX/Ethereum code path.

## Storage: RocksDB Backend

### Motivation

MDBX (the upstream default) uses memory-mapped I/O and has a single-writer model. RocksDB provides:
- Configurable block cache and write buffers (independent of OS page cache)
- Better write amplification characteristics for large state
- Column family isolation for concurrent access patterns

### Implementation

Location: `crates/storage/db/src/implementation/rocksdb/`

```
rocksdb/
  mod.rs      — DatabaseEnv, DatabaseArguments, Database trait impl
  tx.rs       — Tx<RO> / Tx<RW> with WriteBatch semantics
  cursor.rs   — Cursor implementing DbCursorRO, DbCursorRW, DbDupCursorRO, DbDupCursorRW
```

**Key design decisions:**

- **Single-DB architecture**: All 27 tables (defined in `reth-db-api`) map to RocksDB column families within a single database instance. No sharding.

- **DupSort via composite keys**: Tables with `DupSort` semantics store entries as `key || compressed_value`. This avoids modifying upstream Compress/Decompress traits. The key prefix length is derived from `size_of::<<T::Key as Encode>::Encoded>()`, which works for all fixed-size encoded key types.

- **Feature gating**: The `rocksdb` feature in `reth-db` controls compilation. Default features include `rocksdb`; the MDBX path remains functional via `--features mdbx`.

- **Iterator lifetime**: RocksDB iterators borrow the DB instance. We use `Arc<DB>` and transmute iterator lifetimes to `'static`, justified by the Arc keeping the DB alive for the transaction's lifetime.

### Configuration

| Parameter | Default | CLI Flag |
|-----------|---------|----------|
| Block cache | 4 GB | `--db.block-cache-size` |
| Write buffer | 128 MB | `--db.write-buffer-size` |
| Max open files | 512 | `--db.max-open-files` |

## Skip-Merklization Mode

### Motivation

State root computation (Merkle Patricia Trie updates) is the most CPU-intensive part of block processing. When syncing from a trusted source (consensus layer, bridge), the state root can be skipped.

### Implementation

Flag: `--engine.skip-state-root`

When enabled:
1. Block execution proceeds normally (all state transitions applied)
2. State root computation is skipped — the block's claimed state root is trusted
3. Trie-related storage writes (account trie, storage trie) are omitted
4. History indices and change sets are still written for RPC query support

This is controlled via `EngineConfig::skip_state_root` in `crates/engine/primitives/src/config.rs`.

## Execution State Cache

### Motivation

During block execution, the EVM frequently reads the same accounts and storage slots (hot state). A byte-budget-based in-memory cache reduces disk I/O.

### Implementation

Flag: `--engine.execution-cache-max-bytes` (default: 16 GB)

The cache operates as an LRU eviction cache sized by estimated memory consumption (not entry count). This ensures predictable memory usage regardless of value sizes.

Additional parameter:
- `--engine.execution-cache-max-persist-gap` (default: 64) — Maximum number of blocks between cache persistence checkpoints.

## Multi-Chain Architecture

### Design Principle

Each chain is a self-contained set of crates that implement chain-specific logic:

```
Chain Binary → Chain Node Builder → (EVM Config, Consensus, ChainSpec, Primitives, Storage)
```

The shared infrastructure (database, networking, RPC framework, engine) is chain-agnostic.

### Ethereum (default)

```
bin/reth → EthereumNode
  crates/ethereum/node/     — EthereumNode builder
  crates/ethereum/evm/      — EthEvmConfig
  crates/consensus/beacon/  — BeaconConsensus
  crates/chainspec/         — ChainSpec (mainnet, sepolia, holesky)
```

### BSC

```
bin/bsc-reth → BscNode
  crates/bsc/node/          — BscNode builder + BscChainSpecParser
  crates/bsc/evm/           — BscEvmConfig (system contract calls, gas rules)
  crates/bsc/consensus/     — Parlia PoSA consensus
  crates/bsc/chainspec/     — BscChainSpec (chain ID 56, BSC hardforks)
  crates/bsc/primitives/    — BSC-specific transaction types
  crates/bsc/storage/       — BSC receipt codec
```

BSC uses Parlia Proof-of-Staked-Authority consensus with system contract interactions (validator set management, slashing) executed at the EVM level.

### Base / OP Stack

```
bin/base-reth → OpNode
  crates/optimism/node/       — OpNode builder
  crates/optimism/evm/        — OpEvmConfig (deposit tx, L1 data gas)
  crates/optimism/consensus/  — OpBeaconConsensus
  crates/optimism/chainspec/  — OpChainSpec (Base, Optimism, testnets)
  crates/optimism/hardforks/  — OP hardfork definitions (Bedrock → Jovian)
  crates/optimism/primitives/ — Deposit transaction type
  crates/optimism/payload/    — OP payload builder
  crates/optimism/rpc/        — OP-specific RPC (sequencer forwarding, flashblocks)
  crates/optimism/txpool/     — OP transaction pool (DA size estimation, interop)
  crates/optimism/storage/    — OP receipt codec
  crates/optimism/flashblocks/ — Flashblocks preconfirmation support
```

OP Stack chains sync via Engine API. The execution layer (base-reth) receives blocks from an OP consensus layer (op-node) rather than P2P peer discovery.

**Vendoring strategy**: The OP crates were recovered from reth's git history (before removal in commit `372802d06d`) and updated for reth v1.11.0 API compatibility. Key API changes fixed:
- `spawn_blocking` / `spawn_critical` → `spawn_blocking_task` / `spawn_critical_task`
- `validate_transactions`: `Vec` → `impl IntoIterator`
- `send_transaction` gained `origin` parameter
- `CliComponentsBuilder` expects `(Evm, Consensus)` tuple

### Adding a New Chain

To add support for a new EVM chain:

1. Create crates under `crates/<chain>/` implementing:
   - `ChainSpec` — hardfork definitions, genesis, chain ID
   - `EvmConfig` — chain-specific EVM execution rules
   - `Consensus` — block validation logic
   - `Node` — node builder composing the above

2. Create a binary under `bin/<chain>-reth/` with a `ChainSpecParser` and entry point

3. Add workspace members and dependencies in root `Cargo.toml`

## Data Flow

```
Consensus Layer (CL)
        │
        │ Engine API (newPayload, forkchoiceUpdated)
        ▼
┌─────────────────┐
│  Consensus      │  Validate block header / body
│  Engine         │
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│  EVM Execution  │  Execute transactions, produce state changes
│  (chain-specific│
│   EvmConfig)    │
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│  State Root     │  Compute MPT root (or skip if --engine.skip-state-root)
│  (optional)     │
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│  Persistence    │  Write to RocksDB / MDBX + static files
│  Layer          │
└─────────────────┘
```

## Build Profiles

| Profile | Use Case | Command |
|---------|----------|---------|
| `dev` | Development / debugging | `cargo build -p reth` |
| `release` | Production | `cargo build --release -p reth --features "jemalloc asm-keccak min-debug-logs"` |
| `maxperf` | Maximum throughput | `RUSTFLAGS="-C target-cpu=native" cargo build --profile maxperf -p reth --features "jemalloc asm-keccak"` |
