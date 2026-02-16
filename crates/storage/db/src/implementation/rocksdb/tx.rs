//! Transaction implementation for RocksDB.
//!
//! Uses `WriteBatch` for atomic writes and direct DB reads for queries.

use crate::{
    implementation::rocksdb::{get_cf_handle, read_error, to_error_info},
    DatabaseError,
};
use parking_lot::Mutex;
use reth_db_api::{
    table::{Compress, Decompress, DupSort, Encode, Table, TableImporter},
    transaction::{DbTx, DbTxMut},
};
use reth_storage_errors::db::DatabaseErrorInfo;
use rocksdb::DB;
use std::sync::Arc;

pub(crate) use super::cursor::{RO, RW};
use super::cursor;

/// RocksDB transaction.
///
/// Single-database variant: all tables live in one RocksDB instance with per-table column families.
///
/// Write operations are accumulated in a `WriteBatch` and committed atomically.
/// Read operations go directly to the database.
pub struct Tx<K: cursor::TransactionKind> {
    /// The RocksDB database instance.
    pub(crate) db: Arc<DB>,
    /// Write batch for accumulating writes.
    pub(crate) batch: Arc<Mutex<rocksdb::WriteBatch>>,
    /// Phantom data for transaction kind.
    _mode: std::marker::PhantomData<K>,
}

impl<K: cursor::TransactionKind> std::fmt::Debug for Tx<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tx").field("kind", &std::any::type_name::<K>()).finish()
    }
}

impl Tx<RO> {
    /// Creates a new read-only transaction.
    pub(crate) fn new_ro(db: Arc<DB>) -> Self {
        Self { db, batch: Arc::new(Mutex::new(rocksdb::WriteBatch::default())), _mode: std::marker::PhantomData }
    }
}

impl Tx<RW> {
    /// Creates a new read-write transaction.
    pub(crate) fn new_rw(db: Arc<DB>) -> Self {
        Self { db, batch: Arc::new(Mutex::new(rocksdb::WriteBatch::default())), _mode: std::marker::PhantomData }
    }
}

impl<K: cursor::TransactionKind> Tx<K> {
    /// Returns the number of entries in a table (estimated).
    pub fn table_entries(&self, name: &str) -> Result<usize, DatabaseError> {
        let cf_handle = self.db.cf_handle(name).ok_or_else(|| {
            DatabaseError::Open(DatabaseErrorInfo {
                message: format!("Column family '{}' not found", name).into(),
                code: -1,
            })
        })?;

        match self.db.property_value_cf(cf_handle, "rocksdb.estimate-num-keys") {
            Ok(Some(value)) => value.parse::<usize>().map_err(|_| {
                DatabaseError::Read(DatabaseErrorInfo {
                    message: "Failed to parse estimated number of keys".into(),
                    code: -1,
                })
            }),
            Ok(None) => Ok(0),
            Err(e) => Err(read_error(e)),
        }
    }
}

// === DbTx Implementation ===

impl<K: cursor::TransactionKind> DbTx for Tx<K> {
    type Cursor<T: Table> = cursor::Cursor<K, T>;
    type DupCursor<T: DupSort> = cursor::Cursor<K, T>;

    fn get<T: Table>(&self, key: T::Key) -> Result<Option<T::Value>, DatabaseError> {
        let encoded_key = key.encode();
        self.get_by_encoded_key::<T>(&encoded_key)
    }

    fn get_by_encoded_key<T: Table>(
        &self,
        key: &<T::Key as Encode>::Encoded,
    ) -> Result<Option<T::Value>, DatabaseError> {
        let cf_handle = get_cf_handle::<T>(&self.db)?;

        if T::DUPSORT {
            // For DupSort tables, use iterator to find first entry with matching key prefix
            let mut iter = self.db.raw_iterator_cf(cf_handle);
            let encoded_key_ref = key.as_ref();
            iter.seek(encoded_key_ref);

            if iter.valid() {
                if let (Some(k), Some(v)) = (iter.key(), iter.value()) {
                    if k.len() >= encoded_key_ref.len()
                        && &k[..encoded_key_ref.len()] == encoded_key_ref
                    {
                        return T::Value::decompress(v)
                            .map(Some)
                            .map_err(|_| DatabaseError::Decode);
                    }
                }
            }
            Ok(None)
        } else {
            match self.db.get_cf(cf_handle, key) {
                Ok(Some(value)) => {
                    T::Value::decompress(&value).map(Some).map_err(|_| DatabaseError::Decode)
                }
                Ok(None) => Ok(None),
                Err(e) => Err(read_error(e)),
            }
        }
    }

    fn commit(self) -> Result<(), DatabaseError> {
        let batch = {
            let mut batch = self.batch.lock();
            std::mem::take(&mut *batch)
        };
        if !batch.is_empty() {
            self.db.write(batch).map_err(|e| DatabaseError::Commit(to_error_info(e)))?;
        }
        Ok(())
    }

    fn abort(self) {
        // Drop the write batch without committing
    }

    fn cursor_read<T: Table>(&self) -> Result<Self::Cursor<T>, DatabaseError> {
        cursor::Cursor::new(self.db.clone(), self.batch.clone())
    }

    fn cursor_dup_read<T: DupSort>(&self) -> Result<Self::DupCursor<T>, DatabaseError> {
        cursor::Cursor::new(self.db.clone(), self.batch.clone())
    }

    fn entries<T: Table>(&self) -> Result<usize, DatabaseError> {
        self.table_entries(T::NAME)
    }

    fn disable_long_read_transaction_safety(&mut self) {
        // No-op for RocksDB — no long-read-transaction safety mechanism
    }
}

// === DbTxMut Implementation ===

impl DbTxMut for Tx<RW> {
    type CursorMut<T: Table> = cursor::Cursor<RW, T>;
    type DupCursorMut<T: DupSort> = cursor::Cursor<RW, T>;

    fn put<T: Table>(&self, key: T::Key, value: T::Value) -> Result<(), DatabaseError> {
        let cf_handle = get_cf_handle::<T>(&self.db)?;
        let mut batch = self.batch.lock();

        if T::DUPSORT {
            // DupSort composite key: `encoded_key || full_compressed_value`.
            // The compressed value starts with the subkey bytes (by Compact encoding
            // convention), so this preserves MDBX's sort order.
            let encoded_key = key.encode();
            let compressed = value.compress();
            let key_ref = encoded_key.as_ref();
            let compressed_ref = compressed.as_ref();
            let mut composite_key = Vec::with_capacity(key_ref.len() + compressed_ref.len());
            composite_key.extend_from_slice(key_ref);
            composite_key.extend_from_slice(compressed_ref);
            batch.put_cf(cf_handle, &composite_key, compressed_ref);
        } else {
            let encoded_key = key.encode();
            let compressed = value.compress();
            batch.put_cf(cf_handle, encoded_key.as_ref(), compressed.as_ref());
        }
        Ok(())
    }

    fn delete<T: Table>(
        &self,
        key: T::Key,
        value: Option<T::Value>,
    ) -> Result<bool, DatabaseError> {
        let cf_handle = get_cf_handle::<T>(&self.db)?;
        let mut batch = self.batch.lock();

        if T::DUPSORT {
            let encoded_key = key.encode();
            let key_ref = encoded_key.as_ref();

            if let Some(value) = value {
                // Delete specific key/value pair
                let compressed = value.compress();
                let compressed_ref = compressed.as_ref();
                let mut composite_key =
                    Vec::with_capacity(key_ref.len() + compressed_ref.len());
                composite_key.extend_from_slice(key_ref);
                composite_key.extend_from_slice(compressed_ref);
                batch.delete_cf(cf_handle, &composite_key);
            } else {
                // Delete ALL values for this key — scan all entries with matching prefix
                let mut iter = self.db.raw_iterator_cf(cf_handle);
                iter.seek(key_ref);
                while iter.valid() {
                    if let Some(k) = iter.key() {
                        if k.len() >= key_ref.len() && &k[..key_ref.len()] == key_ref {
                            batch.delete_cf(cf_handle, k);
                            iter.next();
                        } else {
                            break;
                        }
                    } else {
                        break;
                    }
                }
            }
        } else {
            let encoded_key = key.encode();
            batch.delete_cf(cf_handle, encoded_key.as_ref());
        }
        Ok(true)
    }

    fn clear<T: Table>(&self) -> Result<(), DatabaseError> {
        let cf_handle = get_cf_handle::<T>(&self.db)?;

        // Find first and last keys
        let mut iter = self.db.raw_iterator_cf(cf_handle);
        iter.seek_to_first();
        if !iter.valid() {
            return Ok(()); // Empty table
        }
        let first_key = iter
            .key()
            .ok_or_else(|| {
                DatabaseError::Read(DatabaseErrorInfo {
                    message: "Failed to get first key".into(),
                    code: -1,
                })
            })?
            .to_vec();

        iter.seek_to_last();
        let last_key = iter
            .key()
            .ok_or_else(|| {
                DatabaseError::Read(DatabaseErrorInfo {
                    message: "Failed to get last key".into(),
                    code: -1,
                })
            })?
            .to_vec();

        let mut batch = self.batch.lock();
        batch.delete_range_cf(cf_handle, &first_key, &last_key);
        batch.delete_cf(cf_handle, &last_key); // delete_range_cf is [start, end)
        Ok(())
    }

    fn cursor_write<T: Table>(&self) -> Result<Self::CursorMut<T>, DatabaseError> {
        cursor::Cursor::new(self.db.clone(), self.batch.clone())
    }

    fn cursor_dup_write<T: DupSort>(&self) -> Result<Self::DupCursorMut<T>, DatabaseError> {
        cursor::Cursor::new(self.db.clone(), self.batch.clone())
    }
}

impl TableImporter for Tx<RW> {}
