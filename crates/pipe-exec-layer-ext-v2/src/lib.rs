//! Pipeline execution layer extension
#[macro_use]
mod channel;
mod metrics;

use channel::Channel;
use metrics::PipeExecLayerMetrics;

use alloy_consensus::{
    constants::EMPTY_WITHDRAWALS, BlockHeader, Header, Transaction, EMPTY_OMMER_ROOT_HASH,
};
use alloy_eips::{eip4895::Withdrawals, merge::BEACON_NONCE};
use alloy_primitives::{Address, B256, U256};
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use reth_chain_state::ExecutedBlockWithTrieUpdates;
use reth_chainspec::{ChainSpec, EthereumHardforks};
use reth_ethereum_primitives::{Block, BlockBody, Receipt, TransactionSigned};
use reth_evm::{
    database::*,
    execute::{BlockExecutorProvider, Executor},
    parallel_database, ConfigureEvmEnv, NextBlockEnvAttributes,
};
use reth_evm_ethereum::{execute::EthExecutorProvider, EthEvmConfig};
use reth_execution_types::{BlockExecutionOutput, ExecutionOutcome};
use reth_primitives::{EthPrimitives, NodePrimitives};
use reth_primitives_traits::{
    proofs::{self},
    Block as _, RecoveredBlock,
};
use revm::primitives::{AccountInfo, HashMap, HashSet};
use std::{any::Any, collections::BTreeMap, sync::Arc, time::Instant};

use once_cell::sync::{Lazy, OnceCell};

use gravity_storage::GravityStorage;
use tokio::sync::{
    mpsc::{UnboundedReceiver, UnboundedSender},
    oneshot,
};

use tracing::*;

#[derive(Debug, Clone, Copy)]
pub struct ExecutedBlockMeta {
    /// Which ordered block is used to execute the block
    pub block_id: B256,
    /// Block hash of the executed block
    pub block_hash: B256,
}

#[derive(Debug)]
pub struct OrderedBlock {
    /// BlockId of the parent block generated by Gravity SDK
    pub parent_id: B256,
    /// BlockId of the block generated by Gravity SDK
    pub id: B256,
    pub number: u64,
    pub timestamp: u64,
    pub coinbase: Address,
    pub prev_randao: B256,
    pub withdrawals: Withdrawals,
    /// Ordered transactions in the block
    pub transactions: Vec<TransactionSigned>,
    /// Senders of the transactions in the block
    pub senders: Vec<Address>,
}

#[derive(Debug)]
pub enum PipeExecLayerEvent<N: NodePrimitives> {
    /// Make executed block canonical
    MakeCanonical(ExecutedBlockWithTrieUpdates<N>, oneshot::Sender<()>),
}

#[derive(Debug)]
pub struct ExecutionArgs {
    pub block_number_to_block_id: BTreeMap<u64, B256>,
}
/// Owned by EL
#[derive(Debug)]
struct PipeExecService<Storage: GravityStorage> {
    /// Immutable part of the state
    core: Arc<Core<Storage>>,
    /// Receive ordered block from Coordinator
    ordered_block_rx: UnboundedReceiver<OrderedBlock>,
    /// Receive the execution init args from GravitySDK
    execution_args_rx: oneshot::Receiver<ExecutionArgs>,
}

#[derive(Debug)]
struct Core<Storage: GravityStorage> {
    /// Send executed block hash to Coordinator
    executed_block_hash_tx: Arc<Channel<B256 /* block id */, B256 /* block hash */>>,
    /// Receive verified block hash from Coordinator
    verified_block_hash_rx: Arc<Channel<B256 /* block id */, B256 /* block hash */>>,
    storage: Storage,
    evm_config: EthEvmConfig,
    chain_spec: Arc<ChainSpec>,
    event_tx: std::sync::mpsc::Sender<PipeExecLayerEvent<EthPrimitives>>,
    execute_block_barrier: Channel<u64 /* block number */, (Header, Instant)>,
    merklize_barrier: Channel<u64 /* block number */, ()>,
    seal_barrier: Channel<u64 /* block number */, B256 /* block hash */>,
    make_canonical_barrier: Channel<u64 /* block number */, Instant>,
    metrics: PipeExecLayerMetrics,
}

impl<Storage: GravityStorage> PipeExecService<Storage> {
    async fn run(mut self, mut latest_block_number: u64) {
        self.core.init_storage(self.execution_args_rx.await.unwrap());
        loop {
            let start_time = Instant::now();
            let ordered_block = match self.ordered_block_rx.recv().await {
                Some(ordered_block) => ordered_block,
                None => {
                    self.core.executed_block_hash_tx.close();
                    self.core.execute_block_barrier.close();
                    self.core.merklize_barrier.close();
                    self.core.make_canonical_barrier.close();
                    return;
                }
            };
            self.core.metrics.recv_block_time_diff.record(start_time.elapsed());
            // TODO: read latest block id from storage
            // assert_eq!(ordered_block.parent_id, latest_block_id);
            // latest_block_id = ordered_block.id;
            assert_eq!(ordered_block.number, latest_block_number + 1);
            latest_block_number = ordered_block.number;

            let core = self.core.clone();
            tokio::spawn(async move {
                core.process(ordered_block).await;
            });
        }
    }
}

const BLOCK_GAS_LIMIT_1G: u64 = 1_000_000_000;

impl<Storage: GravityStorage> Core<Storage> {
    async fn process(&self, ordered_block: OrderedBlock) {
        let block_number = ordered_block.number;
        let block_id = ordered_block.id;
        debug!(target: "PipeExecService.process",
            id=?block_id,
            parent_id=?ordered_block.parent_id,
            number=?block_number,
            "new ordered block"
        );

        self.storage.insert_block_id(block_number, block_id);
        // Retrieve the parent block header to generate the necessary configs for
        // executing the current block
        let (parent_block_header, prev_start_execute_time) =
            self.execute_block_barrier.wait(block_number - 1).await.unwrap();
        let start_time = Instant::now();
        let (mut block, senders, outcome) =
            self.execute_ordered_block(ordered_block, &parent_block_header);
        self.storage.insert_bundle_state(block_number, &outcome.state);
        self.metrics.execute_duration.record(start_time.elapsed());
        self.metrics.start_execute_time_diff.record(start_time - prev_start_execute_time);
        self.execute_block_barrier
            .notify(block_number, (block.header.clone(), start_time))
            .unwrap();

        let execution_outcome = self.calculate_roots(&mut block, outcome);

        // Merkling the state trie
        self.merklize_barrier.wait(block_number - 1).await.unwrap();
        let (state_root, hashed_state, trie_updates) =
            self.storage.state_root_with_updates(block_number).unwrap();
        self.metrics.merklize_duration.record(start_time.elapsed());
        self.merklize_barrier.notify(block_number, ()).unwrap();
        debug!(target: "PipeExecService.process",
            block_number=?block_number,
            block_id=?block_id,
            state_root=?state_root,
            "state trie merklized"
        );
        block.header.state_root = state_root;

        let parent_hash = self.seal_barrier.wait(block_number - 1).await.unwrap();
        let start_time = Instant::now();
        block.header.parent_hash = parent_hash;

        // Seal the block
        let block = block.seal_slow();
        let block_hash = block.hash();
        self.metrics.seal_duration.record(start_time.elapsed());
        self.seal_barrier.notify(block_number, block_hash).unwrap();
        debug!(target: "PipeExecService.process",
            block_number=?block_number,
            block_id=?block_id,
            block_hash=?block_hash,
            transactions_root=?block.header().transactions_root,
            receipts_root=?block.header().receipts_root,
            "block sealed"
        );

        // Commit the executed block hash to Coordinator
        let start_time = Instant::now();
        self.verify_executed_block_hash(ExecutedBlockMeta { block_id, block_hash }).await.unwrap();
        self.metrics.verify_duration.record(start_time.elapsed());
        debug!(target: "PipeExecService.process",
            block_number=?block_number,
            block_id=?block_id,
            block_hash=?block_hash,
            "block verified"
        );

        let gas_used = block.gas_used;

        // Make the block canonical
        let prev_finish_commit_time =
            self.make_canonical_barrier.wait(block_number - 1).await.unwrap();
        self.make_canonical(ExecutedBlockWithTrieUpdates::new(
            Arc::new(RecoveredBlock::new_sealed(block, senders)),
            Arc::new(execution_outcome),
            hashed_state,
            trie_updates,
        ))
        .await;
        self.storage.update_canonical(block_number, block_hash);
        let finish_commit_time = Instant::now();
        self.metrics.make_canonical_duration.record(start_time.elapsed());
        self.metrics.finish_commit_time_diff.record(finish_commit_time - prev_finish_commit_time);
        self.make_canonical_barrier.notify(block_number, finish_commit_time).unwrap();

        self.metrics.total_gas_used.increment(gas_used);
    }

    /// Push executed block hash to Coordinator and wait for verification result from Coordinator.
    /// Returns `None` if the channel has been closed.
    async fn verify_executed_block_hash(&self, block_meta: ExecutedBlockMeta) -> Option<()> {
        self.executed_block_hash_tx.notify(block_meta.block_id, block_meta.block_hash)?;
        let block_hash = self.verified_block_hash_rx.wait(block_meta.block_id).await?;
        assert_eq!(block_meta.block_hash, block_hash);
        Some(())
    }

    fn execute_ordered_block(
        &self,
        ordered_block: OrderedBlock,
        parent_header: &Header,
    ) -> (Block, Vec<Address>, BlockExecutionOutput<Receipt>) {
        assert_eq!(ordered_block.transactions.len(), ordered_block.senders.len());

        debug!(target: "execute_ordered_block",
            id=?ordered_block.id,
            parent_id=?ordered_block.parent_id,
            number=?ordered_block.number,
            "ready to execute block"
        );

        let evm_env = self
            .evm_config
            .next_evm_env(
                parent_header,
                NextBlockEnvAttributes {
                    timestamp: ordered_block.timestamp,
                    suggested_fee_recipient: ordered_block.coinbase,
                    prev_randao: ordered_block.prev_randao,
                    gas_limit: BLOCK_GAS_LIMIT_1G,
                },
            )
            .unwrap();

        let mut block = Block {
            header: Header {
                ommers_hash: EMPTY_OMMER_ROOT_HASH,
                beneficiary: ordered_block.coinbase,
                timestamp: ordered_block.timestamp,
                mix_hash: ordered_block.prev_randao,
                nonce: BEACON_NONCE.into(),
                base_fee_per_gas: Some(evm_env.block_env.basefee.to::<u64>()),
                number: ordered_block.number,
                gas_limit: evm_env.block_env.gas_limit.to(),
                difficulty: U256::ZERO,
                ..Default::default()
            },
            body: BlockBody::default(),
        };

        if self.chain_spec.is_shanghai_active_at_timestamp(block.timestamp) {
            if ordered_block.withdrawals.is_empty() {
                block.header.withdrawals_root = Some(EMPTY_WITHDRAWALS);
                block.body.withdrawals = Some(Withdrawals::default());
            } else {
                block.header.withdrawals_root =
                    Some(proofs::calculate_withdrawals_root(&ordered_block.withdrawals));
                block.body.withdrawals = Some(ordered_block.withdrawals);
            }
        }

        // only determine cancun fields when active
        if self.chain_spec.is_cancun_active_at_timestamp(block.timestamp) {
            // FIXME: Is it OK to use the parent's block id as `parent_beacon_block_root` before
            // execution?
            block.header.parent_beacon_block_root = Some(ordered_block.parent_id);

            // TODO(nekomoto): fill `excess_blob_gas` and `blob_gas_used` fields
            block.header.excess_blob_gas = Some(0);
            block.header.blob_gas_used = Some(0);
        }

        let (parent_id, state) = self.storage.get_state_view(block.number - 1).unwrap();
        assert_eq!(parent_id, ordered_block.parent_id);

        // Discard the invalid txs
        let start_time = Instant::now();
        let (txs, senders) = filter_invalid_txs(
            &state,
            ordered_block.transactions,
            ordered_block.senders,
            evm_env.block_env.basefee,
        );
        self.metrics.filter_transaction_duration.record(start_time.elapsed());

        block.body.transactions = txs;
        let recovered_block = RecoveredBlock::new_unhashed(block, senders);

        let executor = EthExecutorProvider::ethereum(self.chain_spec.clone())
            .executor(parallel_database! { state });

        let outcome = executor.execute(&recovered_block).unwrap_or_else(|err| {
            serde_json::to_writer(
                std::io::BufWriter::new(
                    std::fs::File::create(format!("{}.json", ordered_block.id)).unwrap(),
                ),
                &recovered_block,
            )
            .unwrap();
            panic!("failed to execute block {:?}: {:?}", ordered_block.id, err)
        });

        debug!(target: "execute_ordered_block",
            id=?ordered_block.id,
            parent_id=?ordered_block.parent_id,
            number=?ordered_block.number,
            "block executed"
        );

        let (mut block, senders) = recovered_block.split();
        block.header.gas_used = outcome.gas_used;
        (block, senders, outcome)
    }

    /// Calculate the receipts root, logs bloom, and transactions root, etc. and fill them into the
    /// block header.
    fn calculate_roots(
        &self,
        block: &mut Block,
        execution_outcome: BlockExecutionOutput<Receipt>,
    ) -> ExecutionOutcome {
        // only determine cancun fields when active
        if self.chain_spec.is_prague_active_at_timestamp(block.timestamp) {
            block.header.requests_hash = Some(execution_outcome.requests.requests_hash());
        }

        let execution_outcome = ExecutionOutcome::new(
            execution_outcome.state,
            vec![execution_outcome.receipts],
            block.number,
            vec![execution_outcome.requests.into()],
        );

        let receipts_root =
            execution_outcome.ethereum_receipts_root(block.number).expect("Number is in range");
        let logs_bloom =
            execution_outcome.block_logs_bloom(block.number).expect("Number is in range");

        let transactions_root = proofs::calculate_transaction_root(&block.body.transactions);

        // Fill the block header with the calculated values
        block.header.transactions_root = transactions_root;
        block.header.receipts_root = receipts_root;
        block.header.logs_bloom = logs_bloom;

        execution_outcome
    }

    async fn make_canonical(&self, executed_block: ExecutedBlockWithTrieUpdates) {
        let block_number = executed_block.recovered_block.number();

        // Make executed block canonical
        let (tx, rx) = oneshot::channel();
        self.event_tx.send(PipeExecLayerEvent::MakeCanonical(executed_block, tx)).unwrap();
        rx.await.unwrap();

        debug!(target: "make_canonical", block_number=?block_number, "block made canonical");
    }

    fn init_storage(&self, execution_args: ExecutionArgs) {
        execution_args.block_number_to_block_id.into_iter().for_each(|(block_number, block_id)| {
            self.storage.insert_block_id(block_number, block_id);
        });
    }
}

/// Return the filtered valid transactions with sender without changing the relative order of
/// the transactions.
fn filter_invalid_txs<DB: ParallelDatabase>(
    db: DB,
    txs: Vec<TransactionSigned>,
    senders: Vec<Address>,
    base_fee_per_gas: U256,
) -> (Vec<TransactionSigned>, Vec<Address>) {
    let mut sender_idx: HashMap<&Address, Vec<usize>> = HashMap::default();
    for (i, sender) in senders.iter().enumerate() {
        sender_idx.entry(sender).or_insert_with(Vec::new).push(i);
    }

    let is_tx_valid = |tx: &TransactionSigned, sender: &Address, account: &mut AccountInfo| {
        if account.nonce != tx.transaction().nonce() {
            debug!(target: "filter_invalid_txs",
                tx_hash=?tx.hash(),
                sender=?sender,
                nonce=?tx.transaction().nonce(),
                account_nonce=?account.nonce,
                "nonce mismatch"
            );
            return false;
        }
        let gas_spent = U256::from(tx.transaction().gas_limit()) *
            (U256::from(tx.transaction().priority_fee_or_price()) + base_fee_per_gas);
        if account.balance < gas_spent {
            debug!(target: "filter_invalid_txs",
                tx_hash=?tx.hash(),
                sender=?sender,
                balance=?account.balance,
                gas_spent=?gas_spent,
                "insufficient balance"
            );
            return false;
        }
        account.balance -= gas_spent;
        account.nonce += 1;
        true
    };

    let invalid_idxs = sender_idx
        .into_par_iter()
        .flat_map(|(sender, idxs)| {
            if let Some(mut account) = db.basic_ref(*sender).unwrap() {
                idxs.into_iter()
                    .filter(|&idx| !is_tx_valid(&txs[idx], sender, &mut account))
                    .collect()
            } else {
                // Sender should exist in the state
                debug!(target: "filter_invalid_txs",
                    tx_hash=?txs[idxs[0]].hash(),
                    sender=?sender,
                    "sender not found"
                );
                idxs
            }
        })
        .collect::<HashSet<_>>();

    if !invalid_idxs.is_empty() {
        let mut filtered_txs = Vec::with_capacity(txs.len() - invalid_idxs.len());
        let mut filtered_senders = Vec::with_capacity(filtered_txs.capacity());
        for (i, (tx, sender)) in txs.into_iter().zip(senders.into_iter()).enumerate() {
            if invalid_idxs.contains(&i) {
                continue;
            }
            filtered_txs.push(tx);
            filtered_senders.push(sender);
        }
        (filtered_txs, filtered_senders)
    } else {
        (txs, senders)
    }
}

/// Called by Coordinator
#[derive(Debug)]
pub struct PipeExecLayerApi {
    ordered_block_tx: UnboundedSender<OrderedBlock>,
    executed_block_hash_rx: Arc<Channel<B256 /* block id */, B256 /* block hash */>>,
    verified_block_hash_tx: Arc<Channel<B256 /* block id */, B256 /* block hash */>>,
}

impl PipeExecLayerApi {
    /// Push ordered block to EL for execution.
    /// Returns `None` if the channel has been closed.
    pub fn push_ordered_block(&self, block: OrderedBlock) -> Option<()> {
        self.ordered_block_tx.send(block).ok()
    }

    /// Pull executed block hash from EL for verification.
    /// Returns `None` if the channel has been closed.
    pub async fn pull_executed_block_hash(&self, block_id: B256) -> Option<B256> {
        self.executed_block_hash_rx.wait(block_id).await
    }

    /// Push verified block hash to EL for commit.
    /// Returns `None` if the channel has been closed.
    pub fn commit_executed_block_hash(&self, block_meta: ExecutedBlockMeta) -> Option<()> {
        self.verified_block_hash_tx.notify(block_meta.block_id, block_meta.block_hash)
    }
}

impl Drop for PipeExecLayerApi {
    fn drop(&mut self) {
        self.verified_block_hash_tx.close();
    }
}

/// Called by EL.
#[derive(Debug)]
pub struct PipeExecLayerExt<N: NodePrimitives> {
    /// Receive events from PipeExecService
    pub event_rx: std::sync::Mutex<std::sync::mpsc::Receiver<PipeExecLayerEvent<N>>>,
}

/// A static instance of `PipeExecLayerExt` used for dispatching events.
pub static PIPE_EXEC_LAYER_EXT: OnceCell<Box<dyn Any + Send + Sync>> = OnceCell::new();

pub fn get_pipe_exec_layer_ext<N: NodePrimitives>() -> Option<&'static PipeExecLayerExt<N>> {
    PIPE_EXEC_LAYER_EXT.get().map(|ext| ext.downcast_ref::<PipeExecLayerExt<N>>().unwrap())
}

/// Whether to validate the block before inserting it into `TreeState`.
pub static PIPE_VALIDATE_BLOCK_BEFORE_INSERT: Lazy<bool> =
    Lazy::new(|| std::env::var("PIPE_VALIDATE_BLOCK_BEFORE_INSERT").is_ok());

/// Create a new `PipeExecLayerApi` instance and launch a `PipeExecService`.
pub fn new_pipe_exec_layer_api<Storage: GravityStorage>(
    chain_spec: Arc<ChainSpec>,
    storage: Storage,
    latest_block_header: Header,
    latest_block_hash: B256,
    execution_args_rx: oneshot::Receiver<ExecutionArgs>,
) -> PipeExecLayerApi {
    let (ordered_block_tx, ordered_block_rx) = tokio::sync::mpsc::unbounded_channel();
    let executed_block_hash_ch = Arc::new(Channel::new());
    let verified_block_hash_ch = Arc::new(Channel::new());
    let (event_tx, event_rx) = std::sync::mpsc::channel();

    let latest_block_number = latest_block_header.number;
    let start_time = Instant::now();
    let service = PipeExecService {
        core: Arc::new(Core {
            executed_block_hash_tx: executed_block_hash_ch.clone(),
            verified_block_hash_rx: verified_block_hash_ch.clone(),
            storage,
            evm_config: EthEvmConfig::new(chain_spec.clone()),
            chain_spec,
            event_tx,
            execute_block_barrier: Channel::new_with_states([(
                latest_block_number,
                (latest_block_header, start_time),
            )]),
            merklize_barrier: Channel::new_with_states([(latest_block_number, ())]),
            seal_barrier: Channel::new_with_states([(latest_block_number, latest_block_hash)]),
            make_canonical_barrier: Channel::new_with_states([(latest_block_number, start_time)]),
            metrics: PipeExecLayerMetrics::default(),
        }),
        ordered_block_rx,
        execution_args_rx,
    };
    tokio::spawn(service.run(latest_block_number));

    PIPE_EXEC_LAYER_EXT.get_or_init(|| Box::new(PipeExecLayerExt { event_rx: event_rx.into() }));

    PipeExecLayerApi {
        ordered_block_tx,
        executed_block_hash_rx: executed_block_hash_ch,
        verified_block_hash_tx: verified_block_hash_ch,
    }
}
