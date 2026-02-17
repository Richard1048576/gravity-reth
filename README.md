# gravity-reth

**Multi-chain EVM execution client** — a high-performance fork of [reth](https://github.com/paradigmxyz/reth) with RocksDB storage, skip-merklization, and multi-chain support.

Based on upstream reth v1.11.0 (paradigmxyz/reth).

## Supported Chains

| Binary | Chain | Chain ID | Status |
|--------|-------|----------|--------|
| `reth` | Ethereum Mainnet | 1 | Production |
| `bsc-reth` | BNB Smart Chain | 56 | Production |
| `base-reth` | Base (OP Stack) | 8453 | Production |
| `base-reth` | Optimism | 10 | Production |

## Key Features

### RocksDB Storage Backend

RocksDB replaces MDBX as the default storage engine, providing better write throughput and configurable memory usage.

- Feature-gated: `--features rocksdb` (default) or `--features mdbx`
- Single-DB architecture with column families per table
- DupSort support via composite keys (`key || compressed_value`)
- Configurable block cache (default 4GB) and write buffer (default 128MB)

### Skip-Merklization Mode

Bypass Merkle Patricia Trie state root computation for faster sync when trusting a remote consensus source (e.g., CL/bridge).

```bash
reth node --engine.skip-state-root
```

### Execution State Cache

In-memory hot state cache that keeps recently accessed state in a byte-budget-based LRU cache, reducing disk reads during block execution.

```bash
reth node --engine.execution-cache-max-bytes 17179869184  # 16 GB (default)
```

### Multi-Chain Architecture

Each chain has a dedicated binary with its own chain spec parser, node builder, and hardfork definitions. Shared infrastructure (RocksDB, caching, skip-merklization) is chain-agnostic.

## Build

Requires Rust toolchain 1.88.0+.

```bash
# Debug build (all binaries)
cargo build -p reth -p bsc-reth -p base-reth

# Release build with recommended features
cargo build --release -p reth --features "jemalloc asm-keccak min-debug-logs"
cargo build --release -p bsc-reth --features "jemalloc asm-keccak min-debug-logs"
cargo build --release -p base-reth --features "jemalloc asm-keccak min-debug-logs"

# Max performance build
RUSTFLAGS="-C target-cpu=native" cargo build --profile maxperf -p reth --features "jemalloc asm-keccak"
```

## Usage

### Ethereum

```bash
reth node \
    --chain mainnet \
    --datadir /data/ethereum \
    --engine.skip-state-root \
    --engine.execution-cache-max-bytes 17179869184
```

### BSC

```bash
bsc-reth node \
    --chain bsc \
    --datadir /data/bsc \
    --engine.skip-state-root \
    --engine.execution-cache-max-bytes 17179869184
```

### Base (OP Stack)

Base uses OP Stack architecture. The execution layer (base-reth) receives blocks via Engine API from an OP consensus layer (e.g., op-node).

```bash
base-reth node \
    --chain base \
    --datadir /data/base \
    --authrpc.jwtsecret /path/to/jwt.hex \
    --engine.skip-state-root \
    --engine.execution-cache-max-bytes 17179869184
```

Supported `--chain` values: `base`, `base-sepolia`, `optimism`, `op-sepolia`, `op-dev`, or a path to a custom genesis JSON.

## Project Structure

```
bin/
  reth/              # Ethereum mainnet binary
  bsc-reth/          # BSC binary
  base-reth/         # Base / OP Stack binary

crates/
  bsc/               # BSC-specific crates
    chainspec/       #   Chain spec (hardforks, chain ID 56)
    consensus/       #   Parlia PoSA consensus
    evm/             #   BSC EVM execution rules
    node/            #   BscNode builder
    primitives/      #   BSC transaction types
    storage/         #   BSC receipt codec

  optimism/          # OP Stack crates (Base, Optimism, etc.)
    hardforks/       #   OP hardfork definitions (Bedrock -> Jovian)
    chainspec/       #   Base/OP chain specs
    consensus/       #   OP beacon consensus
    evm/             #   OP EVM execution (deposit tx, L1 gas)
    node/            #   OpNode builder
    primitives/      #   Deposit transaction types
    payload/         #   Payload builder
    rpc/             #   OP-specific RPC extensions
    txpool/          #   OP transaction pool
    storage/         #   OP receipt codec
    flashblocks/     #   Flashblocks preconfirmation

  storage/db/        # Database backends
    src/implementation/
      mdbx/          #   MDBX backend (upstream default)
      rocksdb/       #   RocksDB backend (gravity default)

  engine/            # Consensus engine
    primitives/      #   Engine config (skip-state-root, execution cache)
```

## Feature Flags

| Flag | Default | Description |
|------|---------|-------------|
| `rocksdb` | On | RocksDB storage backend |
| `mdbx` | Off | MDBX storage backend (upstream default) |
| `jemalloc` | On | jemalloc memory allocator |
| `asm-keccak` | Off | Assembly-optimized keccak256 |
| `min-debug-logs` | On | Limit log verbosity in release builds |

## Upstream Tracking

This fork tracks [paradigmxyz/reth](https://github.com/paradigmxyz/reth) `main` branch. The OP Stack crates are vendored from reth's git history (pre-removal in commit `372802d06d`) and maintained in-tree.

### Version History

| Tag | Based On | Changes |
|-----|----------|---------|
| `gravity-v1.0` | reth v1.11.0 | RocksDB, skip-merklization, execution cache, BSC support, Base/OP Stack support |

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option.
