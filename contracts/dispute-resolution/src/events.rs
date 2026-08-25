//! Event definitions for the dispute-resolution contract. See the
//! campaign-escrow crate's `events.rs` for more detail on the
//! `#[contractevent]` pattern used here. `DisputeResolved` is published by
//! `close_dispute` (the escrow-side admin bypass closing out a raised
//! dispute); the arbiter-resolved `resolve_dispute` will publish it too once
//! that `todo!()` is implemented.
#![allow(dead_code)]

use ads_bazaar_shared::DisputeId;
use soroban_sdk::{contractevent, Address, BytesN};

#[contractevent]
#[derive(Clone, Debug)]
pub struct DisputeRaised {
    #[topic]
    pub dispute_id: DisputeId,
    #[topic]
    pub raised_by: Address,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct ArbiterAssigned {
    #[topic]
    pub dispute_id: DisputeId,
    #[topic]
    pub arbiter: Address,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct DisputeResolved {
    #[topic]
    pub dispute_id: DisputeId,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct ContractUpgraded {
    pub new_wasm_hash: BytesN<32>,
}
