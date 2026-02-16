//! Cross-block execution state cache.
//!
//! DashMap-based in-process cache for hot execution state (accounts, storage, bytecodes).
//! Adapted from gravity-reth-dev's `PersistBlockCache`, simplified to exclude trie caches
//! since merklization is skipped (Phase 2).

use alloy_primitives::{Address, Bytes, StorageKey, StorageValue, B256, U256};
use alloy_consensus::constants::KECCAK_EMPTY;
use core::sync::atomic::{AtomicU64, Ordering};
use dashmap::DashMap;
use metrics::{Counter, Gauge, Histogram};
use reth_errors::ProviderResult;
use reth_metrics::Metrics;
use reth_primitives_traits::{Account, Bytecode};
use reth_provider::{
    AccountReader, BlockHashReader, BytecodeReader, HashedPostStateProvider, StateProofProvider,
    StateProvider, StateRootProvider, StorageRootProvider,
};
use reth_revm::db::BundleState;
use reth_trie::{
    updates::TrieUpdates, AccountProof, HashedPostState, HashedStorage, MultiProof,
    MultiProofTargets, StorageMultiProof, StorageProof, TrieInput,
};
use std::{
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

const CACHE_METRICS_INTERVAL: Duration = Duration::from_secs(15);

/// Configuration for the execution state cache.
#[derive(Debug, Clone)]
pub struct StateCacheConfig {
    /// Maximum number of cached items before eviction triggers.
    pub capacity: usize,
    /// Maximum gap between executed and persisted block numbers before
    /// backpressure is applied.
    pub max_persist_gap: u64,
    /// Contract cache eviction threshold.
    pub contracts_threshold: usize,
}

impl Default for StateCacheConfig {
    fn default() -> Self {
        Self { capacity: 2_000_000, max_persist_gap: 64, contracts_threshold: 2_000 }
    }
}

/// Metrics for the state cache.
#[derive(Metrics)]
#[metrics(scope = "state_cache")]
struct StateCacheMetrics {
    /// Cache hit ratio for account/storage reads.
    hit_ratio: Gauge,
    /// Total number of cached items.
    num_items: Gauge,
    /// Latest executed (merged) block number.
    latest_merged_block: Gauge,
    /// Latest persisted block number.
    latest_persist_block: Gauge,
    /// Eviction duration.
    eviction_duration: Histogram,
    /// Wait-for-persist duration (backpressure).
    wait_persist_duration: Histogram,
    /// Total cache hits.
    hits: Counter,
    /// Total cache misses.
    misses: Counter,
}

/// Tracks hit/miss counts for periodic ratio reporting.
#[derive(Default)]
struct HitRecorder {
    miss_cnt: AtomicU64,
    hit_cnt: AtomicU64,
}

impl HitRecorder {
    fn miss(&self) {
        self.miss_cnt.fetch_add(1, Ordering::Relaxed);
    }

    fn hit(&self) {
        self.hit_cnt.fetch_add(1, Ordering::Relaxed);
    }

    fn report(&self) -> Option<f64> {
        let misses = self.miss_cnt.swap(0, Ordering::Relaxed);
        let hits = self.hit_cnt.swap(0, Ordering::Relaxed);
        let total = misses + hits;
        (total > 0).then(|| hits as f64 / total as f64)
    }
}

/// A cache entry tagged with the block number at which it was written.
struct CacheEntry<V> {
    value: V,
    block_number: u64,
}

impl<V> CacheEntry<V> {
    const fn new(value: V, block_number: u64) -> Self {
        Self { value, block_number }
    }
}

/// Cross-block execution state cache.
///
/// Caches accounts, storage slots, and contract bytecodes across block boundaries.
/// A background daemon periodically evicts entries older than the persist height
/// to bound memory usage.
pub struct StateCache {
    accounts: DashMap<Address, CacheEntry<Option<Account>>>,
    storage: DashMap<Address, DashMap<U256, CacheEntry<Option<U256>>>>,
    contracts: DashMap<B256, CacheEntry<Bytecode>>,
    merged_block_number: Mutex<Option<u64>>,
    persist_block_number: Mutex<Option<u64>>,
    persist_wait: Arc<(Mutex<bool>, Condvar)>,
    config: StateCacheConfig,
    hit_recorder: HitRecorder,
    cached_items: AtomicU64,
    merged_block_metric: AtomicU64,
    persist_block_metric: AtomicU64,
    metrics: StateCacheMetrics,
}

impl std::fmt::Debug for StateCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateCache")
            .field("num_cached", &self.cached_items.load(Ordering::Relaxed))
            .finish()
    }
}

impl StateCache {
    /// Create a new state cache with the given configuration.
    ///
    /// Spawns a background daemon thread for periodic eviction and metrics reporting.
    pub fn new(config: StateCacheConfig) -> Self {
        let cache = Self {
            accounts: DashMap::new(),
            storage: DashMap::new(),
            contracts: DashMap::new(),
            merged_block_number: Mutex::new(None),
            persist_block_number: Mutex::new(None),
            persist_wait: Arc::new((Mutex::new(false), Condvar::new())),
            config,
            hit_recorder: HitRecorder::default(),
            cached_items: AtomicU64::new(0),
            merged_block_metric: AtomicU64::new(0),
            persist_block_metric: AtomicU64::new(0),
            metrics: StateCacheMetrics::default(),
        };
        cache
    }

    /// Spawn the background eviction daemon.
    ///
    /// Must be called after wrapping in `Arc` so we can pass a `Weak` reference.
    pub fn spawn_eviction_daemon(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        thread::Builder::new()
            .name("state-cache-eviction".into())
            .spawn(move || {
                let mut last_contract_eviction_height = 0u64;
                let mut last_state_eviction_height = 0u64;

                loop {
                    thread::sleep(CACHE_METRICS_INTERVAL);
                    let Some(cache) = weak.upgrade() else {
                        break;
                    };

                    let start = Instant::now();
                    let num_items = cache.entry_count();
                    cache.cached_items.store(num_items as u64, Ordering::Release);

                    // Report metrics
                    cache.metrics.num_items.set(num_items as f64);
                    cache
                        .metrics
                        .latest_merged_block
                        .set(cache.merged_block_metric.load(Ordering::Relaxed) as f64);
                    cache
                        .metrics
                        .latest_persist_block
                        .set(cache.persist_block_metric.load(Ordering::Relaxed) as f64);
                    if let Some(ratio) = cache.hit_recorder.report() {
                        cache.metrics.hit_ratio.set(ratio);
                    }

                    // Eviction
                    let persist_height =
                        cache.persist_block_number.lock().unwrap().unwrap_or(0);

                    // Evict contracts when count exceeds threshold
                    if cache.contracts.len() > cache.config.contracts_threshold {
                        let eviction_height = if last_contract_eviction_height == 0 {
                            persist_height.saturating_sub(512).max(persist_height / 2)
                        } else {
                            (persist_height + last_contract_eviction_height) / 2
                        };
                        cache.contracts.retain(|_, v| v.block_number > eviction_height);
                        last_contract_eviction_height = eviction_height;
                    }

                    // Evict state when item count exceeds capacity
                    if num_items > cache.config.capacity {
                        let eviction_height = if last_state_eviction_height == 0 {
                            persist_height.saturating_sub(512).max(persist_height / 2)
                        } else {
                            (persist_height + last_state_eviction_height) / 2
                        };
                        cache.accounts.retain(|_, v| v.block_number > eviction_height);
                        cache.storage.iter().for_each(|s| {
                            s.retain(|_, v| v.block_number > eviction_height);
                        });
                        cache.storage.retain(|_, s| !s.is_empty());
                        last_state_eviction_height = eviction_height;
                    }

                    cache.metrics.eviction_duration.record(start.elapsed());
                }
            })
            .expect("failed to spawn state cache eviction daemon");
    }

    /// Count total cached entries (accounts + storage slots).
    fn entry_count(&self) -> usize {
        let mut count = self.accounts.len();
        count += self.storage.iter().map(|s| s.len()).sum::<usize>();
        count
    }

    // ---- Read methods ----

    /// Look up an account in the cache.
    ///
    /// Returns `Some(Some(account))` if found, `Some(None)` if known-absent,
    /// or `None` on cache miss.
    pub fn basic_account(&self, address: &Address) -> Option<Option<Account>> {
        if let Some(entry) = self.accounts.get(address) {
            self.hit_recorder.hit();
            self.metrics.hits.increment(1);
            Some(entry.value)
        } else {
            self.hit_recorder.miss();
            self.metrics.misses.increment(1);
            None
        }
    }

    /// Look up a storage slot in the cache.
    ///
    /// Returns `Some(Some(value))` if found, `Some(None)` if known-zero/absent,
    /// or `None` on cache miss.
    pub fn storage(&self, address: &Address, slot: &U256) -> Option<Option<U256>> {
        // If account is known-absent, storage is zero.
        if let Some(entry) = self.accounts.get(address) {
            if entry.value.is_none() {
                self.hit_recorder.hit();
                self.metrics.hits.increment(1);
                return Some(None);
            }
        }
        if let Some(addr_storage) = self.storage.get(address) {
            if let Some(entry) = addr_storage.get(slot) {
                self.hit_recorder.hit();
                self.metrics.hits.increment(1);
                Some(entry.value)
            } else {
                self.hit_recorder.miss();
                self.metrics.misses.increment(1);
                None
            }
        } else {
            self.hit_recorder.miss();
            self.metrics.misses.increment(1);
            None
        }
    }

    /// Look up a contract bytecode by its code hash.
    pub fn bytecode_by_hash(&self, code_hash: &B256) -> Option<Bytecode> {
        if let Some(entry) = self.contracts.get(code_hash) {
            self.hit_recorder.hit();
            self.metrics.hits.increment(1);
            Some(entry.value.clone())
        } else {
            self.hit_recorder.miss();
            self.metrics.misses.increment(1);
            None
        }
    }

    // ---- Cache-on-miss methods (for read-through caching) ----

    /// Insert an account into the cache if not already present.
    pub fn cache_account(&self, address: Address, account: Option<Account>) {
        if let dashmap::Entry::Vacant(entry) = self.accounts.entry(address) {
            let bn = self.persist_block_metric.load(Ordering::Relaxed);
            entry.insert(CacheEntry::new(account, bn));
        }
    }

    /// Insert a storage slot into the cache if not already present.
    pub fn cache_storage(&self, address: Address, slot: U256, value: Option<U256>) {
        let bn = self.persist_block_metric.load(Ordering::Relaxed);
        match self.storage.entry(address) {
            dashmap::Entry::Occupied(outer) => {
                if let dashmap::Entry::Vacant(inner) = outer.get().entry(slot) {
                    inner.insert(CacheEntry::new(value, bn));
                }
            }
            dashmap::Entry::Vacant(outer) => {
                let map = DashMap::new();
                map.insert(slot, CacheEntry::new(value, bn));
                outer.insert(map);
            }
        }
    }

    /// Insert a bytecode into the cache if not already present.
    pub fn cache_bytecode(&self, code_hash: B256, bytecode: Bytecode) {
        if let dashmap::Entry::Vacant(entry) = self.contracts.entry(code_hash) {
            let bn = self.persist_block_metric.load(Ordering::Relaxed);
            entry.insert(CacheEntry::new(bytecode, bn));
        }
    }

    // ---- Write methods ----

    /// Write execution state changes from a `BundleState` into the cache.
    ///
    /// Called after a block is executed, before persistence.
    pub fn write_state_changes(&self, block_number: u64, bundle: &BundleState) {
        self.merged_block_metric.store(block_number, Ordering::Relaxed);
        {
            let mut guard = self.merged_block_number.lock().unwrap();
            if let Some(ref mut merged) = *guard {
                *merged = block_number;
            } else {
                *guard = Some(block_number);
            }
        }

        // Write contracts
        for (hash, bytecode) in &bundle.contracts {
            if *hash != KECCAK_EMPTY {
                self.contracts
                    .insert(*hash, CacheEntry::new(Bytecode(bytecode.clone()), block_number));
            }
        }

        // Write account and storage changes
        for (address, account) in &bundle.state {
            let was_destroyed = account.was_destroyed();

            // Write account info if changed
            if account.is_info_changed() {
                let info = account.info.as_ref().map(|i| Account {
                    nonce: i.nonce,
                    balance: i.balance,
                    bytecode_hash: if i.code_hash == KECCAK_EMPTY {
                        None
                    } else {
                        Some(i.code_hash)
                    },
                });
                self.accounts.insert(*address, CacheEntry::new(info, block_number));
            }

            // Clear storage for destroyed accounts
            if was_destroyed {
                self.storage.remove(address);
            }

            // Write storage changes
            for (slot, slot_value) in &account.storage {
                let destroyed_and_not_zero =
                    was_destroyed && !slot_value.present_value.is_zero();
                let not_destroyed_and_changed = !was_destroyed && slot_value.is_changed();

                if destroyed_and_not_zero || not_destroyed_and_changed {
                    let value = slot_value.present_value;
                    let cache_value = if value.is_zero() { None } else { Some(value) };

                    match self.storage.entry(*address) {
                        dashmap::Entry::Occupied(outer) => {
                            outer
                                .get()
                                .insert(*slot, CacheEntry::new(cache_value, block_number));
                        }
                        dashmap::Entry::Vacant(outer) => {
                            let map = DashMap::new();
                            map.insert(*slot, CacheEntry::new(cache_value, block_number));
                            outer.insert(map);
                        }
                    }
                }
            }
        }
    }

    /// Update the persist tip after blocks have been written to disk.
    ///
    /// This updates the backpressure state and notifies waiting threads.
    pub fn persist_tip(&self, block_number: u64) {
        self.persist_block_metric.store(block_number, Ordering::Relaxed);
        {
            let mut guard = self.persist_block_number.lock().unwrap();
            if let Some(ref mut persist_bn) = *guard {
                if block_number > *persist_bn {
                    *persist_bn = block_number;
                }
            } else {
                *guard = Some(block_number);
            }
        }
        // Update backpressure state
        if let Some(merged_bn) = *self.merged_block_number.lock().unwrap() {
            let (lock, cvar) = self.persist_wait.as_ref();
            let mut large_gap = lock.lock().unwrap();
            *large_gap = merged_bn.saturating_sub(block_number) >= self.config.max_persist_gap;
            cvar.notify_all();
        }
    }

    /// Wait if the gap between executed and persisted blocks exceeds the threshold.
    ///
    /// Used for backpressure to prevent unbounded memory growth.
    pub fn wait_persist_gap(&self, timeout_ms: Option<u64>) {
        let (lock, cvar) = self.persist_wait.as_ref();
        let mut large_gap = lock.lock().unwrap();
        let start = Instant::now();

        while *large_gap {
            if let Some(timeout_ms) = timeout_ms {
                let elapsed = start.elapsed();
                let timeout = Duration::from_millis(timeout_ms);
                if elapsed >= timeout {
                    break;
                }
                let result = cvar.wait_timeout(large_gap, timeout - elapsed).unwrap();
                large_gap = result.0;
                if result.1.timed_out() {
                    break;
                }
            } else {
                large_gap = cvar.wait(large_gap).unwrap();
            }
        }

        let wait_dur = start.elapsed();
        if !wait_dur.is_zero() {
            self.metrics.wait_persist_duration.record(wait_dur);
        }
    }
}

/// A state provider wrapper that checks the cross-block [`StateCache`] before
/// falling through to the underlying database provider.
///
/// On cache miss, the result from the inner provider is inserted into the cache
/// for future lookups.
#[derive(Debug)]
pub struct CachedDatabaseStateProvider<S> {
    inner: S,
    cache: Arc<StateCache>,
}

impl<S> CachedDatabaseStateProvider<S> {
    /// Create a new cached state provider wrapping `inner`.
    pub fn new(inner: S, cache: Arc<StateCache>) -> Self {
        Self { inner, cache }
    }
}

impl<S: AccountReader> AccountReader for CachedDatabaseStateProvider<S> {
    fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
        if let Some(cached) = self.cache.basic_account(address) {
            return Ok(cached);
        }
        let result = self.inner.basic_account(address)?;
        self.cache.cache_account(*address, result);
        Ok(result)
    }
}

impl<S: BytecodeReader> BytecodeReader for CachedDatabaseStateProvider<S> {
    fn bytecode_by_hash(&self, code_hash: &B256) -> ProviderResult<Option<Bytecode>> {
        if let Some(cached) = self.cache.bytecode_by_hash(code_hash) {
            return Ok(Some(cached));
        }
        let result = self.inner.bytecode_by_hash(code_hash)?;
        if let Some(ref bytecode) = result {
            self.cache.cache_bytecode(*code_hash, bytecode.clone());
        }
        Ok(result)
    }
}

impl<S: StateProvider> StateProvider for CachedDatabaseStateProvider<S> {
    fn storage(
        &self,
        account: Address,
        storage_key: StorageKey,
    ) -> ProviderResult<Option<StorageValue>> {
        let slot = U256::from_be_bytes(storage_key.0);
        if let Some(cached) = self.cache.storage(&account, &slot) {
            return Ok(cached);
        }
        let result = self.inner.storage(account, storage_key)?;
        self.cache.cache_storage(account, slot, result);
        Ok(result)
    }

    fn storage_by_hashed_key(
        &self,
        address: Address,
        hashed_storage_key: StorageKey,
    ) -> ProviderResult<Option<StorageValue>> {
        // Cannot use cache for hashed keys — delegate directly.
        self.inner.storage_by_hashed_key(address, hashed_storage_key)
    }
}

impl<S: BlockHashReader> BlockHashReader for CachedDatabaseStateProvider<S> {
    fn block_hash(
        &self,
        number: alloy_primitives::BlockNumber,
    ) -> ProviderResult<Option<B256>> {
        self.inner.block_hash(number)
    }

    fn canonical_hashes_range(
        &self,
        start: alloy_primitives::BlockNumber,
        end: alloy_primitives::BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        self.inner.canonical_hashes_range(start, end)
    }
}

impl<S: StateRootProvider> StateRootProvider for CachedDatabaseStateProvider<S> {
    fn state_root(&self, hashed_state: HashedPostState) -> ProviderResult<B256> {
        self.inner.state_root(hashed_state)
    }

    fn state_root_from_nodes(&self, input: TrieInput) -> ProviderResult<B256> {
        self.inner.state_root_from_nodes(input)
    }

    fn state_root_with_updates(
        &self,
        hashed_state: HashedPostState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        self.inner.state_root_with_updates(hashed_state)
    }

    fn state_root_from_nodes_with_updates(
        &self,
        input: TrieInput,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        self.inner.state_root_from_nodes_with_updates(input)
    }
}

impl<S: StorageRootProvider> StorageRootProvider for CachedDatabaseStateProvider<S> {
    fn storage_root(
        &self,
        address: Address,
        hashed_storage: HashedStorage,
    ) -> ProviderResult<B256> {
        self.inner.storage_root(address, hashed_storage)
    }

    fn storage_proof(
        &self,
        address: Address,
        slot: B256,
        hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageProof> {
        self.inner.storage_proof(address, slot, hashed_storage)
    }

    fn storage_multiproof(
        &self,
        address: Address,
        slots: &[B256],
        hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageMultiProof> {
        self.inner.storage_multiproof(address, slots, hashed_storage)
    }
}

impl<S: StateProofProvider> StateProofProvider for CachedDatabaseStateProvider<S> {
    fn proof(
        &self,
        input: TrieInput,
        address: Address,
        slots: &[B256],
    ) -> ProviderResult<AccountProof> {
        self.inner.proof(input, address, slots)
    }

    fn multiproof(
        &self,
        input: TrieInput,
        targets: MultiProofTargets,
    ) -> ProviderResult<MultiProof> {
        self.inner.multiproof(input, targets)
    }

    fn witness(
        &self,
        input: TrieInput,
        target: HashedPostState,
    ) -> ProviderResult<Vec<Bytes>> {
        self.inner.witness(input, target)
    }
}

impl<S: HashedPostStateProvider> HashedPostStateProvider for CachedDatabaseStateProvider<S> {
    fn hashed_post_state(&self, bundle_state: &BundleState) -> HashedPostState {
        self.inner.hashed_post_state(bundle_state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> StateCacheConfig {
        StateCacheConfig { capacity: 100, max_persist_gap: 4, contracts_threshold: 5 }
    }

    #[test]
    fn test_basic_account_cache() {
        let cache = StateCache::new(test_config());
        let addr = Address::random();
        let account = Account { nonce: 1, balance: U256::from(100), bytecode_hash: None };

        // Miss
        assert!(cache.basic_account(&addr).is_none());

        // Insert and hit
        cache.cache_account(addr, Some(account));
        let result = cache.basic_account(&addr);
        assert!(result.is_some());
        assert_eq!(result.unwrap().unwrap().nonce, 1);
    }

    #[test]
    fn test_storage_cache() {
        let cache = StateCache::new(test_config());
        let addr = Address::random();
        let slot = U256::from(42);
        let value = U256::from(999);

        // Miss
        assert!(cache.storage(&addr, &slot).is_none());

        // Insert and hit
        cache.cache_storage(addr, slot, Some(value));
        let result = cache.storage(&addr, &slot);
        assert_eq!(result, Some(Some(value)));
    }

    #[test]
    fn test_absent_account_storage() {
        let cache = StateCache::new(test_config());
        let addr = Address::random();
        let slot = U256::from(1);

        // Mark account as absent
        cache.cache_account(addr, None);

        // Storage lookup should return Some(None) due to absent account
        assert_eq!(cache.storage(&addr, &slot), Some(None));
    }

    #[test]
    fn test_persist_tip_backpressure() {
        let cache = StateCache::new(test_config());

        // Simulate executing blocks 1-10 without persisting
        for i in 1..=10 {
            let mut guard = cache.merged_block_number.lock().unwrap();
            *guard = Some(i);
            cache.merged_block_metric.store(i, Ordering::Relaxed);
        }

        // Persist at block 1 — gap of 9 > max_persist_gap of 4
        cache.persist_tip(1);
        {
            let (lock, _) = cache.persist_wait.as_ref();
            let gap = lock.lock().unwrap();
            assert!(*gap, "should have large gap");
        }

        // Persist at block 8 — gap of 2 < max_persist_gap of 4
        cache.persist_tip(8);
        {
            let (lock, _) = cache.persist_wait.as_ref();
            let gap = lock.lock().unwrap();
            assert!(!*gap, "should not have large gap");
        }
    }
}
