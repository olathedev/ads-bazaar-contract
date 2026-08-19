//! # ads-bazaar-campaign-escrow
//!
//! Holds business-funded campaign budgets in escrow and releases them to
//! approved creators. This crate implements the full escrow lifecycle:
//! campaign creation, funding, creator applications, selection, proof
//! submission/review, payout release (with platform fee), cancellation,
//! expiry and surplus reclaim.
//!
//! Money movement goes through the standard SEP-41 token `Client`
//! (`soroban_sdk::token::Client`) against `Campaign::asset.token`, which is
//! how a single contract deployment supports XLM, Naira-pegged stablecoins,
//! USDC, etc. without per-asset special-casing.
#![no_std]

mod error;
mod events;
mod storage;
mod types;

pub use error::Error;
pub use types::{Application, Campaign, DisputeResolution, ProtocolConfig};

use ads_bazaar_shared::{ApplicationStatus, CampaignId, CampaignStatus, PayoutAsset};
use soroban_sdk::{contract, contractimpl, token, Address, BytesN, Env, String};

/// Version string stored at `initialize` time. `upgrade` swaps the WASM
/// binary but does not bump this on its own — see the TODO on `upgrade`
/// below.
const INITIAL_VERSION: &str = "0.1.0";

/// Extra grace period (on top of `completion_deadline` already having
/// passed) required before `emergency_recover_campaign` becomes callable.
/// Deliberately months-scale — far longer than the days/weeks relevant to
/// normal campaign operation — so this path can only ever apply to a
/// campaign that has been abandoned, never as a shortcut around
/// `expire_campaign`.
const EMERGENCY_RECOVERY_GRACE_PERIOD: u64 = 180 * 24 * 60 * 60; // ~6 months

/// Minimum time that must elapse between a dispute being opened over an
/// application (`freeze_for_dispute`, which emits `events::DisputeFrozen`)
/// and `admin` being able to settle it via `resolve_dispute`.
///
/// `resolve_dispute` moves a creator's committed payout in any direction the
/// admin chooses, so without a delay a compromised admin key could reallocate
/// a disputed payout in the same block the dispute is raised — before the
/// other party could submit counter-evidence, or even notice. This window is
/// the guarantee that they get a chance to.
///
/// 72 hours is chosen to span a full weekend in any timezone: the shortest
/// period in which a business or creator who is simply not at their desk can
/// still see the `DisputeFrozen` event and respond. Longer would strand
/// legitimately contested funds (campaigns operate on a days-to-weeks scale —
/// contrast `EMERGENCY_RECOVERY_GRACE_PERIOD`, which is months-scale because
/// it only ever applies to abandoned campaigns); shorter would let a dispute
/// opened Friday evening be resolved unilaterally before Monday.
///
/// Public so integrators and the frontend can show a countdown to when a
/// dispute becomes resolvable, rather than hardcoding a duplicate constant.
pub const MIN_EVIDENCE_WINDOW: u64 = 72 * 60 * 60; // 72 hours

/// Require that `admin` matches the address stored at `initialize` time.
/// Returns `Error::Unauthorized` for any other caller. Used by `pause` and
/// `unpause`.
fn require_admin(env: &Env, admin: &Address) -> Result<(), Error> {
    admin.require_auth();
    let stored_admin = storage::get_admin(env)?;
    if stored_admin != *admin {
        return Err(Error::Unauthorized);
    }
    Ok(())
}

/// Guard called at the top of every state-changing function. Returns
/// `Error::ContractPaused` if the contract is currently paused via `pause`.
/// Read-only functions intentionally do not call this, so users can still
/// read their data while the contract is paused.
fn require_not_paused(env: &Env) -> Result<(), Error> {
    if storage::get_paused(env) {
        return Err(Error::ContractPaused);
    }
    Ok(())
}

/// Reject any change to an application whose payout is frozen for dispute
/// arbitration. This covers more than payout: the proof state is the evidence
/// the arbiter is reviewing, so `reject_submission` clearing `proof_uri`
/// mid-dispute would destroy it.
fn require_not_frozen(application: &Application) -> Result<(), Error> {
    if application.frozen {
        return Err(Error::PayoutFrozen);
    }
    Ok(())
}

#[contract]
pub struct CampaignEscrowContract;

#[contractimpl]
impl CampaignEscrowContract {
    /// One-time setup. Must be called before any other function.
    ///
    /// `dispute_contract` is the only address permitted to call
    /// `freeze_for_dispute` / `resolve_dispute_payout` once those are
    /// implemented — it should be the deployed `dispute-resolution`
    /// contract's address.
    pub fn initialize(
        env: Env,
        admin: Address,
        dispute_contract: Address,
        fee_bps: i128,
    ) -> Result<(), Error> {
        if storage::is_initialized(&env) {
            return Err(Error::AlreadyInitialized);
        }
        if !(0..=ads_bazaar_shared::BASIS_POINTS_DENOMINATOR).contains(&fee_bps) {
            return Err(Error::FeeTooHigh);
        }
        admin.require_auth();

        storage::set_admin(&env, &admin);
        // No separate fee-collection destination exists yet (see the TODO
        // on `release_payment` below) — treasury defaults to admin until a
        // future issue adds a dedicated setter.
        storage::set_treasury(&env, &admin);
        storage::set_dispute_contract(&env, &dispute_contract);
        storage::set_fee_bps(&env, fee_bps);
        storage::set_version(&env, &String::from_str(&env, INITIAL_VERSION));
        Ok(())
    }

    /// Freeze all state-changing operations. Callable only by the admin set
    /// at `initialize`. Emits `events::ContractPaused`. View functions are
    /// unaffected and remain readable.
    pub fn pause(env: Env, admin: Address) -> Result<(), Error> {
        require_admin(&env, &admin)?;
        storage::set_paused(&env, true);
        events::ContractPaused { admin }.publish(&env);
        Ok(())
    }

    /// Resume state-changing operations after a `pause`. Callable only by
    /// the admin set at `initialize`. Emits `events::ContractUnpaused`.
    pub fn unpause(env: Env, admin: Address) -> Result<(), Error> {
        require_admin(&env, &admin)?;
        storage::set_paused(&env, false);
        events::ContractUnpaused { admin }.publish(&env);
        Ok(())
    }

    /// Read-only: current pause state. Accessible regardless of whether the
    /// contract is paused.
    pub fn is_paused(env: Env) -> bool {
        storage::get_paused(&env)
    }

    /// Propose a new admin address. The transfer is not finalized until the
    /// proposed address calls `accept_admin`, proving control of that key.
    pub fn propose_admin(
        env: Env,
        current_admin: Address,
        new_admin: Address,
    ) -> Result<(), Error> {
        require_admin(&env, &current_admin)?;
        storage::set_pending_admin(&env, &new_admin);
        storage::extend_instance_ttl(&env);
        events::AdminProposed {
            current_admin,
            new_admin,
        }
        .publish(&env);
        Ok(())
    }

    /// Accept a pending admin transfer. Only the exact proposed address may
    /// finalize the handover.
    pub fn accept_admin(env: Env, new_admin: Address) -> Result<(), Error> {
        new_admin.require_auth();
        let pending_admin = storage::get_pending_admin(&env).ok_or(Error::Unauthorized)?;
        if pending_admin != new_admin {
            return Err(Error::Unauthorized);
        }

        let previous_admin = storage::get_admin(&env)?;
        storage::set_admin(&env, &new_admin);
        storage::clear_pending_admin(&env);
        storage::extend_instance_ttl(&env);
        events::AdminTransferred {
            previous_admin,
            new_admin,
        }
        .publish(&env);
        Ok(())
    }

    /// Update the platform fee for future `claim_payment` calls.
    /// The fee is read at claim time, so a fee change affects pending campaigns.
    /// Callable only by the admin.
    ///
    /// Capped at 1,000 bps (10%), deliberately tighter than the 0..=10,000
    /// range `initialize` allows — a sane ceiling for adjusting an already-
    /// live fee, even though the wider range remains available at deploy time.
    pub fn update_fee_bps(env: Env, admin: Address, new_fee_bps: i128) -> Result<(), Error> {
        require_admin(&env, &admin)?;
        if !(0..=1_000).contains(&new_fee_bps) {
            return Err(Error::FeeTooHigh);
        }
        storage::set_fee_bps(&env, new_fee_bps);
        events::FeeUpdated { admin, new_fee_bps }.publish(&env);
        Ok(())
    }

    /// Update the treasury address where platform fees are sent.
    /// Callable only by the admin.
    pub fn update_treasury(env: Env, admin: Address, new_treasury: Address) -> Result<(), Error> {
        require_admin(&env, &admin)?;
        storage::set_treasury(&env, &new_treasury);
        events::TreasuryUpdated {
            admin,
            new_treasury,
        }
        .publish(&env);
        Ok(())
    }

    /// Create a new draft campaign owned by `business`. Not yet escrowed —
    /// call `fund_campaign` afterwards to deposit `total_budget`.
    ///
    /// Validates `total_budget > 0`, `max_creators > 0`, that both deadlines
    /// are in the future and that `application_deadline < completion_deadline`.
    #[allow(clippy::too_many_arguments)]
    pub fn create_campaign(
        env: Env,
        business: Address,
        asset: PayoutAsset,
        total_budget: i128,
        max_creators: u32,
        application_deadline: u64,
        completion_deadline: u64,
        metadata_uri: String,
    ) -> Result<CampaignId, Error> {
        require_not_paused(&env)?;
        if !storage::is_initialized(&env) {
            return Err(Error::NotInitialized);
        }
        if total_budget <= 0 {
            return Err(Error::InvalidAmount);
        }
        if max_creators == 0 {
            return Err(Error::InvalidCreatorCount);
        }
        let now = env.ledger().timestamp();
        if application_deadline <= now || completion_deadline <= now {
            return Err(Error::DeadlineInPast);
        }
        if application_deadline >= completion_deadline {
            return Err(Error::InvalidDeadlineOrder);
        }

        business.require_auth();

        let id = storage::next_campaign_id(&env);
        let campaign = Campaign {
            id,
            business: business.clone(),
            asset,
            total_budget,
            escrow_balance: 0,
            committed_payouts: 0,
            // Snapshotted at creation so a later admin fee change (see
            // `update_fee_bps`) doesn't retroactively affect this campaign.
            fee_bps: storage::get_fee_bps(&env)?,
            max_creators,
            approved_count: 0,
            application_deadline,
            completion_deadline,
            metadata_uri,
            status: CampaignStatus::Draft,
        };
        storage::set_campaign(&env, &campaign);
        events::CampaignCreated {
            business,
            campaign_id: id,
        }
        .publish(&env);
        Ok(id)
    }

    /// Transfer `campaign.total_budget` of `campaign.asset.token` from
    /// `business` into this contract, moving the campaign from `Draft` to
    /// `Funded`. Funding is strictly all-at-once.
    pub fn fund_campaign(
        env: Env,
        business: Address,
        campaign_id: CampaignId,
    ) -> Result<(), Error> {
        require_not_paused(&env)?;
        business.require_auth();
        let mut campaign = storage::get_campaign(&env, campaign_id)?;
        if campaign.status != CampaignStatus::Draft {
            return Err(Error::InvalidStatus);
        }
        if campaign.business != business {
            return Err(Error::NotCampaignOwner);
        }

        let token = token::Client::new(&env, &campaign.asset.token);
        token.transfer(
            &business,
            env.current_contract_address(),
            &campaign.total_budget,
        );
        campaign.escrow_balance = campaign.total_budget;
        campaign.status = CampaignStatus::Funded;
        storage::set_campaign(&env, &campaign);
        events::CampaignFunded {
            campaign_id,
            amount: campaign.total_budget,
        }
        .publish(&env);
        Ok(())
    }

    /// Creator applies to a funded (`Funded`) campaign before its application
    /// deadline. A creator may apply only once per campaign.
    pub fn apply_to_campaign(
        env: Env,
        creator: Address,
        campaign_id: CampaignId,
        pitch_uri: String,
    ) -> Result<(), Error> {
        require_not_paused(&env)?;
        creator.require_auth();
        let campaign = storage::get_campaign(&env, campaign_id)?;
        if campaign.status != CampaignStatus::Funded && campaign.status != CampaignStatus::Active {
            return Err(Error::InvalidStatus);
        }
        if env.ledger().timestamp() > campaign.application_deadline {
            return Err(Error::ApplicationDeadlinePassed);
        }
        if storage::get_application(&env, campaign_id, &creator).is_ok() {
            return Err(Error::AlreadyApplied);
        }

        let application = Application {
            campaign_id,
            creator: creator.clone(),
            pitch_uri,
            proof_uri: None,
            payout_amount: 0,
            proof_approved: false,
            frozen: false,
            dispute_opened_at: None,
            status: ApplicationStatus::Pending,
        };
        storage::set_application(&env, &application);
        storage::add_campaign_applicant(&env, campaign_id, &creator);
        events::CreatorApplied {
            campaign_id,
            creator,
        }
        .publish(&env);
        Ok(())
    }

    /// Business approves a pending application, selecting the creator and
    /// setting their agreed `payout_amount`. Guards against selecting more
    /// than `max_creators`, double-selection, and over-committing escrow.
    pub fn approve_creator(
        env: Env,
        business: Address,
        campaign_id: CampaignId,
        creator: Address,
        payout_amount: i128,
    ) -> Result<(), Error> {
        require_not_paused(&env)?;
        business.require_auth();
        let mut campaign = storage::get_campaign(&env, campaign_id)?;
        if campaign.business != business {
            return Err(Error::NotCampaignOwner);
        }
        if campaign.status != CampaignStatus::Funded && campaign.status != CampaignStatus::Active {
            return Err(Error::InvalidStatus);
        }
        if payout_amount <= 0 {
            return Err(Error::InvalidAmount);
        }

        let mut application = storage::get_application(&env, campaign_id, &creator)?;
        if application.status != ApplicationStatus::Pending {
            return Err(Error::AlreadySelected);
        }

        if campaign.approved_count >= campaign.max_creators {
            return Err(Error::MaxCreatorsReached);
        }
        let new_committed = campaign
            .committed_payouts
            .checked_add(payout_amount)
            .ok_or(Error::InvalidAmount)?;
        if new_committed > campaign.escrow_balance {
            return Err(Error::InsufficientEscrowBalance);
        }

        application.payout_amount = payout_amount;
        application.status = ApplicationStatus::Approved;
        storage::set_application(&env, &application);

        campaign.approved_count += 1;
        campaign.committed_payouts = new_committed;
        if campaign.status == CampaignStatus::Funded {
            campaign.status = CampaignStatus::Active;
        }
        storage::set_campaign(&env, &campaign);
        events::CreatorApproved {
            campaign_id,
            creator,
            payout_amount,
        }
        .publish(&env);
        Ok(())
    }

    /// Approved creator submits proof of completed work. May only be called
    /// before the content deadline.
    pub fn submit_proof(
        env: Env,
        creator: Address,
        campaign_id: CampaignId,
        proof_uri: String,
    ) -> Result<(), Error> {
        require_not_paused(&env)?;
        creator.require_auth();
        let campaign = storage::get_campaign(&env, campaign_id)?;
        if env.ledger().timestamp() > campaign.completion_deadline {
            return Err(Error::ContentDeadlinePassed);
        }

        let mut application = storage::get_application(&env, campaign_id, &creator)?;
        require_not_frozen(&application)?;
        if application.status != ApplicationStatus::Approved
            && application.status != ApplicationStatus::Rejected
        {
            return Err(Error::InvalidStatus);
        }

        application.proof_uri = Some(proof_uri);
        application.status = ApplicationStatus::ProofSubmitted;
        application.proof_approved = false;
        storage::set_application(&env, &application);
        events::ProofSubmitted {
            campaign_id,
            creator,
        }
        .publish(&env);
        Ok(())
    }

    /// Business accepts a submitted proof, marking the submission payable.
    pub fn approve_submission(
        env: Env,
        business: Address,
        campaign_id: CampaignId,
        creator: Address,
    ) -> Result<(), Error> {
        require_not_paused(&env)?;
        business.require_auth();
        let campaign = storage::get_campaign(&env, campaign_id)?;
        if campaign.business != business {
            return Err(Error::NotCampaignOwner);
        }

        let mut application = storage::get_application(&env, campaign_id, &creator)?;
        require_not_frozen(&application)?;
        if application.status != ApplicationStatus::ProofSubmitted {
            return Err(Error::InvalidStatus);
        }
        application.proof_approved = true;
        storage::set_application(&env, &application);
        Ok(())
    }

    /// Business rejects a submitted proof, returning the creator to the
    /// selected state so they may re-submit proof.
    pub fn reject_submission(
        env: Env,
        business: Address,
        campaign_id: CampaignId,
        creator: Address,
    ) -> Result<(), Error> {
        require_not_paused(&env)?;
        business.require_auth();
        let campaign = storage::get_campaign(&env, campaign_id)?;
        if campaign.business != business {
            return Err(Error::NotCampaignOwner);
        }

        let mut application = storage::get_application(&env, campaign_id, &creator)?;
        require_not_frozen(&application)?;
        if application.status != ApplicationStatus::ProofSubmitted {
            return Err(Error::InvalidStatus);
        }
        application.proof_uri = None;
        application.proof_approved = false;
        application.status = ApplicationStatus::Rejected;
        storage::set_application(&env, &application);
        events::SubmissionRejected {
            campaign_id,
            creator,
        }
        .publish(&env);
        Ok(())
    }

    /// Release an approved creator's escrowed payout, deducting the platform
    /// fee configured at `initialize`. Callable by the creator once their
    /// submission is approved, or automatically once the content deadline has
    /// passed (auto-approval).
    pub fn claim_payment(env: Env, creator: Address, campaign_id: CampaignId) -> Result<(), Error> {
        require_not_paused(&env)?;
        creator.require_auth();
        let mut campaign = storage::get_campaign(&env, campaign_id)?;

        let mut application = storage::get_application(&env, campaign_id, &creator)?;
        require_not_frozen(&application)?;
        if application.status != ApplicationStatus::ProofSubmitted {
            return Err(Error::SubmissionNotPayable);
        }

        let auto_approved = env.ledger().timestamp() > campaign.completion_deadline;
        if !application.proof_approved && !auto_approved {
            return Err(Error::SubmissionNotPayable);
        }

        // Use the fee snapshotted at campaign creation, not the current
        // instance value — an admin fee change (`update_fee_bps`) must not
        // retroactively affect a campaign's already-agreed payouts.
        let fee_bps = campaign.fee_bps;
        let fee = application
            .payout_amount
            .checked_mul(fee_bps)
            .ok_or(Error::InvalidAmount)?
            / ads_bazaar_shared::BASIS_POINTS_DENOMINATOR;
        let net = application
            .payout_amount
            .checked_sub(fee)
            .ok_or(Error::InvalidAmount)?;

        let token = token::Client::new(&env, &campaign.asset.token);
        let contract = env.current_contract_address();
        if fee > 0 {
            token.transfer(&contract, &storage::get_treasury(&env)?, &fee);
        }
        token.transfer(&contract, &creator, &net);

        application.status = ApplicationStatus::Paid;
        storage::set_application(&env, &application);

        campaign.escrow_balance = campaign
            .escrow_balance
            .checked_sub(application.payout_amount)
            .ok_or(Error::InvalidAmount)?;
        campaign.committed_payouts = campaign
            .committed_payouts
            .checked_sub(application.payout_amount)
            .ok_or(Error::InvalidAmount)?;
        if campaign.escrow_balance == 0 {
            campaign.status = CampaignStatus::Completed;
        }
        storage::set_campaign(&env, &campaign);
        events::PaymentReleased {
            campaign_id,
            creator,
            amount: net,
        }
        .publish(&env);
        Ok(())
    }

    /// Cancel a campaign and refund the unallocated (never-committed) portion
    /// of the escrow to the business. Allowed at any point before full payout
    /// completion. Payouts already committed to approved creators remain
    /// reserved and can still be claimed via `claim_payment` afterward.
    pub fn cancel_campaign(
        env: Env,
        business: Address,
        campaign_id: CampaignId,
    ) -> Result<(), Error> {
        require_not_paused(&env)?;
        business.require_auth();
        let mut campaign = storage::get_campaign(&env, campaign_id)?;
        if campaign.business != business {
            return Err(Error::NotCampaignOwner);
        }
        if campaign.status == CampaignStatus::Cancelled
            || campaign.status == CampaignStatus::Completed
        {
            return Err(Error::InvalidStatus);
        }

        let token = token::Client::new(&env, &campaign.asset.token);
        let contract = env.current_contract_address();
        // Never refund more than the unallocated balance. `committed_payouts`
        // is reserved for approved creators who are still owed payment and can
        // `claim_payment` even after the campaign is cancelled.
        let refund = campaign
            .escrow_balance
            .checked_sub(campaign.committed_payouts)
            .ok_or(Error::InvalidAmount)?;
        if refund > 0 {
            token.transfer(&contract, &business, &refund);
        }
        // Leave `committed_payouts` intact so approved-but-unpaid creators can
        // still claim their payouts afterward.
        campaign.escrow_balance = campaign.committed_payouts;
        campaign.status = CampaignStatus::Cancelled;
        storage::set_campaign(&env, &campaign);
        events::CampaignCancelled {
            campaign_id,
            refunded_amount: refund,
        }
        .publish(&env);
        Ok(())
    }

    /// Expire a campaign past its content deadline, refunding the unallocated
    /// (never-committed) portion of the escrow balance to the business. Fails
    /// if called before the content deadline is reached. Any payout already
    /// committed to an approved creator remains reserved and claimable.
    pub fn expire_campaign(
        env: Env,
        business: Address,
        campaign_id: CampaignId,
    ) -> Result<(), Error> {
        require_not_paused(&env)?;
        business.require_auth();
        let mut campaign = storage::get_campaign(&env, campaign_id)?;
        if campaign.business != business {
            return Err(Error::NotCampaignOwner);
        }
        if env.ledger().timestamp() <= campaign.completion_deadline {
            return Err(Error::DeadlineNotReached);
        }
        if campaign.status == CampaignStatus::Cancelled
            || campaign.status == CampaignStatus::Completed
        {
            return Err(Error::InvalidStatus);
        }

        let token = token::Client::new(&env, &campaign.asset.token);
        let contract = env.current_contract_address();
        // Only the unallocated balance is refundable; committed payouts stay
        // reserved for approved creators who can still `claim_payment`.
        let refund = campaign
            .escrow_balance
            .checked_sub(campaign.committed_payouts)
            .ok_or(Error::InvalidAmount)?;
        if refund > 0 {
            token.transfer(&contract, &business, &refund);
        }
        // Leave `committed_payouts` intact so approved-but-unpaid creators can
        // still claim their payouts afterward.
        campaign.escrow_balance = campaign.committed_payouts;
        campaign.status = CampaignStatus::Cancelled;
        storage::set_campaign(&env, &campaign);
        events::CampaignCancelled {
            campaign_id,
            refunded_amount: refund,
        }
        .publish(&env);
        Ok(())
    }

    /// Admin-only emergency recovery for a campaign abandoned long past every
    /// normal deadline — e.g. the business's signing key is lost, rotated
    /// away from, or otherwise unreachable, so none of `cancel_campaign` /
    /// `expire_campaign` / `reclaim_surplus` (all gated on
    /// `business.require_auth()`) can ever be called again for it.
    ///
    /// Deliberately harder to reach than `expire_campaign`: gated on
    /// `EMERGENCY_RECOVERY_GRACE_PERIOD` (months, not the days/weeks scale of
    /// `completion_deadline`) *in addition to* `completion_deadline` having
    /// already passed, so it can never substitute for the normal expiry path
    /// and can't be used to casually bypass business consent.
    ///
    /// Only ever sweeps the unallocated remainder (`escrow_balance -
    /// committed_payouts`), exactly like `cancel_campaign` / `expire_campaign`
    /// / `reclaim_surplus` — a payout already committed to an approved
    /// creator stays reserved and claimable via `claim_payment` regardless of
    /// how long the business has been unreachable.
    ///
    /// Recovered funds go to `treasury`, not back to `business`: the whole
    /// premise of this path is that the business's on-record address is
    /// unreachable, so crediting funds there would just recreate the same
    /// stuck-fund problem. Routing to `treasury` instead leaves them
    /// reachable through a deliberate off-chain claims process (e.g. the
    /// business proving ownership through some other channel), rather than
    /// silently vanishing back into an address nobody can move funds out of.
    pub fn emergency_recover_campaign(
        env: Env,
        admin: Address,
        campaign_id: CampaignId,
    ) -> Result<(), Error> {
        require_not_paused(&env)?;
        require_admin(&env, &admin)?;

        let mut campaign = storage::get_campaign(&env, campaign_id)?;
        if campaign.status == CampaignStatus::Cancelled
            || campaign.status == CampaignStatus::Completed
        {
            return Err(Error::InvalidStatus);
        }
        let grace_period_end = campaign
            .completion_deadline
            .checked_add(EMERGENCY_RECOVERY_GRACE_PERIOD)
            .ok_or(Error::InvalidAmount)?;
        if env.ledger().timestamp() <= grace_period_end {
            return Err(Error::DeadlineNotReached);
        }

        let token = token::Client::new(&env, &campaign.asset.token);
        let contract = env.current_contract_address();
        // Only the unallocated balance is recoverable; committed payouts stay
        // reserved for approved creators who can still `claim_payment`.
        let recovered = campaign
            .escrow_balance
            .checked_sub(campaign.committed_payouts)
            .ok_or(Error::InvalidAmount)?;
        if recovered > 0 {
            token.transfer(&contract, &storage::get_treasury(&env)?, &recovered);
        }
        // Leave `committed_payouts` intact so approved-but-unpaid creators can
        // still claim their payouts afterward.
        campaign.escrow_balance = campaign.committed_payouts;
        campaign.status = CampaignStatus::Cancelled;
        storage::set_campaign(&env, &campaign);
        events::EmergencyRecovery {
            campaign_id,
            amount: recovered,
        }
        .publish(&env);
        Ok(())
    }

    /// Reclaim any unallocated (surplus) escrow back to the business. Surplus
    /// is whatever escrow remains once committed payouts are excluded, so it
    /// can be called while approved creators are still owed payment — those
    /// reserved payouts remain claimable afterward.
    pub fn reclaim_surplus(
        env: Env,
        business: Address,
        campaign_id: CampaignId,
    ) -> Result<(), Error> {
        require_not_paused(&env)?;
        business.require_auth();
        let mut campaign = storage::get_campaign(&env, campaign_id)?;
        if campaign.business != business {
            return Err(Error::NotCampaignOwner);
        }
        if campaign.status == CampaignStatus::Cancelled {
            return Err(Error::InvalidStatus);
        }

        let token = token::Client::new(&env, &campaign.asset.token);
        let contract = env.current_contract_address();
        // Surplus is the unallocated balance only; committed payouts stay
        // reserved for approved creators who can still `claim_payment`.
        let surplus = campaign
            .escrow_balance
            .checked_sub(campaign.committed_payouts)
            .ok_or(Error::InvalidAmount)?;
        if surplus > 0 {
            token.transfer(&contract, &business, &surplus);
        }
        // Leave `committed_payouts` intact so approved-but-unpaid creators can
        // still claim their payouts afterward.
        campaign.escrow_balance = campaign.committed_payouts;
        if campaign.escrow_balance == 0 {
            campaign.status = CampaignStatus::Completed;
        }
        storage::set_campaign(&env, &campaign);
        events::SurplusReclaimed {
            campaign_id,
            amount: surplus,
        }
        .publish(&env);
        Ok(())
    }

    /// Freeze one creator's escrowed payout so it cannot be claimed while a
    /// dispute is under review. Callable by the trusted `dispute-resolution`
    /// contract set at `initialize` (the normal path, reached via
    /// `raise_dispute`), or directly by `admin` — matching `resolve_dispute`'s
    /// auth model — so an emergency freeze doesn't require routing through
    /// the dispute-resolution contract.
    ///
    /// The freeze is scoped to a single application, not the whole campaign —
    /// a dispute with one creator must not stall payouts to every other
    /// creator working the same brief. It therefore does not move the
    /// campaign into `CampaignStatus::Disputed`; that status stays reserved
    /// for a campaign-wide halt.
    ///
    /// Only an application that is still settleable can be frozen: one that
    /// was approved at some point (nonzero `payout_amount`) and has not
    /// already been paid. That is deliberately the *only* time bound on
    /// raising a dispute — see `dispute-resolution::raise_dispute`.
    ///
    /// Freezing also stamps `Application::dispute_opened_at`, which starts
    /// the `MIN_EVIDENCE_WINDOW` that `resolve_dispute` will not settle
    /// before. `require_not_frozen` below rejects a second freeze over an
    /// already-frozen payout, so that clock cannot be reset by re-freezing.
    ///
    /// TODO(contributors): `resolve_dispute_payout` must clear `frozen` and
    /// `dispute_opened_at` when it settles, otherwise a resolved application
    /// stays locked forever.
    pub fn freeze_for_dispute(
        env: Env,
        caller: Address,
        campaign_id: CampaignId,
        creator: Address,
    ) -> Result<(), Error> {
        require_not_paused(&env)?;
        caller.require_auth();
        if caller != storage::get_dispute_contract(&env)? && caller != storage::get_admin(&env)? {
            return Err(Error::Unauthorized);
        }

        let campaign = storage::get_campaign(&env, campaign_id)?;
        if campaign.status == CampaignStatus::Cancelled {
            return Err(Error::InvalidStatus);
        }

        let mut application = storage::get_application(&env, campaign_id, &creator)?;
        require_not_frozen(&application)?;
        if application.status == ApplicationStatus::Paid || application.payout_amount <= 0 {
            return Err(Error::SubmissionNotPayable);
        }

        application.frozen = true;
        application.dispute_opened_at = Some(env.ledger().timestamp());
        storage::set_application(&env, &application);
        events::DisputeFrozen {
            campaign_id,
            creator,
        }
        .publish(&env);
        Ok(())
    }

    /// Read-only lookup of the business that owns `campaign_id`.
    ///
    /// Exists so `dispute-resolution` can authorize a business-raised dispute
    /// with a single cross-contract read, without having to know this
    /// contract's full `Campaign` type.
    pub fn get_campaign_business(env: Env, campaign_id: CampaignId) -> Result<Address, Error> {
        Ok(storage::get_campaign(&env, campaign_id)?.business)
    }

    /// Apply a dispute outcome (from `dispute-resolution`) by releasing or
    /// refunding the frozen escrow amount accordingly.
    ///
    /// TODO(contributors): implement alongside `freeze_for_dispute`.
    #[allow(unused_variables)]
    pub fn resolve_dispute_payout(
        env: Env,
        campaign_id: CampaignId,
        creator: Address,
        creator_bps: i128,
    ) -> Result<(), Error> {
        require_not_paused(&env)?;
        todo!("design + implement dispute payout resolution — see doc comment above")
    }

    /// Admin-resolved settlement for a single creator's committed-but-not-
    /// yet-paid application, as a simplified interim path alongside the
    /// arbiter-resolved `dispute-resolution` contract (`resolve_dispute_payout`
    /// above is the intended integration point for that contract once it's
    /// implemented; this is a separate admin-only shortcut that works today
    /// without it). Admin-only.
    ///
    /// Requires an application with a nonzero `payout_amount` that hasn't
    /// already been paid — i.e. one that was approved via `approve_creator`
    /// at some point, regardless of its current `Approved` / `ProofSubmitted`
    /// / `Rejected` state. Moves funds per `resolution` (see
    /// `DisputeResolution`) and marks the application `Paid`.
    ///
    /// Settling is gated on there being a dispute to settle, and on that
    /// dispute having been open long enough for the other side to answer it:
    ///
    /// - The application must have been frozen by `freeze_for_dispute`
    ///   (`Error::NoDisputeOpen` otherwise). Admin can call that themselves,
    ///   so this is not a dependency on the `dispute-resolution` contract —
    ///   but it does mean every admin settlement is preceded by a public
    ///   `events::DisputeFrozen`, which is what gives the counterparty
    ///   something to notice.
    /// - At least `MIN_EVIDENCE_WINDOW` must have elapsed since that freeze
    ///   (`Error::EvidenceWindowOpen` otherwise), so neither party's payout
    ///   can be reallocated out from under them without warning.
    pub fn resolve_dispute(
        env: Env,
        admin: Address,
        campaign_id: CampaignId,
        creator: Address,
        resolution: DisputeResolution,
    ) -> Result<(), Error> {
        require_not_paused(&env)?;
        require_admin(&env, &admin)?;

        let mut campaign = storage::get_campaign(&env, campaign_id)?;
        if campaign.status == CampaignStatus::Cancelled
            || campaign.status == CampaignStatus::Completed
        {
            return Err(Error::InvalidStatus);
        }

        let mut application = storage::get_application(&env, campaign_id, &creator)?;
        if application.status == ApplicationStatus::Paid || application.payout_amount <= 0 {
            return Err(Error::SubmissionNotPayable);
        }

        // A payout can only be reallocated if it is actually contested, and
        // only once the evidence window on that dispute has run out.
        let dispute_opened_at = application.dispute_opened_at.ok_or(Error::NoDisputeOpen)?;
        let window_end = dispute_opened_at
            .checked_add(MIN_EVIDENCE_WINDOW)
            .ok_or(Error::InvalidAmount)?;
        if window_end > env.ledger().timestamp() {
            return Err(Error::EvidenceWindowOpen);
        }

        let payout_amount = application.payout_amount;
        let fee_bps = campaign.fee_bps;
        let (creator_gross, business_amount) = match resolution {
            DisputeResolution::PayCreator => (payout_amount, 0),
            DisputeResolution::RefundBusiness => (0, payout_amount),
            DisputeResolution::Split(bps) => {
                if !(0..=ads_bazaar_shared::BASIS_POINTS_DENOMINATOR).contains(&bps) {
                    return Err(Error::InvalidAmount);
                }
                let creator_gross = payout_amount.checked_mul(bps).ok_or(Error::InvalidAmount)?
                    / ads_bazaar_shared::BASIS_POINTS_DENOMINATOR;
                (creator_gross, payout_amount - creator_gross)
            }
        };

        // Fee only ever applies to the creator's gross share — a full
        // RefundBusiness (creator_gross == 0) correctly incurs no fee.
        let fee = creator_gross
            .checked_mul(fee_bps)
            .ok_or(Error::InvalidAmount)?
            / ads_bazaar_shared::BASIS_POINTS_DENOMINATOR;
        let creator_net = creator_gross.checked_sub(fee).ok_or(Error::InvalidAmount)?;

        let token = token::Client::new(&env, &campaign.asset.token);
        let contract = env.current_contract_address();
        if fee > 0 {
            token.transfer(&contract, &storage::get_treasury(&env)?, &fee);
        }
        if creator_net > 0 {
            token.transfer(&contract, &creator, &creator_net);
        }
        if business_amount > 0 {
            token.transfer(&contract, &campaign.business, &business_amount);
        }

        application.status = ApplicationStatus::Paid;
        // The dispute is settled, so drop both the freeze and the window
        // clock rather than leaving a paid application marked contested.
        application.frozen = false;
        application.dispute_opened_at = None;
        storage::set_application(&env, &application);

        campaign.escrow_balance = campaign
            .escrow_balance
            .checked_sub(payout_amount)
            .ok_or(Error::InvalidAmount)?;
        campaign.committed_payouts = campaign
            .committed_payouts
            .checked_sub(payout_amount)
            .ok_or(Error::InvalidAmount)?;
        if campaign.escrow_balance == 0 {
            campaign.status = CampaignStatus::Completed;
        }
        storage::set_campaign(&env, &campaign);

        events::DisputeResolved {
            campaign_id,
            creator,
            creator_amount: creator_net,
            business_amount,
        }
        .publish(&env);
        Ok(())
    }

    /// Update the metadata URI of a campaign. Only the campaign's business
    /// may call this, and only when no creator has applied yet — once
    /// creators have applied the brief locks to protect applicant trust.
    ///
    /// `new_metadata` must be a non-empty string. Does not move funds or
    /// change the campaign's status.
    pub fn update_campaign_metadata(
        env: Env,
        campaign_id: CampaignId,
        business: Address,
        new_metadata: String,
    ) -> Result<(), Error> {
        require_not_paused(&env)?;
        business.require_auth();
        let mut campaign = storage::get_campaign(&env, campaign_id)?;
        if campaign.business != business {
            return Err(Error::NotCampaignOwner);
        }
        if campaign.status == CampaignStatus::Cancelled
            || campaign.status == CampaignStatus::Completed
        {
            return Err(Error::InvalidStatus);
        }
        if storage::has_campaign_applicants(&env, campaign_id) {
            return Err(Error::ApplicationsExist);
        }
        if new_metadata.is_empty() {
            return Err(Error::InvalidMetadata);
        }

        campaign.metadata_uri = new_metadata.clone();
        storage::set_campaign(&env, &campaign);
        events::CampaignMetadataUpdated {
            campaign_id,
            business,
            new_metadata,
        }
        .publish(&env);
        Ok(())
    }

    /// Read-only lookup of a campaign's current state.
    pub fn get_campaign(env: Env, campaign_id: CampaignId) -> Result<Campaign, Error> {
        storage::get_campaign(&env, campaign_id)
    }

    /// Read-only lookup of a creator's application to a campaign.
    pub fn get_application(
        env: Env,
        campaign_id: CampaignId,
        creator: Address,
    ) -> Result<Application, Error> {
        storage::get_application(&env, campaign_id, &creator)
    }

    /// Read-only lookup of protocol-level config (admin, treasury, fee_bps)
    /// so the frontend can compute a fee breakdown before funding a
    /// campaign. Requires no auth. Errors with `Error::NotInitialized` if
    /// called before `initialize`.
    pub fn get_protocol_config(env: Env) -> Result<ProtocolConfig, Error> {
        let admin = storage::get_admin(&env)?;
        let treasury = storage::get_treasury(&env)?;
        let fee_bps = storage::get_fee_bps(&env)?;

        storage::extend_instance_ttl(&env);

        Ok(ProtocolConfig {
            admin,
            treasury,
            fee_bps,
        })
    }

    /// Read-only lookup of the WASM version string set at `initialize`.
    pub fn version(env: Env) -> Result<String, Error> {
        storage::get_version(&env)
    }

    /// Replace this contract's WASM binary in place via Soroban's native
    /// upgrade mechanism, preserving the contract address and all existing
    /// storage. Admin-only.
    ///
    /// TODO(contributors): this does not bump the stored `Version` — decide
    /// whether `upgrade` should take a new version string to persist, or
    /// whether version tracking should be derived from the wasm hash instead.
    pub fn upgrade(env: Env, admin: Address, new_wasm_hash: BytesN<32>) -> Result<(), Error> {
        admin.require_auth();
        let stored_admin = storage::get_admin(&env)?;
        if admin != stored_admin {
            return Err(Error::Unauthorized);
        }

        env.deployer()
            .update_current_contract_wasm(new_wasm_hash.clone());
        events::ContractUpgraded { new_wasm_hash }.publish(&env);
        Ok(())
    }
}

#[cfg(test)]
mod test;
