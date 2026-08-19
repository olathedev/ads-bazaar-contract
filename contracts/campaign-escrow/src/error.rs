use soroban_sdk::contracterror;

/// Errors returned by the campaign-escrow contract.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    NotInitialized = 1,
    AlreadyInitialized = 2,
    Unauthorized = 3,
    CampaignNotFound = 4,
    ApplicationNotFound = 5,
    InvalidStatus = 6,
    InvalidAmount = 7,
    DeadlinePassed = 8,
    MaxCreatorsReached = 9,
    InsufficientEscrowBalance = 10,
    /// Caller is not the campaign owner (business) that created the campaign.
    NotCampaignOwner = 11,
    /// A submission/claim was attempted that is not yet eligible for payout.
    SubmissionNotPayable = 12,
    /// The creator has already applied to this campaign.
    AlreadyApplied = 13,
    /// The creator has already been selected (approved) for this campaign.
    AlreadySelected = 14,
    /// Applications are no longer accepted (application deadline passed).
    ApplicationDeadlinePassed = 15,
    /// Proof of work can no longer be submitted (content deadline passed).
    ContentDeadlinePassed = 16,
    /// A deadline was supplied that is in the past.
    DeadlineInPast = 17,
    /// The campaign is not yet past its content deadline.
    DeadlineNotReached = 18,
    /// Returned by any guarded state-changing function while the contract
    /// is paused via `pause`. See `require_not_paused` in `lib.rs`.
    ContractPaused = 19,
    /// The application deadline is not before the completion deadline.
    InvalidDeadlineOrder = 20,
    /// The updated fee is too high (exceeds the 1000 bps maximum).
    FeeTooHigh = 21,
    /// At least one creator has already applied to this campaign, so the
    /// campaign brief / metadata cannot be changed.
    ApplicationsExist = 22,
    /// The metadata string must be non-empty.
    InvalidMetadata = 23,
    /// `max_creators` was set to zero when creating a campaign.
    InvalidCreatorCount = 24,
    /// The application is frozen pending dispute arbitration, so it can
    /// neither be paid out nor have its proof state changed.
    PayoutFrozen = 25,
    /// `resolve_dispute` was called before `MIN_EVIDENCE_WINDOW` had elapsed
    /// since the dispute was opened by `freeze_for_dispute`. The other party
    /// still has time to submit counter-evidence.
    EvidenceWindowOpen = 26,
    /// `resolve_dispute` was called on an application that has no open
    /// dispute — nothing has been frozen for it via `freeze_for_dispute`, so
    /// there is no contested payout to settle and no evidence window to
    /// measure from.
    NoDisputeOpen = 27,
}
