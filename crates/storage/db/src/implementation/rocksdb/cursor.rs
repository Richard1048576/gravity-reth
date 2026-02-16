//! Cursor implementation for RocksDB.
//!
//! Provides `Cursor<K, T>` backed by a RocksDB raw iterator, implementing all
//! cursor traits from `reth-db-api`.
//!
//! For DupSort tables, entries use composite keys: `encoded_primary_key || compressed_value`.
//! This preserves MDBX's DupSort ordering semantics in RocksDB's single-key model.

use crate::{
    implementation::rocksdb::get_cf_handle,
    DatabaseError,
};
use parking_lot::Mutex;
use reth_db_api::{
    cursor::{
        DbCursorRO, DbCursorRW, DbDupCursorRO, DbDupCursorRW, DupWalker, RangeWalker,
        ReverseWalker, Walker,
    },
    table::{Compress, Decode, Decompress, DupSort, Encode, Table},
};
use rocksdb::DB;
use std::{
    ops::Bound,
    sync::Arc,
};

/// Read-only transaction kind marker.
#[derive(Debug)]
pub struct RO;

/// Read-write transaction kind marker.
#[derive(Debug)]
pub struct RW;

/// Transaction kind trait (sealed).
pub trait TransactionKind: Send + Sync + 'static {}
impl TransactionKind for RO {}
impl TransactionKind for RW {}

/// RocksDB cursor backed by a raw iterator.
///
/// For DupSort tables, entries are stored with composite keys:
/// `encoded_primary_key || compressed_value`. The full compressed value is
/// also stored as the RocksDB value, so reads decompress from `iter.value()`.
pub struct Cursor<K: TransactionKind, T: Table> {
    /// Keeps the database alive for the iterator's lifetime.
    #[allow(dead_code)]
    db: Arc<DB>,
    /// RocksDB raw iterator.
    ///
    /// # Safety
    ///
    /// The lifetime is transmuted to `'static`. This is sound because the `Arc<DB>`
    /// field guarantees the database outlives this cursor.
    iter: rocksdb::DBRawIterator<'static>,
    /// Write batch shared with the parent transaction.
    batch: Arc<Mutex<rocksdb::WriteBatch>>,
    /// Phantom for `K` (transaction kind) and `T` (table).
    _phantom: std::marker::PhantomData<(K, T)>,
}

// Safety: All fields are Send (Arc<DB>, Arc<Mutex<WriteBatch>>). The raw iterator
// holds a pointer into the DB's state which is thread-safe by RocksDB's guarantees.
unsafe impl<K: TransactionKind, T: Table> Send for Cursor<K, T> {}

impl<K: TransactionKind, T: Table> Cursor<K, T> {
    /// Creates a new cursor for the given table's column family.
    pub(crate) fn new(
        db: Arc<DB>,
        batch: Arc<Mutex<rocksdb::WriteBatch>>,
    ) -> Result<Self, DatabaseError> {
        let cf_handle = get_cf_handle::<T>(&db)?;
        let iter = db.raw_iterator_cf(cf_handle);
        // Safety: Arc<DB> guarantees the DB outlives this cursor and its iterator.
        let iter: rocksdb::DBRawIterator<'static> = unsafe { std::mem::transmute(iter) };
        Ok(Self { db, iter, batch, _phantom: std::marker::PhantomData })
    }

    /// Encoded primary-key byte length for this table.
    ///
    /// Uses `size_of::<<T::Key as Encode>::Encoded>()`, which is correct for all
    /// fixed-size `Encoded` types (`[u8; N]`). All DupSort table keys in reth use
    /// fixed-size encodings.
    fn key_encode_len() -> usize {
        let len = std::mem::size_of::<<T::Key as Encode>::Encoded>();
        debug_assert!(
            len > 0 && len <= 64,
            "Unexpected key_encode_len {} for table {} — likely a variable-length Encoded type",
            len,
            T::NAME,
        );
        len
    }

    /// Decodes the key-value pair at the current iterator position.
    ///
    /// For DupSort tables: extracts the primary key from the first `key_encode_len()`
    /// bytes of the composite key.
    /// For regular tables: the full iterator key **is** the encoded primary key.
    fn decode_current(&self) -> Result<Option<(T::Key, T::Value)>, DatabaseError> {
        if !self.iter.valid() {
            return Ok(None);
        }

        let (raw_key, raw_value) = match (self.iter.key(), self.iter.value()) {
            (Some(k), Some(v)) => (k, v),
            _ => return Ok(None),
        };

        let key_bytes = if T::DUPSORT {
            let key_len = Self::key_encode_len();
            if raw_key.len() < key_len {
                return Err(DatabaseError::Decode);
            }
            &raw_key[..key_len]
        } else {
            raw_key
        };

        let key = T::Key::decode(key_bytes).map_err(|_| DatabaseError::Decode)?;
        let value = T::Value::decompress(raw_value).map_err(|_| DatabaseError::Decode)?;
        Ok(Some((key, value)))
    }
}

// ========================================================================
// DbCursorRO
// ========================================================================

impl<K: TransactionKind, T: Table> DbCursorRO<T> for Cursor<K, T> {
    fn first(&mut self) -> Result<Option<(T::Key, T::Value)>, DatabaseError> {
        self.iter.seek_to_first();
        self.decode_current()
    }

    fn seek_exact(&mut self, key: T::Key) -> Result<Option<(T::Key, T::Value)>, DatabaseError> {
        let encoded_key = key.encode();
        let key_ref = encoded_key.as_ref();

        if T::DUPSORT {
            // Seek by key prefix; first composite key >= key is a match iff its prefix equals key.
            self.iter.seek(key_ref);
            if self.iter.valid() {
                if let Some(k) = self.iter.key() {
                    let key_len = Self::key_encode_len();
                    if k.len() >= key_len && &k[..key_len] == key_ref {
                        return self.decode_current();
                    }
                }
            }
            Ok(None)
        } else {
            self.iter.seek(key_ref);
            if self.iter.valid() {
                if let Some(k) = self.iter.key() {
                    if k == key_ref {
                        return self.decode_current();
                    }
                }
            }
            Ok(None)
        }
    }

    fn seek(&mut self, key: T::Key) -> Result<Option<(T::Key, T::Value)>, DatabaseError> {
        let encoded_key = key.encode();
        self.iter.seek(encoded_key.as_ref());
        self.decode_current()
    }

    fn next(&mut self) -> Result<Option<(T::Key, T::Value)>, DatabaseError> {
        if self.iter.valid() {
            self.iter.next();
        }
        self.decode_current()
    }

    fn prev(&mut self) -> Result<Option<(T::Key, T::Value)>, DatabaseError> {
        if self.iter.valid() {
            self.iter.prev();
        }
        self.decode_current()
    }

    fn last(&mut self) -> Result<Option<(T::Key, T::Value)>, DatabaseError> {
        self.iter.seek_to_last();
        self.decode_current()
    }

    fn current(&mut self) -> Result<Option<(T::Key, T::Value)>, DatabaseError> {
        self.decode_current()
    }

    fn walk(
        &mut self,
        start_key: Option<T::Key>,
    ) -> Result<Walker<'_, T, Self>, DatabaseError>
    where
        Self: Sized,
    {
        let start = if let Some(key) = start_key {
            self.seek(key).transpose()
        } else {
            self.first().transpose()
        };
        Ok(Walker::new(self, start))
    }

    fn walk_range(
        &mut self,
        range: impl std::ops::RangeBounds<T::Key>,
    ) -> Result<RangeWalker<'_, T, Self>, DatabaseError>
    where
        Self: Sized,
    {
        let start = match range.start_bound().cloned() {
            Bound::Included(key) => self.seek(key).transpose(),
            Bound::Excluded(key) => {
                // Seek to the excluded key, then skip past all matching entries.
                let mut result = self.seek(key.clone())?;
                if result.as_ref().is_some_and(|e| e.0 == key) {
                    loop {
                        result = self.next()?;
                        match &result {
                            Some(e) if e.0 == key => continue,
                            _ => break,
                        }
                    }
                }
                result.map(Ok)
            }
            Bound::Unbounded => self.first().transpose(),
        };
        Ok(RangeWalker::new(self, start, range.end_bound().cloned()))
    }

    fn walk_back(
        &mut self,
        start_key: Option<T::Key>,
    ) -> Result<ReverseWalker<'_, T, Self>, DatabaseError>
    where
        Self: Sized,
    {
        let start = if let Some(key) = start_key {
            self.seek(key).transpose()
        } else {
            self.last().transpose()
        };
        Ok(ReverseWalker::new(self, start))
    }
}

// ========================================================================
// DbDupCursorRO
// ========================================================================

impl<K: TransactionKind, T: DupSort> DbDupCursorRO<T> for Cursor<K, T> {
    fn prev_dup(&mut self) -> Result<Option<(T::Key, T::Value)>, DatabaseError> {
        if !self.iter.valid() {
            return Ok(None);
        }

        let key_len = Self::key_encode_len();
        let current_prefix = match self.iter.key() {
            Some(k) if k.len() >= key_len => k[..key_len].to_vec(),
            _ => return Ok(None),
        };

        self.iter.prev();
        if !self.iter.valid() {
            return Ok(None);
        }

        if let Some(k) = self.iter.key() {
            if k.len() >= key_len && k[..key_len] == *current_prefix {
                return self.decode_current();
            }
        }
        Ok(None)
    }

    fn next_dup(&mut self) -> Result<Option<(T::Key, T::Value)>, DatabaseError> {
        if !self.iter.valid() {
            return Ok(None);
        }

        let key_len = Self::key_encode_len();
        let current_prefix = match self.iter.key() {
            Some(k) if k.len() >= key_len => k[..key_len].to_vec(),
            _ => return Ok(None),
        };

        self.iter.next();
        if !self.iter.valid() {
            return Ok(None);
        }

        if let Some(k) = self.iter.key() {
            if k.len() >= key_len && k[..key_len] == *current_prefix {
                return self.decode_current();
            }
        }
        Ok(None)
    }

    fn last_dup(&mut self) -> Result<Option<T::Value>, DatabaseError> {
        if !self.iter.valid() {
            return Ok(None);
        }

        let key_len = Self::key_encode_len();
        let current_prefix = match self.iter.key() {
            Some(k) if k.len() >= key_len => k[..key_len].to_vec(),
            _ => return Ok(None),
        };

        // Advance to the last entry sharing this primary key.
        loop {
            self.iter.next();
            if !self.iter.valid() {
                break;
            }
            match self.iter.key() {
                Some(k) if k.len() >= key_len && k[..key_len] == *current_prefix => continue,
                _ => break,
            }
        }
        // Step back to the last dup.
        self.iter.prev();

        if self.iter.valid() {
            if let Some(v) = self.iter.value() {
                return T::Value::decompress(v).map(Some).map_err(|_| DatabaseError::Decode);
            }
        }
        Ok(None)
    }

    fn next_no_dup(&mut self) -> Result<Option<(T::Key, T::Value)>, DatabaseError> {
        if !self.iter.valid() {
            return Ok(None);
        }

        let key_len = Self::key_encode_len();
        let current_prefix = match self.iter.key() {
            Some(k) if k.len() >= key_len => k[..key_len].to_vec(),
            _ => return Ok(None),
        };

        loop {
            self.iter.next();
            if !self.iter.valid() {
                return Ok(None);
            }
            match self.iter.key() {
                Some(k) if k.len() >= key_len && k[..key_len] == *current_prefix => continue,
                Some(_) => return self.decode_current(),
                None => return Ok(None),
            }
        }
    }

    fn next_dup_val(&mut self) -> Result<Option<T::Value>, DatabaseError> {
        self.next_dup().map(|opt| opt.map(|(_, v)| v))
    }

    fn seek_by_key_subkey(
        &mut self,
        key: T::Key,
        subkey: T::SubKey,
    ) -> Result<Option<T::Value>, DatabaseError> {
        let encoded_key = key.encode();
        let encoded_subkey = subkey.encode();
        let key_ref = encoded_key.as_ref();
        let subkey_ref = encoded_subkey.as_ref();

        // Seek target: primary_key || subkey (prefix of composite keys).
        let mut seek_target = Vec::with_capacity(key_ref.len() + subkey_ref.len());
        seek_target.extend_from_slice(key_ref);
        seek_target.extend_from_slice(subkey_ref);

        self.iter.seek(&seek_target);
        if !self.iter.valid() {
            return Ok(None);
        }

        // Verify the found entry shares the same primary key.
        let key_len = Self::key_encode_len();
        if let Some(k) = self.iter.key() {
            if k.len() >= key_len && &k[..key_len] == key_ref {
                if let Some(v) = self.iter.value() {
                    return T::Value::decompress(v)
                        .map(Some)
                        .map_err(|_| DatabaseError::Decode);
                }
            }
        }
        Ok(None)
    }

    fn walk_dup(
        &mut self,
        key: Option<T::Key>,
        subkey: Option<T::SubKey>,
    ) -> Result<DupWalker<'_, T, Self>, DatabaseError>
    where
        Self: Sized,
    {
        let start = match (key, subkey) {
            (Some(key), Some(subkey)) => {
                let val = self.seek_by_key_subkey(key.clone(), subkey)?;
                val.map(|v| Ok((key, v)))
            }
            (Some(key), None) => self.seek_exact(key).transpose(),
            _ => self.first().transpose(),
        };
        Ok(DupWalker { cursor: self, start })
    }
}

// ========================================================================
// DbCursorRW
// ========================================================================

impl<T: Table> DbCursorRW<T> for Cursor<RW, T> {
    fn upsert(&mut self, key: T::Key, value: &T::Value) -> Result<(), DatabaseError> {
        let cf_handle = get_cf_handle::<T>(&self.db)?;
        let encoded_key = key.encode();
        let mut compressed = <T::Value as Compress>::Compressed::default();
        value.compress_to_buf(&mut compressed);

        let mut batch = self.batch.lock();
        if T::DUPSORT {
            let key_ref = encoded_key.as_ref();
            let compressed_ref = compressed.as_ref();
            let mut composite = Vec::with_capacity(key_ref.len() + compressed_ref.len());
            composite.extend_from_slice(key_ref);
            composite.extend_from_slice(compressed_ref);
            batch.put_cf(cf_handle, &composite, compressed_ref);
        } else {
            batch.put_cf(cf_handle, encoded_key.as_ref(), compressed.as_ref());
        }
        Ok(())
    }

    fn insert(&mut self, key: T::Key, value: &T::Value) -> Result<(), DatabaseError> {
        self.upsert(key, value)
    }

    fn append(&mut self, key: T::Key, value: &T::Value) -> Result<(), DatabaseError> {
        self.upsert(key, value)
    }

    fn delete_current(&mut self) -> Result<(), DatabaseError> {
        if !self.iter.valid() {
            return Ok(());
        }

        let raw_key = match self.iter.key() {
            Some(k) => k.to_vec(),
            None => return Ok(()),
        };

        let cf_handle = get_cf_handle::<T>(&self.db)?;
        let mut batch = self.batch.lock();
        batch.delete_cf(cf_handle, &raw_key);
        drop(batch);

        self.iter.next();
        Ok(())
    }
}

// ========================================================================
// DbDupCursorRW
// ========================================================================

impl<T: DupSort> DbDupCursorRW<T> for Cursor<RW, T> {
    fn delete_current_duplicates(&mut self) -> Result<(), DatabaseError> {
        if !self.iter.valid() {
            return Ok(());
        }

        let key_len = Self::key_encode_len();
        let current_prefix = match self.iter.key() {
            Some(k) if k.len() >= key_len => k[..key_len].to_vec(),
            _ => return Ok(()),
        };

        let cf_handle = get_cf_handle::<T>(&self.db)?;
        let mut batch = self.batch.lock();

        while self.iter.valid() {
            match self.iter.key() {
                Some(k) if k.len() >= key_len && k[..key_len] == *current_prefix => {
                    batch.delete_cf(cf_handle, k);
                    self.iter.next();
                }
                _ => break,
            }
        }
        Ok(())
    }

    fn append_dup(&mut self, key: T::Key, value: T::Value) -> Result<(), DatabaseError> {
        self.upsert(key, &value)
    }
}

// ========================================================================
// Debug
// ========================================================================

impl<K: TransactionKind, T: Table> std::fmt::Debug for Cursor<K, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cursor")
            .field("table", &T::NAME)
            .field("kind", &std::any::type_name::<K>())
            .finish()
    }
}
