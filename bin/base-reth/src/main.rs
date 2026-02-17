#![allow(missing_docs)]
//! Base (OP Stack) execution client built on reth.
//!
//! # Status: Blocked on op-reth compatibility
//!
//! As of 2026-02-17, op-rs/op-reth (stable branch) targets reth v1.10.2 with
//! incompatible dependency versions:
//! - alloy: 1.4.3 (op-reth) vs 1.6.3 (our reth v1.11.0)
//! - alloy-evm: 0.26.3 vs 0.27.2
//!
//! Once op-rs/op-reth releases a version compatible with reth v1.11, this binary
//! should use `OpNode` from `reth-optimism-node` with `BASE_MAINNET` chainspec
//! from `reth-optimism-chainspec`.
//!
//! ## Target implementation:
//!
//! ```ignore
//! use reth_optimism_cli::Cli;
//! use reth_optimism_node::OpNode;
//! use reth_optimism_chainspec::BASE_MAINNET;
//!
//! fn main() {
//!     Cli::parse_args()
//!         .with_default_chain_spec(BASE_MAINNET.clone())
//!         .run(|builder, _| async move {
//!             builder.node(OpNode::default()).launch().await
//!         });
//! }
//! ```

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

use clap::Parser;
use reth::cli::Cli;
use reth_ethereum_cli::chainspec::EthereumChainSpecParser;
use reth_node_ethereum::EthereumNode;
use tracing::info;

fn main() {
    reth_cli_util::sigsegv_handler::install();

    if std::env::var_os("RUST_BACKTRACE").is_none() {
        unsafe { std::env::set_var("RUST_BACKTRACE", "1") };
    }

    // TODO(base): Replace with OpNode + BASE_MAINNET chainspec when op-reth is compatible.
    // For now, this launches an Ethereum node as a placeholder.
    // To sync Base, you'll need to pass --chain <base-genesis.json> with a custom genesis.
    eprintln!(
        "WARNING: base-reth is a placeholder. OP Stack support requires op-rs/op-reth \
         compatible with reth v1.11. Currently using Ethereum node as fallback."
    );

    if let Err(err) = Cli::<EthereumChainSpecParser>::parse().run(async move |builder, _| {
        info!(target: "reth::cli", "Launching Base node (placeholder mode)");
        let handle = builder.node(EthereumNode::default()).launch().await?;

        handle.wait_for_node_exit().await
    }) {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
