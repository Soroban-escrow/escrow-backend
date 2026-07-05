#![forbid(unsafe_code)]
//! Everything that talks to Stellar: Soroban RPC, XDR, transaction building and signing, and
//! decoding of the escrow contract's events.
//!
//! The chain is the source of truth. This crate only *reads* it and *prepares or relays*
//! transactions; the one key it can hold (the relayer's) can do exactly one thing, `release`,
//! and only for bounties whose maintainer opted in.

pub mod client;
pub mod error;
pub mod events;
pub mod keys;
pub mod ports;
pub mod relayer;
pub mod rpc;
pub mod scval;
pub mod tx;
pub mod types;

pub use client::{ChainSettings, SorobanChain};
pub use error::ChainError;
pub use events::{ChainEvent, RawEvent, decode_event};
pub use keys::Signer;
pub use ports::{ChainReader, EventSource, ReleaseSigner, TxBuilder};
pub use relayer::Relayer;
pub use types::{ChainBounty, ContractConfig, EventsPage, FundParams, TxOutcome};
