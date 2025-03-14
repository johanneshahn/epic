// Copyright 2019 The Grin Developers
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Transaction pool implementation leveraging txhashset for chain state
//! validation. It is a valid operation to add a tx to the tx pool if the
//! resulting tx pool can be added to the current chain state to produce a
//! valid chain state.

use self::core::core::hash::{Hash, Hashed};
use self::core::core::id::ShortId;
use self::core::core::{transaction, Block, BlockHeader, Transaction, Weighting};
use self::util::RwLock;
use crate::pool::Pool;
use crate::types::{BlockChain, PoolAdapter, PoolConfig, PoolEntry, PoolError, TxSource};
use chrono::prelude::*;
use epic_core as core;
use epic_util as util;
use std::collections::VecDeque;
use std::sync::Arc;

/// Transaction pool implementation.
pub struct TransactionPool<B, P>
where
	B: BlockChain,
	P: PoolAdapter,
{
	/// Pool Config
	pub config: PoolConfig,
	/// Our transaction pool.
	pub txpool: Pool<B>,
	/// Our Dandelion "stempool".
	pub stempool: Pool<B>,
	/// Cache of previous txs in case of a re-org.
	pub reorg_cache: Arc<RwLock<VecDeque<PoolEntry>>>,
	/// The blockchain
	pub blockchain: Arc<B>,
	/// The pool adapter
	pub adapter: Arc<P>,
}

impl<B, P> TransactionPool<B, P>
where
	B: BlockChain,
	P: PoolAdapter,
{
	/// Create a new transaction pool
	pub fn new(config: PoolConfig, chain: Arc<B>, adapter: Arc<P>) -> Self {
		TransactionPool {
			config,
			txpool: Pool::new(chain.clone(), "txpool".to_string()),
			stempool: Pool::new(chain.clone(), "stempool".to_string()),
			reorg_cache: Arc::new(RwLock::new(VecDeque::new())),
			blockchain: chain,
			adapter,
		}
	}

	pub fn chain_head(&self) -> Result<BlockHeader, PoolError> {
		self.blockchain.chain_head()
	}

	// Add tx to stempool (passing in all txs from txpool to validate against).
	fn add_to_stempool(&mut self, entry: PoolEntry, header: &BlockHeader) -> Result<(), PoolError> {
		self.stempool
			.add_to_pool(entry, self.txpool.all_transactions(), header)?;
		Ok(())
	}

	fn add_to_reorg_cache(&mut self, entry: PoolEntry) {
		let mut cache = self.reorg_cache.write();
		cache.push_back(entry);

		// We cache 30 mins of txs but we have a hard limit to avoid catastrophic failure.
		// For simplicity use the same value as the actual tx pool limit.
		if cache.len() > self.config.max_pool_size {
			let _ = cache.pop_front();
		}
		debug!("added tx to reorg_cache: size now {}", cache.len());
	}

	fn add_to_txpool(
		&mut self,
		mut entry: PoolEntry,
		header: &BlockHeader,
	) -> Result<(), PoolError> {
		// First deaggregate the tx based on current txpool txs.
		if entry.tx.kernels().len() > 1 {
			let txs = self.txpool.find_matching_transactions(entry.tx.kernels());
			if !txs.is_empty() {
				let tx = transaction::deaggregate(entry.tx, txs)?;

				// Validate this deaggregated tx "as tx", subject to regular tx weight limits.
				tx.validate(Weighting::AsTransaction)?;

				entry.tx = tx;
				entry.src = TxSource::Deaggregate;
			}
		}
		self.txpool.add_to_pool(entry.clone(), vec![], header)?;

		// We now need to reconcile the stempool based on the new state of the txpool.
		// Some stempool txs may no longer be valid and we need to evict them.
		{
			let txpool_tx = self.txpool.all_transactions_aggregate()?;
			self.stempool.reconcile(txpool_tx, header)?;
		}
		Ok(())
	}

	/// Add the given tx to the pool, directing it to either the stempool or
	/// txpool based on stem flag provided.
	pub fn add_to_pool(
		&mut self,
		src: TxSource,
		tx: Transaction,
		stem: bool,
		header: &BlockHeader,
	) -> Result<(), PoolError> {
		// Quick check to deal with common case of seeing the *same* tx
		// broadcast from multiple peers simultaneously.
		if !stem && self.txpool.contains_tx(tx.hash()) {
			return Err(PoolError::DuplicateTx);
		}

		// Do we have the capacity to accept this transaction?
		let acceptability = self.is_acceptable(&tx, stem);
		let mut evict = false;
		if !stem && acceptability.as_ref().err() == Some(&PoolError::OverCapacity) {
			evict = true;
		} else if acceptability.is_err() {
			return acceptability;
		}

		// Make sure the transaction is valid before anything else.
		// Validate tx accounting for max tx weight.
		tx.validate(Weighting::AsTransaction)
			.map_err(PoolError::InvalidTx)?;

		// Check the tx lock_time is valid based on current chain state.
		self.blockchain.verify_tx_lock_height(&tx)?;

		// Check coinbase maturity before we go any further.
		self.blockchain.verify_coinbase_maturity(&tx)?;

		let entry = PoolEntry {
			src,
			tx_at: Utc::now(),
			tx,
		};

		// If not stem then we are fluff.
		// If this is a stem tx then attempt to stem.
		// Any problems during stem, fallback to fluff.
		if !stem
			|| self
				.add_to_stempool(entry.clone(), header)
				.and_then(|_| self.adapter.stem_tx_accepted(&entry))
				.is_err()
		{
			self.add_to_txpool(entry.clone(), header)?;
			self.add_to_reorg_cache(entry.clone());
			self.adapter.tx_accepted(&entry);
		}

		// Transaction passed all the checks but we have to make space for it
		if evict {
			self.evict_from_txpool();
		}

		Ok(())
	}

	// Remove the last transaction from the flattened bucket transactions.
	// No other tx depends on it, it has low fee_to_weight and is unlikely to participate in any cut-through.
	pub fn evict_from_txpool(&mut self) {
		// Get bucket transactions
		let bucket_transactions = self.txpool.bucket_transactions(Weighting::NoLimit);

		// Get last transaction and remove it
		match bucket_transactions.last() {
			Some(evictable_transaction) => {
				// Remove transaction
				self.txpool.entries = self
					.txpool
					.entries
					.iter()
					.filter(|x| x.tx != *evictable_transaction)
					.map(|x| x.clone())
					.collect::<Vec<_>>();
			}
			None => (),
		}
	}

	// Old txs will "age out" after 30 mins.
	pub fn truncate_reorg_cache(&mut self, cutoff: DateTime<Utc>) {
		let mut cache = self.reorg_cache.write();

		while cache.front().map(|x| x.tx_at < cutoff).unwrap_or(false) {
			let _ = cache.pop_front();
		}

		debug!("truncate_reorg_cache: size: {}", cache.len());
	}

	pub fn reconcile_reorg_cache(&mut self, header: &BlockHeader) -> Result<(), PoolError> {
		let entries = self.reorg_cache.read().iter().cloned().collect::<Vec<_>>();
		debug!(
			"reconcile_reorg_cache: size: {}, block: {:?} ...",
			entries.len(),
			header.hash(),
		);
		for entry in entries {
			let _ = &self.add_to_txpool(entry.clone(), header);
		}
		debug!(
			"reconcile_reorg_cache: block: {:?} ... done.",
			header.hash()
		);
		Ok(())
	}

	/// Reconcile the transaction pool (both txpool and stempool) against the
	/// provided block.
	pub fn reconcile_block(&mut self, block: &Block) -> Result<(), PoolError> {
		// First reconcile the txpool.
		self.txpool.reconcile_block(block);
		self.txpool.reconcile(None, &block.header)?;

		// Now reconcile our stempool, accounting for the updated txpool txs.
		self.stempool.reconcile_block(block);
		{
			let txpool_tx = self.txpool.all_transactions_aggregate()?;
			self.stempool.reconcile(txpool_tx, &block.header)?;
		}

		Ok(())
	}

	/// Retrieve individual transaction for the given kernel hash.
	pub fn retrieve_tx_by_kernel_hash(&self, hash: Hash) -> Option<Transaction> {
		self.txpool.retrieve_tx_by_kernel_hash(hash)
	}

	/// Retrieve all transactions matching the provided "compact block"
	/// based on the kernel set.
	/// Note: we only look in the txpool for this (stempool is under embargo).
	pub fn retrieve_transactions(
		&self,
		hash: Hash,
		nonce: u64,
		kern_ids: &[ShortId],
	) -> (Vec<Transaction>, Vec<ShortId>) {
		self.txpool.retrieve_transactions(hash, nonce, kern_ids)
	}

	/// Whether the transaction is acceptable to the pool, given both how
	/// full the pool is and the transaction weight.
	fn is_acceptable(&self, tx: &Transaction, stem: bool) -> Result<(), PoolError> {
		if self.total_size() > self.config.max_pool_size {
			return Err(PoolError::OverCapacity);
		}

		// Check that the stempool can accept this transaction
		if stem && self.stempool.size() > self.config.max_stempool_size {
			return Err(PoolError::OverCapacity);
		} else if self.total_size() > self.config.max_pool_size {
			return Err(PoolError::OverCapacity);
		}

		// for a basic transaction (1 input, 2 outputs) -
		// (-1 * 1) + (4 * 2) + 1 = 8
		// 8 * 10 = 80
		if self.config.accept_fee_base > 0 {
			let threshold = (tx.tx_weight() as u64) * self.config.accept_fee_base;
			if tx.fee() < threshold {
				return Err(PoolError::LowFeeTransaction(threshold));
			}
		}
		Ok(())
	}

	/// Get the total size of the pool.
	/// Note: we only consider the txpool here as stempool is under embargo.
	pub fn total_size(&self) -> usize {
		self.txpool.size()
	}

	/// Returns a vector of transactions from the txpool so we can build a
	/// block from them.
	pub fn prepare_mineable_transactions(&self) -> Result<Vec<Transaction>, PoolError> {
		self.txpool
			.prepare_mineable_transactions(self.config.mineable_max_weight)
	}
}
