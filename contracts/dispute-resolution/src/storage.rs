#![allow(dead_code)]

use ads_bazaar_shared::{CampaignId, DisputeId};
use soroban_sdk::{contracttype, Address, Env, String};

use crate::error::Error;
use crate::types::Dispute;

const PERSISTENT_BUMP_LEDGERS: u32 = 518_400;
const PERSISTENT_LIFETIME_THRESHOLD: u32 = 500_000;

/// Same ~30-day-at-5s/ledger bump as `PERSISTENT_BUMP_LEDGERS`, but for
/// instance storage (the `Admin`/`EscrowContract`/`Version`/`NextDisputeId`
/// config keys). Kept as a separate constant since instance and persistent
/// TTL are tracked independently by the ledger even when the numbers happen
/// to match. Mirrors `campaign-escrow`'s pair of the same name.
pub(crate) const INSTANCE_BUMP_LEDGERS: u32 = 518_400;
pub(crate) const INSTANCE_LIFETIME_THRESHOLD: u32 = 500_000;

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Admin,
    EscrowContract,
    Version,
    NextDisputeId,
    Dispute(DisputeId),
    /// The open dispute over a given campaign/creator payout, if any. Keeps
    /// `raise_dispute` from opening a second dispute over the same payout.
    OpenDispute(CampaignId, Address),
}

pub fn is_initialized(env: &Env) -> bool {
    env.storage().instance().has(&DataKey::Admin)
}

/// Bump the instance entry's TTL. Every public entry point in `lib.rs` calls
/// this, reads included: nothing else writes to instance storage after
/// `initialize`, so without a bump on the read paths the config keys would
/// run out their TTL and get archived, and `get_admin`/`get_escrow_contract`
/// would start failing with `Error::NotInitialized` until someone submits a
/// `RestoreFootprint` operation.
pub fn extend_instance_ttl(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_LEDGERS);
}

pub fn set_admin(env: &Env, admin: &Address) {
    env.storage().instance().set(&DataKey::Admin, admin);
}

pub fn get_admin(env: &Env) -> Result<Address, Error> {
    env.storage()
        .instance()
        .get(&DataKey::Admin)
        .ok_or(Error::NotInitialized)
}

pub fn set_escrow_contract(env: &Env, escrow_contract: &Address) {
    env.storage()
        .instance()
        .set(&DataKey::EscrowContract, escrow_contract);
}

pub fn get_escrow_contract(env: &Env) -> Result<Address, Error> {
    env.storage()
        .instance()
        .get(&DataKey::EscrowContract)
        .ok_or(Error::NotInitialized)
}

pub fn set_version(env: &Env, version: &String) {
    env.storage().instance().set(&DataKey::Version, version);
}

pub fn get_version(env: &Env) -> Result<String, Error> {
    env.storage()
        .instance()
        .get(&DataKey::Version)
        .ok_or(Error::NotInitialized)
}

pub fn next_dispute_id(env: &Env) -> DisputeId {
    let id: DisputeId = env
        .storage()
        .instance()
        .get(&DataKey::NextDisputeId)
        .unwrap_or(0);
    env.storage()
        .instance()
        .set(&DataKey::NextDisputeId, &(id + 1));
    id
}

pub fn get_dispute(env: &Env, id: DisputeId) -> Result<Dispute, Error> {
    env.storage()
        .persistent()
        .get(&DataKey::Dispute(id))
        .ok_or(Error::DisputeNotFound)
}

pub fn set_dispute(env: &Env, id: DisputeId, dispute: &Dispute) {
    let key = DataKey::Dispute(id);
    env.storage().persistent().set(&key, dispute);
    env.storage().persistent().extend_ttl(
        &key,
        PERSISTENT_LIFETIME_THRESHOLD,
        PERSISTENT_BUMP_LEDGERS,
    );
}

pub fn get_open_dispute(
    env: &Env,
    campaign_id: CampaignId,
    creator: &Address,
) -> Option<DisputeId> {
    env.storage()
        .persistent()
        .get(&DataKey::OpenDispute(campaign_id, creator.clone()))
}

pub fn set_open_dispute(env: &Env, campaign_id: CampaignId, creator: &Address, id: DisputeId) {
    let key = DataKey::OpenDispute(campaign_id, creator.clone());
    env.storage().persistent().set(&key, &id);
    env.storage().persistent().extend_ttl(
        &key,
        PERSISTENT_LIFETIME_THRESHOLD,
        PERSISTENT_BUMP_LEDGERS,
    );
}

/// Clear the open-dispute marker for a payout so a fresh dispute can be
/// raised over it later. Called by `close_dispute`, which the
/// `campaign-escrow` admin bypass invokes after settling a payout.
pub fn clear_open_dispute(env: &Env, campaign_id: CampaignId, creator: &Address) {
    env.storage()
        .persistent()
        .remove(&DataKey::OpenDispute(campaign_id, creator.clone()));
}
