//! BSC Node types configuration.
//!
//! BSC is EVM-compatible, so we reuse Ethereum execution components
//! with BSC-specific chain specification and hardforks.

use reth_chainspec::{ChainSpec, EthChainSpec, EthereumHardforks, Hardforks};
use reth_engine_primitives::EngineTypes;
use reth_ethereum_consensus::EthBeaconConsensus;
use reth_ethereum_engine_primitives::{
    EthBuiltPayload, EthPayloadAttributes, EthPayloadBuilderAttributes,
};
use reth_ethereum_primitives::{EthPrimitives, TransactionSigned};
use reth_evm::{
    eth::spec::EthExecutorSpec, ConfigureEvm, EvmFactory, EvmFactoryFor, NextBlockEnvAttributes,
};
use reth_node_api::{FullNodeComponents, NodeAddOns, NodePrimitives, PrimitivesTy, TxTy};
use reth_node_builder::{
    components::{
        BasicPayloadServiceBuilder, ComponentsBuilder, ConsensusBuilder, ExecutorBuilder,
        NetworkBuilder, PoolBuilder, TxPoolBuilder,
    },
    node::{FullNodeTypes, NodeTypes},
    rpc::{
        BasicEngineApiBuilder, BasicEngineValidatorBuilder, EngineApiBuilder,
        EngineValidatorAddOn, EngineValidatorBuilder, EthApiBuilder, Identity,
        PayloadValidatorBuilder, RethRpcAddOns, RpcAddOns, RpcHandle,
    },
    BuilderContext, Node, NodeAdapter,
};
use reth_node_ethereum::{
    EthEvmConfig, EthereumEngineValidator, EthereumEngineValidatorBuilder, EthereumEthApiBuilder,
    EthereumPayloadBuilder,
};
use reth_payload_primitives::PayloadTypes;
use reth_provider::{providers::ProviderFactoryBuilder, EthStorage};
use reth_rpc::ValidationApi;
use reth_rpc_api::servers::BlockSubmissionValidationApiServer;
use reth_rpc_builder::{config::RethRpcServerConfig, middleware::RethRpcMiddleware};
use reth_rpc_eth_api::helpers::config::{EthConfigApiServer, EthConfigHandler};
use reth_rpc_eth_types::{error::FromEvmError, EthApiError};
use reth_rpc_server_types::RethRpcModule;
use reth_transaction_pool::{
    blobstore::DiskFileBlobStore, EthTransactionPool, PoolPooledTx, PoolTransaction,
    TransactionPool, TransactionValidationTaskExecutor,
};

use alloy_eips::{eip7840::BlobParams, merge::EPOCH_SLOTS};
use alloy_rpc_types_engine::ExecutionData;
use revm::context::TxEnv;

use reth_network::{primitives::BasicNetworkPrimitives, NetworkHandle, PeersInfo};
use reth_tracing::tracing::{debug, info};
use std::{sync::Arc, time::SystemTime};

use crate::EthEngineTypes;

/// Type configuration for a BSC node.
///
/// BSC is EVM-compatible, so it reuses Ethereum primitives, execution, and engine types.
/// The key difference is the chain specification which includes BSC-specific hardforks.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct BscNode;

impl BscNode {
    /// Returns a [`ComponentsBuilder`] configured for a BSC node.
    pub fn components<Node>() -> ComponentsBuilder<
        Node,
        BscPoolBuilder,
        BasicPayloadServiceBuilder<EthereumPayloadBuilder>,
        BscNetworkBuilder,
        BscExecutorBuilder,
        BscConsensusBuilder,
    >
    where
        Node: FullNodeTypes<
            Types: NodeTypes<
                ChainSpec: Hardforks + EthereumHardforks + EthExecutorSpec,
                Primitives = EthPrimitives,
            >,
        >,
        <Node::Types as NodeTypes>::Payload: PayloadTypes<
            BuiltPayload = EthBuiltPayload,
            PayloadAttributes = EthPayloadAttributes,
            PayloadBuilderAttributes = EthPayloadBuilderAttributes,
        >,
    {
        ComponentsBuilder::default()
            .node_types::<Node>()
            .pool(BscPoolBuilder::default())
            .executor(BscExecutorBuilder::default())
            .payload(BasicPayloadServiceBuilder::default())
            .network(BscNetworkBuilder::default())
            .consensus(BscConsensusBuilder::default())
    }

    /// Instantiates the [`ProviderFactoryBuilder`] for a BSC node.
    pub fn provider_factory_builder() -> ProviderFactoryBuilder<Self> {
        ProviderFactoryBuilder::default()
    }
}

impl NodeTypes for BscNode {
    type Primitives = EthPrimitives;
    type ChainSpec = ChainSpec;
    type Storage = EthStorage;
    type Payload = EthEngineTypes;
}

/// Add-ons for BSC node.
#[derive(Debug)]
pub struct BscAddOns<
    N: FullNodeComponents,
    EthB: EthApiBuilder<N>,
    PVB,
    EB = BasicEngineApiBuilder<PVB>,
    EVB = BasicEngineValidatorBuilder<PVB>,
    RpcMiddleware = Identity,
> {
    inner: RpcAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware>,
}

impl<N, EthB, PVB, EB, EVB, RpcMiddleware> BscAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware>
where
    N: FullNodeComponents,
    EthB: EthApiBuilder<N>,
{
    /// Creates a new instance from the inner `RpcAddOns`.
    pub const fn new(inner: RpcAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware>) -> Self {
        Self { inner }
    }
}

impl<N> Default for BscAddOns<N, EthereumEthApiBuilder, EthereumEngineValidatorBuilder>
where
    N: FullNodeComponents<
        Types: NodeTypes<
            ChainSpec: EthereumHardforks + Clone + 'static,
            Payload: EngineTypes<ExecutionData = ExecutionData>
                         + PayloadTypes<PayloadAttributes = EthPayloadAttributes>,
            Primitives = EthPrimitives,
        >,
    >,
    EthereumEthApiBuilder: EthApiBuilder<N>,
{
    fn default() -> Self {
        Self::new(RpcAddOns::new(
            EthereumEthApiBuilder::default(),
            EthereumEngineValidatorBuilder::default(),
            BasicEngineApiBuilder::default(),
            BasicEngineValidatorBuilder::default(),
            Default::default(),
        ))
    }
}

impl<N, EthB, PVB, EB, EVB, RpcMiddleware> NodeAddOns<N>
    for BscAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware>
where
    N: FullNodeComponents<
        Types: NodeTypes<
            ChainSpec: Hardforks + EthereumHardforks,
            Primitives = EthPrimitives,
            Payload: EngineTypes<ExecutionData = ExecutionData>,
        >,
        Evm: ConfigureEvm<NextBlockEnvCtx = NextBlockEnvAttributes>,
    >,
    EthB: EthApiBuilder<N>,
    PVB: Send,
    EB: EngineApiBuilder<N>,
    EVB: EngineValidatorBuilder<N>,
    EthApiError: FromEvmError<N::Evm>,
    EvmFactoryFor<N::Evm>: EvmFactory<Tx = TxEnv>,
    RpcMiddleware: RethRpcMiddleware,
{
    type Handle = RpcHandle<N, EthB::EthApi>;

    async fn launch_add_ons(
        self,
        ctx: reth_node_api::AddOnsContext<'_, N>,
    ) -> eyre::Result<Self::Handle> {
        let validation_api = ValidationApi::<_, _, <N::Types as NodeTypes>::Payload>::new(
            ctx.node.provider().clone(),
            Arc::new(ctx.node.consensus().clone()),
            ctx.node.evm_config().clone(),
            ctx.config.rpc.flashbots_config(),
            Box::new(ctx.node.task_executor().clone()),
            Arc::new(EthereumEngineValidator::new(ctx.config.chain.clone())),
        );

        let eth_config =
            EthConfigHandler::new(ctx.node.provider().clone(), ctx.node.evm_config().clone());

        self.inner
            .launch_add_ons_with(ctx, move |container| {
                container.modules.merge_if_module_configured(
                    RethRpcModule::Flashbots,
                    validation_api.into_rpc(),
                )?;

                container
                    .modules
                    .merge_if_module_configured(RethRpcModule::Eth, eth_config.into_rpc())?;

                Ok(())
            })
            .await
    }
}

impl<N, EthB, PVB, EB, EVB, RpcMiddleware> RethRpcAddOns<N>
    for BscAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware>
where
    N: FullNodeComponents<
        Types: NodeTypes<
            ChainSpec: Hardforks + EthereumHardforks,
            Primitives = EthPrimitives,
            Payload: EngineTypes<ExecutionData = ExecutionData>,
        >,
        Evm: ConfigureEvm<NextBlockEnvCtx = NextBlockEnvAttributes>,
    >,
    EthB: EthApiBuilder<N>,
    PVB: PayloadValidatorBuilder<N>,
    EB: EngineApiBuilder<N>,
    EVB: EngineValidatorBuilder<N>,
    EthApiError: FromEvmError<N::Evm>,
    EvmFactoryFor<N::Evm>: EvmFactory<Tx = TxEnv>,
    RpcMiddleware: RethRpcMiddleware,
{
    type EthApi = EthB::EthApi;

    fn hooks_mut(&mut self) -> &mut reth_node_builder::rpc::RpcHooks<N, Self::EthApi> {
        self.inner.hooks_mut()
    }
}

impl<N, EthB, PVB, EB, EVB, RpcMiddleware> EngineValidatorAddOn<N>
    for BscAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware>
where
    N: FullNodeComponents<
        Types: NodeTypes<
            ChainSpec: EthChainSpec + EthereumHardforks,
            Primitives = EthPrimitives,
            Payload: EngineTypes<ExecutionData = ExecutionData>,
        >,
        Evm: ConfigureEvm<NextBlockEnvCtx = NextBlockEnvAttributes>,
    >,
    EthB: EthApiBuilder<N>,
    PVB: Send,
    EB: EngineApiBuilder<N>,
    EVB: EngineValidatorBuilder<N>,
    EthApiError: FromEvmError<N::Evm>,
    EvmFactoryFor<N::Evm>: EvmFactory<Tx = TxEnv>,
    RpcMiddleware: Send,
{
    type ValidatorBuilder = EVB;

    fn engine_validator_builder(&self) -> Self::ValidatorBuilder {
        self.inner.engine_validator_builder()
    }
}

impl<N> Node<N> for BscNode
where
    N: FullNodeTypes<Types = Self>,
{
    type ComponentsBuilder = ComponentsBuilder<
        N,
        BscPoolBuilder,
        BasicPayloadServiceBuilder<EthereumPayloadBuilder>,
        BscNetworkBuilder,
        BscExecutorBuilder,
        BscConsensusBuilder,
    >;

    type AddOns =
        BscAddOns<NodeAdapter<N>, EthereumEthApiBuilder, EthereumEngineValidatorBuilder>;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        Self::components()
    }

    fn add_ons(&self) -> Self::AddOns {
        BscAddOns::default()
    }
}

/// BSC EVM and executor builder.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct BscExecutorBuilder;

impl<Types, Node> ExecutorBuilder<Node> for BscExecutorBuilder
where
    Types: NodeTypes<
        ChainSpec: Hardforks + EthExecutorSpec + EthereumHardforks,
        Primitives = EthPrimitives,
    >,
    Node: FullNodeTypes<Types = Types>,
{
    type EVM = EthEvmConfig<Types::ChainSpec>;

    async fn build_evm(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::EVM> {
        Ok(EthEvmConfig::new(ctx.chain_spec()))
    }
}

/// BSC transaction pool builder.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct BscPoolBuilder;

impl<Types, Node, Evm> PoolBuilder<Node, Evm> for BscPoolBuilder
where
    Types: NodeTypes<
        ChainSpec: EthereumHardforks,
        Primitives: NodePrimitives<SignedTx = TransactionSigned>,
    >,
    Node: FullNodeTypes<Types = Types>,
    Evm: ConfigureEvm<Primitives = PrimitivesTy<Types>> + Clone + 'static,
{
    type Pool = EthTransactionPool<Node::Provider, DiskFileBlobStore, Evm>;

    async fn build_pool(
        self,
        ctx: &BuilderContext<Node>,
        evm_config: Evm,
    ) -> eyre::Result<Self::Pool> {
        let pool_config = ctx.pool_config();

        let blobs_disabled = ctx.config().txpool.disable_blobs_support ||
            ctx.config().txpool.blobpool_max_count == 0;

        let blob_cache_size = if let Some(blob_cache_size) = pool_config.blob_cache_size {
            Some(blob_cache_size)
        } else {
            let current_timestamp =
                SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?.as_secs();
            let blob_params = ctx
                .chain_spec()
                .blob_params_at_timestamp(current_timestamp)
                .unwrap_or_else(BlobParams::cancun);
            Some((blob_params.target_blob_count * EPOCH_SLOTS * 2) as u32)
        };

        let blob_store =
            reth_node_builder::components::create_blob_store_with_cache(ctx, blob_cache_size)?;

        let validator =
            TransactionValidationTaskExecutor::eth_builder(ctx.provider().clone(), evm_config)
                .set_eip4844(!blobs_disabled)
                .kzg_settings(ctx.kzg_settings()?)
                .with_max_tx_input_bytes(ctx.config().txpool.max_tx_input_bytes)
                .with_local_transactions_config(pool_config.local_transactions_config.clone())
                .set_tx_fee_cap(ctx.config().rpc.rpc_tx_fee_cap)
                .with_max_tx_gas_limit(ctx.config().txpool.max_tx_gas_limit)
                .with_minimum_priority_fee(ctx.config().txpool.minimum_priority_fee)
                .with_additional_tasks(ctx.config().txpool.additional_validation_tasks)
                .build_with_tasks(ctx.task_executor().clone(), blob_store.clone());

        if validator.validator().eip4844() {
            let kzg_settings = validator.validator().kzg_settings().clone();
            ctx.task_executor().spawn_blocking_task(async move {
                let _ = kzg_settings.get();
                debug!(target: "reth::cli", "Initialized KZG settings");
            });
        }

        let transaction_pool = TxPoolBuilder::new(ctx)
            .with_validator(validator)
            .build_and_spawn_maintenance_task(blob_store, pool_config)?;

        info!(target: "reth::cli", "Transaction pool initialized");
        debug!(target: "reth::cli", "Spawned txpool maintenance task");

        Ok(transaction_pool)
    }
}

/// BSC network builder.
#[derive(Debug, Default, Clone, Copy)]
pub struct BscNetworkBuilder;

impl<Node, Pool> NetworkBuilder<Node, Pool> for BscNetworkBuilder
where
    Node: FullNodeTypes<Types: NodeTypes<ChainSpec: Hardforks>>,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TxTy<Node::Types>>>
        + Unpin
        + 'static,
{
    type Network =
        NetworkHandle<BasicNetworkPrimitives<PrimitivesTy<Node::Types>, PoolPooledTx<Pool>>>;

    async fn build_network(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
    ) -> eyre::Result<Self::Network> {
        let network = ctx.network_builder().await?;
        let handle = ctx.start_network(network, pool);
        info!(target: "reth::cli", enode=%handle.local_node_record(), "P2P networking initialized");
        Ok(handle)
    }
}

/// BSC consensus builder.
///
/// Uses Ethereum beacon consensus in trust mode. With `--engine.skip-state-root`,
/// consensus validation is effectively bypassed for fast block processing.
#[derive(Debug, Default, Clone, Copy)]
pub struct BscConsensusBuilder;

impl<Node> ConsensusBuilder<Node> for BscConsensusBuilder
where
    Node: FullNodeTypes<
        Types: NodeTypes<ChainSpec: EthChainSpec + EthereumHardforks, Primitives = EthPrimitives>,
    >,
{
    type Consensus = Arc<EthBeaconConsensus<<Node::Types as NodeTypes>::ChainSpec>>;

    async fn build_consensus(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::Consensus> {
        Ok(Arc::new(EthBeaconConsensus::new(ctx.chain_spec())))
    }
}
