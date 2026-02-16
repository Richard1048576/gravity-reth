# Custom Reth Client Design: Lightweight Knowledge-Graph Node

## 1. Overview

### 1.1 Motivation

Build a customized Reth client optimized for **on-chain data indexing and knowledge graph construction**, not for consensus participation or full validation. The client follows the chain via P2P or Engine API, maintains only the **latest world state** and **block-level data** (headers, transactions, receipts, logs), and discards historical state entirely.

### 1.2 Core Principles

| Principle | Description |
|-----------|-------------|
| **Minimal storage** | Only keep block data + latest state. No historical account/storage changesets. |
| **Trust-but-verify** | Accept remote state root, but independently verify receipt root and block hash to detect local divergence. |
| **Crash resilient** | Survive ungraceful shutdown with bounded recovery time. |
| **Performance first** | RocksDB backend, Grevm parallel execution, aggressive caching for 32GB RAM. |
| **Configurable** | All major parameters (cache sizes, pruning depth, parallelism) exposed via CLI flags. |

### 1.3 Requirements Traceability

| # | Requirement | Design Section |
|---|-------------|----------------|
| R1 | MDBX → RocksDB | §3 Storage Layer |
| R2 | Grevm compatibility | §4 Parallel Execution |
| R3 | Configurable caching (32GB target) | §5 Cache Architecture |
| R4 | Minimal DB: blocks + latest state only | §6 Data Retention Policy |
| R5 | Crash recovery | §7 Crash Recovery |
| R6 | Skip merklization, verify receipt root + block hash | §8 Verification Strategy |

---

## 2. Architecture Overview

```
                    ┌──────────────────────────────┐
                    │   Block Source (P2P / Engine)  │
                    └──────────────┬───────────────┘
                                   │ NewPayload / Downloaded Block
                                   ▼
                    ┌──────────────────────────────┐
                    │       Execution Pipeline      │
                    │  ┌─────────┐  ┌───────────┐  │
                    │  │  Grevm  │  │ Sequential │  │
                    │  │Parallel │  │  Fallback  │  │
                    │  └────┬────┘  └─────┬─────┘  │
                    │       └──────┬──────┘        │
                    │              ▼               │
                    │    State Changes (BundleState)│
                    └──────────────┬───────────────┘
                                   │
                    ┌──────────────┴───────────────┐
                    │       Verification            │
                    │  ✗ State Root (trusted)        │
                    │  ✓ Receipt Root (computed)     │
                    │  ✓ Block Hash (computed)       │
                    └──────────────┬───────────────┘
                                   │
                    ┌──────────────┴───────────────┐
                    │       Persistence             │
                    │  ┌──────────┐ ┌────────────┐ │
                    │  │ RocksDB  │ │Static Files│ │
                    │  │(state +  │ │(headers,   │ │
                    │  │ indexes) │ │txs, rcpts) │ │
                    │  └──────────┘ └────────────┘ │
                    └──────────────────────────────┘
```

---

## 3. Storage Layer: RocksDB (R1)

### 3.1 Why RocksDB

选择 RocksDB 替换 MDBX 的核心原因：

1. **WAL（Write-Ahead Log）写入保障** — RocksDB 的 WAL 提供 crash-safe 的原子写入，即使进程被 kill -9 也能通过 WAL replay 恢复到一致状态。MDBX 虽有类似机制但 RocksDB 的 WAL 更适合高吞吐写入场景。
2. **优秀的查询性能** — RocksDB 的 LSM-tree + block cache + bloom filter 组合在大数据集上的点查和范围扫描性能优异，尤其适合 latest state 的高频读取模式。
3. **成熟的调优生态** — 丰富的 compaction 策略、压缩算法、缓存配置选项，适合针对 32GB 系统做精细调优。

**替换优先级**：
- **P0（主链路）**：PlainAccountState、PlainStorageState、Bytecodes、BlockHeaders、BlockBodyIndices、TransactionHashNumbers — 这些是区块处理和 latest state 查询的关键路径
- **P1（辅助）**：Stage checkpoints、DB version、PruneCheckpoints 等 meta/config 表 — 顺便一起替换，但非关键路径

### 3.2 Merge-Friendly Architecture

> **核心原则：最小化与 upstream 的 diff，方便持续 merge upstream/main。**

upstream reth 的 `Database` trait 是 sealed trait，定义在 `reth-db-api` 中，实现必须在 `reth-db` crate 内。当前架构：

```
reth-db-api (traits)          reth-db (implementations)
├── Database                  ├── implementation/
├── DbTx / DbTxMut           │   └── mdbx/        ← #[cfg(feature = "mdbx")]
├── DbCursorRO / DbCursorRW  │       ├── mod.rs    (DatabaseEnv)
└── Table / DupSort           │       ├── tx.rs     (Tx<K>)
                              │       └── cursor.rs (Cursor<K,T>)
                              └── lib.rs            (re-exports)
```

**我们的策略：Feature-gate 并行实现，不修改 MDBX 代码**

```
reth-db (our fork)
├── implementation/
│   ├── mdbx/           ← 保持不动，upstream 更新直接 merge
│   │   ├── mod.rs
│   │   ├── tx.rs
│   │   └── cursor.rs
│   └── rocksdb/         ← 新增，#[cfg(feature = "rocksdb")]
│       ├── mod.rs       (DatabaseEnv — 实现 Database trait)
│       ├── tx.rs        (RocksDbTx — 实现 DbTx/DbTxMut)
│       └── cursor.rs    (RocksDbCursor — 实现 DbCursorRO/RW)
├── lib.rs               ← 仅改动 re-export 的 cfg 条件
└── Cargo.toml           ← 新增 rocksdb feature + dep
```

**Diff 最小化规则**：

| 文件 | 改动 | merge 冲突风险 |
|------|------|---------------|
| `implementation/mdbx/*` | **零改动** | 无 — upstream 随意改 |
| `implementation/mod.rs` | 加一行 `#[cfg(feature = "rocksdb")] pub(crate) mod rocksdb;` | 极低 |
| `lib.rs` | 改 re-export 的 cfg 条件（~10 行） | 低 — 仅 use 语句 |
| `Cargo.toml` | 加 `rocksdb` feature + dependency | 低 — 新增行 |
| `implementation/rocksdb/*` | **全新文件** | 无 — 不冲突 |

**Feature 互斥**：

```toml
# Cargo.toml
[features]
default = ["rocksdb"]  # 我们的默认
mdbx = ["dep:reth-libmdbx", ...]  # upstream 默认，保留可选
rocksdb = ["dep:rocksdb", ...]     # 我们的新增
```

编译时只能选其一：`--features rocksdb`（我们的默认）或 `--features mdbx`（回退/对比测试）。`lib.rs` 中：

```rust
#[cfg(feature = "mdbx")]
pub use implementation::mdbx::{DatabaseEnv, DatabaseEnvKind, ...};

#[cfg(feature = "rocksdb")]
pub use implementation::rocksdb::{DatabaseEnv, DatabaseEnvKind, ...};
```

这样两边导出的类型名完全一致，上层 `reth-provider`、`reth-stages` 等 crate **零改动**。

### 3.3 Implementation: Trait Mapping

RocksDB 需要实现的 trait 到内部结构的映射：

```
Database trait              RocksDB 实现
─────────────────────────   ────────────────────────────────
Database                    DatabaseEnv {
  tx() → TX                    inner: rocksdb::OptimisticTransactionDB,
  tx_mut() → TXMut             cfs: HashMap<&str, ColumnFamily>,
                                metrics: Option<DatabaseEnvMetrics>,
                            }

DbTx                        RocksDbTx<RO> {
  get(table, key)               snapshot: rocksdb::Snapshot,
  cursor_read()                 // Point reads via snapshot.get_cf()
  commit()                      // No-op for RO
  entries()                 }

DbTxMut                     RocksDbTx<RW> {
  put(table, key, value)        tx: rocksdb::Transaction,
  delete(table, key)            // Writes go through OptimisticTransaction
  clear(table)                  // commit() → tx.commit()
  cursor_write()            }

DbCursorRO                  RocksDbCursor<K, T> {
  first/last/seek/next/prev     iter: rocksdb::DBIterator,
  walk() → Walker               // Prefix-seek via column family
                            }

DbCursorRW                  (extends RocksDbCursor for RW)
  upsert/insert/delete_current  // Direct put/delete on transaction
  append/append_dup             // Sequential write optimization
```

**DupSort 表处理**：MDBX 原生支持 duplicate keys（一个 key 对应多个 value）。RocksDB 没有此特性。策略：将 `(key, subkey)` 编码为 composite key `key || subkey`，利用 RocksDB 的前缀迭代器实现 `seek_by_key_subkey()` 和 `next_dup()` 语义。这是 main 分支已验证的方案。

### 3.4 Column Family Design

采用 **one CF per table** 的简单策略，而非 main 的 3-shard 设计（因为我们跳过 merklization，不需要 trie 表的分片优化）：

| Column Family | 对应 reth 表 | 优先级 | 特点 |
|--------------|-------------|--------|------|
| `PlainAccountState` | 账户状态 | P0 | 高频读写，latest state 核心 |
| `PlainStorageState` | 存储槽位 | P0 | 最大的表，DupSort |
| `Bytecodes` | 合约字节码 | P0 | 大 value，不可压缩 |
| `BlockHeaders` | 区块头 | P0 | 范围扫描 |
| `BlockBodyIndices` | 区块体索引 | P0 | 关联 static files |
| `TransactionHashNumbers` | 交易哈希→编号 | P0 | 点查为主，B256 key 不可压缩 |
| `StageCheckpoints` | 阶段检查点 | P1 | 小表，crash recovery 关键 |
| `other reth tables` | 其余 | P1 | 全部对应各自 CF |

**CF 级别调优**：
- `PlainStorageState`：最大的表，给更大的 write buffer（512MB）
- `TransactionHashNumbers`：禁用压缩（B256 不可压缩），禁用 bloom filter（总是命中）
- `Bytecodes`：禁用压缩（字节码不可压缩），大 block size（64KB）
- 其余表：LZ4 压缩 + 默认配置

### 3.5 Configuration (32GB System)

```rust
pub struct RocksDBConfig {
    /// Block cache — shared across all CFs, LRU for uncompressed data blocks.
    /// 32GB system: ~4GB (12.5% of RAM). This is the single most impactful setting.
    pub block_cache_size: usize,          // default: 4GB

    /// Write buffer per CF. Larger = better write throughput, more memory.
    /// PlainStorageState gets 2x this value due to its size.
    pub write_buffer_size: usize,         // default: 256MB

    /// Number of write buffers before stall (memtable pipeline depth).
    pub max_write_buffer_number: i32,     // default: 4

    /// Background compaction/flush threads.
    pub max_background_jobs: i32,         // default: 6

    /// Max open SST file handles.
    pub max_open_files: i32,              // default: 4096

    /// Compression per level: LZ4 for L0-L5, Zstd for bottommost.
    /// Disabled for TransactionHashNumbers and Bytecodes CFs.
    pub compression_per_level: Vec<CompressionType>,

    /// WAL directory (separate disk recommended for production).
    pub wal_dir: Option<PathBuf>,         // default: <datadir>/wal

    /// WAL size limit before rotation.
    pub wal_size_limit_mb: u64,           // default: 1024 (1GB)

    /// Compaction readahead for sequential scans.
    pub compaction_readahead_size: usize,  // default: 4MB
}
```

**总内存预算（32GB 系统）**：

| 组件 | 内存 | 说明 |
|------|------|------|
| RocksDB block cache | 4 GB | 共享 LRU，点查/范围扫描的核心 |
| RocksDB memtables | ~2 GB | ~8 CFs × 256MB write buffer |
| RocksDB indexes/filters | ~1 GB | SST 文件索引 |
| **RocksDB 小计** | **~7 GB** | |
| 应用层 state cache (§5) | 12 GB | DashMap 缓存 |
| RPC cache (§5) | 2 GB | moka LRU |
| OS / runtime / headroom | ~11 GB | 页面缓存 + tokio + 执行 |
| **总计** | **~32 GB** | |

### 3.6 What to Reference from main

main 分支中值得参考的代码（**不直接 cherry-pick，而是参考实现**）：

| main 中的文件 | 参考内容 | 注意事项 |
|--------------|---------|---------|
| `crates/storage/db/src/implementation/rocksdb/mod.rs` | DatabaseEnv 结构、CF 初始化、metrics | main 的 API 可能与最新 upstream trait 不同步，需对齐 |
| `crates/storage/db/src/implementation/rocksdb/tx.rs` | 事务包装、snapshot/transaction 模式 | 检查 OptimisticTransaction vs Transaction 选择 |
| `crates/storage/db/src/implementation/rocksdb/cursor.rs` | 迭代器包装、DupSort composite key 编码 | 最复杂的部分，需仔细验证 seek 语义 |
| `crates/storage/db/Cargo.toml` | rocksdb crate 版本和 feature flags | 用最新 rocksdb crate 版本 |

**为什么不直接 cherry-pick**：main 基于较旧的 upstream 版本，trait 签名可能已变化（新增方法如 `commit_view()`、`TableImporter` 等）。需要基于当前 upstream 的 trait 定义重新实现。

### 3.7 Verification Criteria

- [ ] `#[cfg(feature = "rocksdb")]` 编译通过，`#[cfg(feature = "mdbx")]` 编译不受影响
- [ ] `cargo nextest run -p reth-db --features rocksdb` 通过所有现有 DB 单元测试
- [ ] DupSort 表操作（seek_by_key_subkey, next_dup, walk_dup）语义与 MDBX 一致
- [ ] WAL recovery 测试：写入中 kill -9，重启后数据一致
- [ ] 上层 crate 零改动验证：`cargo check -p reth-provider -p reth-stages --features rocksdb`
- [ ] **Merge 测试**：`git merge upstream/main` 后无冲突（或仅 Cargo.toml 冲突，秒级解决）

---

## 4. Parallel Execution: Grevm (R2)

### 4.1 Strategy

Port the Grevm integration from main. The key components:

| Component | Source File (main) | Purpose |
|-----------|-------------------|---------|
| `GrevmExecutor` | `crates/ethereum/evm/src/parallel_execute.rs` | Block-STM parallel executor |
| `ParallelExecutor` trait | `crates/evm/evm/src/parallel_execute.rs` | Abstraction layer |
| `ParallelDatabase` trait | `crates/evm/evm/src/lib.rs` | `DatabaseRef + Send + Sync` marker |
| `ConfigureEvm::parallel_executor` | `crates/ethereum/evm/src/lib.rs` | Factory method for executor selection |

### 4.2 Dependency

```toml
grevm = { package = "grevm", git = "https://github.com/Galxe/grevm", tag = "v2.2.4" }
```

Grevm requires a specific revm fork. Verify compatibility with the upstream reth's revm version on the feature branch before porting. If there's a version mismatch (as noted in main: v34 vs v29), create a compatibility shim or pin to the compatible revm version.

### 4.3 Integration Point

```rust
// In ConfigureEvm implementation
fn parallel_executor<'a, DB: ParallelDatabase + 'a>(
    &self, db: DB,
) -> Box<dyn ParallelExecutor<...> + 'a> {
    if config.disable_grevm {
        Box::new(WrapExecutor::new(BasicBlockExecutor::new(...)))
    } else {
        Box::new(GrevmExecutor::new(chain_spec, evm_config, db))
    }
}
```

### 4.4 Verification Criteria

- [ ] `GrevmExecutor` produces identical receipts and state changes as `BasicBlockExecutor` on the same block
- [ ] `--disable-grevm` flag correctly falls back to sequential execution
- [ ] No deadlocks or data races under concurrent execution (run with `RUSTFLAGS="-Z sanitizer=thread"`)

---

## 5. Cache Architecture (R3)

### 5.1 Design Philosophy

Three-tier caching optimized for a 32GB system, targeting ~18GB total cache budget (leaving ~14GB for OS page cache, RocksDB internals, and execution overhead).

### 5.2 Tier 1: RocksDB Block Cache (4GB)

RocksDB's internal LRU cache for uncompressed data blocks. Shared across all column families. This is the foundation — no application-level caching of raw KV pairs is needed since RocksDB handles it.

### 5.3 Tier 2: Execution State Cache (12GB)

A DashMap-based in-process cache for hot execution state, inspired by main's `PersistBlockCache` but redesigned for configurability.

```rust
pub struct StateCache {
    /// Account state: Address → Account (nonce, balance, code_hash)
    accounts: DashMap<Address, CacheEntry<Option<Account>>>,
    /// Storage slots: Address → (StorageKey → Value)
    storage: DashMap<Address, DashMap<U256, CacheEntry<Option<U256>>>>,
    /// Contract bytecodes: CodeHash → Bytecode
    contracts: DashMap<B256, CacheEntry<Bytecode>>,
}

pub struct CacheEntry<V> {
    value: V,
    block_number: u64,  // When this entry was last written
}
```

**Memory Budget Allocation** (configurable via CLI):

| Component | Default (32GB) | Percentage | Rationale |
|-----------|---------------|------------|-----------|
| Accounts | 1 GB | 8% | ~10M accounts × ~100 bytes each |
| Storage | 10 GB | 83% | Hot DeFi contracts have millions of slots |
| Contracts | 1 GB | 8% | ~10K unique bytecodes, large but infrequently changing |

**Eviction Policy**: Block-height-based sliding window (same as main):
- Background thread runs every 15 seconds
- Evicts entries older than `persist_height - eviction_window` (default: 512 blocks)
- Contracts evicted when count > threshold (default: 2000)
- State evicted when item count > capacity

### 5.4 Tier 3: RPC Response Cache (2GB)

The existing `EthStateCache` with moka-based LRU, sized up:

| Cache | Default (32GB) | Entries |
|-------|---------------|---------|
| Block cache | 512 MB | ~2048 full blocks |
| Receipt cache | 512 MB | ~2048 receipt sets |
| Header cache | 256 MB | ~8192 headers |
| Code cache | 768 MB | Weight-based (bytecode size) |

### 5.5 CLI Configuration

```
--cache.state-capacity <bytes>      Total state cache size (default: 12GB)
--cache.rpc-capacity <bytes>        RPC cache size (default: 2GB)
--cache.rocksdb-block <bytes>       RocksDB block cache (default: 4GB)
--cache.eviction-window <blocks>    Sliding window for eviction (default: 512)
--cache.max-persist-gap <blocks>    Backpressure threshold (default: 64)
```

### 5.6 Verification Criteria

- [ ] Total memory usage stays under 20GB under sustained block processing (measured via `/proc/self/status` VmRSS)
- [ ] Cache hit ratio > 90% for account reads during sequential block processing (measured via metrics)
- [ ] Backpressure correctly pauses execution when persist gap exceeds threshold
- [ ] Eviction completes in < 100ms for 2M entries

---

## 6. Data Retention Policy (R4)

### 6.1 What to Keep

| Data Type | Storage | Retention | Purpose |
|-----------|---------|-----------|---------|
| Block headers | Static files | **Forever** | Chain structure, timestamp, state root reference |
| Transaction bodies | Static files | **Forever** | Input data, from/to, value, calldata |
| Transaction receipts | Static files | **Forever** | Logs, events, gas used — primary knowledge graph source |
| Transaction senders | Static files | **Forever** | Recovered signer addresses |
| Latest account state | RocksDB | **Latest only** | Current nonce, balance, code hash per address |
| Latest storage state | RocksDB | **Latest only** | Current storage slot values |
| Contract bytecodes | RocksDB | **Forever** | Deployed contract code |
| Block body indices | RocksDB | **Forever** | Maps block numbers to tx ranges in static files |

### 6.2 What to Discard

| Data Type | Upstream Table | Action |
|-----------|---------------|--------|
| Account changesets | `AccountChangeSets` | **Never write** |
| Storage changesets | `StorageChangeSets` | **Never write** |
| Account history indices | `AccountsHistory` | **Never write** |
| Storage history indices | `StoragesHistory` | **Never write** |
| Hashed account state | `HashedAccounts` | **Never write** (no trie needed) |
| Hashed storage state | `HashedStorages` | **Never write** (no trie needed) |
| Account trie nodes | `AccountsTrie` | **Never write** |
| Storage trie nodes | `StoragesTrie` | **Never write** |
| Transaction lookup (hash→number) | `TransactionHashNumbers` | **Keep** (useful for RPC `eth_getTransactionByHash`) |

### 6.3 Implementation

Modify the `DatabaseProvider::save_blocks()` path to skip writing changeset and trie tables. This is controlled by a `PruneModes` configuration:

```rust
PruneModes {
    sender_recovery: None,           // Keep (in static files)
    transaction_lookup: None,        // Keep
    receipts: None,                  // Keep
    account_history: Some(PruneMode::Full),  // Discard all
    storage_history: Some(PruneMode::Full),  // Discard all
    // Custom: skip trie tables entirely
}
```

Additionally, introduce a `--minimal-state` flag that:
1. Disables the merkle stage entirely (no trie writes)
2. Disables changeset writing in the execution stage
3. Disables hashed state table writes
4. Keeps only plain account/storage state (for RPC queries and re-execution)

### 6.4 Estimated Storage

For Ethereum mainnet reference (approximate):

| Component | Size | Notes |
|-----------|------|-------|
| Headers (static) | ~30 GB | ~20M blocks × ~0.5 KB RLP header + 1 KB overhead |
| Transactions (static) | ~400 GB | Average ~150 txs/block × ~300 bytes |
| Receipts (static) | ~300 GB | Logs are the bulk |
| Latest state (RocksDB) | ~80 GB | ~300M accounts + storage |
| Contract bytecodes | ~10 GB | ~100K contracts |
| Transaction lookup | ~40 GB | Hash → number mapping |
| **Total** | **~860 GB** | vs ~2.5 TB for full archive node |

For Gravity chain (lower volume), this will be significantly smaller.

### 6.5 Verification Criteria

- [ ] No writes to `AccountChangeSets`, `StorageChangeSets`, `AccountsHistory`, `StoragesHistory`, `AccountsTrie`, `StoragesTrie`, `HashedAccounts`, `HashedStorages` tables
- [ ] `eth_getBalance`, `eth_getCode`, `eth_getStorageAt` return correct latest values
- [ ] `eth_getTransactionReceipt`, `eth_getLogs` work correctly
- [ ] `eth_getTransactionByHash` works via lookup table
- [ ] Total disk usage is < 50% of a full archive node

---

## 7. Crash Recovery (R5)

### 7.1 Consistency Model

The system maintains consistency across three storage backends:

```
RocksDB (state + indices) ←──→ Static Files (headers, txs, receipts)
                    │
              Stage Checkpoints (in RocksDB)
```

**Invariant**: At any point, the stage checkpoints represent the **minimum consistent block number** across all backends. On startup, recovery ensures all backends are rolled back to this checkpoint.

### 7.2 Write Ordering

For each block batch persistence:

1. **Write static files** (headers, transactions, receipts) — append-only, crash-safe via fsync
2. **Write RocksDB state** — WAL-protected atomic batch
3. **Update stage checkpoint** — single atomic RocksDB write
4. **Fsync all** — ensure durability

If crash occurs between steps:
- After step 1, before step 2: Static files have extra data. On startup, truncate static files to match RocksDB checkpoint.
- After step 2, before step 3: State is ahead of checkpoint. On startup, checkpoint tells us the consistent point; state may have extra entries but queries use checkpoint as boundary.
- After step 3: Fully consistent.

### 7.3 Startup Recovery Sequence

```
1. Open RocksDB (WAL replay happens automatically)
2. Read stage checkpoints → find min consistent block
3. Open static files → check highest block in each segment
4. If static_file_highest > checkpoint:
     truncate static files to checkpoint
5. If state_highest > checkpoint:
     (optional) purge state entries above checkpoint
     OR simply treat checkpoint as logical tip
6. Resume processing from checkpoint + 1
```

### 7.4 RocksDB WAL Configuration

```rust
// Ensure WAL is enabled (default) for crash safety
db_options.set_wal_dir(data_dir.join("wal"));
db_options.set_wal_size_limit_mb(1024);  // 1GB WAL before rotation
db_options.set_manual_wal_flush(false);  // Auto-flush on commit
```

### 7.5 Verification Criteria

- [ ] Kill process at random points during block processing (use `kill -9`), restart, verify:
  - No data corruption in RocksDB
  - No data corruption in static files
  - Checkpoint is consistent
  - Client resumes syncing from correct block
- [ ] Simulate power failure (echo 1 > /proc/sysrq-trigger for test) on a test setup
- [ ] Recovery time < 30 seconds for normal shutdowns, < 5 minutes for crash recovery

---

## 8. Verification Strategy: Skip Merklization (R6)

### 8.1 Rationale

State root computation (merklization) is the most expensive per-block operation (~40-60% of block processing time). For a non-validator node focused on data indexing, computing the state root provides no value — we trust the remote block's state root and only need to verify that our local execution is consistent.

### 8.2 What We Trust

| Field | Source | Action |
|-------|--------|--------|
| `state_root` | Remote block header | **Trust** — accept as-is |
| `receipt_root` | Locally computed from execution receipts | **Verify** — must match header |
| `block_hash` | Locally computed from full header | **Verify** — must match expected |
| `logs_bloom` | Locally computed from receipts | **Verify** — must match header |
| `gas_used` | Locally computed from execution | **Verify** — must match header |

### 8.3 Divergence Detection

If receipt root or block hash doesn't match, the local state has diverged from the canonical chain. This indicates a bug in execution or a corrupted state database.

```rust
fn verify_block_consistency(
    block: &SealedBlock,
    execution_result: &BlockExecutionResult<Receipt>,
) -> Result<(), ConsensusError> {
    // 1. Verify gas used
    let cumulative_gas = execution_result.receipts.last()
        .map(|r| r.cumulative_gas_used())
        .unwrap_or(0);
    if cumulative_gas != block.gas_used() {
        return Err(ConsensusError::GasUsedMismatch { local: cumulative_gas, remote: block.gas_used() });
    }

    // 2. Verify receipt root
    let receipts_with_bloom = build_receipts_with_bloom(&execution_result.receipts);
    let local_receipt_root = calculate_receipt_root(&receipts_with_bloom);
    if local_receipt_root != block.receipts_root() {
        return Err(ConsensusError::ReceiptRootMismatch {
            local: local_receipt_root,
            remote: block.receipts_root(),
        });
    }

    // 3. Verify logs bloom
    let local_bloom = compute_logs_bloom(&receipts_with_bloom);
    if local_bloom != block.logs_bloom() {
        return Err(ConsensusError::LogsBloomMismatch);
    }

    // 4. Block hash is inherently verified since we accept the sealed header
    // The header already contains the remote state_root, so block hash
    // verification confirms we're processing the same block.

    Ok(())
}
```

### 8.4 Implementation: Disable Merklization

**Changes needed in the execution pipeline:**

1. **Merkle Stage** (`crates/stages/stages/src/stages/merkle.rs`):
   - When `--skip-state-root` is set, the merkle stage becomes a no-op that immediately returns `ExecOutput::done()`
   - No trie tables are written

2. **Engine Tree** (`crates/engine/tree/src/tree/`):
   - In `validate_block_state_root()`, skip the `ParallelStateRoot` computation
   - Accept the remote state root from the block header without verification

3. **Post-execution validation** (`crates/ethereum/consensus/src/validation.rs`):
   - Keep `validate_block_post_execution()` intact — it verifies receipt root, logs bloom, gas used
   - These are independent of state root

4. **State root in header construction** (for pipe execution):
   - Use the remote block's `state_root` field directly instead of computing it

### 8.5 CLI Flags

```
--skip-state-root              Skip state root computation (trust remote)
--verify-receipt-root          Verify receipt root matches (default: true)
--verify-block-hash            Verify block hash matches (default: true)
--on-divergence <action>       Action on mismatch: "halt" | "warn" | "rewind"
                               default: "halt"
```

### 8.6 Verification Criteria

- [ ] Block processing throughput increases by ≥ 30% with `--skip-state-root` (measure before/after)
- [ ] Receipt root verification catches intentionally corrupted receipts
- [ ] Block hash verification catches header tampering
- [ ] Divergence detection triggers halt (or configured action) within 1 block
- [ ] No writes to trie-related tables when `--skip-state-root` is active

---

## 9. Implementation Phases

### Phase 1: RocksDB Foundation (R1)
**Goal**: Replace MDBX with RocksDB as sole database backend.
- Port `DatabaseEnv` implementation from main
- Adapt `ProviderFactory` to remove MDBX dependency
- Verify all existing tests pass
- **Deliverable**: `cargo nextest run -p reth-db -p reth-provider` passes

### Phase 2: Skip Merklization + Minimal State (R4, R6)
**Goal**: Eliminate trie computation and historical state.
- Implement `--skip-state-root` flag and no-op merkle stage
- Implement `--minimal-state` flag to skip changeset/trie/hashed writes
- Add receipt root + block hash verification in post-execution
- **Deliverable**: Client syncs chain with correct receipts, no trie tables written

### Phase 3: Grevm Integration (R2)
**Goal**: Enable parallel EVM execution.
- Port `GrevmExecutor` and `ParallelExecutor` trait from main
- Resolve revm version compatibility
- Wire into `ConfigureEvm::parallel_executor()`
- **Deliverable**: Identical execution results in parallel vs sequential mode

### Phase 4: Cache Optimization (R3)
**Goal**: Tune caching for 32GB system.
- Implement configurable `StateCache` with DashMap
- Integrate with RocksDB block cache sizing
- Add RPC cache tier with moka
- Add CLI flags for all cache parameters
- **Deliverable**: Memory usage < 20GB, cache hit ratio > 90%

### Phase 5: Crash Recovery Hardening (R5)
**Goal**: Guarantee data integrity across unexpected shutdowns.
- Implement startup consistency check
- Add static file truncation logic
- Configure RocksDB WAL settings
- Stress test with random kill -9
- **Deliverable**: Zero data corruption across 1000 random kill tests

---

## 10. CLI Summary

```
Custom Reth Node Flags:

Storage:
  --rocksdb.block-cache <bytes>       RocksDB block cache size [default: 4GB]
  --rocksdb.write-buffer <bytes>      Write buffer per CF [default: 256MB]
  --rocksdb.max-background-jobs <n>   Compaction threads [default: 6]
  --rocksdb.max-open-files <n>        Max SST file handles [default: 4096]

Caching:
  --cache.state-capacity <bytes>      State cache total size [default: 12GB]
  --cache.rpc-capacity <bytes>        RPC response cache [default: 2GB]
  --cache.eviction-window <blocks>    Eviction sliding window [default: 512]
  --cache.max-persist-gap <blocks>    Backpressure threshold [default: 64]

Execution:
  --disable-grevm                     Disable parallel EVM execution
  --minimal-state                     Keep only latest state (no history)
  --skip-state-root                   Trust remote state root
  --on-divergence <halt|warn|rewind>  Action on receipt/hash mismatch [default: halt]
```

---

## 11. Monitoring & Metrics

| Metric | Type | Description |
|--------|------|-------------|
| `reth.cache.hit_ratio` | Gauge | State cache hit ratio (per 15s window) |
| `reth.cache.items_count` | Gauge | Total cached items |
| `reth.cache.eviction_duration` | Histogram | Time spent evicting |
| `reth.execution.block_time` | Histogram | Per-block execution time |
| `reth.execution.grevm_speedup` | Gauge | Parallel vs sequential ratio |
| `reth.verification.receipt_root_match` | Counter | Receipt root matches |
| `reth.verification.divergence_count` | Counter | Divergence detections |
| `reth.rocksdb.block_cache_hit_ratio` | Gauge | RocksDB internal cache hits |
| `reth.rocksdb.compaction_pending` | Gauge | Pending compaction bytes |
| `reth.recovery.startup_time` | Gauge | Time from process start to ready |
| `reth.storage.disk_usage_bytes` | Gauge | Total disk usage |
