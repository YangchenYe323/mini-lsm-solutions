#![allow(unused_variables)] // TODO(you): remove this lint after implementing this mod
#![allow(dead_code)] // TODO(you): remove this lint after implementing this mod

use std::{
    collections::HashSet,
    ops::Bound,
    sync::{atomic::AtomicBool, Arc},
};

use anyhow::{anyhow, Result};
use bytes::Bytes;
use crossbeam_skiplist::SkipMap;
use ouroboros::self_referencing;
use parking_lot::{Mutex, MutexGuard};

use crate::{
    iterators::{two_merge_iterator::TwoMergeIterator, StorageIterator},
    lsm_iterator::{FusedIterator, LsmIterator},
    lsm_storage::{LsmStorageInner, WriteBatchRecord},
    mem_table::map_bytes_bound,
};

use super::CommittedTxnData;

pub struct Transaction {
    pub(crate) read_ts: u64,
    pub(crate) inner: Arc<LsmStorageInner>,
    pub(crate) local_storage: Arc<SkipMap<Bytes, Bytes>>,
    pub(crate) committed: Arc<AtomicBool>,
    /// Write set and read set
    pub(crate) key_hashes: Option<Mutex<(HashSet<u32>, HashSet<u32>)>>,
}

impl Transaction {
    pub fn get(&self, key: &[u8]) -> Result<Option<Bytes>> {
        self.abort_if_committed();

        if let Some(key_hashes) = &self.key_hashes {
            let mut guard = key_hashes.lock();
            guard.1.insert(farmhash::hash32(key));
        }

        if let Some(entry) = self.local_storage.get(key) {
            if entry.value().is_empty() {
                return Ok(None);
            }
            return Ok(Some(Bytes::copy_from_slice(entry.value())));
        }
        self.inner.get_with_ts(key, self.read_ts)
    }

    pub fn scan(self: &Arc<Self>, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> Result<TxnIterator> {
        self.abort_if_committed();

        let lsm_iter = self.inner.scan_with_ts(lower, upper, self.read_ts)?;
        let mut local_iter = TxnLocalIterator::new(
            Arc::clone(&self.local_storage),
            |map| map.range((map_bytes_bound(lower), map_bytes_bound(upper))),
            (Bytes::from_static(&[]), Bytes::from_static(&[])),
        );
        local_iter.next()?;

        let iter = TwoMergeIterator::create(local_iter, lsm_iter)?;
        let txn_iter = TxnIterator::create(Arc::clone(self), iter)?;
        Ok(txn_iter)
    }

    pub fn put(&self, key: &[u8], value: &[u8]) {
        self.abort_if_committed();

        if let Some(key_hashes) = &self.key_hashes {
            let mut guard = key_hashes.lock();
            guard.0.insert(farmhash::hash32(key));
        }

        self.local_storage
            .insert(Bytes::copy_from_slice(key), Bytes::copy_from_slice(value));
    }

    pub fn delete(&self, key: &[u8]) {
        self.abort_if_committed();

        if let Some(key_hashes) = &self.key_hashes {
            let mut guard = key_hashes.lock();
            guard.0.insert(farmhash::hash32(key));
        }

        self.local_storage
            .insert(Bytes::copy_from_slice(key), Bytes::from_static(&[]));
    }

    pub fn commit(&self) -> Result<()> {
        let commit_lock = self.inner.mvcc().commit_lock.lock();
        self.validate_commit(&commit_lock)?;

        self.committed
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let record_batch: Vec<WriteBatchRecord<Bytes>> = self
            .local_storage
            .iter()
            .map(|entry| {
                if entry.value().is_empty() {
                    return WriteBatchRecord::Del(entry.key().clone());
                }

                WriteBatchRecord::Put(entry.key().clone(), entry.value().clone())
            })
            .collect();

        let commit_ts = self.inner.write_batch_inner(&record_batch)?;

        if let Some(key_hashes) = &self.key_hashes {
            let commit_data = CommittedTxnData {
                key_hashes: key_hashes.lock().0.clone(),
                read_ts: self.read_ts,
                commit_ts,
            };

            let mut committed_txns = self.inner.mvcc().committed_txns.lock();
            committed_txns.insert(commit_ts, commit_data);

            // Remove txns below the watermark
            for ts in committed_txns.keys().copied().collect::<Vec<_>>() {
                if ts < self.inner.mvcc().watermark() {
                    committed_txns.remove(&ts);
                }
            }
        }

        Ok(())
    }

    fn validate_commit(&self, _commit_lock: &MutexGuard<()>) -> Result<()> {
        let Some(key_hashes) = &self.key_hashes else {
            return Ok(());
        };

        let guard = key_hashes.lock();

        if guard.0.is_empty() {
            // read-only txns
            return Ok(());
        }

        let txn_start_ts = self.read_ts;
        let expected_commit_ts = self.inner.mvcc().latest_commit_ts() + 1;

        let commited_txns = self.inner.mvcc().committed_txns.lock();
        let earlier_txns = commited_txns.range((
            Bound::Included(txn_start_ts),
            Bound::Excluded(expected_commit_ts),
        ));

        for (_, earlier_txn) in earlier_txns {
            println!("{:?}", earlier_txn.key_hashes);
            // Abort if earlier-txn's write set intersects with our read set
            if earlier_txn
                .key_hashes
                .intersection(&guard.1)
                .next()
                .is_some()
            {
                return Err(anyhow!("Abort transaction"));
            }
        }

        Ok(())
    }

    fn abort_if_committed(&self) {
        assert!(
            !self.committed.load(std::sync::atomic::Ordering::Relaxed),
            "Could not perform operation on committed transactions"
        );
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        let mut ts = self.inner.mvcc().ts.lock();
        ts.1.remove_reader(self.read_ts);
    }
}

type SkipMapRangeIter<'a> =
    crossbeam_skiplist::map::Range<'a, Bytes, (Bound<Bytes>, Bound<Bytes>), Bytes, Bytes>;

#[self_referencing]
pub struct TxnLocalIterator {
    /// Stores a reference to the skipmap.
    map: Arc<SkipMap<Bytes, Bytes>>,
    /// Stores a skipmap iterator that refers to the lifetime of `MemTableIterator` itself.
    #[borrows(map)]
    #[not_covariant]
    iter: SkipMapRangeIter<'this>,
    /// Stores the current key-value pair.
    item: (Bytes, Bytes),
}

impl StorageIterator for TxnLocalIterator {
    type KeyType<'a> = &'a [u8];

    fn value(&self) -> &[u8] {
        &self.borrow_item().1
    }

    fn key(&self) -> &[u8] {
        &self.borrow_item().0
    }

    fn is_valid(&self) -> bool {
        !self.borrow_item().0.is_empty()
    }

    fn next(&mut self) -> Result<()> {
        let next_item = self.with_iter_mut(|iter| {
            iter.next()
                .map(|entry| (entry.key().clone(), entry.value().clone()))
        });

        if let Some(next_item) = next_item {
            self.with_item_mut(|it| *it = next_item);
        } else {
            self.with_item_mut(|it| *it = (Bytes::from_static(&[]), Bytes::from_static(&[])));
        }

        Ok(())
    }
}

pub struct TxnIterator {
    txn: Arc<Transaction>,
    iter: TwoMergeIterator<TxnLocalIterator, FusedIterator<LsmIterator>>,
}

impl TxnIterator {
    pub fn create(
        txn: Arc<Transaction>,
        iter: TwoMergeIterator<TxnLocalIterator, FusedIterator<LsmIterator>>,
    ) -> Result<Self> {
        Ok(Self { txn, iter })
    }
}

impl StorageIterator for TxnIterator {
    type KeyType<'a> = &'a [u8] where Self: 'a;

    fn value(&self) -> &[u8] {
        self.iter.value()
    }

    fn key(&self) -> Self::KeyType<'_> {
        self.iter.key()
    }

    fn is_valid(&self) -> bool {
        self.iter.is_valid()
    }

    fn next(&mut self) -> Result<()> {
        if let Some(key_hashes) = &self.txn.key_hashes {
            let mut guard = key_hashes.lock();
            guard.1.insert(farmhash::hash32(self.iter.key()));
        }

        self.iter.next()?;
        // Skip deleted entries. This is necessary as TxnLocalIterators will return
        // deletion tombstones even if LsmIterator has its internal deletion handling
        while self.iter.is_valid() && self.iter.value().is_empty() {
            self.iter.next()?;
        }

        Ok(())
    }

    fn num_active_iterators(&self) -> usize {
        self.iter.num_active_iterators()
    }
}
