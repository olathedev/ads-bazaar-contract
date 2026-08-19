use ads_bazaar_shared::{ApplicationStatus, CampaignId, CampaignStatus, PayoutAsset};
use soroban_sdk::{contracttype, Address, String};

/// A creator campaign funded and escrowed by a single business.
///
/// `escrow_balance` is tracked separately from `total_budget` so partial
/// releases (once implemented) can be reconciled against what is actually
/// still held by the contract, independent of what was originally deposited.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Campaign {
    /// Platform fee in basis points, snapshotted at campaign creation so admin changes don't affect live campaigns.
    pub fee_bps: i128,
    pub id: CampaignId,
    pub business: Address,
    pub asset: PayoutAsset,
    pub total_budget: i128,
    pub escrow_balance: i128,
    /// Sum of payout amounts committed to selected (not-yet-paid) creators.
    /// Reserved against `escrow_balance` so total commitments never exceed funds.
    pub committed_payouts: i128,
    pub max_creators: u32,
    pub approved_count: u32,
    /// Ledger timestamp (unix seconds) after which new applications are rejected.
    pub application_deadline: u64,
    /// Ledger timestamp (unix seconds) by which approved creators must submit proof.
    pub completion_deadline: u64,
    /// Off-chain pointer (IPFS/HTTPS URI) to the full campaign brief.
    pub metadata_uri: String,
    pub status: CampaignStatus,
}

/// A single creator's application to a campaign.
///
/// TODO(contributors): `payout_amount` is set at approval time in the current
/// design sketch (business decides per-creator pay when approving). Revisit
/// if campaigns should instead split `total_budget` evenly across
/// `max_creators`, or support tiered/milestone payouts.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Application {
    pub campaign_id: CampaignId,
    pub creator: Address,
    pub pitch_uri: String,
    pub proof_uri: Option<String>,
    pub payout_amount: i128,
    /// Whether the business has accepted the submitted proof (making it payable).
    pub proof_approved: bool,
    /// Set by `freeze_for_dispute` while a dispute is under review for this
    /// application. Freezing is per-application rather than per-campaign so
    /// one contested creator doesn't block payouts to every other creator on
    /// the same campaign. Cleared by `resolve_dispute` (and, eventually,
    /// `resolve_dispute_payout`) once the application settles.
    pub frozen: bool,
    /// Ledger timestamp (unix seconds) at which the dispute over this
    /// application was opened by `freeze_for_dispute`, or `None` if no
    /// dispute is open. Written in the same storage entry as `frozen` so the
    /// two can never drift apart.
    ///
    /// This is what `resolve_dispute` measures `MIN_EVIDENCE_WINDOW` from:
    /// the admin cannot settle a contested payout until the other party has
    /// had that long to respond. Exposed via `get_application` so a frontend
    /// can show when resolution becomes possible.
    pub dispute_opened_at: Option<u64>,
    pub status: ApplicationStatus,
}

/// Snapshot of protocol-level configuration, returned by
/// `get_protocol_config` so the frontend can compute fee breakdowns before a
/// business funds a campaign.
///
/// `treasury` defaults to `admin` at `initialize` time — there is no
/// separate fee-collection destination yet (see the TODO on
/// `release_payment` in `lib.rs`). A future issue can add a
/// `set_treasury` admin-only setter if/when that needs to diverge.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProtocolConfig {
    pub admin: Address,
    pub treasury: Address,
    pub fee_bps: i128,
}

/// Admin-chosen outcome for `resolve_dispute`, settling a creator's
/// committed-but-unsettled payout on a single application.
///
/// This is a simplified, admin-resolved path that exists alongside the
/// separate arbiter-resolved `dispute-resolution` contract (see
/// `ads_bazaar_shared::DisputeOutcome` and `resolve_dispute_payout` below)
/// rather than replacing it — which of the two a given campaign actually
/// uses is still an open product question (see `docs/ARCHITECTURE.md`).
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DisputeResolution {
    /// Creator receives their full committed payout, minus the platform fee.
    PayCreator,
    /// Business receives the full committed amount back; creator gets nothing.
    RefundBusiness,
    /// Creator receives `bps / 10_000` of the committed payout (minus fee on
    /// that portion); business receives the remainder.
    Split(i128),
}
