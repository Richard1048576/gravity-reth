//! BSC node builder for reth.
//!
//! This crate provides a BSC-specific node configuration using reth's
//! node builder pattern. It reuses Ethereum execution components since
//! BSC is EVM-compatible, with BSC-specific chain specification and hardforks.

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

use reth_revm as _;
use revm as _;

pub use reth_ethereum_engine_primitives::EthEngineTypes;

pub mod chainspec;
pub use chainspec::{bsc_chain_spec, boot_nodes, BscChainSpecParser, BscHardfork};

pub mod node;
pub use node::BscNode;
