#![allow(unused_variables)] // TODO(you): remove this lint after implementing this mod
#![allow(dead_code)] // TODO(you): remove this lint after implementing this mod

pub(crate) mod bloom;
mod builder;
mod iterator;

use std::fs::File;
use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, Result};
pub use builder::SsTableBuilder;
use bytes::{Buf, BufMut};
pub use iterator::SsTableIterator;

use crate::block::Block;
use crate::key::{KeyBytes, KeySlice};
use crate::lsm_storage::BlockCache;

use self::bloom::Bloom;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockMeta {
    /// Offset of this data block.
    pub offset: usize,
    /// The first key of the data block.
    pub first_key: KeyBytes,
    /// The last key of the data block.
    pub last_key: KeyBytes,
}

impl BlockMeta {
    /// Encode block meta to a buffer.
    /// You may add extra fields to the buffer,
    /// in order to help keep track of `first_key` when decoding from the same buffer in the future.
    pub fn encode_block_meta(block_meta: &[BlockMeta], buf: &mut Vec<u8>) {
        block_meta.iter().for_each(|meta| {
            buf.put_u32(meta.offset as u32);
            buf.put_u32(meta.first_key.len() as u32);
            buf.put_slice(meta.first_key.raw_ref());
            buf.put_u32(meta.last_key.len() as u32);
            buf.put_slice(meta.last_key.raw_ref());
        })
    }

    /// Decode block meta from a buffer.
    pub fn decode_block_meta(mut buf: impl Buf) -> Vec<BlockMeta> {
        let mut res = Vec::new();

        while buf.remaining() > 0 {
            let offset = buf.get_u32() as usize;
            let first_key_len = buf.get_u32() as usize;
            let first_key = KeyBytes::from_bytes(buf.copy_to_bytes(first_key_len));
            let last_key_len = buf.get_u32() as usize;
            let last_key = KeyBytes::from_bytes(buf.copy_to_bytes(last_key_len));
            res.push(BlockMeta {
                offset,
                first_key,
                last_key,
            })
        }

        res
    }
}

/// A file object.
pub struct FileObject(Option<File>, u64);

impl FileObject {
    pub fn read(&self, offset: u64, len: u64) -> Result<Vec<u8>> {
        use std::os::unix::fs::FileExt;
        let mut data = vec![0; len as usize];
        self.0
            .as_ref()
            .unwrap()
            .read_exact_at(&mut data[..], offset)?;
        Ok(data)
    }

    pub fn size(&self) -> u64 {
        self.1
    }

    /// Create a new file object (day 2) and write the file to the disk (day 4).
    pub fn create(path: &Path, data: Vec<u8>) -> Result<Self> {
        std::fs::write(path, &data)?;
        File::open(path)?.sync_all()?;
        Ok(FileObject(
            Some(File::options().read(true).write(false).open(path)?),
            data.len() as u64,
        ))
    }

    pub fn open(path: &Path) -> Result<Self> {
        let file = File::options().read(true).write(false).open(path)?;
        let size = file.metadata()?.len();
        Ok(FileObject(Some(file), size))
    }
}

/// An SSTable.
pub struct SsTable {
    /// The actual storage unit of SsTable, the format is as above.
    pub(crate) file: FileObject,
    /// The meta blocks that hold info for data blocks.
    pub(crate) block_meta: Vec<BlockMeta>,
    /// The offset that indicates the start point of meta blocks in `file`.
    pub(crate) block_meta_offset: usize,
    id: usize,
    block_cache: Option<Arc<BlockCache>>,
    first_key: KeyBytes,
    last_key: KeyBytes,
    pub(crate) bloom: Option<Bloom>,
    /// The maximum timestamp stored in this SST, implemented in week 3.
    max_ts: u64,
}

impl SsTable {
    #[cfg(test)]
    pub(crate) fn open_for_test(file: FileObject) -> Result<Self> {
        Self::open(0, None, file)
    }

    /// Open SSTable from a file.
    pub fn open(id: usize, block_cache: Option<Arc<BlockCache>>, file: FileObject) -> Result<Self> {
        let file_size = file.size();
        // Step 1: Read bloom filter at the end
        let bloom_offset = file.read(
            file_size - std::mem::size_of::<u32>() as u64,
            std::mem::size_of::<u32>() as u64,
        )?;

        let bloom_offset = bloom_offset.as_slice().get_u32() as u64;

        let bloom_buffer = file.read(
            bloom_offset,
            file_size - bloom_offset - std::mem::size_of::<u32>() as u64,
        )?;
        let bloom = Bloom::decode(&bloom_buffer)?;

        let block_meta_offset = file.read(
            bloom_offset - std::mem::size_of::<u32>() as u64,
            std::mem::size_of::<u32>() as u64,
        )?;
        let block_meta_offset = block_meta_offset.as_slice().get_u32() as u64;
        let block_meta_buffer = file.read(
            block_meta_offset,
            bloom_offset - block_meta_offset - std::mem::size_of::<u32>() as u64,
        )?;
        let block_meta = BlockMeta::decode_block_meta(&block_meta_buffer[..]);
        let first_key = block_meta.first().unwrap().first_key.clone();
        let last_key = block_meta.last().unwrap().last_key.clone();

        Ok(Self {
            file,
            block_meta,
            block_meta_offset: block_meta_offset as usize,
            id,
            block_cache,
            first_key,
            last_key,
            bloom: Some(bloom),
            max_ts: u64::MAX,
        })
    }

    /// Create a mock SST with only first key + last key metadata
    pub fn create_meta_only(
        id: usize,
        file_size: u64,
        first_key: KeyBytes,
        last_key: KeyBytes,
    ) -> Self {
        Self {
            file: FileObject(None, file_size),
            block_meta: vec![],
            block_meta_offset: 0,
            id,
            block_cache: None,
            first_key,
            last_key,
            bloom: None,
            max_ts: 0,
        }
    }

    /// Read a block from the disk.
    pub fn read_block(&self, block_idx: usize) -> Result<Arc<Block>> {
        let offset = self.block_meta[block_idx].offset;
        let next_offset = match self.block_meta.get(block_idx + 1) {
            Some(next_meta) => next_meta.offset,
            None => self.block_meta_offset,
        };
        let block_len = (next_offset - offset) as u64;
        let block_buffer = self.file.read(offset as u64, block_len)?;
        let block = Block::decode(&block_buffer);
        Ok(Arc::new(block))
    }

    /// Read a block from disk, with block cache. (Day 4)
    pub fn read_block_cached(&self, block_idx: usize) -> Result<Arc<Block>> {
        let Some(cache) = self.block_cache.as_deref() else {
            return self.read_block(block_idx);
        };
        let offset = self.block_meta[block_idx].offset;
        cache
            .try_get_with((self.id, offset), || self.read_block(block_idx))
            .map_err(|error| anyhow!("{}", error))
    }

    /// Find the block that may contain `key`.
    /// Note: You may want to make use of the `first_key` stored in `BlockMeta`.
    /// You may also assume the key-value pairs stored in each consecutive block are sorted.
    pub fn find_block_idx(&self, key: KeySlice) -> usize {
        self.block_meta
            .binary_search_by(|meta| match meta.last_key.as_key_slice().cmp(&key) {
                std::cmp::Ordering::Equal => std::cmp::Ordering::Greater,
                ord => ord,
            })
            .unwrap_err()
    }

    /// Get number of data blocks.
    pub fn num_of_blocks(&self) -> usize {
        self.block_meta.len()
    }

    pub fn first_key(&self) -> &KeyBytes {
        &self.first_key
    }

    pub fn last_key(&self) -> &KeyBytes {
        &self.last_key
    }

    pub fn table_size(&self) -> u64 {
        self.file.1
    }

    pub fn sst_id(&self) -> usize {
        self.id
    }

    pub fn max_ts(&self) -> u64 {
        self.max_ts
    }

    pub fn range_overlap(&self, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> bool {
        let last_key = self.last_key.as_key_slice();
        let first_key = self.first_key.as_key_slice();

        let lower_out_of_bound = match lower {
            Bound::Included(s) => KeySlice::from_slice(s) > last_key,
            Bound::Excluded(s) => KeySlice::from_slice(s) >= last_key,
            Bound::Unbounded => false,
        };

        let upper_out_of_bound = match upper {
            Bound::Included(s) => KeySlice::from_slice(s) < first_key,
            Bound::Excluded(s) => KeySlice::from_slice(s) <= first_key,
            Bound::Unbounded => false,
        };

        !lower_out_of_bound && !upper_out_of_bound
    }
}
