//! Cross-contract integration tests between `campaign-escrow` and
//! `dispute-resolution`.
//!
//! Every test in the unit-test modules (`campaign-escrow/src/test.rs`,
//! `dispute-resolution/src/test.rs`) registers only a single contract in the
//! `Env`. This file is specifically for scenarios where **both** contracts must
//! be live in the same environment so that real Soroban cross-contract call
//! semantics are exercised — particularly the auth/invocation chain between
//! `dispute-resolution::raise_dispute` and
//! `campaign-escrow::freeze_for_dispute`.
//!
//! ## What is tested here
//!
//! | # | Scenario | Key assertion |
//! |---|---|---|
//! | 1 | Both contracts initialize pointing at each other | version readable; fee stored |
//! | 2 | Creator raises dispute; payout freezes | application.frozen == true |
//! | 3 | Frozen payout cannot be claimed | PayoutFrozen error |
//! | 4 | Business raises dispute; freeze verified | application.frozen == true |
//! | 5 | Dispute record fields match raise call | all fields asserted |
//! | 6 | raise → admin resolve PayCreator | creator net balance, treasury fee |
//! | 7 | raise → admin resolve RefundBusiness | business balance restored |
//! | 8 | raise → admin resolve Split | proportional balances |
//! | 9 | Stranger cannot call freeze_for_dispute directly | Unauthorized error |
//! | 10 | Admin can bypass dispute contract for emergency freeze | frozen == true |
//! | 11 | Other creators unaffected by one dispute | clean creator can claim |
//! | 12 | Duplicate raise rejected | DisputeAlreadyRaised |
//! | 13 | Raise on already-paid payout rejected | no dispute record created |
//! | 14 | Raise in auto-approval window still blocks claim | PayoutFrozen |
//! | 15 | Two creators on same campaign can have independent disputes | separate dispute ids |
//! | 16 | raise → immediate admin resolve rejected | EvidenceWindowOpen; no funds moved |
//!
//! Note that tests 6–8 advance the ledger past `MIN_EVIDENCE_WINDOW` before
//! resolving. That is the real flow, not test scaffolding: a dispute raised
//! through `raise_dispute` is not admin-resolvable until the window closes
//! (test 16).
//!
//! ## What is NOT tested here (pending implementations)
//!
//! - `dispute-resolution::assign_arbiter` — `todo!()` upstream (issue #40)
//! - `dispute-resolution::resolve_dispute` — `todo!()` upstream (issue #41)
//! - `campaign-escrow::resolve_dispute_payout` — `todo!()` upstream (issue #42)
//!
//! Once those issues are implemented, extend tests 6–8 with the full
//! arbiter-resolved path so `raise → assign → resolve → payout` has
//! balance-level coverage.

use ads_bazaar_campaign_escrow::{
    CampaignEscrowContract, Error as EscrowError, MIN_EVIDENCE_WINDOW,
};
use ads_bazaar_dispute_resolution::DisputeResolutionContract;
use ads_bazaar_shared::{ApplicationStatus, DisputeOutcome, DisputeStatus, PayoutAsset};
use soroban_sdk::testutils::{Address as _, Ledger as _};
use soroban_sdk::token::StellarAssetClient;
use soroban_sdk::{Address, Env, String};

// ── Constants ──────────────────────────────────────────────────────────────────

const BASE_TIME: u64 = 1_000_000;
const APPLICATION_WINDOW: u64 = 86_400;
const COMPLETION_WINDOW: u64 = 604_800;
const TOTAL_BUDGET: i128 = 10_000_000;
const PAYOUT: i128 = 2_000_000;
const FEE_BPS: i128 = 500; // 5 %

// ── Fixture ────────────────────────────────────────────────────────────────────

struct Fixture {
    env: Env,
    escrow_id: Address,
    dispute_id: Address,
    token: Address,
    admin: Address,
    business: Address,
    treasury: Address,
}

impl Fixture {
    fn setup() -> Self {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = BASE_TIME);

        let escrow_id = env.register(CampaignEscrowContract, ());
        let dispute_id = env.register(DisputeResolutionContract, ());

        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);

        let escrow =
            ads_bazaar_campaign_escrow::CampaignEscrowContractClient::new(&env, &escrow_id);
        let disputes =
            ads_bazaar_dispute_resolution::DisputeResolutionContractClient::new(&env, &dispute_id);

        escrow.initialize(&admin, &dispute_id, &FEE_BPS);
        escrow.update_treasury(&admin, &treasury);
        disputes.initialize(&admin, &escrow_id);

        let business = Address::generate(&env);
        let token_contract = env.register_stellar_asset_contract_v2(Address::generate(&env));
        let token = token_contract.address();
        StellarAssetClient::new(&env, &token).mint(&business, &1_000_000_000);

        Fixture {
            env,
            escrow_id,
            dispute_id,
            token,
            admin,
            business,
            treasury,
        }
    }

    fn escrow(&self) -> ads_bazaar_campaign_escrow::CampaignEscrowContractClient<'_> {
        ads_bazaar_campaign_escrow::CampaignEscrowContractClient::new(&self.env, &self.escrow_id)
    }

    fn disputes(&self) -> ads_bazaar_dispute_resolution::DisputeResolutionContractClient<'_> {
        ads_bazaar_dispute_resolution::DisputeResolutionContractClient::new(
            &self.env,
            &self.dispute_id,
        )
    }

    /// Create a funded campaign and return its id.
    fn create_funded_campaign(&self) -> u64 {
        let now = self.env.ledger().timestamp();
        let escrow = self.escrow();
        let id = escrow.create_campaign(
            &self.business,
            &PayoutAsset {
                token: self.token.clone(),
                symbol: String::from_str(&self.env, "USDC"),
            },
            &TOTAL_BUDGET,
            &5,
            &(now + APPLICATION_WINDOW),
            &(now + COMPLETION_WINDOW),
            &String::from_str(&self.env, "ipfs://brief"),
        );
        escrow.fund_campaign(&self.business, &id);
        id
    }

    /// Apply a creator to `campaign_id`, get them approved, and submit proof.
    fn add_creator_with_proof(&self, campaign_id: u64) -> Address {
        let escrow = self.escrow();
        let creator = Address::generate(&self.env);
        escrow.apply_to_campaign(
            &creator,
            &campaign_id,
            &String::from_str(&self.env, "ipfs://pitch"),
        );
        escrow.approve_creator(&self.business, &campaign_id, &creator, &PAYOUT);
        escrow.submit_proof(
            &creator,
            &campaign_id,
            &String::from_str(&self.env, "ipfs://proof"),
        );
        creator
    }

    fn token_client(&self) -> soroban_sdk::token::Client<'_> {
        soroban_sdk::token::Client::new(&self.env, &self.token)
    }

    /// Advance past the escrow's evidence window, so a dispute raised now
    /// becomes resolvable by `resolve_dispute`.
    fn wait_out_evidence_window(&self) {
        self.env
            .ledger()
            .with_mut(|l| l.timestamp += MIN_EVIDENCE_WINDOW);
    }
}

// ── 1. Initialization cross-check ─────────────────────────────────────────────

#[test]
fn both_contracts_initialize_pointing_at_each_other() {
    let f = Fixture::setup();

    assert_eq!(f.escrow().version(), String::from_str(&f.env, "0.1.0"));
    assert_eq!(f.disputes().version(), String::from_str(&f.env, "0.1.0"));

    let cfg = f.escrow().get_protocol_config();
    assert_eq!(cfg.fee_bps, FEE_BPS);
    assert_eq!(cfg.admin, f.admin);
    assert_eq!(cfg.treasury, f.treasury);
}

// ── 2. raise_dispute cross-contract call freezes the payout ───────────────────

#[test]
fn creator_raise_dispute_freezes_payout_via_cross_contract_call() {
    let f = Fixture::setup();
    let campaign_id = f.create_funded_campaign();
    let creator = f.add_creator_with_proof(campaign_id);

    assert!(!f.escrow().get_application(&campaign_id, &creator).frozen);

    f.disputes().raise_dispute(
        &creator,
        &campaign_id,
        &creator,
        &String::from_str(&f.env, "ipfs://evidence"),
    );

    let app = f.escrow().get_application(&campaign_id, &creator);
    assert!(app.frozen);
    assert_eq!(app.status, ApplicationStatus::ProofSubmitted);
}

// ── 3. Frozen payout cannot be claimed ────────────────────────────────────────

#[test]
fn frozen_payout_cannot_be_claimed() {
    let f = Fixture::setup();
    let campaign_id = f.create_funded_campaign();
    let creator = f.add_creator_with_proof(campaign_id);
    f.escrow()
        .approve_submission(&f.business, &campaign_id, &creator);

    f.disputes().raise_dispute(
        &creator,
        &campaign_id,
        &creator,
        &String::from_str(&f.env, "ipfs://evidence"),
    );

    assert_eq!(
        f.escrow().try_claim_payment(&creator, &campaign_id),
        Err(Ok(EscrowError::PayoutFrozen))
    );
}

// ── 4. Business raises dispute ────────────────────────────────────────────────

#[test]
fn business_raise_dispute_freezes_creator_payout() {
    let f = Fixture::setup();
    let campaign_id = f.create_funded_campaign();
    let creator = f.add_creator_with_proof(campaign_id);

    f.disputes().raise_dispute(
        &f.business,
        &campaign_id,
        &creator,
        &String::from_str(&f.env, "ipfs://not-as-briefed"),
    );

    assert!(f.escrow().get_application(&campaign_id, &creator).frozen);
}

// ── 5. Dispute record fields match raise call ──────────────────────────────────

#[test]
fn dispute_record_fields_match_raise_call() {
    let f = Fixture::setup();
    let campaign_id = f.create_funded_campaign();
    let creator = f.add_creator_with_proof(campaign_id);
    let reason = String::from_str(&f.env, "ipfs://evidence");

    let dispute_id = f
        .disputes()
        .raise_dispute(&creator, &campaign_id, &creator, &reason);

    let d = f.disputes().get_dispute(&dispute_id);
    assert_eq!(d.campaign_id, campaign_id);
    assert_eq!(d.creator, creator);
    assert_eq!(d.raised_by, creator);
    assert_eq!(d.reason_uri, reason);
    assert_eq!(d.status, DisputeStatus::Raised);
    assert_eq!(d.outcome, DisputeOutcome::Pending);
    assert_eq!(d.arbiter, None);
    assert_eq!(d.raised_at, BASE_TIME);
    assert_eq!(d.resolved_at, None);
}

// ── 6. raise → admin resolve PayCreator: balance assertions ───────────────────

#[test]
fn raise_then_admin_resolve_pay_creator_correct_balances() {
    let f = Fixture::setup();
    let campaign_id = f.create_funded_campaign();
    let creator = f.add_creator_with_proof(campaign_id);

    f.disputes().raise_dispute(
        &creator,
        &campaign_id,
        &creator,
        &String::from_str(&f.env, "ipfs://evidence"),
    );
    assert!(f.escrow().get_application(&campaign_id, &creator).frozen);
    f.wait_out_evidence_window();

    let tc = f.token_client();
    let creator_before = tc.balance(&creator);
    let treasury_before = tc.balance(&f.treasury);
    let escrow_before = tc.balance(&f.escrow_id);

    f.escrow().resolve_dispute(
        &f.admin,
        &campaign_id,
        &creator,
        &ads_bazaar_campaign_escrow::DisputeResolution::PayCreator,
    );

    let fee = PAYOUT * FEE_BPS / 10_000;
    let net = PAYOUT - fee;

    assert_eq!(tc.balance(&creator), creator_before + net);
    assert_eq!(tc.balance(&f.treasury), treasury_before + fee);
    assert_eq!(tc.balance(&f.escrow_id), escrow_before - PAYOUT);

    let app = f.escrow().get_application(&campaign_id, &creator);
    assert_eq!(app.status, ApplicationStatus::Paid);
    assert!(!app.frozen, "frozen flag must be cleared after settlement");
}

// ── 7. raise → admin resolve RefundBusiness ───────────────────────────────────

#[test]
fn raise_then_admin_resolve_refund_business_correct_balances() {
    let f = Fixture::setup();
    let campaign_id = f.create_funded_campaign();
    let creator = f.add_creator_with_proof(campaign_id);

    f.disputes().raise_dispute(
        &f.business,
        &campaign_id,
        &creator,
        &String::from_str(&f.env, "ipfs://work-not-delivered"),
    );
    f.wait_out_evidence_window();

    let tc = f.token_client();
    let business_before = tc.balance(&f.business);
    let escrow_before = tc.balance(&f.escrow_id);

    f.escrow().resolve_dispute(
        &f.admin,
        &campaign_id,
        &creator,
        &ads_bazaar_campaign_escrow::DisputeResolution::RefundBusiness,
    );

    // Full amount goes back to business; no fee on a full refund
    assert_eq!(tc.balance(&f.business), business_before + PAYOUT);
    assert_eq!(tc.balance(&f.escrow_id), escrow_before - PAYOUT);

    let app = f.escrow().get_application(&campaign_id, &creator);
    assert_eq!(app.status, ApplicationStatus::Paid);
    assert!(!app.frozen);
}

// ── 8. raise → admin resolve Split ────────────────────────────────────────────

#[test]
fn raise_then_admin_resolve_split_distributes_proportionally() {
    let f = Fixture::setup();
    let campaign_id = f.create_funded_campaign();
    let creator = f.add_creator_with_proof(campaign_id);

    f.disputes().raise_dispute(
        &creator,
        &campaign_id,
        &creator,
        &String::from_str(&f.env, "ipfs://partial-delivery"),
    );
    f.wait_out_evidence_window();

    let tc = f.token_client();
    let creator_before = tc.balance(&creator);
    let business_before = tc.balance(&f.business);
    let treasury_before = tc.balance(&f.treasury);

    // 60 % to creator (6_000 bps), 40 % to business
    f.escrow().resolve_dispute(
        &f.admin,
        &campaign_id,
        &creator,
        &ads_bazaar_campaign_escrow::DisputeResolution::Split(6_000),
    );

    let creator_gross = PAYOUT * 6_000 / 10_000; // 1_200_000
    let business_share = PAYOUT - creator_gross; // 800_000
    let fee = creator_gross * FEE_BPS / 10_000; // 60_000
    let creator_net = creator_gross - fee; // 1_140_000

    assert_eq!(tc.balance(&creator), creator_before + creator_net);
    assert_eq!(tc.balance(&f.business), business_before + business_share);
    assert_eq!(tc.balance(&f.treasury), treasury_before + fee);
}

// ── 9. Stranger cannot call freeze_for_dispute directly ───────────────────────

#[test]
fn stranger_cannot_call_freeze_for_dispute_directly() {
    let f = Fixture::setup();
    let campaign_id = f.create_funded_campaign();
    let creator = f.add_creator_with_proof(campaign_id);
    let stranger = Address::generate(&f.env);

    let result = f
        .escrow()
        .try_freeze_for_dispute(&stranger, &campaign_id, &creator);

    assert_eq!(result, Err(Ok(EscrowError::Unauthorized)));
    assert!(!f.escrow().get_application(&campaign_id, &creator).frozen);
}

// ── 10. Admin emergency bypass ────────────────────────────────────────────────

#[test]
fn admin_can_freeze_directly_without_going_through_dispute_contract() {
    let f = Fixture::setup();
    let campaign_id = f.create_funded_campaign();
    let creator = f.add_creator_with_proof(campaign_id);

    f.escrow()
        .freeze_for_dispute(&f.admin, &campaign_id, &creator);

    assert!(f.escrow().get_application(&campaign_id, &creator).frozen);
    assert_eq!(
        f.escrow().try_claim_payment(&creator, &campaign_id),
        Err(Ok(EscrowError::PayoutFrozen))
    );
}

// ── 11. Other creators unaffected while one is disputed ───────────────────────

#[test]
fn other_creators_payout_unaffected_while_one_is_disputed() {
    let f = Fixture::setup();
    let campaign_id = f.create_funded_campaign();
    let disputed = f.add_creator_with_proof(campaign_id);
    let clean = f.add_creator_with_proof(campaign_id);
    f.escrow()
        .approve_submission(&f.business, &campaign_id, &clean);

    f.disputes().raise_dispute(
        &disputed,
        &campaign_id,
        &disputed,
        &String::from_str(&f.env, "ipfs://evidence"),
    );

    f.escrow().claim_payment(&clean, &campaign_id);
    assert_eq!(
        f.escrow().get_application(&campaign_id, &clean).status,
        ApplicationStatus::Paid
    );
    assert!(f.escrow().get_application(&campaign_id, &disputed).frozen);
}

// ── 12. Duplicate raise rejected ──────────────────────────────────────────────

#[test]
fn second_raise_over_same_payout_is_rejected() {
    let f = Fixture::setup();
    let campaign_id = f.create_funded_campaign();
    let creator = f.add_creator_with_proof(campaign_id);
    let reason = String::from_str(&f.env, "ipfs://evidence");

    f.disputes()
        .raise_dispute(&creator, &campaign_id, &creator, &reason);

    let result = f
        .disputes()
        .try_raise_dispute(&f.business, &campaign_id, &creator, &reason);
    assert_eq!(
        result,
        Err(Ok(
            ads_bazaar_dispute_resolution::Error::DisputeAlreadyRaised
        ))
    );
    assert!(f.disputes().try_get_dispute(&1).is_err());
}

// ── 13. Raise on already-paid payout rejected ─────────────────────────────────

#[test]
fn raise_dispute_on_paid_payout_is_rejected_and_leaves_no_record() {
    let f = Fixture::setup();
    let campaign_id = f.create_funded_campaign();
    let creator = f.add_creator_with_proof(campaign_id);
    f.escrow()
        .approve_submission(&f.business, &campaign_id, &creator);
    f.escrow().claim_payment(&creator, &campaign_id);

    assert_eq!(
        f.escrow().get_application(&campaign_id, &creator).status,
        ApplicationStatus::Paid
    );

    let result = f.disputes().try_raise_dispute(
        &creator,
        &campaign_id,
        &creator,
        &String::from_str(&f.env, "ipfs://too-late"),
    );
    assert!(result.is_err());
    assert!(f.disputes().try_get_dispute(&0).is_err());
}

// ── 14. Raise in auto-approval window blocks claim ────────────────────────────

#[test]
fn dispute_in_auto_approval_window_blocks_claim() {
    let f = Fixture::setup();
    let campaign_id = f.create_funded_campaign();
    let creator = f.add_creator_with_proof(campaign_id);

    f.env
        .ledger()
        .with_mut(|l| l.timestamp = BASE_TIME + COMPLETION_WINDOW + 1);

    f.disputes().raise_dispute(
        &f.business,
        &campaign_id,
        &creator,
        &String::from_str(&f.env, "ipfs://work-incomplete"),
    );

    assert_eq!(
        f.escrow().try_claim_payment(&creator, &campaign_id),
        Err(Ok(EscrowError::PayoutFrozen))
    );
}

// ── 15. Two creators on same campaign can have independent disputes ────────────

#[test]
fn two_creators_on_same_campaign_can_each_be_disputed_independently() {
    let f = Fixture::setup();
    let campaign_id = f.create_funded_campaign();
    let creator_a = f.add_creator_with_proof(campaign_id);
    let creator_b = f.add_creator_with_proof(campaign_id);
    let reason = String::from_str(&f.env, "ipfs://evidence");

    let id_a = f
        .disputes()
        .raise_dispute(&creator_a, &campaign_id, &creator_a, &reason);
    let id_b = f
        .disputes()
        .raise_dispute(&creator_b, &campaign_id, &creator_b, &reason);

    assert_ne!(id_a, id_b);
    assert_eq!(f.disputes().get_dispute(&id_a).creator, creator_a);
    assert_eq!(f.disputes().get_dispute(&id_b).creator, creator_b);
    assert!(f.escrow().get_application(&campaign_id, &creator_a).frozen);
    assert!(f.escrow().get_application(&campaign_id, &creator_b).frozen);
}

// ── 16. Evidence window blocks an instant admin resolve after raise ───────────

/// A dispute raised through the real cross-contract path cannot be settled by
/// the admin in the same breath — the party that did *not* raise it gets
/// `MIN_EVIDENCE_WINDOW` to answer first.
#[test]
fn admin_cannot_resolve_dispute_immediately_after_cross_contract_raise() {
    let f = Fixture::setup();
    let campaign_id = f.create_funded_campaign();
    let creator = f.add_creator_with_proof(campaign_id);

    f.disputes().raise_dispute(
        &creator,
        &campaign_id,
        &creator,
        &String::from_str(&f.env, "ipfs://evidence"),
    );

    // The cross-contract freeze started the clock at the raise.
    let app = f.escrow().get_application(&campaign_id, &creator);
    assert_eq!(app.dispute_opened_at, Some(BASE_TIME));

    let tc = f.token_client();
    let creator_before = tc.balance(&creator);
    let business_before = tc.balance(&f.business);
    let escrow_before = tc.balance(&f.escrow_id);

    assert_eq!(
        f.escrow().try_resolve_dispute(
            &f.admin,
            &campaign_id,
            &creator,
            &ads_bazaar_campaign_escrow::DisputeResolution::PayCreator,
        ),
        Err(Ok(EscrowError::EvidenceWindowOpen))
    );

    // Nothing moved, and the payout is still frozen and contested.
    assert_eq!(tc.balance(&creator), creator_before);
    assert_eq!(tc.balance(&f.business), business_before);
    assert_eq!(tc.balance(&f.escrow_id), escrow_before);
    let app = f.escrow().get_application(&campaign_id, &creator);
    assert!(app.frozen);
    assert_eq!(app.status, ApplicationStatus::ProofSubmitted);

    // Once the window closes the same call goes through — this is a delay,
    // not a permanent block on the admin path.
    f.wait_out_evidence_window();
    f.escrow().resolve_dispute(
        &f.admin,
        &campaign_id,
        &creator,
        &ads_bazaar_campaign_escrow::DisputeResolution::PayCreator,
    );
    assert_eq!(
        f.escrow().get_application(&campaign_id, &creator).status,
        ApplicationStatus::Paid
    );
}
