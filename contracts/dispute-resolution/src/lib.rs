//! # ads-bazaar-dispute-resolution
//!
//! Arbitrates disputes over campaign payouts held by `campaign-escrow`. As
//! with that contract, this crate ships the data model, storage schema,
//! errors and public API surface; the arbitration workflow itself
//! (assigning arbiters, evidence windows, resolving outcomes and calling
//! back into escrow) is left as `todo!()` for contributors — the
//! arbitration *model* (single trusted arbiter vs. staked jurors vs. an
//! oracle) is the biggest open design question in this repo.
#![no_std]

mod error;
mod escrow;
mod events;
mod storage;
mod types;

pub use error::Error;
pub use types::Dispute;

use ads_bazaar_shared::{CampaignId, DisputeId, DisputeOutcome, DisputeStatus};
use soroban_sdk::{contract, contractimpl, Address, BytesN, Env, String};

/// Version string stored at `initialize` time. `upgrade` swaps the WASM
/// binary but does not bump this on its own — see the TODO on `upgrade`
/// below.
const INITIAL_VERSION: &str = "0.1.0";

/// Require that `admin` matches the address stored at `initialize` time.
/// Returns `Error::Unauthorized` for any other caller. Mirrors
/// `campaign-escrow`'s helper of the same name.
fn require_admin(env: &Env, admin: &Address) -> Result<(), Error> {
    admin.require_auth();
    let stored_admin = storage::get_admin(env)?;
    if stored_admin != *admin {
        return Err(Error::Unauthorized);
    }
    Ok(())
}

#[contract]
pub struct DisputeResolutionContract;

#[contractimpl]
impl DisputeResolutionContract {
    /// One-time setup. `escrow_contract` should be the deployed
    /// `campaign-escrow` contract's address — this contract will need to
    /// call back into it (`resolve_dispute_payout`) once resolution logic
    /// is implemented.
    pub fn initialize(env: Env, admin: Address, escrow_contract: Address) -> Result<(), Error> {
        if storage::is_initialized(&env) {
            return Err(Error::AlreadyInitialized);
        }
        admin.require_auth();

        storage::set_admin(&env, &admin);
        storage::set_escrow_contract(&env, &escrow_contract);
        storage::set_version(&env, &String::from_str(&env, INITIAL_VERSION));
        Ok(())
    }

    /// Raise a dispute over a creator's payout on a given campaign, freezing
    /// that payout in escrow so it can't be claimed mid-review.
    ///
    /// **Who may raise.** Either side of the contested payout: the `creator`
    /// themselves, or the campaign's business. Both need it — a creator
    /// disputes a business that won't approve delivered work, and a business
    /// disputes a creator about to be auto-approved past the content
    /// deadline. Business ownership lives in `campaign-escrow`, so it is
    /// verified with a cross-contract read rather than trusted from the
    /// caller. `raised_by == creator` needs no such read: escrow's
    /// `freeze_for_dispute` refuses a campaign/creator pair with no
    /// settleable application, so a stranger can't name themselves creator on
    /// a campaign they never worked.
    ///
    /// **Time window.** There is deliberately none. This contract can't see
    /// proof-submission timestamps, and the bound that actually protects
    /// funds is "before the payout is claimed" — which escrow already
    /// enforces by refusing to freeze an already-`Paid` application. A
    /// deadline expressed in days would only add a second, weaker rule.
    ///
    /// **Repeat disputes.** One open dispute per `(campaign_id, creator)`.
    /// A second attempt fails with `Error::DisputeAlreadyRaised` rather than
    /// re-freezing an already-frozen payout.
    pub fn raise_dispute(
        env: Env,
        raised_by: Address,
        campaign_id: CampaignId,
        creator: Address,
        reason_uri: String,
    ) -> Result<DisputeId, Error> {
        raised_by.require_auth();
        if reason_uri.is_empty() {
            return Err(Error::InvalidReason);
        }
        if storage::get_open_dispute(&env, campaign_id, &creator).is_some() {
            return Err(Error::DisputeAlreadyRaised);
        }

        let escrow = escrow::CampaignEscrowClient::new(&env, &storage::get_escrow_contract(&env)?);
        if raised_by != creator && raised_by != escrow.get_campaign_business(&campaign_id) {
            return Err(Error::Unauthorized);
        }

        // Freeze first: if escrow refuses (no application, already paid) the
        // whole invocation traps and no dispute record is left behind.
        escrow.freeze_for_dispute(&env.current_contract_address(), &campaign_id, &creator);

        let dispute_id = storage::next_dispute_id(&env);
        storage::set_dispute(
            &env,
            dispute_id,
            &Dispute {
                campaign_id,
                creator: creator.clone(),
                raised_by: raised_by.clone(),
                reason_uri,
                arbiter: None,
                status: DisputeStatus::Raised,
                outcome: DisputeOutcome::Pending,
                raised_at: env.ledger().timestamp(),
                resolved_at: None,
            },
        );
        storage::set_open_dispute(&env, campaign_id, &creator, dispute_id);

        events::DisputeRaised {
            dispute_id,
            raised_by,
        }
        .publish(&env);
        Ok(dispute_id)
    }

    /// Assign an arbiter to review a raised dispute.
    ///
    /// Only valid from `DisputeStatus::Raised` — a dispute already
    /// `UnderReview` (an arbiter assigned) or `Resolved` cannot be
    /// reassigned through this call.
    pub fn assign_arbiter(
        env: Env,
        admin: Address,
        dispute_id: DisputeId,
        arbiter: Address,
    ) -> Result<(), Error> {
        require_admin(&env, &admin)?;

        let mut dispute = storage::get_dispute(&env, dispute_id)?;
        if dispute.status != DisputeStatus::Raised {
            return Err(Error::InvalidStatus);
        }

        dispute.arbiter = Some(arbiter.clone());
        dispute.status = DisputeStatus::UnderReview;
        storage::set_dispute(&env, dispute_id, &dispute);

        events::ArbiterAssigned {
            dispute_id,
            arbiter,
        }
        .publish(&env);
        Ok(())
    }

    /// Arbiter resolves a dispute with a final outcome, then calls back
    /// into `campaign-escrow::resolve_dispute_payout` to release/refund
    /// the frozen funds accordingly.
    ///
    /// TODO(contributors): implement.
    #[allow(unused_variables)]
    pub fn resolve_dispute(
        env: Env,
        arbiter: Address,
        dispute_id: DisputeId,
        outcome: DisputeOutcome,
    ) -> Result<(), Error> {
        arbiter.require_auth();
        todo!("design + implement dispute resolution — see doc comment above")
    }

    /// Close out the open dispute over a `(campaign_id, creator)` payout that
    /// `campaign-escrow::resolve_dispute` just settled. Only the
    /// `campaign-escrow` contract set at `initialize` may call this.
    ///
    /// Idempotent: if there is no open dispute record for that payout (the
    /// freeze came from the admin's direct `freeze_for_dispute` path, or the
    /// dispute was already closed) this is a no-op rather than an error.
    pub fn close_dispute(
        env: Env,
        caller: Address,
        campaign_id: CampaignId,
        creator: Address,
        outcome: DisputeOutcome,
    ) -> Result<(), Error> {
        caller.require_auth();
        if caller != storage::get_escrow_contract(&env)? {
            return Err(Error::Unauthorized);
        }
        if outcome == DisputeOutcome::Pending {
            return Err(Error::InvalidStatus);
        }

        let Some(dispute_id) = storage::get_open_dispute(&env, campaign_id, &creator) else {
            return Ok(());
        };

        let mut dispute = storage::get_dispute(&env, dispute_id)?;
        dispute.status = DisputeStatus::Resolved;
        dispute.outcome = outcome;
        dispute.resolved_at = Some(env.ledger().timestamp());
        storage::set_dispute(&env, dispute_id, &dispute);
        storage::clear_open_dispute(&env, campaign_id, &creator);

        events::DisputeResolved { dispute_id }.publish(&env);
        Ok(())
    }

    /// Read-only lookup of a dispute's current state.
    pub fn get_dispute(env: Env, dispute_id: DisputeId) -> Result<Dispute, Error> {
        storage::get_dispute(&env, dispute_id)
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
        require_admin(&env, &admin)?;

        env.deployer()
            .update_current_contract_wasm(new_wasm_hash.clone());
        events::ContractUpgraded { new_wasm_hash }.publish(&env);
        Ok(())
    }
}

#[cfg(test)]
mod test;
