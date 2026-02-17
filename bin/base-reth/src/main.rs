#![allow(missing_docs)]

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

use clap::Parser;
use reth::cli::Cli;
use reth_cli::chainspec::{parse_genesis, ChainSpecParser};
use reth_optimism_chainspec::{
    OpChainSpec, BASE_MAINNET, BASE_SEPOLIA, OP_DEV, OP_MAINNET, OP_SEPOLIA,
};
use reth_optimism_consensus::OpBeaconConsensus;
use reth_optimism_evm::OpEvmConfig;
use reth_optimism_node::OpNode;
use std::sync::Arc;
use tracing::info;

/// Supported chains for base-reth.
const OP_SUPPORTED_CHAINS: &[&str] =
    &["base", "base-sepolia", "optimism", "op-sepolia", "op-dev"];

/// OP Stack chain specification parser.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct OpChainSpecParser;

impl ChainSpecParser for OpChainSpecParser {
    type ChainSpec = OpChainSpec;

    const SUPPORTED_CHAINS: &'static [&'static str] = OP_SUPPORTED_CHAINS;

    fn parse(s: &str) -> eyre::Result<Arc<OpChainSpec>> {
        Ok(match s {
            "base" => BASE_MAINNET.clone(),
            "base-sepolia" => BASE_SEPOLIA.clone(),
            "optimism" => OP_MAINNET.clone(),
            "op-sepolia" => OP_SEPOLIA.clone(),
            "op-dev" | "dev" => OP_DEV.clone(),
            _ => Arc::new(parse_genesis(s)?.into()),
        })
    }
}

fn main() {
    reth_cli_util::sigsegv_handler::install();

    if std::env::var_os("RUST_BACKTRACE").is_none() {
        unsafe { std::env::set_var("RUST_BACKTRACE", "1") };
    }

    if let Err(err) = Cli::<OpChainSpecParser>::parse().run_with_components::<OpNode>(
        |chain_spec: Arc<OpChainSpec>| {
            let evm = OpEvmConfig::optimism(chain_spec.clone());
            let consensus = Arc::new(OpBeaconConsensus::new(chain_spec));
            (evm, consensus)
        },
        async move |builder, _| {
            info!(target: "reth::cli", "Launching Base (OP Stack) node");
            let handle = builder.node(OpNode::default()).launch().await?;
            handle.wait_for_node_exit().await
        },
    ) {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
