//! Event definitions for the campaign-escrow contract, using the
//! `#[contractevent]` macro so events are part of the contract's on-chain
//! interface spec (discoverable by indexers/SDKs), not just ad-hoc
//! `env.events().publish(...)` calls.
//!
//! None of these are published yet — wire up `.publish(&env)` calls at the
//! matching point in `lib.rs` as each `todo!()` handler is implemented.
#![allow(dead_code)]

use ads_bazaar_shared::{CampaignId, DisputeOutcome};
use soroban_sdk::{contractevent, Address, BytesN, String};

#[contractevent]
#[derive(Clone, Debug)]
pub struct CampaignCreated {
    #[topic]
    pub business: Address,
    pub campaign_id: CampaignId,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct CampaignFunded {
    #[topic]
    pub campaign_id: CampaignId,
    pub amount: i128,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct CreatorApplied {
    #[topic]
    pub campaign_id: CampaignId,
    #[topic]
    pub creator: Address,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct CreatorApproved {
    #[topic]
    pub campaign_id: CampaignId,
    #[topic]
    pub creator: Address,
    pub payout_amount: i128,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct ProofSubmitted {
    #[topic]
    pub campaign_id: CampaignId,
    #[topic]
    pub creator: Address,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct SubmissionRejected {
    #[topic]
    pub campaign_id: CampaignId,
    #[topic]
    pub creator: Address,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct PaymentReleased {
    #[topic]
    pub campaign_id: CampaignId,
    #[topic]
    pub creator: Address,
    pub amount: i128,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct CampaignCancelled {
    #[topic]
    pub campaign_id: CampaignId,
    pub refunded_amount: i128,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct SurplusReclaimed {
    #[topic]
    pub campaign_id: CampaignId,
    pub amount: i128,
}

/// Emitted by `pause`. Already wired up (unlike most events above, which
/// are still waiting on their corresponding `todo!()` handlers).
#[contractevent]
#[derive(Clone, Debug)]
pub struct ContractPaused {
    #[topic]
    pub admin: Address,
}

/// Emitted by `unpause`.
#[contractevent]
#[derive(Clone, Debug)]
pub struct ContractUnpaused {
    #[topic]
    pub admin: Address,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct AdminProposed {
    #[topic]
    pub current_admin: Address,
    #[topic]
    pub new_admin: Address,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct AdminTransferred {
    #[topic]
    pub previous_admin: Address,
    #[topic]
    pub new_admin: Address,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct ContractUpgraded {
    pub new_wasm_hash: BytesN<32>,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct FeeUpdated {
    #[topic]
    pub admin: Address,
    pub new_fee_bps: i128,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct TreasuryUpdated {
    #[topic]
    pub admin: Address,
    pub new_treasury: Address,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct CampaignMetadataUpdated {
    #[topic]
    pub campaign_id: CampaignId,
    pub business: Address,
    pub new_metadata: String,
}

/// Emitted by `freeze_for_dispute` when a creator's payout is locked for
/// arbitration, whether triggered by the `dispute-resolution` contract or
/// directly by `admin`.
#[contractevent]
#[derive(Clone, Debug)]
pub struct DisputeFrozen {
    #[topic]
    pub campaign_id: CampaignId,
    #[topic]
    pub creator: Address,
}

#[contractevent]
#[derive(Clone, Debug)]
pub struct DisputeResolved {
    #[topic]
    pub campaign_id: CampaignId,
    #[topic]
    pub creator: Address,
    pub dispute_outcome: DisputeOutcome,
    pub creator_amount: i128,
    pub business_amount: i128,
}

/// Emitted by `emergency_recover_campaign`. Kept distinct from
/// `CampaignCancelled` on purpose — this path bypasses the business's
/// consent entirely, so it must be trivially greppable/auditable on its own,
/// not blend in with routine cancellations.
#[contractevent]
#[derive(Clone, Debug)]
pub struct EmergencyRecovery {
    #[topic]
    pub campaign_id: CampaignId,
    pub amount: i128,
}
