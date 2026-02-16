//! RocksDB implementation for the database.
//!
//! Single-database architecture using one RocksDB instance with per-table column families.
//! Simplified from main's 3-shard design since we skip trie tables.

use crate::{DatabaseError, TableSet};
use reth_db_api::{
    database_metrics::DatabaseMetrics, models::ClientVersion, table::Table, tables::Tables,
};
use reth_storage_errors::db::{DatabaseErrorInfo, LogLevel};
use reth_tracing::tracing::info;
use rocksdb::{BlockBasedOptions, Cache, ColumnFamilyDescriptor, Options, DB};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

pub(crate) mod cursor;
pub(crate) mod tx;

/// Database environment kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatabaseEnvKind {
    /// Read-write database.
    RW,
    /// Read-only database.
    RO,
}

impl DatabaseEnvKind {
    /// Returns `true` if the database is read-write.
    pub const fn is_rw(self) -> bool {
        matches!(self, Self::RW)
    }
}

/// Database arguments for RocksDB.
#[derive(Debug, Clone)]
pub struct DatabaseArguments {
    client_version: ClientVersion,
    log_level: Option<LogLevel>,
    /// Block cache size in bytes (default: 4GB).
    pub block_cache_size: Option<usize>,
    /// Write buffer size in bytes (default: 256MB).
    pub write_buffer_size: Option<usize>,
    /// Maximum number of background jobs (default: 6).
    pub max_background_jobs: Option<i32>,
    /// Maximum number of open files (default: 4096).
    pub max_open_files: Option<i32>,
    /// Maximum number of write buffers (default: 4).
    pub max_write_buffer_number: Option<i32>,
    /// WAL directory override.
    pub wal_dir: Option<PathBuf>,
}

impl DatabaseArguments {
    /// Default block cache size: 4GB
    pub const DEFAULT_BLOCK_CACHE_SIZE: usize = 4 * 1024 * 1024 * 1024;
    /// Default write buffer size: 256MB
    pub const DEFAULT_WRITE_BUFFER_SIZE: usize = 256 * 1024 * 1024;
    /// Default max background jobs: 6
    pub const DEFAULT_MAX_BACKGROUND_JOBS: i32 = 6;
    /// Default max open files: 4096
    pub const DEFAULT_MAX_OPEN_FILES: i32 = 4096;
    /// Default max write buffer number: 4
    pub const DEFAULT_MAX_WRITE_BUFFER_NUMBER: i32 = 4;

    /// Creates a new instance of [`DatabaseArguments`].
    pub const fn new(client_version: ClientVersion) -> Self {
        Self {
            client_version,
            log_level: None,
            block_cache_size: None,
            write_buffer_size: None,
            max_background_jobs: None,
            max_open_files: None,
            max_write_buffer_number: None,
            wal_dir: None,
        }
    }

    /// Creates arguments suitable for tests (smaller memory footprint).
    pub fn test() -> Self {
        Self::new(ClientVersion::default())
            .with_block_cache_size(Some(64 * 1024 * 1024)) // 64MB
            .with_write_buffer_size(Some(16 * 1024 * 1024)) // 16MB
            .with_max_write_buffer_number(Some(2))
    }

    /// Set the log level.
    pub const fn with_log_level(mut self, log_level: Option<LogLevel>) -> Self {
        self.log_level = log_level;
        self
    }

    /// Set the block cache size in bytes.
    pub const fn with_block_cache_size(mut self, size: Option<usize>) -> Self {
        self.block_cache_size = size;
        self
    }

    /// Set the write buffer size in bytes.
    pub const fn with_write_buffer_size(mut self, size: Option<usize>) -> Self {
        self.write_buffer_size = size;
        self
    }

    /// Set the maximum number of background jobs.
    pub const fn with_max_background_jobs(mut self, jobs: Option<i32>) -> Self {
        self.max_background_jobs = jobs;
        self
    }

    /// Set the maximum number of open files.
    pub const fn with_max_open_files(mut self, files: Option<i32>) -> Self {
        self.max_open_files = files;
        self
    }

    /// Set the maximum number of write buffers.
    pub const fn with_max_write_buffer_number(mut self, num: Option<i32>) -> Self {
        self.max_write_buffer_number = num;
        self
    }

    /// Set the WAL directory override.
    pub fn with_wal_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.wal_dir = dir;
        self
    }

    /// Get the client version.
    pub const fn client_version(&self) -> &ClientVersion {
        &self.client_version
    }

    fn block_cache_size(&self) -> usize {
        self.block_cache_size.unwrap_or(Self::DEFAULT_BLOCK_CACHE_SIZE)
    }

    fn write_buffer_size(&self) -> usize {
        self.write_buffer_size.unwrap_or(Self::DEFAULT_WRITE_BUFFER_SIZE)
    }

    fn max_background_jobs(&self) -> i32 {
        self.max_background_jobs.unwrap_or(Self::DEFAULT_MAX_BACKGROUND_JOBS)
    }

    fn max_open_files(&self) -> i32 {
        self.max_open_files.unwrap_or(Self::DEFAULT_MAX_OPEN_FILES)
    }

    fn max_write_buffer_number(&self) -> i32 {
        self.max_write_buffer_number.unwrap_or(Self::DEFAULT_MAX_WRITE_BUFFER_NUMBER)
    }
}

impl Default for DatabaseArguments {
    fn default() -> Self {
        Self::new(ClientVersion::default())
    }
}

/// RocksDB database environment.
///
/// Single-database architecture: one RocksDB instance with one column family per table.
#[derive(Debug)]
pub struct DatabaseEnv {
    /// The RocksDB database instance.
    pub(crate) inner: Arc<DB>,
    /// Database environment kind (read-only or read-write).
    kind: DatabaseEnvKind,
}

impl DatabaseEnv {
    /// Opens the database at the specified path.
    pub fn open(
        path: &Path,
        kind: DatabaseEnvKind,
        args: DatabaseArguments,
    ) -> Result<Self, DatabaseError> {
        let opts = Self::create_db_options(&args);

        // Collect all table names for column families
        let cf_names: Vec<String> = Tables::tables().map(|t| t.name().to_string()).collect();

        let cf_descriptors: Vec<ColumnFamilyDescriptor> = cf_names
            .iter()
            .map(|name| {
                let mut cf_opts = Options::default();
                cf_opts.set_write_buffer_size(args.write_buffer_size());
                cf_opts.set_max_write_buffer_number(args.max_write_buffer_number());

                // Per-CF compression tuning
                cf_opts.set_compression_per_level(&[
                    rocksdb::DBCompressionType::Lz4,
                    rocksdb::DBCompressionType::Lz4,
                    rocksdb::DBCompressionType::Zstd,
                    rocksdb::DBCompressionType::Zstd,
                    rocksdb::DBCompressionType::Zstd,
                    rocksdb::DBCompressionType::Zstd,
                    rocksdb::DBCompressionType::Zstd,
                ]);

                ColumnFamilyDescriptor::new(name.clone(), cf_opts)
            })
            .collect();

        let db = if kind.is_rw() {
            DB::open_cf_descriptors(&opts, path, cf_descriptors)
        } else {
            DB::open_cf_descriptors_read_only(&opts, path, cf_descriptors, false)
        }
        .map_err(|e| {
            DatabaseError::Open(DatabaseErrorInfo {
                message: format!("Failed to open RocksDB at {}: {}", path.display(), e).into(),
                code: -1,
            })
        })?;

        info!(target: "reth::db::rocksdb", path = ?path, kind = ?kind, cfs = cf_names.len(), "Opened RocksDB");

        Ok(Self { inner: Arc::new(db), kind })
    }

    /// Create optimized RocksDB options.
    fn create_db_options(args: &DatabaseArguments) -> Options {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);

        // Parallelism
        opts.set_max_background_jobs(args.max_background_jobs());
        opts.increase_parallelism(args.max_background_jobs());
        opts.set_max_open_files(args.max_open_files());

        // Memory
        opts.set_write_buffer_size(args.write_buffer_size());
        opts.set_max_write_buffer_number(args.max_write_buffer_number());
        opts.set_min_write_buffer_number_to_merge(2);

        // Block cache (shared LRU)
        let cache = Cache::new_lru_cache(args.block_cache_size());
        let mut block_opts = BlockBasedOptions::default();
        block_opts.set_block_cache(&cache);
        block_opts.set_block_size(32 * 1024); // 32KB
        block_opts.set_cache_index_and_filter_blocks(true);
        block_opts.set_pin_l0_filter_and_index_blocks_in_cache(true);
        block_opts.set_bloom_filter(10.0, false);
        opts.set_block_based_table_factory(&block_opts);

        // Compaction
        opts.set_level_compaction_dynamic_level_bytes(true);
        opts.set_level_zero_file_num_compaction_trigger(4);
        opts.set_level_zero_slowdown_writes_trigger(28);
        opts.set_level_zero_stop_writes_trigger(48);
        opts.set_max_bytes_for_level_base(512 * 1024 * 1024); // 512MB
        opts.set_max_bytes_for_level_multiplier(10.0);
        opts.set_target_file_size_base(256 * 1024 * 1024); // 256MB

        // Write
        opts.set_enable_pipelined_write(true);
        opts.set_max_total_wal_size(1024 * 1024 * 1024); // 1GB
        opts.set_wal_bytes_per_sync(4 * 1024 * 1024); // 4MB

        // Compression per level
        opts.set_compression_per_level(&[
            rocksdb::DBCompressionType::Lz4,
            rocksdb::DBCompressionType::Lz4,
            rocksdb::DBCompressionType::Zstd,
            rocksdb::DBCompressionType::Zstd,
            rocksdb::DBCompressionType::Zstd,
            rocksdb::DBCompressionType::Zstd,
            rocksdb::DBCompressionType::Zstd,
        ]);

        // I/O
        opts.set_bytes_per_sync(4 * 1024 * 1024); // 4MB
        opts.set_compaction_readahead_size(4 * 1024 * 1024); // 4MB

        // WAL directory
        if let Some(ref wal_dir) = args.wal_dir {
            opts.set_wal_dir(wal_dir);
        }

        opts
    }

    /// Returns `true` if the database is read-only.
    pub const fn is_read_only(&self) -> bool {
        matches!(self.kind, DatabaseEnvKind::RO)
    }

    /// Creates all the tables defined in the given [`TableSet`], if necessary.
    ///
    /// For RocksDB, column families are created at open time, so this is a no-op.
    pub fn create_and_track_tables_for<TS: TableSet>(&mut self) -> Result<(), DatabaseError> {
        // CFs are created at DB::open_cf_descriptors time. Nothing to do here.
        Ok(())
    }

    /// Records the client version in the database.
    pub fn record_client_version(&self, version: ClientVersion) -> Result<(), DatabaseError> {
        use reth_db_api::{
            cursor::{DbCursorRO, DbCursorRW},
            database::Database,
            tables,
            transaction::{DbTx, DbTxMut},
        };
        use std::time::{SystemTime, UNIX_EPOCH};

        if version.is_empty() {
            return Ok(());
        }

        let tx = self.tx_mut()?;
        let mut version_cursor = tx.cursor_write::<tables::VersionHistory>()?;

        let last_version = version_cursor.last()?.map(|(_, v)| v);
        if Some(&version) != last_version.as_ref() {
            version_cursor.upsert(
                SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
                &version,
            )?;
            tx.commit()?;
        }

        Ok(())
    }

    /// Drops an orphaned table (column family) if it exists.
    ///
    /// Returns `true` if the table was dropped.
    pub fn drop_orphan_table(&self, _table_name: &str) -> Result<bool, DatabaseError> {
        // RocksDB column families are fixed at open time.
        // Orphan CFs are simply not created, so this is a no-op.
        Ok(false)
    }
}

impl DatabaseMetrics for DatabaseEnv {
    fn gauge_metrics(&self) -> Vec<(&'static str, f64, Vec<metrics::Label>)> {
        let mut result = Vec::new();
        for table in Tables::ALL.iter().map(Tables::name) {
            if let Some(cf_handle) = self.inner.cf_handle(table) {
                if let Ok(Some(value)) =
                    self.inner.property_value_cf(cf_handle, "rocksdb.estimate-num-keys")
                {
                    if let Ok(entries) = value.parse::<usize>() {
                        result.push((
                            "db.table_entries",
                            entries as f64,
                            vec![metrics::Label::new("table", table)],
                        ));
                    }
                }
            }
        }
        result
    }

    fn histogram_metrics(&self) -> Vec<(&'static str, f64, Vec<metrics::Label>)> {
        let mut result = Vec::new();

        let properties = [
            ("rocksdb.actual-delayed-write-rate", "rocksdb.actual_delayed_write_rate"),
            ("rocksdb.is-write-stopped", "rocksdb.is_write_stopped"),
            ("rocksdb.num-immutable-mem-table", "rocksdb.num_immutable_mem_table"),
            ("rocksdb.num-files-at-level0", "rocksdb.num_files_at_level0"),
            (
                "rocksdb.estimate-pending-compaction-bytes",
                "rocksdb.estimate_pending_compaction_bytes",
            ),
            ("rocksdb.num-running-compactions", "rocksdb.num_running_compactions"),
            ("rocksdb.compaction-pending", "rocksdb.compaction_pending"),
        ];

        for (prop, metric_name) in properties {
            if let Ok(Some(value)) = self.inner.property_value(prop) {
                if let Ok(v) = value.parse::<f64>() {
                    result.push((metric_name, v, vec![]));
                }
            }
        }

        result
    }
}

impl reth_db_api::database::Database for DatabaseEnv {
    type TX = tx::Tx<tx::RO>;
    type TXMut = tx::Tx<tx::RW>;

    fn tx(&self) -> Result<Self::TX, DatabaseError> {
        Ok(tx::Tx::new_ro(self.inner.clone()))
    }

    fn tx_mut(&self) -> Result<Self::TXMut, DatabaseError> {
        if self.is_read_only() {
            return Err(DatabaseError::Open(DatabaseErrorInfo {
                message: "Cannot create write transaction on read-only database".into(),
                code: -1,
            }));
        }
        Ok(tx::Tx::new_rw(self.inner.clone()))
    }
}

/// Convert RocksDB error to [`DatabaseErrorInfo`].
pub(crate) fn to_error_info(e: rocksdb::Error) -> DatabaseErrorInfo {
    DatabaseErrorInfo { message: e.to_string().into(), code: -1 }
}

/// Create a read error from RocksDB error.
pub(crate) fn read_error(e: rocksdb::Error) -> DatabaseError {
    DatabaseError::Read(to_error_info(e))
}

/// Helper function to get column family handle.
pub(crate) fn get_cf_handle<T: Table>(db: &DB) -> Result<&rocksdb::ColumnFamily, DatabaseError> {
    db.cf_handle(T::NAME).ok_or_else(|| {
        DatabaseError::Open(DatabaseErrorInfo {
            message: format!("Column family '{}' not found", T::NAME).into(),
            code: -1,
        })
    })
}
