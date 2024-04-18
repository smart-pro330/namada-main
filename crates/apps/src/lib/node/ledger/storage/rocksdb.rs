//! The persistent storage in RocksDB.
//!
//! The current storage tree is:
//! - `state`: the latest ledger state
//!   - `ethereum_height`: the height of the last eth block processed by the
//!     oracle
//!   - `eth_events_queue`: a queue of confirmed ethereum events to be processed
//!     in order
//!   - `height`: the last committed block height
//!   - `tx_queue`: txs to be decrypted in the next block
//!   - `next_epoch_min_start_height`: minimum block height from which the next
//!     epoch can start
//!   - `next_epoch_min_start_time`: minimum block time from which the next
//!     epoch can start
//!   - `update_epoch_blocks_delay`: number of missing blocks before updating
//!     PoS with CometBFT
//!   - `pred`: predecessor values of the top-level keys of the same name
//!     - `tx_queue`
//!     - `next_epoch_min_start_height`
//!     - `next_epoch_min_start_time`
//!     - `update_epoch_blocks_delay`
//!   - `conversion_state`: MASP conversion state
//! - `subspace`: accounts sub-spaces
//!   - `{address}/{dyn}`: any byte data associated with accounts
//! - `diffs`: diffs in account subspaces' key-vals
//!   - `new/{dyn}`: value set in block height `h`
//!   - `old/{dyn}`: value from predecessor block height
//! - `block`: block state
//!   - `results/{h}`: block results at height `h`
//!   - `h`: for each block at height `h`:
//!     - `tree`: merkle tree
//!       - `root`: root hash
//!       - `store`: the tree's store
//!     - `hash`: block hash
//!     - `time`: block time
//!     - `epoch`: block epoch
//!     - `address_gen`: established address generator
//!     - `header`: block's header
//! - `replay_protection`: hashes of processed tx
//!     - `all`: the hashes included up to the last block
//!     - `last`: the hashes included in the last block

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::str::FromStr;
use std::sync::Mutex;

use borsh::BorshDeserialize;
use borsh_ext::BorshSerializeExt;
use data_encoding::HEXLOWER;
use itertools::Either;
use namada::core::storage::{
    BlockHeight, BlockResults, Epoch, EthEventsQueue, Header, Key, KeySeg,
    KEY_SEGMENT_SEPARATOR,
};
use namada::core::time::DateTimeUtc;
use namada::core::{decode, encode, ethereum_events, ethereum_structs};
use namada::eth_bridge::storage::proof::BridgePoolRootProof;
use namada::ledger::eth_bridge::storage::bridge_pool;
use namada::ledger::storage::tx_queue::TxQueue;
use namada::replay_protection;
use namada::state::merkle_tree::{base_tree_key_prefix, subtree_key_prefix};
use namada::state::{
    BlockStateRead, BlockStateWrite, DBIter, DBWriteBatch, DbError as Error,
    DbResult as Result, MerkleTreeStoresRead, PatternIterator, PrefixIterator,
    StoreType, DB,
};
use namada::storage::{
    DbColFam, BLOCK_CF, DIFFS_CF, REPLAY_PROTECTION_CF, STATE_CF, SUBSPACE_CF,
};
use namada::token::ConversionState;
use namada_sdk::migrations::DBUpdateVisitor;
use rayon::prelude::*;
use regex::Regex;
use rocksdb::{
    BlockBasedOptions, ColumnFamily, ColumnFamilyDescriptor, DBCompactionStyle,
    DBCompressionType, Direction, FlushOptions, IteratorMode, Options,
    ReadOptions, WriteBatch,
};

use crate::config::utils::num_of_threads;

// TODO the DB schema will probably need some kind of versioning

/// Env. var to set a number of Rayon global worker threads
const ENV_VAR_ROCKSDB_COMPACTION_THREADS: &str =
    "NAMADA_ROCKSDB_COMPACTION_THREADS";

const OLD_DIFF_PREFIX: &str = "old";
const NEW_DIFF_PREFIX: &str = "new";

/// RocksDB handle
#[derive(Debug)]
pub struct RocksDB(rocksdb::DB);

/// DB Handle for batch writes.
#[derive(Default)]
pub struct RocksDBWriteBatch(WriteBatch);

/// Open RocksDB for the DB
pub fn open(
    path: impl AsRef<Path>,
    cache: Option<&rocksdb::Cache>,
) -> Result<RocksDB> {
    let logical_cores = num_cpus::get();
    let compaction_threads = num_of_threads(
        ENV_VAR_ROCKSDB_COMPACTION_THREADS,
        // If not set, default to quarter of logical CPUs count
        logical_cores / 4,
    ) as i32;
    tracing::info!(
        "Using {} compactions threads for RocksDB.",
        compaction_threads
    );

    // DB options
    let mut db_opts = Options::default();

    // This gives `compaction_threads` number to compaction threads and 1 thread
    // for flush background jobs: https://github.com/facebook/rocksdb/blob/17ce1ca48be53ba29138f92dafc9c853d9241377/options/options.cc#L622
    db_opts.increase_parallelism(compaction_threads);

    db_opts.set_bytes_per_sync(1048576);
    set_max_open_files(&mut db_opts);

    // TODO the recommended default `options.compaction_pri =
    // kMinOverlappingRatio` doesn't seem to be available in Rust

    db_opts.create_missing_column_families(true);
    db_opts.create_if_missing(true);
    db_opts.set_atomic_flush(true);

    let mut cfs = Vec::new();
    let mut table_opts = BlockBasedOptions::default();
    table_opts.set_block_size(16 * 1024);
    table_opts.set_cache_index_and_filter_blocks(true);
    table_opts.set_pin_l0_filter_and_index_blocks_in_cache(true);
    if let Some(cache) = cache {
        table_opts.set_block_cache(cache);
    }
    // latest format versions https://github.com/facebook/rocksdb/blob/d1c510baecc1aef758f91f786c4fbee3bc847a63/include/rocksdb/table.h#L394
    table_opts.set_format_version(5);

    // for subspace (read/update-intensive)
    let mut subspace_cf_opts = Options::default();
    subspace_cf_opts.set_compression_type(DBCompressionType::Zstd);
    subspace_cf_opts.set_compression_options(0, 0, 0, 1024 * 1024);
    // ! recommended initial setup https://github.com/facebook/rocksdb/wiki/Setup-Options-and-Basic-Tuning#other-general-options
    subspace_cf_opts.set_level_compaction_dynamic_level_bytes(true);
    subspace_cf_opts.set_compaction_style(DBCompactionStyle::Level);
    subspace_cf_opts.set_block_based_table_factory(&table_opts);
    cfs.push(ColumnFamilyDescriptor::new(SUBSPACE_CF, subspace_cf_opts));

    // for diffs (insert-intensive)
    let mut diffs_cf_opts = Options::default();
    diffs_cf_opts.set_compression_type(DBCompressionType::Zstd);
    diffs_cf_opts.set_compression_options(0, 0, 0, 1024 * 1024);
    diffs_cf_opts.set_compaction_style(DBCompactionStyle::Universal);
    diffs_cf_opts.set_block_based_table_factory(&table_opts);
    cfs.push(ColumnFamilyDescriptor::new(DIFFS_CF, diffs_cf_opts));

    // for the ledger state (update-intensive)
    let mut state_cf_opts = Options::default();
    // No compression since the size of the state is small
    state_cf_opts.set_level_compaction_dynamic_level_bytes(true);
    state_cf_opts.set_compaction_style(DBCompactionStyle::Level);
    state_cf_opts.set_block_based_table_factory(&table_opts);
    cfs.push(ColumnFamilyDescriptor::new(STATE_CF, state_cf_opts));

    // for blocks (insert-intensive)
    let mut block_cf_opts = Options::default();
    block_cf_opts.set_compression_type(DBCompressionType::Zstd);
    block_cf_opts.set_compression_options(0, 0, 0, 1024 * 1024);
    block_cf_opts.set_compaction_style(DBCompactionStyle::Universal);
    block_cf_opts.set_block_based_table_factory(&table_opts);
    cfs.push(ColumnFamilyDescriptor::new(BLOCK_CF, block_cf_opts));

    // for replay protection (read/insert-intensive)
    let mut replay_protection_cf_opts = Options::default();
    replay_protection_cf_opts.set_compression_type(DBCompressionType::Zstd);
    replay_protection_cf_opts.set_compression_options(0, 0, 0, 1024 * 1024);
    replay_protection_cf_opts.set_level_compaction_dynamic_level_bytes(true);
    // Prioritize minimizing read amplification
    replay_protection_cf_opts.set_compaction_style(DBCompactionStyle::Level);
    replay_protection_cf_opts.set_block_based_table_factory(&table_opts);
    cfs.push(ColumnFamilyDescriptor::new(
        REPLAY_PROTECTION_CF,
        replay_protection_cf_opts,
    ));

    rocksdb::DB::open_cf_descriptors(&db_opts, path, cfs)
        .map(RocksDB)
        .map_err(|e| Error::DBError(e.into_string()))
}

impl Drop for RocksDB {
    fn drop(&mut self) {
        self.flush(true).expect("flush failed");
    }
}

impl RocksDB {
    fn get_column_family(&self, cf_name: &str) -> Result<&ColumnFamily> {
        self.0
            .cf_handle(cf_name)
            .ok_or(Error::DBError("No {cf_name} column family".to_string()))
    }

    /// Persist the diff of an account subspace key-val under the height where
    /// it was changed.
    fn write_subspace_diff(
        &self,
        height: BlockHeight,
        key: &Key,
        old_value: Option<&[u8]>,
        new_value: Option<&[u8]>,
        persist_diffs: bool,
    ) -> Result<()> {
        let cf = self.get_column_family(DIFFS_CF)?;
        let (old_val_key, new_val_key) = old_and_new_diff_key(key, height)?;

        if let Some(old_value) = old_value {
            self.0
                .put_cf(cf, old_val_key, old_value)
                .map_err(|e| Error::DBError(e.into_string()))?;
        }

        if let Some(new_value) = new_value {
            self.0
                .put_cf(cf, new_val_key, new_value)
                .map_err(|e| Error::DBError(e.into_string()))?;
        }

        // If not persisting the diffs, remove the last diffs.
        if !persist_diffs && height > BlockHeight::first() {
            let mut height = height.prev_height();
            while height >= BlockHeight::first() {
                let (old_diff_key, new_diff_key) =
                    old_and_new_diff_key(key, height)?;
                let has_old_diff = self
                    .0
                    .get_cf(cf, &old_diff_key)
                    .map_err(|e| Error::DBError(e.into_string()))?
                    .is_some();
                let has_new_diff = self
                    .0
                    .get_cf(cf, &new_diff_key)
                    .map_err(|e| Error::DBError(e.into_string()))?
                    .is_some();
                if has_old_diff {
                    self.0
                        .delete_cf(cf, old_diff_key)
                        .map_err(|e| Error::DBError(e.into_string()))?;
                }
                if has_new_diff {
                    self.0
                        .delete_cf(cf, new_diff_key)
                        .map_err(|e| Error::DBError(e.into_string()))?;
                }
                if has_old_diff || has_new_diff {
                    break;
                }
                height = height.prev_height();
            }
        }
        Ok(())
    }

    /// Persist the diff of an account subspace key-val under the height where
    /// it was changed in a batch write.
    fn batch_write_subspace_diff(
        &self,
        batch: &mut RocksDBWriteBatch,
        height: BlockHeight,
        key: &Key,
        old_value: Option<&[u8]>,
        new_value: Option<&[u8]>,
        persist_diffs: bool,
    ) -> Result<()> {
        let cf = self.get_column_family(DIFFS_CF)?;
        let (old_val_key, new_val_key) = old_and_new_diff_key(key, height)?;

        if let Some(old_value) = old_value {
            batch.0.put_cf(cf, old_val_key, old_value);
        }

        if let Some(new_value) = new_value {
            batch.0.put_cf(cf, new_val_key, new_value);
        }

        // If not persisting the diffs, remove the last diffs.
        if !persist_diffs && height > BlockHeight::first() {
            let mut height = height.prev_height();
            while height >= BlockHeight::first() {
                let (old_diff_key, new_diff_key) =
                    old_and_new_diff_key(key, height)?;
                let has_old_diff = self
                    .0
                    .get_cf(cf, &old_diff_key)
                    .map_err(|e| Error::DBError(e.into_string()))?
                    .is_some();
                let has_new_diff = self
                    .0
                    .get_cf(cf, &new_diff_key)
                    .map_err(|e| Error::DBError(e.into_string()))?
                    .is_some();
                if has_old_diff {
                    batch.0.delete_cf(cf, old_diff_key);
                }
                if has_new_diff {
                    batch.0.delete_cf(cf, new_diff_key);
                }
                if has_old_diff || has_new_diff {
                    break;
                }
                height = height.prev_height();
            }
        }
        Ok(())
    }

    fn exec_batch(&mut self, batch: WriteBatch) -> Result<()> {
        self.0
            .write(batch)
            .map_err(|e| Error::DBError(e.into_string()))
    }

    /// Dump last known block
    pub fn dump_block(
        &self,
        out_file_path: std::path::PathBuf,
        historic: bool,
        height: Option<BlockHeight>,
    ) {
        // Find the last block height
        let state_cf = self
            .get_column_family(STATE_CF)
            .expect("State column family should exist");

        let last_height: BlockHeight = decode(
            self.0
                .get_cf(state_cf, "height")
                .expect("Unable to read DB")
                .expect("No block height found"),
        )
        .expect("Unable to decode block height");

        let height = height.unwrap_or(last_height);

        let full_path = out_file_path
            .with_file_name(format!(
                "{}_{height}",
                out_file_path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "dump_db".to_string())
            ))
            .with_extension("toml");

        let mut file = File::options()
            .append(true)
            .create_new(true)
            .open(&full_path)
            .expect("Cannot open the output file");

        println!("Will write to {} ...", full_path.to_string_lossy());

        if historic {
            // Dump the keys prepended with the selected block height (includes
            // subspace diff keys)

            // Diffs
            let cf = self
                .get_column_family(DIFFS_CF)
                .expect("Diffs column family should exist");
            let prefix = height.raw();
            self.dump_it(cf, Some(prefix.clone()), &mut file);

            // Block
            let cf = self
                .get_column_family(BLOCK_CF)
                .expect("Block column family should exist");
            self.dump_it(cf, Some(prefix), &mut file);
        }

        // subspace
        if height != last_height {
            // Restoring subspace at specified height
            let restored_subspace = self
                .iter_prefix(None)
                .par_bridge()
                .fold(
                    || "".to_string(),
                    |mut cur, (key, _value, _gas)| match self
                        .read_subspace_val_with_height(
                            &Key::from(key.to_db_key()),
                            height,
                            last_height,
                        )
                        .expect("Unable to find subspace key")
                    {
                        Some(value) => {
                            let val = HEXLOWER.encode(&value);
                            let new_line = format!("\"{key}\" = \"{val}\"\n");
                            cur.push_str(new_line.as_str());
                            cur
                        }
                        None => cur,
                    },
                )
                .reduce(
                    || "".to_string(),
                    |mut a: String, b: String| {
                        a.push_str(&b);
                        a
                    },
                );
            file.write_all(restored_subspace.as_bytes())
                .expect("Unable to write to output file");
        } else {
            // Just dump the current subspace
            let cf = self
                .get_column_family(SUBSPACE_CF)
                .expect("Subspace column family should exist");
            self.dump_it(cf, None, &mut file);
        }

        // replay protection
        // Dump of replay protection keys is possible only at the last height or
        // the previous one
        if height == last_height {
            let cf = self
                .get_column_family(REPLAY_PROTECTION_CF)
                .expect("Replay protection column family should exist");
            self.dump_it(cf, None, &mut file);
        } else if height == last_height - 1 {
            let cf = self
                .get_column_family(REPLAY_PROTECTION_CF)
                .expect("Replay protection column family should exist");
            self.dump_it(cf, Some("all".to_string()), &mut file);
        }

        println!("Done writing to {}", full_path.to_string_lossy());
    }

    /// Dump data
    fn dump_it(
        &self,
        cf: &ColumnFamily,
        prefix: Option<String>,
        file: &mut File,
    ) {
        let read_opts = make_iter_read_opts(prefix.clone());
        let iter = if let Some(prefix) = prefix {
            self.0.iterator_cf_opt(
                cf,
                read_opts,
                IteratorMode::From(prefix.as_bytes(), Direction::Forward),
            )
        } else {
            self.0.iterator_cf_opt(cf, read_opts, IteratorMode::Start)
        };

        let mut buf = BufWriter::new(file);
        for (key, raw_val, _gas) in PersistentPrefixIterator(
            PrefixIterator::new(iter, String::default()),
            // Empty string to prevent prefix stripping, the prefix is
            // already in the enclosed iterator
        ) {
            let val = HEXLOWER.encode(&raw_val);
            let bytes = format!("\"{key}\" = \"{val}\"\n");
            buf.write_all(bytes.as_bytes())
                .expect("Unable to write to buffer");
        }
        buf.flush().expect("Unable to write to output file");
    }

    /// Rollback to previous block. Given the inner working of tendermint
    /// rollback and of the key structure of Namada, calling rollback more than
    /// once without restarting the chain results in a single rollback.
    pub fn rollback(
        &mut self,
        tendermint_block_height: BlockHeight,
    ) -> Result<()> {
        let last_block = self.read_last_block()?.ok_or(Error::DBError(
            "Missing last block in storage".to_string(),
        ))?;
        tracing::info!(
            "Namada last block height: {}, Tendermint last block height: {}",
            last_block.height,
            tendermint_block_height
        );

        // If the block height to which tendermint rolled back matches the
        // Namada height, there's no need to rollback
        if tendermint_block_height == last_block.height {
            tracing::info!(
                "Namada height already matches the rollback Tendermint \
                 height, no need to rollback."
            );
            return Ok(());
        }

        let mut batch = WriteBatch::default();
        let previous_height =
            BlockHeight::from(u64::from(last_block.height) - 1);

        let state_cf = self.get_column_family(STATE_CF)?;
        // Revert the non-height-prepended metadata storage keys which get
        // updated with every block. Because of the way we save these
        // three keys in storage we can only perform one rollback before
        // restarting the chain
        tracing::info!("Reverting non-height-prepended metadata keys");
        batch.put_cf(state_cf, "height", encode(&previous_height));
        for metadata_key in [
            "next_epoch_min_start_height",
            "next_epoch_min_start_time",
            "update_epoch_blocks_delay",
            "tx_queue",
        ] {
            let previous_key = format!("pred/{}", metadata_key);
            let previous_value = self
                .0
                .get_cf(state_cf, previous_key.as_bytes())
                .map_err(|e| Error::DBError(e.to_string()))?
                .ok_or(Error::UnknownKey { key: previous_key })?;

            batch.put_cf(state_cf, metadata_key, previous_value);
            // NOTE: we cannot restore the "pred/" keys themselves since we
            // don't have their predecessors in storage, but there's no need to
            // since we cannot do more than one rollback anyway because of
            // CometBFT.
        }

        // Revert conversion state if the epoch had been changed
        if last_block.pred_epochs.get_epoch(previous_height)
            != Some(last_block.epoch)
        {
            let previous_key = "pred/conversion_state".to_string();
            let previous_value = self
                .0
                .get_cf(state_cf, previous_key.as_bytes())
                .map_err(|e| Error::DBError(e.to_string()))?
                .ok_or(Error::UnknownKey { key: previous_key })?;
            batch.put_cf(state_cf, "conversion_state", previous_value);
        }

        // Delete block results for the last block
        let block_cf = self.get_column_family(BLOCK_CF)?;
        tracing::info!("Removing last block results");
        batch.delete_cf(block_cf, format!("results/{}", last_block.height));

        // Restore the state of replay protection to the last block
        let reprot_cf = self.get_column_family(REPLAY_PROTECTION_CF)?;
        tracing::info!("Restoring replay protection state");
        // Remove the "last" tx hashes
        for (ref hash_str, _, _) in self.iter_replay_protection() {
            let hash = namada::core::hash::Hash::from_str(hash_str)
                .expect("Failed hash conversion");
            let key = replay_protection::last_key(&hash);
            batch.delete_cf(reprot_cf, key.to_string());
        }

        for (ref hash_str, _, _) in self.iter_replay_protection_buffer() {
            let hash = namada::core::hash::Hash::from_str(hash_str)
                .expect("Failed hash conversion");
            let last_key = replay_protection::last_key(&hash);
            // Restore "buffer" bucket to "last"
            batch.put_cf(reprot_cf, last_key.to_string(), vec![]);

            // Remove anything in the buffer from the "all" prefix. Note that
            // some hashes might be missing from "all" if they have been
            // deleted, this is fine, in this case just continue
            let all_key = replay_protection::all_key(&hash);
            batch.delete_cf(reprot_cf, all_key.to_string());
        }

        // Execute next step in parallel
        let batch = Mutex::new(batch);

        tracing::info!("Restoring previous height subspace diffs");
        self.iter_prefix(None).par_bridge().try_for_each(
            |(key, _value, _gas)| -> Result<()> {
                // Restore previous height diff if present, otherwise delete the
                // subspace key
                let subspace_cf = self.get_column_family(SUBSPACE_CF)?;
                match self.read_subspace_val_with_height(
                    &Key::from(key.to_db_key()),
                    previous_height,
                    last_block.height,
                )? {
                    Some(previous_value) => batch.lock().unwrap().put_cf(
                        subspace_cf,
                        &key,
                        previous_value,
                    ),
                    None => batch.lock().unwrap().delete_cf(subspace_cf, &key),
                }

                Ok(())
            },
        )?;

        // Look for diffs in this block to find what has been deleted
        let diff_new_key_prefix = Key {
            segments: vec![
                last_block.height.to_db_key(),
                NEW_DIFF_PREFIX.to_string().to_db_key(),
            ],
        };
        {
            let mut batch_guard = batch.lock().unwrap();
            let subspace_cf = self.get_column_family(SUBSPACE_CF)?;
            for (key, val, _) in
                iter_diffs_prefix(self, last_block.height, None, true)
            {
                let key = Key::parse(key).unwrap();
                let diff_new_key = diff_new_key_prefix.join(&key);
                if self.read_subspace_val(&diff_new_key)?.is_none() {
                    // If there is no new value, it has been deleted in this
                    // block and we have to restore it
                    batch_guard.put_cf(subspace_cf, key.to_string(), val)
                }
            }
        }

        tracing::info!("Deleting keys prepended with the last height");
        let mut batch = batch.into_inner().unwrap();
        let prefix = last_block.height.to_string();
        let mut delete_keys = |cf: &ColumnFamily| {
            let read_opts = make_iter_read_opts(Some(prefix.clone()));
            let iter = self.0.iterator_cf_opt(
                cf,
                read_opts,
                IteratorMode::From(prefix.as_bytes(), Direction::Forward),
            );
            for (key, _value, _gas) in PersistentPrefixIterator(
                // Empty prefix string to prevent stripping
                PrefixIterator::new(iter, String::default()),
            ) {
                batch.delete_cf(cf, key);
            }
        };
        // Delete any height-prepended key in subspace diffs
        let diffs_cf = self.get_column_family(DIFFS_CF)?;
        delete_keys(diffs_cf);
        // Delete any height-prepended key in the block
        delete_keys(block_cf);

        // Write the batch and persist changes to disk
        tracing::info!("Flushing restored state to disk");
        self.exec_batch(batch)
    }
}

impl DB for RocksDB {
    type Cache = rocksdb::Cache;
    type WriteBatch = RocksDBWriteBatch;

    fn open(
        db_path: impl AsRef<std::path::Path>,
        cache: Option<&Self::Cache>,
    ) -> Self {
        open(db_path, cache).expect("cannot open the DB")
    }

    fn flush(&self, wait: bool) -> Result<()> {
        let mut flush_opts = FlushOptions::default();
        flush_opts.set_wait(wait);
        self.0
            .flush_opt(&flush_opts)
            .map_err(|e| Error::DBError(e.into_string()))
    }

    fn read_last_block(&self) -> Result<Option<BlockStateRead>> {
        // Block height
        let state_cf = self.get_column_family(STATE_CF)?;
        let height: BlockHeight = match self
            .0
            .get_cf(state_cf, "height")
            .map_err(|e| Error::DBError(e.into_string()))?
        {
            Some(bytes) => {
                // TODO if there's an issue decoding this height, should we try
                // load its predecessor instead?
                decode(bytes).map_err(Error::CodingError)?
            }
            None => return Ok(None),
        };

        // Block results
        let block_cf = self.get_column_family(BLOCK_CF)?;
        let results_path = format!("results/{}", height.raw());
        let results: BlockResults = match self
            .0
            .get_cf(block_cf, results_path)
            .map_err(|e| Error::DBError(e.into_string()))?
        {
            Some(bytes) => decode(bytes).map_err(Error::CodingError)?,
            None => return Ok(None),
        };

        // Epoch start height and time
        let next_epoch_min_start_height: BlockHeight = match self
            .0
            .get_cf(state_cf, "next_epoch_min_start_height")
            .map_err(|e| Error::DBError(e.into_string()))?
        {
            Some(bytes) => decode(bytes).map_err(Error::CodingError)?,
            None => {
                tracing::error!(
                    "Couldn't load next epoch start height from the DB"
                );
                return Ok(None);
            }
        };
        let next_epoch_min_start_time: DateTimeUtc = match self
            .0
            .get_cf(state_cf, "next_epoch_min_start_time")
            .map_err(|e| Error::DBError(e.into_string()))?
        {
            Some(bytes) => decode(bytes).map_err(Error::CodingError)?,
            None => {
                tracing::error!(
                    "Couldn't load next epoch start time from the DB"
                );
                return Ok(None);
            }
        };
        let update_epoch_blocks_delay: Option<u32> = match self
            .0
            .get_cf(state_cf, "update_epoch_blocks_delay")
            .map_err(|e| Error::DBError(e.into_string()))?
        {
            Some(bytes) => decode(bytes).map_err(Error::CodingError)?,
            None => {
                tracing::error!(
                    "Couldn't load epoch update block delay from the DB"
                );
                return Ok(None);
            }
        };
        let conversion_state: ConversionState = match self
            .0
            .get_cf(state_cf, "conversion_state")
            .map_err(|e| Error::DBError(e.into_string()))?
        {
            Some(bytes) => decode(bytes).map_err(Error::CodingError)?,
            None => {
                tracing::error!("Couldn't load conversion state from the DB");
                return Ok(None);
            }
        };
        let tx_queue: TxQueue = match self
            .0
            .get_cf(state_cf, "tx_queue")
            .map_err(|e| Error::DBError(e.into_string()))?
        {
            Some(bytes) => decode(bytes).map_err(Error::CodingError)?,
            None => {
                tracing::error!("Couldn't load tx queue from the DB");
                return Ok(None);
            }
        };

        let ethereum_height: Option<ethereum_structs::BlockHeight> = match self
            .0
            .get_cf(state_cf, "ethereum_height")
            .map_err(|e| Error::DBError(e.into_string()))?
        {
            Some(bytes) => decode(bytes).map_err(Error::CodingError)?,
            None => {
                tracing::error!("Couldn't load ethereum height from the DB");
                return Ok(None);
            }
        };

        let eth_events_queue: EthEventsQueue = match self
            .0
            .get_cf(state_cf, "eth_events_queue")
            .map_err(|e| Error::DBError(e.into_string()))?
        {
            Some(bytes) => decode(bytes).map_err(Error::CodingError)?,
            None => {
                tracing::error!(
                    "Couldn't load the eth events queue from the DB"
                );
                return Ok(None);
            }
        };

        // Load data at the height
        let prefix = format!("{}/", height.raw());
        let mut read_opts = ReadOptions::default();
        read_opts.set_total_order_seek(false);
        let next_height_prefix = format!("{}/", height.next_height().raw());
        read_opts.set_iterate_upper_bound(next_height_prefix);
        let mut merkle_tree_stores = MerkleTreeStoresRead::default();
        let mut hash = None;
        let mut time = None;
        let mut epoch: Option<Epoch> = None;
        let mut pred_epochs = None;
        let mut address_gen = None;
        for value in self.0.iterator_cf_opt(
            block_cf,
            read_opts,
            IteratorMode::From(prefix.as_bytes(), Direction::Forward),
        ) {
            let (key, bytes) = match value {
                Ok(data) => data,
                Err(e) => return Err(Error::DBError(e.into_string())),
            };
            let path = &String::from_utf8((*key).to_vec()).map_err(|e| {
                Error::Temporary {
                    error: format!(
                        "Cannot convert path from utf8 bytes to string: {}",
                        e
                    ),
                }
            })?;
            let segments: Vec<&str> =
                path.split(KEY_SEGMENT_SEPARATOR).collect();
            match segments.get(1) {
                Some(prefix) => match *prefix {
                    // Restore the base tree of Merkle tree
                    "tree" => match segments.get(2) {
                        Some(s) => {
                            let st = StoreType::from_str(s)?;
                            match segments.get(3) {
                                Some(&"root") => merkle_tree_stores.set_root(
                                    &st,
                                    decode(bytes)
                                        .map_err(Error::CodingError)?,
                                ),
                                Some(&"store") => merkle_tree_stores
                                    .set_store(st.decode_store(bytes)?),
                                _ => unknown_key_error(path)?,
                            }
                        }
                        None => unknown_key_error(path)?,
                    },
                    "header" => {
                        // the block header doesn't have to be restored
                    }
                    "hash" => {
                        hash = Some(decode(bytes).map_err(Error::CodingError)?)
                    }
                    "time" => {
                        time = Some(decode(bytes).map_err(Error::CodingError)?)
                    }
                    "epoch" => {
                        epoch = Some(decode(bytes).map_err(Error::CodingError)?)
                    }
                    "pred_epochs" => {
                        pred_epochs =
                            Some(decode(bytes).map_err(Error::CodingError)?)
                    }
                    "address_gen" => {
                        address_gen =
                            Some(decode(bytes).map_err(Error::CodingError)?);
                    }
                    _ => unknown_key_error(path)?,
                },
                None => unknown_key_error(path)?,
            }
        }
        // Restore subtrees of Merkle tree
        if let Some(epoch) = epoch {
            for st in StoreType::iter_subtrees() {
                let key_prefix = subtree_key_prefix(st, epoch);
                let root_key =
                    key_prefix.clone().with_segment("root".to_owned());
                if let Some(bytes) = self
                    .0
                    .get_cf(block_cf, &root_key.to_string())
                    .map_err(|e| Error::DBError(e.into_string()))?
                {
                    merkle_tree_stores.set_root(
                        st,
                        decode(bytes).map_err(Error::CodingError)?,
                    );
                }
                let store_key = key_prefix.with_segment("store".to_owned());
                if let Some(bytes) = self
                    .0
                    .get_cf(block_cf, &store_key.to_string())
                    .map_err(|e| Error::DBError(e.into_string()))?
                {
                    merkle_tree_stores.set_store(st.decode_store(bytes)?);
                }
            }
        }
        match (hash, time, epoch, pred_epochs, address_gen) {
            (
                Some(hash),
                Some(time),
                Some(epoch),
                Some(pred_epochs),
                Some(address_gen),
            ) => Ok(Some(BlockStateRead {
                merkle_tree_stores,
                hash,
                height,
                time,
                epoch,
                pred_epochs,
                results,
                conversion_state,
                next_epoch_min_start_height,
                next_epoch_min_start_time,
                update_epoch_blocks_delay,
                address_gen,
                tx_queue,
                ethereum_height,
                eth_events_queue,
            })),
            _ => Err(Error::Temporary {
                error: "Essential data couldn't be read from the DB"
                    .to_string(),
            }),
        }
    }

    fn add_block_to_batch(
        &self,
        state: BlockStateWrite,
        batch: &mut Self::WriteBatch,
        is_full_commit: bool,
    ) -> Result<()> {
        let BlockStateWrite {
            merkle_tree_stores,
            header,
            hash,
            height,
            time,
            epoch,
            pred_epochs,
            next_epoch_min_start_height,
            next_epoch_min_start_time,
            update_epoch_blocks_delay,
            address_gen,
            results,
            conversion_state,
            tx_queue,
            ethereum_height,
            eth_events_queue,
        }: BlockStateWrite = state;

        // Epoch start height and time
        let state_cf = self.get_column_family(STATE_CF)?;
        if let Some(current_value) = self
            .0
            .get_cf(state_cf, "next_epoch_min_start_height")
            .map_err(|e| Error::DBError(e.into_string()))?
        {
            // Write the predecessor value for rollback
            batch.0.put_cf(
                state_cf,
                "pred/next_epoch_min_start_height",
                current_value,
            );
        }
        batch.0.put_cf(
            state_cf,
            "next_epoch_min_start_height",
            encode(&next_epoch_min_start_height),
        );

        if let Some(current_value) = self
            .0
            .get_cf(state_cf, "next_epoch_min_start_time")
            .map_err(|e| Error::DBError(e.into_string()))?
        {
            // Write the predecessor value for rollback
            batch.0.put_cf(
                state_cf,
                "pred/next_epoch_min_start_time",
                current_value,
            );
        }
        batch.0.put_cf(
            state_cf,
            "next_epoch_min_start_time",
            encode(&next_epoch_min_start_time),
        );
        if let Some(current_value) = self
            .0
            .get_cf(state_cf, "update_epoch_blocks_delay")
            .map_err(|e| Error::DBError(e.into_string()))?
        {
            // Write the predecessor value for rollback
            batch.0.put_cf(
                state_cf,
                "pred/update_epoch_blocks_delay",
                current_value,
            );
        }
        batch.0.put_cf(
            state_cf,
            "update_epoch_blocks_delay",
            encode(&update_epoch_blocks_delay),
        );

        // Save the conversion state when the epoch is updated
        if is_full_commit {
            if let Some(current_value) = self
                .0
                .get_cf(state_cf, "conversion_state")
                .map_err(|e| Error::DBError(e.into_string()))?
            {
                // Write the predecessor value for rollback
                batch.0.put_cf(
                    state_cf,
                    "pred/conversion_state",
                    current_value,
                );
            }
            batch.0.put_cf(
                state_cf,
                "conversion_state",
                encode(conversion_state),
            );
        }

        // Tx queue
        if let Some(pred_tx_queue) = self
            .0
            .get_cf(state_cf, "tx_queue")
            .map_err(|e| Error::DBError(e.into_string()))?
        {
            // Write the predecessor value for rollback
            batch.0.put_cf(state_cf, "pred/tx_queue", pred_tx_queue);
        }
        batch.0.put_cf(state_cf, "tx_queue", encode(&tx_queue));
        batch
            .0
            .put_cf(state_cf, "ethereum_height", encode(&ethereum_height));
        batch
            .0
            .put_cf(state_cf, "eth_events_queue", encode(&eth_events_queue));

        let block_cf = self.get_column_family(BLOCK_CF)?;
        let prefix_key = Key::from(height.to_db_key());
        // Merkle tree
        {
            for st in StoreType::iter() {
                if *st == StoreType::Base || is_full_commit {
                    let key_prefix = if *st == StoreType::Base {
                        base_tree_key_prefix(height)
                    } else {
                        subtree_key_prefix(st, epoch)
                    };
                    let root_key =
                        key_prefix.clone().with_segment("root".to_owned());
                    batch.0.put_cf(
                        block_cf,
                        root_key.to_string(),
                        encode(merkle_tree_stores.root(st)),
                    );
                    let store_key = key_prefix.with_segment("store".to_owned());
                    batch.0.put_cf(
                        block_cf,
                        store_key.to_string(),
                        merkle_tree_stores.store(st).encode(),
                    );
                }
            }
        }
        // Block header
        {
            if let Some(h) = header {
                let key = prefix_key
                    .push(&"header".to_owned())
                    .map_err(Error::KeyError)?;
                batch
                    .0
                    .put_cf(block_cf, key.to_string(), h.serialize_to_vec());
            }
        }
        // Block hash
        {
            let key = prefix_key
                .push(&"hash".to_owned())
                .map_err(Error::KeyError)?;
            batch.0.put_cf(block_cf, key.to_string(), encode(&hash));
        }
        // Block time
        {
            let key = prefix_key
                .push(&"time".to_owned())
                .map_err(Error::KeyError)?;
            batch.0.put_cf(block_cf, key.to_string(), encode(&time));
        }
        // Block epoch
        {
            let key = prefix_key
                .push(&"epoch".to_owned())
                .map_err(Error::KeyError)?;
            batch.0.put_cf(block_cf, key.to_string(), encode(&epoch));
        }
        // Block results
        {
            let results_path = format!("results/{}", height.raw());
            batch.0.put_cf(block_cf, results_path, encode(&results));
        }
        // Predecessor block epochs
        {
            let key = prefix_key
                .push(&"pred_epochs".to_owned())
                .map_err(Error::KeyError)?;
            batch
                .0
                .put_cf(block_cf, key.to_string(), encode(&pred_epochs));
        }
        // Address gen
        {
            let key = prefix_key
                .push(&"address_gen".to_owned())
                .map_err(Error::KeyError)?;
            batch
                .0
                .put_cf(block_cf, key.to_string(), encode(&address_gen));
        }

        // Block height
        batch.0.put_cf(state_cf, "height", encode(&height));

        Ok(())
    }

    fn read_block_header(&self, height: BlockHeight) -> Result<Option<Header>> {
        let block_cf = self.get_column_family(BLOCK_CF)?;
        let prefix_key = Key::from(height.to_db_key());
        let key = prefix_key
            .push(&"header".to_owned())
            .map_err(Error::KeyError)?;
        let value = self
            .0
            .get_cf(block_cf, key.to_string())
            .map_err(|e| Error::DBError(e.into_string()))?;
        match value {
            Some(v) => Ok(Some(
                Header::try_from_slice(&v[..])
                    .map_err(Error::BorshCodingError)?,
            )),
            None => Ok(None),
        }
    }

    fn read_merkle_tree_stores(
        &self,
        epoch: Epoch,
        base_height: BlockHeight,
        store_type: Option<StoreType>,
    ) -> Result<Option<MerkleTreeStoresRead>> {
        // Get the latest height at which the tree stores were written
        let block_cf = self.get_column_family(BLOCK_CF)?;
        let mut merkle_tree_stores = MerkleTreeStoresRead::default();
        let store_types = store_type
            .as_ref()
            .map(|st| Either::Left(std::iter::once(st)))
            .unwrap_or_else(|| Either::Right(StoreType::iter()));
        for st in store_types {
            let key_prefix = if *st == StoreType::Base {
                base_tree_key_prefix(base_height)
            } else {
                subtree_key_prefix(st, epoch)
            };
            let root_key = key_prefix.clone().with_segment("root".to_owned());
            let bytes = self
                .0
                .get_cf(block_cf, root_key.to_string())
                .map_err(|e| Error::DBError(e.into_string()))?;
            match bytes {
                Some(b) => {
                    let root = decode(b).map_err(Error::CodingError)?;
                    merkle_tree_stores.set_root(st, root);
                }
                None => return Ok(None),
            }

            let store_key = key_prefix.with_segment("store".to_owned());
            let bytes = self
                .0
                .get_cf(block_cf, store_key.to_string())
                .map_err(|e| Error::DBError(e.into_string()))?;
            match bytes {
                Some(b) => {
                    merkle_tree_stores.set_store(st.decode_store(b)?);
                }
                None => return Ok(None),
            }
        }
        Ok(Some(merkle_tree_stores))
    }

    fn has_replay_protection_entry(
        &self,
        hash: &namada::core::hash::Hash,
    ) -> Result<bool> {
        let replay_protection_cf =
            self.get_column_family(REPLAY_PROTECTION_CF)?;

        for key in [
            replay_protection::last_key(hash),
            replay_protection::all_key(hash),
        ] {
            if self
                .0
                .get_pinned_cf(replay_protection_cf, key.to_string())
                .map_err(|e| Error::DBError(e.into_string()))?
                .is_some()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn read_diffs_val(
        &self,
        key: &Key,
        height: BlockHeight,
        is_old: bool,
    ) -> Result<Option<Vec<u8>>> {
        let diffs_cf = self.get_column_family(DIFFS_CF)?;
        let key = if is_old {
            old_and_new_diff_key(key, height)?.0
        } else {
            old_and_new_diff_key(key, height)?.1
        };

        self.0
            .get_cf(diffs_cf, key)
            .map_err(|e| Error::DBError(e.into_string()))
    }

    fn read_subspace_val(&self, key: &Key) -> Result<Option<Vec<u8>>> {
        let subspace_cf = self.get_column_family(SUBSPACE_CF)?;
        self.0
            .get_cf(subspace_cf, key.to_string())
            .map_err(|e| Error::DBError(e.into_string()))
    }

    fn read_subspace_val_with_height(
        &self,
        key: &Key,
        height: BlockHeight,
        last_height: BlockHeight,
    ) -> Result<Option<Vec<u8>>> {
        // Check if the value changed at this height
        let diffs_cf = self.get_column_family(DIFFS_CF)?;
        let (old_val_key, new_val_key) = old_and_new_diff_key(key, height)?;

        // If it has a "new" val, it was written at this height
        match self
            .0
            .get_cf(diffs_cf, new_val_key)
            .map_err(|e| Error::DBError(e.into_string()))?
        {
            Some(new_val) => {
                return Ok(Some(new_val));
            }
            None => {
                // If it has an "old" val, it was deleted at this height
                if self.0.key_may_exist_cf(diffs_cf, &old_val_key) {
                    // check if it actually exists
                    if self
                        .0
                        .get_cf(diffs_cf, old_val_key)
                        .map_err(|e| Error::DBError(e.into_string()))?
                        .is_some()
                    {
                        return Ok(None);
                    }
                }
            }
        }

        // If the value didn't change at the given height, we try to look for it
        // at successor heights, up to the `last_height`
        let mut raw_height = height.0 + 1;
        loop {
            // Try to find the next diff on this key
            let (old_val_key, new_val_key) =
                old_and_new_diff_key(key, BlockHeight(raw_height))?;
            let old_val = self
                .0
                .get_cf(diffs_cf, &old_val_key)
                .map_err(|e| Error::DBError(e.into_string()))?;
            // If it has an "old" val, it's the one we're looking for
            match old_val {
                Some(bytes) => return Ok(Some(bytes)),
                None => {
                    // Check if the value was created at this height instead,
                    // which would mean that it wasn't present before
                    if self.0.key_may_exist_cf(diffs_cf, &new_val_key) {
                        // check if it actually exists
                        if self
                            .0
                            .get_cf(diffs_cf, new_val_key)
                            .map_err(|e| Error::DBError(e.into_string()))?
                            .is_some()
                        {
                            return Ok(None);
                        }
                    }

                    if raw_height >= last_height.0 {
                        // Read from latest height
                        return self.read_subspace_val(key);
                    } else {
                        raw_height += 1
                    }
                }
            }
        }
    }

    fn write_subspace_val(
        &mut self,
        height: BlockHeight,
        key: &Key,
        value: impl AsRef<[u8]>,
        persist_diffs: bool,
    ) -> Result<i64> {
        let subspace_cf = self.get_column_family(SUBSPACE_CF)?;
        let value = value.as_ref();
        let size_diff = match self
            .0
            .get_cf(subspace_cf, key.to_string())
            .map_err(|e| Error::DBError(e.into_string()))?
        {
            Some(prev_value) => {
                let size_diff = value.len() as i64 - prev_value.len() as i64;
                self.write_subspace_diff(
                    height,
                    key,
                    Some(&prev_value),
                    Some(value),
                    persist_diffs,
                )?;
                size_diff
            }
            None => {
                self.write_subspace_diff(
                    height,
                    key,
                    None,
                    Some(value),
                    persist_diffs,
                )?;
                value.len() as i64
            }
        };

        // Write the new key-val
        self.0
            .put_cf(subspace_cf, key.to_string(), value)
            .map_err(|e| Error::DBError(e.into_string()))?;

        Ok(size_diff)
    }

    fn delete_subspace_val(
        &mut self,
        height: BlockHeight,
        key: &Key,
        persist_diffs: bool,
    ) -> Result<i64> {
        let subspace_cf = self.get_column_family(SUBSPACE_CF)?;

        // Check the length of previous value, if any
        let prev_len = match self
            .0
            .get_cf(subspace_cf, key.to_string())
            .map_err(|e| Error::DBError(e.into_string()))?
        {
            Some(prev_value) => {
                let prev_len = prev_value.len() as i64;
                self.write_subspace_diff(
                    height,
                    key,
                    Some(&prev_value),
                    None,
                    persist_diffs,
                )?;
                prev_len
            }
            None => 0,
        };

        // Delete the key-val
        self.0
            .delete_cf(subspace_cf, key.to_string())
            .map_err(|e| Error::DBError(e.into_string()))?;

        Ok(prev_len)
    }

    fn batch() -> Self::WriteBatch {
        RocksDBWriteBatch::default()
    }

    fn exec_batch(&mut self, batch: Self::WriteBatch) -> Result<()> {
        self.exec_batch(batch.0)
    }

    fn batch_write_subspace_val(
        &self,
        batch: &mut Self::WriteBatch,
        height: BlockHeight,
        key: &Key,
        value: impl AsRef<[u8]>,
        persist_diffs: bool,
    ) -> Result<i64> {
        let value = value.as_ref();
        let subspace_cf = self.get_column_family(SUBSPACE_CF)?;
        let size_diff = match self
            .0
            .get_cf(subspace_cf, key.to_string())
            .map_err(|e| Error::DBError(e.into_string()))?
        {
            Some(old_value) => {
                let size_diff = value.len() as i64 - old_value.len() as i64;
                // Persist the previous value
                self.batch_write_subspace_diff(
                    batch,
                    height,
                    key,
                    Some(&old_value),
                    Some(value),
                    persist_diffs,
                )?;
                size_diff
            }
            None => {
                self.batch_write_subspace_diff(
                    batch,
                    height,
                    key,
                    None,
                    Some(value),
                    persist_diffs,
                )?;
                value.len() as i64
            }
        };

        // Write the new key-val
        batch.0.put_cf(subspace_cf, key.to_string(), value);

        Ok(size_diff)
    }

    fn batch_delete_subspace_val(
        &self,
        batch: &mut Self::WriteBatch,
        height: BlockHeight,
        key: &Key,
        persist_diffs: bool,
    ) -> Result<i64> {
        let subspace_cf = self.get_column_family(SUBSPACE_CF)?;

        // Check the length of previous value, if any
        let prev_len = match self
            .0
            .get_cf(subspace_cf, key.to_string())
            .map_err(|e| Error::DBError(e.into_string()))?
        {
            Some(prev_value) => {
                let prev_len = prev_value.len() as i64;
                // Persist the previous value
                self.batch_write_subspace_diff(
                    batch,
                    height,
                    key,
                    Some(&prev_value),
                    None,
                    persist_diffs,
                )?;
                prev_len
            }
            None => 0,
        };

        // Delete the key-val
        batch.0.delete_cf(subspace_cf, key.to_string());

        Ok(prev_len)
    }

    fn prune_merkle_tree_store(
        &mut self,
        batch: &mut Self::WriteBatch,
        store_type: &StoreType,
        epoch: Epoch,
    ) -> Result<()> {
        let block_cf = self.get_column_family(BLOCK_CF)?;
        let key_prefix = subtree_key_prefix(store_type, epoch);
        let root_key = key_prefix.clone().with_segment("root".to_owned());
        batch.0.delete_cf(block_cf, root_key.to_string());
        let store_key = key_prefix.with_segment("store".to_owned());
        batch.0.delete_cf(block_cf, store_key.to_string());
        Ok(())
    }

    fn read_bridge_pool_signed_nonce(
        &self,
        height: BlockHeight,
        last_height: BlockHeight,
    ) -> Result<Option<ethereum_events::Uint>> {
        let nonce_key = bridge_pool::get_signed_root_key();
        let bytes = if height == BlockHeight(0) || height >= last_height {
            self.read_subspace_val(&nonce_key)?
        } else {
            self.read_subspace_val_with_height(&nonce_key, height, last_height)?
        };
        match bytes {
            Some(bytes) => {
                let bp_root_proof = BridgePoolRootProof::try_from_slice(&bytes)
                    .map_err(Error::BorshCodingError)?;
                Ok(Some(bp_root_proof.data.1))
            }
            None => Ok(None),
        }
    }

    fn write_replay_protection_entry(
        &mut self,
        batch: &mut Self::WriteBatch,
        key: &Key,
    ) -> Result<()> {
        let replay_protection_cf =
            self.get_column_family(REPLAY_PROTECTION_CF)?;

        batch
            .0
            .put_cf(replay_protection_cf, key.to_string(), vec![]);

        Ok(())
    }

    fn delete_replay_protection_entry(
        &mut self,
        batch: &mut Self::WriteBatch,
        key: &Key,
    ) -> Result<()> {
        let replay_protection_cf =
            self.get_column_family(REPLAY_PROTECTION_CF)?;

        batch.0.delete_cf(replay_protection_cf, key.to_string());

        Ok(())
    }

    fn prune_replay_protection_buffer(
        &mut self,
        batch: &mut Self::WriteBatch,
    ) -> Result<()> {
        let replay_protection_cf =
            self.get_column_family(REPLAY_PROTECTION_CF)?;

        for (ref hash_str, _, _) in self.iter_replay_protection_buffer() {
            let hash = namada::core::hash::Hash::from_str(hash_str)
                .expect("Failed hash conversion");
            let key = replay_protection::buffer_key(&hash);
            batch.0.delete_cf(replay_protection_cf, key.to_string());
        }

        Ok(())
    }

    #[inline]
    fn overwrite_entry(
        &self,
        batch: &mut Self::WriteBatch,
        height: Option<BlockHeight>,
        cf: &DbColFam,
        key: &Key,
        new_value: impl AsRef<[u8]>,
    ) -> Result<()> {
        let last_height: BlockHeight = {
            let state_cf = self.get_column_family(STATE_CF)?;

            decode(
                self.0
                    .get_cf(state_cf, "height")
                    .map_err(|e| Error::DBError(e.to_string()))?
                    .ok_or_else(|| {
                        Error::DBError("No block height found".to_string())
                    })?,
            )
            .map_err(|e| {
                Error::DBError(format!("Unable to decode block height: {e}"))
            })?
        };
        let desired_height = height.unwrap_or(last_height);

        if desired_height != last_height {
            todo!(
                "Overwriting values at heights different than the last \
                 committed height hast yet to be implemented"
            );
        }
        // NB: the following code only updates values
        // written to at the last committed height

        let val = new_value.as_ref();

        // Write the new key-val in the Db column family
        let cf_name = self.get_column_family(cf.to_str())?;
        batch.0.put_cf(cf_name, key.to_string(), val);

        // If the CF is subspace, additionally update the diffs
        if cf == &DbColFam::SUBSPACE {
            let diffs_cf = self.get_column_family(DIFFS_CF)?;
            let diffs_key = Key::from(last_height.to_db_key())
                .with_segment("new".to_owned())
                .join(key)
                .to_string();

            batch.0.put_cf(diffs_cf, diffs_key, val);
        }

        Ok(())
    }
}

/// A struct that can visit a set of updates,
/// registering them all in the batch
pub struct RocksDBUpdateVisitor<'db> {
    db: &'db RocksDB,
    batch: RocksDBWriteBatch,
}

impl<'db> RocksDBUpdateVisitor<'db> {
    pub fn new(db: &'db RocksDB) -> Self {
        Self {
            db,
            batch: Default::default(),
        }
    }

    pub fn take_batch(self) -> RocksDBWriteBatch {
        self.batch
    }
}

impl<'db> DBUpdateVisitor for RocksDBUpdateVisitor<'db> {
    fn read(&self, key: &Key, cf: &DbColFam) -> Option<Vec<u8>> {
        match cf {
            DbColFam::SUBSPACE => self
                .db
                .read_subspace_val(key)
                .expect("Failed to read from storage"),
            _ => {
                let cf_str = cf.to_str();
                let cf = self
                    .db
                    .get_column_family(cf_str)
                    .expect("Failed to read column family from storage");
                self.db
                    .0
                    .get_cf(cf, key.to_string())
                    .expect("Failed to get key from storage")
            }
        }
    }

    fn write(&mut self, key: &Key, cf: &DbColFam, value: impl AsRef<[u8]>) {
        self.db
            .overwrite_entry(&mut self.batch, None, cf, key, value)
            .expect("Failed to overwrite a key in storage")
    }

    fn delete(&mut self, key: &Key, cf: &DbColFam) {
        let last_height: BlockHeight = {
            let state_cf = self.db.get_column_family(STATE_CF).unwrap();

            decode(
                self.db
                    .0
                    .get_cf(state_cf, "height")
                    .map_err(|e| Error::DBError(e.to_string()))
                    .unwrap()
                    .ok_or_else(|| {
                        Error::DBError("No block height found".to_string())
                    })
                    .unwrap(),
            )
            .map_err(|e| {
                Error::DBError(format!("Unable to decode block height: {e}"))
            })
            .unwrap()
        };
        match cf {
            DbColFam::SUBSPACE => {
                self.db
                    .batch_delete_subspace_val(
                        &mut self.batch,
                        last_height,
                        key,
                        true,
                    )
                    .expect("Failed to delete key from storage");
            }
            _ => {
                let cf_str = cf.to_str();
                let cf = self
                    .db
                    .get_column_family(cf_str)
                    .expect("Failed to get read column family from storage");
                self.batch.0.delete_cf(cf, key.to_string());
            }
        };
    }

    fn get_pattern(&self, pattern: Regex) -> Vec<(String, Vec<u8>)> {
        self.db
            .iter_pattern(None, pattern)
            .map(|(k, v, _)| (k, v))
            .collect()
    }
}

impl<'iter> DBIter<'iter> for RocksDB {
    type PatternIter = PersistentPatternIterator<'iter>;
    type PrefixIter = PersistentPrefixIterator<'iter>;

    fn iter_prefix(
        &'iter self,
        prefix: Option<&Key>,
    ) -> PersistentPrefixIterator<'iter> {
        iter_subspace_prefix(self, prefix)
    }

    fn iter_pattern(
        &'iter self,
        prefix: Option<&Key>,
        pattern: Regex,
    ) -> PersistentPatternIterator<'iter> {
        iter_subspace_pattern(self, prefix, pattern)
    }

    fn iter_results(&'iter self) -> PersistentPrefixIterator<'iter> {
        let db_prefix = "results/".to_owned();
        let prefix = "results".to_owned();

        let block_cf = self
            .get_column_family(BLOCK_CF)
            .expect("{BLOCK_CF} column family should exist");
        let read_opts = make_iter_read_opts(Some(prefix.clone()));
        let iter = self.0.iterator_cf_opt(
            block_cf,
            read_opts,
            IteratorMode::From(prefix.as_bytes(), Direction::Forward),
        );
        PersistentPrefixIterator(PrefixIterator::new(iter, db_prefix))
    }

    fn iter_old_diffs(
        &'iter self,
        height: BlockHeight,
        prefix: Option<&'iter Key>,
    ) -> PersistentPrefixIterator<'iter> {
        iter_diffs_prefix(self, height, prefix, true)
    }

    fn iter_new_diffs(
        &'iter self,
        height: BlockHeight,
        prefix: Option<&'iter Key>,
    ) -> PersistentPrefixIterator<'iter> {
        iter_diffs_prefix(self, height, prefix, false)
    }

    fn iter_replay_protection(&'iter self) -> Self::PrefixIter {
        let replay_protection_cf = self
            .get_column_family(REPLAY_PROTECTION_CF)
            .expect("{REPLAY_PROTECTION_CF} column family should exist");

        let stripped_prefix = Some(replay_protection::last_prefix());
        iter_prefix(self, replay_protection_cf, stripped_prefix.as_ref(), None)
    }

    fn iter_replay_protection_buffer(&'iter self) -> Self::PrefixIter {
        let replay_protection_cf = self
            .get_column_family(REPLAY_PROTECTION_CF)
            .expect("{REPLAY_PROTECTION_CF} column family should exist");

        let stripped_prefix = Some(replay_protection::buffer_prefix());
        iter_prefix(self, replay_protection_cf, stripped_prefix.as_ref(), None)
    }
}

fn iter_subspace_prefix<'iter>(
    db: &'iter RocksDB,
    prefix: Option<&Key>,
) -> PersistentPrefixIterator<'iter> {
    let subspace_cf = db
        .get_column_family(SUBSPACE_CF)
        .expect("{SUBSPACE_CF} column family should exist");
    let stripped_prefix = None;
    iter_prefix(db, subspace_cf, stripped_prefix, prefix)
}

fn iter_subspace_pattern<'iter>(
    db: &'iter RocksDB,
    prefix: Option<&Key>,
    pattern: Regex,
) -> PersistentPatternIterator<'iter> {
    let subspace_cf = db
        .get_column_family(SUBSPACE_CF)
        .expect("{SUBSPACE_CF} column family should exist");
    let stripped_prefix = None;
    iter_pattern(db, subspace_cf, stripped_prefix, prefix, pattern)
}

fn iter_diffs_prefix<'a>(
    db: &'a RocksDB,
    height: BlockHeight,
    prefix: Option<&Key>,
    is_old: bool,
) -> PersistentPrefixIterator<'a> {
    let diffs_cf = db
        .get_column_family(DIFFS_CF)
        .expect("{DIFFS_CF} column family should exist");
    let kind = if is_old {
        OLD_DIFF_PREFIX
    } else {
        NEW_DIFF_PREFIX
    };
    let stripped_prefix = Some(
        Key::from(height.to_db_key())
            .push(&kind.to_string())
            .unwrap(),
    );
    // get keys without the `stripped_prefix`
    iter_prefix(db, diffs_cf, stripped_prefix.as_ref(), prefix)
}

/// Create an iterator over key-vals in the given CF matching the given
/// prefix(es). If any, the `stripped_prefix` is matched first and will be
/// removed from the matched keys. If any, the second `prefix` is matched
/// against the stripped keys and remains in the matched keys.
fn iter_prefix<'a>(
    db: &'a RocksDB,
    cf: &'a ColumnFamily,
    stripped_prefix: Option<&Key>,
    prefix: Option<&Key>,
) -> PersistentPrefixIterator<'a> {
    let stripped_prefix = match stripped_prefix {
        Some(p) if !p.is_empty() => format!("{p}/"),
        _ => "".to_owned(),
    };
    let prefix = match prefix {
        Some(p) if !p.is_empty() => {
            format!("{stripped_prefix}{p}/")
        }
        _ => stripped_prefix.clone(),
    };
    let read_opts = make_iter_read_opts(Some(prefix.clone()));
    let iter = db.0.iterator_cf_opt(
        cf,
        read_opts,
        IteratorMode::From(prefix.as_bytes(), Direction::Forward),
    );
    PersistentPrefixIterator(PrefixIterator::new(iter, stripped_prefix))
}

/// Create an iterator over key-vals in the given CF matching the given
/// pattern(s).
fn iter_pattern<'a>(
    db: &'a RocksDB,
    cf: &'a ColumnFamily,
    stripped_prefix: Option<&Key>,
    prefix: Option<&Key>,
    pattern: Regex,
) -> PersistentPatternIterator<'a> {
    PersistentPatternIterator {
        inner: PatternIterator {
            iter: iter_prefix(db, cf, stripped_prefix, prefix),
            pattern,
        },
    }
}

#[derive(Debug)]
pub struct PersistentPrefixIterator<'a>(
    PrefixIterator<rocksdb::DBIterator<'a>>,
);

impl<'a> Iterator for PersistentPrefixIterator<'a> {
    type Item = (String, Vec<u8>, u64);

    /// Returns the next pair and the gas cost
    fn next(&mut self) -> Option<(String, Vec<u8>, u64)> {
        loop {
            match self.0.iter.next() {
                Some(result) => {
                    let (key, val) =
                        result.expect("Prefix iterator shouldn't fail");
                    let key = String::from_utf8(key.to_vec())
                        .expect("Cannot convert from bytes to key string");
                    if let Some(k) = key.strip_prefix(&self.0.stripped_prefix) {
                        let gas = k.len() + val.len();
                        return Some((k.to_owned(), val.to_vec(), gas as _));
                    } else {
                        tracing::warn!(
                            "Unmatched prefix \"{}\" in iterator's key \
                             \"{key}\"",
                            self.0.stripped_prefix
                        );
                    }
                }
                None => return None,
            }
        }
    }
}

#[derive(Debug)]
pub struct PersistentPatternIterator<'a> {
    inner: PatternIterator<PersistentPrefixIterator<'a>>,
}

impl<'a> Iterator for PersistentPatternIterator<'a> {
    type Item = (String, Vec<u8>, u64);

    /// Returns the next pair and the gas cost
    fn next(&mut self) -> Option<(String, Vec<u8>, u64)> {
        loop {
            let next_result = self.inner.iter.next()?;
            if self.inner.pattern.is_match(&next_result.0) {
                return Some(next_result);
            }
        }
    }
}

/// Make read options for RocksDB iterator with the given prefix
fn make_iter_read_opts(prefix: Option<String>) -> ReadOptions {
    let mut read_opts = ReadOptions::default();
    // don't use the prefix bloom filter
    read_opts.set_total_order_seek(true);

    if let Some(prefix) = prefix {
        let mut upper_prefix = prefix.into_bytes();
        if let Some(last) = upper_prefix.last_mut() {
            *last += 1;
            read_opts.set_iterate_upper_bound(upper_prefix);
        }
    }

    read_opts
}

impl DBWriteBatch for RocksDBWriteBatch {}

fn old_and_new_diff_key(
    key: &Key,
    height: BlockHeight,
) -> Result<(String, String)> {
    let key_prefix = Key::from(height.to_db_key());
    let old = key_prefix
        .push(&OLD_DIFF_PREFIX.to_owned())
        .map_err(Error::KeyError)?
        .join(key);
    let new = key_prefix
        .push(&NEW_DIFF_PREFIX.to_owned())
        .map_err(Error::KeyError)?
        .join(key);
    Ok((old.to_string(), new.to_string()))
}

fn unknown_key_error(key: &str) -> Result<()> {
    Err(Error::UnknownKey {
        key: key.to_owned(),
    })
}

/// Try to increase NOFILE limit and set the `max_open_files` limit to it in
/// RocksDB options.
fn set_max_open_files(cf_opts: &mut rocksdb::Options) {
    #[cfg(unix)]
    imp::set_max_open_files(cf_opts);
    // Nothing to do on non-unix
    #[cfg(not(unix))]
    let _ = cf_opts;
}

#[cfg(unix)]
mod imp {
    use rlimit::{Resource, Rlim};

    const DEFAULT_NOFILE_LIMIT: Rlim = Rlim::from_raw(16384);

    pub fn set_max_open_files(cf_opts: &mut rocksdb::Options) {
        let max_open_files = match increase_nofile_limit() {
            Ok(max_open_files) => Some(max_open_files),
            Err(err) => {
                tracing::error!("Failed to increase NOFILE limit: {}", err);
                None
            }
        };
        if let Some(max_open_files) =
            max_open_files.and_then(|max| max.as_raw().try_into().ok())
        {
            cf_opts.set_max_open_files(max_open_files);
        }
    }

    /// Try to increase NOFILE limit and return the current soft limit.
    fn increase_nofile_limit() -> std::io::Result<Rlim> {
        let (soft, hard) = Resource::NOFILE.get()?;
        tracing::debug!("Current NOFILE limit, soft={}, hard={}", soft, hard);

        let target = std::cmp::min(DEFAULT_NOFILE_LIMIT, hard);
        if soft >= target {
            tracing::debug!(
                "NOFILE limit already large enough, not attempting to increase"
            );
            Ok(soft)
        } else {
            tracing::debug!("Try to increase to {}", target);
            Resource::NOFILE.set(target, target)?;

            let (soft, hard) = Resource::NOFILE.get()?;
            tracing::debug!(
                "Increased NOFILE limit, soft={}, hard={}",
                soft,
                hard
            );
            Ok(soft)
        }
    }
}

#[cfg(test)]
mod test {
    use namada::core::address::EstablishedAddressGen;
    use namada::core::hash::Hash;
    use namada::core::storage::{BlockHash, Epochs};
    use namada::state::{MerkleTree, Sha256Hasher};
    use tempfile::tempdir;
    use test_log::test;

    use super::*;

    /// Test that a block written can be loaded back from DB.
    #[test]
    fn test_load_state() {
        let dir = tempdir().unwrap();
        let mut db = open(dir.path(), None).unwrap();

        let mut batch = RocksDB::batch();
        let last_height = BlockHeight::default();
        db.batch_write_subspace_val(
            &mut batch,
            last_height,
            &Key::parse("test").unwrap(),
            vec![1_u8, 1, 1, 1],
            true,
        )
        .unwrap();

        add_block_to_batch(
            &db,
            &mut batch,
            BlockHeight::default(),
            Epoch::default(),
            Epochs::default(),
            &ConversionState::default(),
        )
        .unwrap();
        db.exec_batch(batch.0).unwrap();

        let _state = db
            .read_last_block()
            .expect("Should be able to read last block")
            .expect("Block should have been written");
    }

    #[test]
    fn test_read() {
        let dir = tempdir().unwrap();
        let mut db = open(dir.path(), None).unwrap();

        let key = Key::parse("test").unwrap();
        let batch_key = Key::parse("batch").unwrap();

        let mut batch = RocksDB::batch();
        let last_height = BlockHeight(100);
        db.batch_write_subspace_val(
            &mut batch,
            last_height,
            &batch_key,
            vec![1_u8, 1, 1, 1],
            true,
        )
        .unwrap();
        db.exec_batch(batch.0).unwrap();

        db.write_subspace_val(last_height, &key, vec![1_u8, 1, 1, 0], true)
            .unwrap();

        let mut batch = RocksDB::batch();
        let last_height = BlockHeight(111);
        db.batch_write_subspace_val(
            &mut batch,
            last_height,
            &batch_key,
            vec![2_u8, 2, 2, 2],
            true,
        )
        .unwrap();
        db.exec_batch(batch.0).unwrap();

        db.write_subspace_val(last_height, &key, vec![2_u8, 2, 2, 0], true)
            .unwrap();

        let prev_value = db
            .read_subspace_val_with_height(
                &batch_key,
                BlockHeight(100),
                last_height,
            )
            .expect("read should succeed");
        assert_eq!(prev_value, Some(vec![1_u8, 1, 1, 1]));
        let prev_value = db
            .read_subspace_val_with_height(&key, BlockHeight(100), last_height)
            .expect("read should succeed");
        assert_eq!(prev_value, Some(vec![1_u8, 1, 1, 0]));

        let updated_value = db
            .read_subspace_val_with_height(
                &batch_key,
                BlockHeight(111),
                last_height,
            )
            .expect("read should succeed");
        assert_eq!(updated_value, Some(vec![2_u8, 2, 2, 2]));
        let updated_value = db
            .read_subspace_val_with_height(&key, BlockHeight(111), last_height)
            .expect("read should succeed");
        assert_eq!(updated_value, Some(vec![2_u8, 2, 2, 0]));

        let latest_value = db
            .read_subspace_val(&batch_key)
            .expect("read should succeed");
        assert_eq!(latest_value, Some(vec![2_u8, 2, 2, 2]));
        let latest_value =
            db.read_subspace_val(&key).expect("read should succeed");
        assert_eq!(latest_value, Some(vec![2_u8, 2, 2, 0]));

        let mut batch = RocksDB::batch();
        let last_height = BlockHeight(222);
        db.batch_delete_subspace_val(&mut batch, last_height, &batch_key, true)
            .unwrap();
        db.exec_batch(batch.0).unwrap();

        db.delete_subspace_val(last_height, &key, true).unwrap();

        let deleted_value = db
            .read_subspace_val_with_height(
                &batch_key,
                BlockHeight(222),
                last_height,
            )
            .expect("read should succeed");
        assert_eq!(deleted_value, None);
        let deleted_value = db
            .read_subspace_val_with_height(&key, BlockHeight(222), last_height)
            .expect("read should succeed");
        assert_eq!(deleted_value, None);

        let latest_value = db
            .read_subspace_val(&batch_key)
            .expect("read should succeed");
        assert_eq!(latest_value, None);
        let latest_value =
            db.read_subspace_val(&key).expect("read should succeed");
        assert_eq!(latest_value, None);
    }

    #[test]
    fn test_prefix_iter() {
        let dir = tempdir().unwrap();
        let mut db = open(dir.path(), None).unwrap();

        let prefix_0 = Key::parse("0").unwrap();
        let key_0_a = prefix_0.push(&"a".to_string()).unwrap();
        let key_0_b = prefix_0.push(&"b".to_string()).unwrap();
        let key_0_c = prefix_0.push(&"c".to_string()).unwrap();
        let prefix_1 = Key::parse("1").unwrap();
        let key_1_a = prefix_1.push(&"a".to_string()).unwrap();
        let key_1_b = prefix_1.push(&"b".to_string()).unwrap();
        let key_1_c = prefix_1.push(&"c".to_string()).unwrap();
        let prefix_01 = Key::parse("01").unwrap();
        let key_01_a = prefix_01.push(&"a".to_string()).unwrap();

        let keys_0 = vec![key_0_a, key_0_b, key_0_c];
        let keys_1 = vec![key_1_a, key_1_b, key_1_c];
        let keys_01 = vec![key_01_a];
        let all_keys = [keys_0.clone(), keys_01, keys_1.clone()].concat();

        // Write the keys
        let mut batch = RocksDB::batch();
        let height = BlockHeight(1);
        for key in &all_keys {
            db.batch_write_subspace_val(&mut batch, height, key, [0_u8], true)
                .unwrap();
        }
        db.exec_batch(batch.0).unwrap();

        // Prefix "0" shouldn't match prefix "01"
        let itered_keys: Vec<Key> = db
            .iter_prefix(Some(&prefix_0))
            .map(|(key, _val, _)| Key::parse(key).unwrap())
            .collect();
        itertools::assert_equal(keys_0, itered_keys);

        let itered_keys: Vec<Key> = db
            .iter_prefix(Some(&prefix_1))
            .map(|(key, _val, _)| Key::parse(key).unwrap())
            .collect();
        itertools::assert_equal(keys_1, itered_keys);

        let itered_keys: Vec<Key> = db
            .iter_prefix(None)
            .map(|(key, _val, _)| Key::parse(key).unwrap())
            .collect();
        itertools::assert_equal(all_keys, itered_keys);
    }

    #[test]
    fn test_rollback() {
        let dir = tempdir().unwrap();
        let mut db = open(dir.path(), None).unwrap();

        // A key that's gonna be added on a second block
        let add_key = Key::parse("add").unwrap();
        // A key that's gonna be deleted on a second block
        let delete_key = Key::parse("delete").unwrap();
        // A key that's gonna be overwritten on a second block
        let overwrite_key = Key::parse("overwrite").unwrap();

        // Write first block
        let mut batch = RocksDB::batch();
        let height_0 = BlockHeight(100);
        let mut pred_epochs = Epochs::default();
        pred_epochs.new_epoch(height_0);
        let conversion_state_0 = ConversionState::default();
        let to_delete_val = vec![1_u8, 1, 0, 0];
        let to_overwrite_val = vec![1_u8, 1, 1, 0];
        db.batch_write_subspace_val(
            &mut batch,
            height_0,
            &delete_key,
            &to_delete_val,
            true,
        )
        .unwrap();
        db.batch_write_subspace_val(
            &mut batch,
            height_0,
            &overwrite_key,
            &to_overwrite_val,
            true,
        )
        .unwrap();
        for tx in [b"tx1", b"tx2"] {
            db.write_replay_protection_entry(
                &mut batch,
                &replay_protection::all_key(&Hash::sha256(tx)),
            )
            .unwrap();
            db.write_replay_protection_entry(
                &mut batch,
                &replay_protection::buffer_key(&Hash::sha256(tx)),
            )
            .unwrap();
        }

        for tx in [b"tx3", b"tx4"] {
            db.write_replay_protection_entry(
                &mut batch,
                &replay_protection::last_key(&Hash::sha256(tx)),
            )
            .unwrap();
        }

        add_block_to_batch(
            &db,
            &mut batch,
            height_0,
            Epoch(1),
            pred_epochs.clone(),
            &conversion_state_0,
        )
        .unwrap();
        db.exec_batch(batch.0).unwrap();

        // Write second block
        let mut batch = RocksDB::batch();
        let height_1 = BlockHeight(101);
        pred_epochs.new_epoch(height_1);
        let conversion_state_1 = ConversionState::default();
        let add_val = vec![1_u8, 0, 0, 0];
        let overwrite_val = vec![1_u8, 1, 1, 1];
        db.batch_write_subspace_val(
            &mut batch, height_1, &add_key, &add_val, true,
        )
        .unwrap();
        db.batch_write_subspace_val(
            &mut batch,
            height_1,
            &overwrite_key,
            &overwrite_val,
            true,
        )
        .unwrap();
        db.batch_delete_subspace_val(&mut batch, height_1, &delete_key, true)
            .unwrap();

        db.prune_replay_protection_buffer(&mut batch).unwrap();
        db.write_replay_protection_entry(
            &mut batch,
            &replay_protection::all_key(&Hash::sha256(b"tx3")),
        )
        .unwrap();

        for tx in [b"tx3", b"tx4"] {
            db.delete_replay_protection_entry(
                &mut batch,
                &replay_protection::last_key(&Hash::sha256(tx)),
            )
            .unwrap();
            db.write_replay_protection_entry(
                &mut batch,
                &replay_protection::buffer_key(&Hash::sha256(tx)),
            )
            .unwrap();
        }

        for tx in [b"tx5", b"tx6"] {
            db.write_replay_protection_entry(
                &mut batch,
                &replay_protection::last_key(&Hash::sha256(tx)),
            )
            .unwrap();
        }

        add_block_to_batch(
            &db,
            &mut batch,
            height_1,
            Epoch(2),
            pred_epochs,
            &conversion_state_1,
        )
        .unwrap();
        db.exec_batch(batch.0).unwrap();

        // Check that the values are as expected from second block
        let added = db.read_subspace_val(&add_key).unwrap();
        assert_eq!(added, Some(add_val));
        let overwritten = db.read_subspace_val(&overwrite_key).unwrap();
        assert_eq!(overwritten, Some(overwrite_val));
        let deleted = db.read_subspace_val(&delete_key).unwrap();
        assert_eq!(deleted, None);

        for tx in [b"tx1", b"tx2", b"tx3", b"tx5", b"tx6"] {
            assert!(db.has_replay_protection_entry(&Hash::sha256(tx)).unwrap());
        }
        assert!(
            !db.has_replay_protection_entry(&Hash::sha256(b"tx4"))
                .unwrap()
        );

        // Rollback to the first block height
        db.rollback(height_0).unwrap();

        // Check that the values are back to the state at the first block
        let added = db.read_subspace_val(&add_key).unwrap();
        assert_eq!(added, None);
        let overwritten = db.read_subspace_val(&overwrite_key).unwrap();
        assert_eq!(overwritten, Some(to_overwrite_val));
        let deleted = db.read_subspace_val(&delete_key).unwrap();
        assert_eq!(deleted, Some(to_delete_val));
        // Check the conversion state
        let state_cf = db.get_column_family(STATE_CF).unwrap();
        let conversion_state =
            db.0.get_cf(state_cf, "conversion_state".as_bytes())
                .unwrap()
                .unwrap();
        assert_eq!(conversion_state, encode(&conversion_state_0));
        for tx in [b"tx1", b"tx2", b"tx3", b"tx4"] {
            assert!(db.has_replay_protection_entry(&Hash::sha256(tx)).unwrap());
        }

        for tx in [b"tx5", b"tx6"] {
            assert!(
                !db.has_replay_protection_entry(&Hash::sha256(tx)).unwrap()
            );
        }
    }

    #[test]
    fn test_diffs() {
        let dir = tempdir().unwrap();
        let mut db = open(dir.path(), None).unwrap();

        let key_with_diffs = Key::parse("with_diffs").unwrap();
        let key_without_diffs = Key::parse("without_diffs").unwrap();

        let initial_val = vec![1_u8, 1, 0, 0];
        let overwrite_val = vec![1_u8, 1, 1, 0];

        // Write first block
        let mut batch = RocksDB::batch();
        let height_0 = BlockHeight::first();
        db.batch_write_subspace_val(
            &mut batch,
            height_0,
            &key_with_diffs,
            &initial_val,
            true,
        )
        .unwrap();
        db.batch_write_subspace_val(
            &mut batch,
            height_0,
            &key_without_diffs,
            &initial_val,
            false,
        )
        .unwrap();
        db.exec_batch(batch.0).unwrap();

        {
            let diffs_cf = db.get_column_family(DIFFS_CF).unwrap();

            // Diffs new key for `key_with_diffs` at height_0 must be present
            let (old_with_h0, new_with_h0) =
                old_and_new_diff_key(&key_with_diffs, height_0).unwrap();
            assert!(db.0.get_cf(diffs_cf, old_with_h0).unwrap().is_none());
            assert!(db.0.get_cf(diffs_cf, new_with_h0).unwrap().is_some());

            // Diffs new key for `key_without_diffs` at height_0 must be present
            let (old_wo_h0, new_wo_h0) =
                old_and_new_diff_key(&key_without_diffs, height_0).unwrap();
            assert!(db.0.get_cf(diffs_cf, old_wo_h0).unwrap().is_none());
            assert!(db.0.get_cf(diffs_cf, new_wo_h0).unwrap().is_some());
        }

        // Write second block
        let mut batch = RocksDB::batch();
        let height_1 = height_0 + 10;
        db.batch_write_subspace_val(
            &mut batch,
            height_1,
            &key_with_diffs,
            &overwrite_val,
            true,
        )
        .unwrap();
        db.batch_write_subspace_val(
            &mut batch,
            height_1,
            &key_without_diffs,
            &overwrite_val,
            false,
        )
        .unwrap();
        db.exec_batch(batch.0).unwrap();

        {
            let diffs_cf = db.get_column_family(DIFFS_CF).unwrap();

            // Diffs keys for `key_with_diffs` at height_0 must be present
            let (old_with_h0, new_with_h0) =
                old_and_new_diff_key(&key_with_diffs, height_0).unwrap();
            assert!(db.0.get_cf(diffs_cf, old_with_h0).unwrap().is_none());
            assert!(db.0.get_cf(diffs_cf, new_with_h0).unwrap().is_some());

            // Diffs keys for `key_without_diffs` at height_0 must be gone
            let (old_wo_h0, new_wo_h0) =
                old_and_new_diff_key(&key_without_diffs, height_0).unwrap();
            assert!(db.0.get_cf(diffs_cf, old_wo_h0).unwrap().is_none());
            assert!(db.0.get_cf(diffs_cf, new_wo_h0).unwrap().is_none());

            // Diffs keys for `key_with_diffs` at height_1 must be present
            let (old_with_h1, new_with_h1) =
                old_and_new_diff_key(&key_with_diffs, height_1).unwrap();
            assert!(db.0.get_cf(diffs_cf, old_with_h1).unwrap().is_some());
            assert!(db.0.get_cf(diffs_cf, new_with_h1).unwrap().is_some());

            // Diffs keys for `key_without_diffs` at height_1 must be present
            let (old_wo_h1, new_wo_h1) =
                old_and_new_diff_key(&key_without_diffs, height_1).unwrap();
            assert!(db.0.get_cf(diffs_cf, old_wo_h1).unwrap().is_some());
            assert!(db.0.get_cf(diffs_cf, new_wo_h1).unwrap().is_some());
        }

        // Write third block
        let mut batch = RocksDB::batch();
        let height_2 = height_1 + 10;
        db.batch_write_subspace_val(
            &mut batch,
            height_2,
            &key_with_diffs,
            &initial_val,
            true,
        )
        .unwrap();
        db.batch_write_subspace_val(
            &mut batch,
            height_2,
            &key_without_diffs,
            &initial_val,
            false,
        )
        .unwrap();
        db.exec_batch(batch.0).unwrap();

        {
            let diffs_cf = db.get_column_family(DIFFS_CF).unwrap();

            // Diffs keys for `key_with_diffs` at height_1 must be present
            let (old_with_h1, new_with_h1) =
                old_and_new_diff_key(&key_with_diffs, height_1).unwrap();
            assert!(db.0.get_cf(diffs_cf, old_with_h1).unwrap().is_some());
            assert!(db.0.get_cf(diffs_cf, new_with_h1).unwrap().is_some());

            // Diffs keys for `key_without_diffs` at height_1 must be gone
            let (old_wo_h1, new_wo_h1) =
                old_and_new_diff_key(&key_without_diffs, height_1).unwrap();
            assert!(db.0.get_cf(diffs_cf, old_wo_h1).unwrap().is_none());
            assert!(db.0.get_cf(diffs_cf, new_wo_h1).unwrap().is_none());

            // Diffs keys for `key_with_diffs` at height_2 must be present
            let (old_with_h2, new_with_h2) =
                old_and_new_diff_key(&key_with_diffs, height_2).unwrap();
            assert!(db.0.get_cf(diffs_cf, old_with_h2).unwrap().is_some());
            assert!(db.0.get_cf(diffs_cf, new_with_h2).unwrap().is_some());

            // Diffs keys for `key_without_diffs` at height_2 must be present
            let (old_wo_h2, new_wo_h2) =
                old_and_new_diff_key(&key_without_diffs, height_2).unwrap();
            assert!(db.0.get_cf(diffs_cf, old_wo_h2).unwrap().is_some());
            assert!(db.0.get_cf(diffs_cf, new_wo_h2).unwrap().is_some());
        }
    }

    /// A test helper to write a block
    fn add_block_to_batch(
        db: &RocksDB,
        batch: &mut RocksDBWriteBatch,
        height: BlockHeight,
        epoch: Epoch,
        pred_epochs: Epochs,
        conversion_state: &ConversionState,
    ) -> Result<()> {
        let merkle_tree = MerkleTree::<Sha256Hasher>::default();
        let merkle_tree_stores = merkle_tree.stores();
        let hash = BlockHash::default();
        let time = DateTimeUtc::now();
        let next_epoch_min_start_height = BlockHeight::default();
        let next_epoch_min_start_time = DateTimeUtc::now();
        let update_epoch_blocks_delay = None;
        let address_gen = EstablishedAddressGen::new("whatever");
        let tx_queue = TxQueue::default();
        let results = BlockResults::default();
        let eth_events_queue = EthEventsQueue::default();
        let block = BlockStateWrite {
            merkle_tree_stores,
            header: None,
            hash: &hash,
            height,
            time,
            epoch,
            results: &results,
            conversion_state,
            pred_epochs: &pred_epochs,
            next_epoch_min_start_height,
            next_epoch_min_start_time,
            update_epoch_blocks_delay,
            address_gen: &address_gen,
            tx_queue: &tx_queue,
            ethereum_height: None,
            eth_events_queue: &eth_events_queue,
        };

        db.add_block_to_batch(block, batch, true)
    }
}
