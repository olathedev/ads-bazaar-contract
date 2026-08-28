#![allow(dead_code)]

use ads_bazaar_shared::CampaignId;
use soroban_sdk::{contracttype, Address, Env, String};

use crate::error::Error;
use crate::types::{Application, Campaign};

/// Extend persistent entries by roughly this many ledgers on every write
/// (~30 days at 5s/ledger). TODO(contributors): tune once real rent/TTL
/// costs on target networks are benchmarked, and consider a max-TTL bump on
/// read-heavy paths too.
const PERSISTENT_BUMP_LEDGERS: u32 = 518_400;
const PERSISTENT_LIFETIME_THRESHOLD: u32 = 500_000;

/// Same ~30-day-at-5s/ledger bump as `PERSISTENT_BUMP_LEDGERS`, but for
/// instance storage (admin/treasury/fee_bps/dispute_contract config keys).
/// Kept as a separate constant since instance and persistent TTL are
/// tracked independently by the ledger even when the numbers happen to match.
const INSTANCE_BUMP_LEDGERS: u32 = 518_400;
const INSTANCE_LIFETIME_THRESHOLD: u32 = 500_000;

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Admin,
    PendingAdmin,
    Treasury,
    FeeBps,
    DisputeContract,
    Version,
    NextCampaignId,
    Campaign(CampaignId),
    Application(CampaignId, Address),
    /// Whether the contract is currently paused. See `require_not_paused`
    /// and `pause`/`unpause` in `lib.rs`.
    Paused,
    /// Count of creators that have applied to a campaign. Used by
    /// `update_campaign_metadata` to enforce that the brief is locked once
    /// any creator has applied, and as the upper bound when paging through
    /// `CampaignApplicant`.
    ApplicantCount(CampaignId),
    /// The applicant at a given zero-based ordinal within a campaign, in the
    /// order they applied. One fixed-size entry per applicant rather than a
    /// single growing `Vec<Address>`: that keeps the write cost of
    /// `apply_to_campaign` constant no matter how many creators applied
    /// before, which is what #43 was about. Read back in pages by
    /// `campaign_applicants`.
    CampaignApplicant(CampaignId, u32),
}

pub fn is_initialized(env: &Env) -> bool {
    env.storage().instance().has(&DataKey::Admin)
}

/// Bump the instance entry's TTL. Call this from any read-heavy path that
/// touches instance storage (config reads, not just writes) so the config
/// doesn't expire from lack of writes alone.
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

pub fn set_pending_admin(env: &Env, pending_admin: &Address) {
    env.storage()
        .instance()
        .set(&DataKey::PendingAdmin, pending_admin);
}

pub fn get_pending_admin(env: &Env) -> Option<Address> {
    env.storage().instance().get(&DataKey::PendingAdmin)
}

pub fn clear_pending_admin(env: &Env) {
    env.storage().instance().remove(&DataKey::PendingAdmin);
}

pub fn set_treasury(env: &Env, treasury: &Address) {
    env.storage().instance().set(&DataKey::Treasury, treasury);
}

pub fn get_treasury(env: &Env) -> Result<Address, Error> {
    env.storage()
        .instance()
        .get(&DataKey::Treasury)
        .ok_or(Error::NotInitialized)
}

pub fn set_fee_bps(env: &Env, fee_bps: i128) {
    env.storage().instance().set(&DataKey::FeeBps, &fee_bps);
}

pub fn get_fee_bps(env: &Env) -> Result<i128, Error> {
    env.storage()
        .instance()
        .get(&DataKey::FeeBps)
        .ok_or(Error::NotInitialized)
}

pub fn set_dispute_contract(env: &Env, dispute_contract: &Address) {
    env.storage()
        .instance()
        .set(&DataKey::DisputeContract, dispute_contract);
}

pub fn get_dispute_contract(env: &Env) -> Result<Address, Error> {
    env.storage()
        .instance()
        .get(&DataKey::DisputeContract)
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

pub fn next_campaign_id(env: &Env) -> CampaignId {
    let id: CampaignId = env
        .storage()
        .instance()
        .get(&DataKey::NextCampaignId)
        .unwrap_or(0);
    env.storage()
        .instance()
        .set(&DataKey::NextCampaignId, &(id + 1));
    id
}

pub fn get_campaign(env: &Env, id: CampaignId) -> Result<Campaign, Error> {
    env.storage()
        .persistent()
        .get(&DataKey::Campaign(id))
        .ok_or(Error::CampaignNotFound)
}

pub fn set_campaign(env: &Env, campaign: &Campaign) {
    let key = DataKey::Campaign(campaign.id);
    env.storage().persistent().set(&key, campaign);
    env.storage().persistent().extend_ttl(
        &key,
        PERSISTENT_LIFETIME_THRESHOLD,
        PERSISTENT_BUMP_LEDGERS,
    );
}

pub fn get_application(
    env: &Env,
    campaign_id: CampaignId,
    creator: &Address,
) -> Result<Application, Error> {
    env.storage()
        .persistent()
        .get(&DataKey::Application(campaign_id, creator.clone()))
        .ok_or(Error::ApplicationNotFound)
}

pub fn set_application(env: &Env, application: &Application) {
    let key = DataKey::Application(application.campaign_id, application.creator.clone());
    env.storage().persistent().set(&key, application);
    env.storage().persistent().extend_ttl(
        &key,
        PERSISTENT_LIFETIME_THRESHOLD,
        PERSISTENT_BUMP_LEDGERS,
    );
}

/// Read the current pause state. Defaults to `false` (unpaused) if never
/// explicitly set, which is also the correct behavior pre-`initialize`.
pub fn get_paused(env: &Env) -> bool {
    env.storage()
        .instance()
        .get(&DataKey::Paused)
        .unwrap_or(false)
}

pub fn set_paused(env: &Env, paused: bool) {
    env.storage().instance().set(&DataKey::Paused, &paused);
}

/// Record `creator` as the next applicant on `campaign_id`: append them at
/// the current count's ordinal, then increment the count.
///
/// Both writes are fixed-size — one `Address` under a fresh
/// `CampaignApplicant` key, one `u32` under `ApplicantCount` — so the cost
/// of applying does not grow with the number of prior applicants. That
/// property is the point of #43, and
/// `applying_with_many_prior_applicants_does_not_regress_write_cost` asserts
/// it exactly.
pub fn add_campaign_applicant(env: &Env, campaign_id: CampaignId, creator: &Address) {
    let count_key = DataKey::ApplicantCount(campaign_id);
    let count: u32 = env.storage().persistent().get(&count_key).unwrap_or(0);

    let applicant_key = DataKey::CampaignApplicant(campaign_id, count);
    env.storage().persistent().set(&applicant_key, creator);
    env.storage().persistent().extend_ttl(
        &applicant_key,
        PERSISTENT_LIFETIME_THRESHOLD,
        PERSISTENT_BUMP_LEDGERS,
    );

    env.storage().persistent().set(&count_key, &(count + 1));
    env.storage().persistent().extend_ttl(
        &count_key,
        PERSISTENT_LIFETIME_THRESHOLD,
        PERSISTENT_BUMP_LEDGERS,
    );
}

/// Number of creators that have applied to `campaign_id`. Zero if the
/// campaign has no applicants (or does not exist — callers that care about
/// the difference check the campaign first).
pub fn get_applicant_count(env: &Env, campaign_id: CampaignId) -> u32 {
    env.storage()
        .persistent()
        .get(&DataKey::ApplicantCount(campaign_id))
        .unwrap_or(0)
}

/// The applicant at `index`, bumping its TTL so that paging through an
/// index keeps it alive — the entries are written once at apply time and
/// never rewritten, so reads are the only thing that can refresh them.
pub fn get_campaign_applicant(env: &Env, campaign_id: CampaignId, index: u32) -> Option<Address> {
    let key = DataKey::CampaignApplicant(campaign_id, index);
    let applicant: Option<Address> = env.storage().persistent().get(&key);
    if applicant.is_some() {
        env.storage().persistent().extend_ttl(
            &key,
            PERSISTENT_LIFETIME_THRESHOLD,
            PERSISTENT_BUMP_LEDGERS,
        );
    }
    applicant
}

/// Return whether any creator has applied to `campaign_id`.
pub fn has_campaign_applicants(env: &Env, campaign_id: CampaignId) -> bool {
    let count: Option<u32> = env
        .storage()
        .persistent()
        .get(&DataKey::ApplicantCount(campaign_id));
    count.is_some_and(|c| c > 0)
}
