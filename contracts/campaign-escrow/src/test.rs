//! Comprehensive test suite for the campaign-escrow contract.
//!
//! Covers the full lifecycle (create → fund → apply → select → submit proof →
//! approve → claim), fee calculation, cancellation, surplus reclaim, expiry,
//! plus auth and deadline enforcement. Helpers live in `test_helpers` so the
//! individual test modules stay focused on assertions.
#![cfg(test)]

mod test_helpers {
    use crate::{CampaignEscrowContract, CampaignEscrowContractClient, PayoutAsset};
    use soroban_sdk::testutils::{Address as _, Ledger as _};
    use soroban_sdk::token::StellarAssetClient;
    use soroban_sdk::{Address, Env, String};

    /// Base ledger timestamp all tests start from (so deadlines are relative
    /// and controllable via `advance_time` / direct assignment).
    pub const BASE_TIME: u64 = 1_000_000;
    /// Amount minted to the business so it can fund campaigns.
    pub const BUSINESS_FUNDS: i128 = 1_000_000_000;

    /// Register the contract with `mock_all_auths` enabled and a fixed base
    /// timestamp. Returns `(env, contract_id)`.
    pub fn setup_env() -> (Env, Address) {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = BASE_TIME);
        let contract_id = env.register(CampaignEscrowContract, ());
        (env, contract_id)
    }

    /// Register a Stellar Asset Contract, mint `amount` to `mint_to`, and
    /// return the token address.
    pub fn setup_token(env: &Env, mint_to: &Address, amount: i128) -> Address {
        let admin = Address::generate(env);
        let token = env.register_stellar_asset_contract_v2(admin);
        let token_address = token.address();
        let sac = StellarAssetClient::new(env, &token_address);
        sac.mint(mint_to, &amount);
        token_address
    }

    /// Advance the ledger timestamp by `seconds`.
    pub fn advance_time(env: &Env, seconds: u64) {
        env.ledger().with_mut(|l| l.timestamp += seconds);
    }

    /// Build a USDC `PayoutAsset` pointing at `token`.
    pub fn usdc(env: &Env, token: &Address) -> PayoutAsset {
        PayoutAsset {
            token: token.clone(),
            symbol: String::from_str(env, "USDC"),
        }
    }

    /// Initialize the contract (admin + dispute contract + fee_bps) and mint
    /// `BUSINESS_FUNDS` to a freshly generated business address. Returns the
    /// client plus the generated identities.
    pub fn bootstrap<'a>(
        env: &'a Env,
        contract_id: &Address,
        fee_bps: i128,
    ) -> (
        CampaignEscrowContractClient<'a>,
        Address,
        Address,
        Address,
        Address,
    ) {
        let client = CampaignEscrowContractClient::new(env, contract_id);
        let admin = Address::generate(env);
        let dispute = Address::generate(env);
        let business = Address::generate(env);
        client.initialize(&admin, &dispute, &fee_bps);
        let token = setup_token(env, &business, BUSINESS_FUNDS);
        (client, admin, dispute, business, token)
    }

    /// Create a campaign and immediately fund it (Draft → Funded), returning
    /// the campaign id.
    pub fn create_funded_campaign(
        env: &Env,
        client: &CampaignEscrowContractClient,
        business: &Address,
        token: &Address,
        total_budget: i128,
        max_creators: u32,
    ) -> u64 {
        let now = env.ledger().timestamp();
        let asset = usdc(env, token);
        let id = client.create_campaign(
            business,
            &asset,
            &total_budget,
            &max_creators,
            &(now + 86_400),
            &(now + 604_800),
            &String::from_str(env, "ipfs://brief"),
        );
        client.fund_campaign(business, &id);
        id
    }
}

mod test_initialize {
    use crate::{CampaignEscrowContractClient, Error};
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::{Address, Env};

    #[test]
    fn initialize_sets_admin_and_fee() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(crate::CampaignEscrowContract, ());
        let client = CampaignEscrowContractClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let dispute = Address::generate(&env);
        client.initialize(&admin, &dispute, &250);
    }

    #[test]
    fn initialize_twice_fails() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(crate::CampaignEscrowContract, ());
        let client = CampaignEscrowContractClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let dispute = Address::generate(&env);
        client.initialize(&admin, &dispute, &250);
        let result = client.try_initialize(&admin, &dispute, &250);
        assert_eq!(result, Err(Ok(Error::AlreadyInitialized)));
    }

    #[test]
    fn initialize_rejects_out_of_range_fee() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(crate::CampaignEscrowContract, ());
        let client = CampaignEscrowContractClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let dispute = Address::generate(&env);
        let result = client.try_initialize(
            &admin,
            &dispute,
            &(ads_bazaar_shared::BASIS_POINTS_DENOMINATOR + 1),
        );
        assert_eq!(result, Err(Ok(Error::FeeTooHigh)));
    }

    #[test]
    fn get_campaign_not_found_before_creation() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(crate::CampaignEscrowContract, ());
        let client = CampaignEscrowContractClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let dispute = Address::generate(&env);
        client.initialize(&admin, &dispute, &250);
        let result = client.try_get_campaign(&0);
        assert_eq!(result, Err(Ok(Error::CampaignNotFound)));
    }
}

mod test_happy_path {
    use super::test_helpers::*;
    use crate::{CampaignEscrowContractClient, Error};
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::token::Client as TokenClient;
    use soroban_sdk::{Address, Env};

    /// Drive a campaign to the point where `creator` has an approved,
    /// payable submission: applied → selected → proof submitted → approved.
    fn run_to_payable(
        env: &Env,
        client: &CampaignEscrowContractClient,
        business: &Address,
        creator: &Address,
        id: &u64,
        payout: i128,
    ) {
        client.apply_to_campaign(creator, id, &soroban_sdk::String::from_str(env, "pitch"));
        client.approve_creator(business, id, creator, &payout);
        client.submit_proof(creator, id, &soroban_sdk::String::from_str(env, "proof"));
        client.approve_submission(business, id, creator);
    }

    #[test]
    fn full_lifecycle() {
        let (env, contract_id) = setup_env();
        let (client, admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let token_client = TokenClient::new(&env, &token);

        let creator = Address::generate(&env);
        let gross: i128 = 1_000_000;
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);

        run_to_payable(&env, &client, &business, &creator, &id, gross);

        let creator_before = token_client.balance(&creator);
        let treasury_before = token_client.balance(&admin);

        client.claim_payment(&creator, &id);

        let fee = gross * 50 / ads_bazaar_shared::BASIS_POINTS_DENOMINATOR;
        let net = gross - fee;
        assert_eq!(token_client.balance(&creator), creator_before + net);
        assert_eq!(token_client.balance(&admin), treasury_before + fee);
    }

    #[test]
    fn approve_two_distinct_creators() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);

        let creator_a = Address::generate(&env);
        let creator_b = Address::generate(&env);

        client.apply_to_campaign(
            &creator_a,
            &id,
            &soroban_sdk::String::from_str(&env, "pitch-a"),
        );
        client.apply_to_campaign(
            &creator_b,
            &id,
            &soroban_sdk::String::from_str(&env, "pitch-b"),
        );

        client.approve_creator(&business, &id, &creator_a, &1_000_000);
        client.approve_creator(&business, &id, &creator_b, &1_000_000);

        let campaign = client.get_campaign(&id);
        assert_eq!(campaign.approved_count, 2);
    }

    #[test]
    fn create_campaign_rejects_non_contract_token() {
        let (env, contract_id) = setup_env();
        let client = CampaignEscrowContractClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let dispute = Address::generate(&env);
        let business = Address::generate(&env);
        client.initialize(&admin, &dispute, &50);

        // A random account address — not a deployed token contract.
        // create_campaign must catch this via try_invoke_contract and return
        // Error::InvalidAsset rather than aborting with a host trap.
        let bogus_token = Address::generate(&env);
        let asset = usdc(&env, &bogus_token);

        let now = env.ledger().timestamp();
        let result = client.try_create_campaign(
            &business,
            &asset,
            &1_000,
            &1,
            &(now + 86_400),
            &(now + 604_800),
            &soroban_sdk::String::from_str(&env, "ipfs://brief"),
        );

        assert_eq!(
            result,
            Err(Ok(Error::InvalidAsset)),
            "expected Error::InvalidAsset for a non-contract token address, got: {:?}",
            result
        );
    }

    #[test]
    fn fee_calculation_50bps() {
        let (env, contract_id) = setup_env();
        let (client, admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let token_client = TokenClient::new(&env, &token);

        let creator = Address::generate(&env);
        let gross: i128 = 2_000_000;
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);

        run_to_payable(&env, &client, &business, &creator, &id, gross);
        client.claim_payment(&creator, &id);

        let fee = gross * 50 / ads_bazaar_shared::BASIS_POINTS_DENOMINATOR;
        let net = gross - fee;
        // creator_net == gross * 0.995
        assert_eq!(net, gross * 995 / 1_000);
        // treasury == gross * 0.005
        assert_eq!(fee, gross * 5 / 1_000);
        assert_eq!(token_client.balance(&creator), net);
        assert_eq!(token_client.balance(&admin), fee);
    }

    #[test]
    fn auto_approve_past_deadline() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let token_client = TokenClient::new(&env, &token);

        let creator = Address::generate(&env);
        let gross: i128 = 1_000_000;
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);

        client.apply_to_campaign(&creator, &id, &soroban_sdk::String::from_str(&env, "pitch"));
        client.approve_creator(&business, &id, &creator, &gross);
        // submit before the deadline (still pending business approval)...
        client.submit_proof(&creator, &id, &soroban_sdk::String::from_str(&env, "proof"));
        // ...then move past the content deadline so it auto-approves.
        advance_time(&env, 604_800 + 10);

        let creator_before = token_client.balance(&creator);
        // Claim without an explicit approve_submission call.
        client.claim_payment(&creator, &id);
        let fee = gross * 50 / ads_bazaar_shared::BASIS_POINTS_DENOMINATOR;
        assert_eq!(token_client.balance(&creator), creator_before + gross - fee);
    }

    #[test]
    fn cancel_open_campaign() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let token_client = TokenClient::new(&env, &token);

        let business_before = token_client.balance(&business);
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);

        client.cancel_campaign(&business, &id);
        // Business balance is fully restored (no commitments outstanding).
        assert_eq!(token_client.balance(&business), business_before);
        assert_eq!(token_client.balance(&contract_id), 0);
    }

    #[test]
    fn reclaim_surplus_after_payouts() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 0);
        let token_client = TokenClient::new(&env, &token);

        let payout: i128 = 1_000_000;
        // max 5, budget covers 5 payouts of `payout`.
        let id = create_funded_campaign(&env, &client, &business, &token, payout * 5, 5);

        let c1 = Address::generate(&env);
        let c2 = Address::generate(&env);
        for c in [&c1, &c2] {
            client.apply_to_campaign(c, &id, &soroban_sdk::String::from_str(&env, "pitch"));
            client.approve_creator(&business, &id, c, &payout);
            client.submit_proof(c, &id, &soroban_sdk::String::from_str(&env, "proof"));
            client.approve_submission(&business, &id, c);
            client.claim_payment(c, &id);
        }

        let business_before = token_client.balance(&business);
        client.reclaim_surplus(&business, &id);
        // Surplus == (5 - 2) * payout == 3 * payout_per_creator.
        assert_eq!(
            token_client.balance(&business),
            business_before + payout * 3
        );
        assert_eq!(token_client.balance(&contract_id), 0);
    }

    #[test]
    fn reject_and_resubmit() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 0);
        let token_client = TokenClient::new(&env, &token);

        let creator = Address::generate(&env);
        let gross: i128 = 1_000_000;
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);

        client.apply_to_campaign(&creator, &id, &soroban_sdk::String::from_str(&env, "pitch"));
        client.approve_creator(&business, &id, &creator, &gross);
        client.submit_proof(
            &creator,
            &id,
            &soroban_sdk::String::from_str(&env, "proof-v1"),
        );
        // Business rejects the proof.
        client.reject_submission(&business, &id, &creator);

        // Verify it was marked as Rejected
        let app = client.get_application(&id, &creator);
        assert_eq!(app.status, ads_bazaar_shared::ApplicationStatus::Rejected);
        // Creator resubmits.
        client.submit_proof(
            &creator,
            &id,
            &soroban_sdk::String::from_str(&env, "proof-v2"),
        );
        client.approve_submission(&business, &id, &creator);

        let creator_before = token_client.balance(&creator);
        client.claim_payment(&creator, &id);
        assert_eq!(token_client.balance(&creator), creator_before + gross);
    }

    #[test]
    fn expire_no_submissions() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let token_client = TokenClient::new(&env, &token);

        let budget: i128 = 10_000_000;
        let id = create_funded_campaign(&env, &client, &business, &token, budget, 5);

        let c1 = Address::generate(&env);
        let c2 = Address::generate(&env);
        // Two creators are selected (committing 1_000_000 each) but never
        // submit proof — their payouts stay reserved against escrow.
        for c in [&c1, &c2] {
            client.apply_to_campaign(c, &id, &soroban_sdk::String::from_str(&env, "pitch"));
            client.approve_creator(&business, &id, c, &1_000_000);
        }

        // Advance past the content deadline.
        advance_time(&env, 604_800 + 10);

        let business_before = token_client.balance(&business);
        client.expire_campaign(&business, &id);
        // Only the unallocated balance (budget - committed) is refunded.
        let committed = 1_000_000 * 2;
        assert_eq!(
            token_client.balance(&business),
            business_before + budget - committed
        );
        // Reserved funds remain in the contract for the selected creators.
        assert_eq!(token_client.balance(&contract_id), committed);
    }

    // --- Fund-safety regression tests: committed payouts must survive
    // cancel / expire / reclaim and remain claimable by the creator. ---

    #[test]
    fn cancel_preserves_committed_payout() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 0);
        let token_client = TokenClient::new(&env, &token);

        let payout: i128 = 1_000_000;
        let budget: i128 = payout * 5;
        let id = create_funded_campaign(&env, &client, &business, &token, budget, 5);

        let creator = Address::generate(&env);
        run_to_payable(&env, &client, &business, &creator, &id, payout);

        // Business cancels before the creator claims.
        let business_before = token_client.balance(&business);
        client.cancel_campaign(&business, &id);
        // Business recovers only the unallocated (budget - payout) portion.
        assert_eq!(
            token_client.balance(&business),
            business_before + budget - payout
        );
        // The committed payout is still held by the contract.
        assert_eq!(token_client.balance(&contract_id), payout);

        // The approved creator can still claim their payout after cancellation.
        let creator_before = token_client.balance(&creator);
        client.claim_payment(&creator, &id);
        assert_eq!(token_client.balance(&creator), creator_before + payout);
        assert_eq!(token_client.balance(&contract_id), 0);
    }

    #[test]
    fn expire_preserves_committed_payout() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 0);
        let token_client = TokenClient::new(&env, &token);

        let payout: i128 = 1_000_000;
        let budget: i128 = payout * 5;
        let id = create_funded_campaign(&env, &client, &business, &token, budget, 5);

        let creator = Address::generate(&env);
        run_to_payable(&env, &client, &business, &creator, &id, payout);

        // Advance past the content deadline, then expire.
        advance_time(&env, 604_800 + 10);
        let business_before = token_client.balance(&business);
        client.expire_campaign(&business, &id);
        assert_eq!(
            token_client.balance(&business),
            business_before + budget - payout
        );
        assert_eq!(token_client.balance(&contract_id), payout);

        // Creator still gets paid after expiry.
        let creator_before = token_client.balance(&creator);
        client.claim_payment(&creator, &id);
        assert_eq!(token_client.balance(&creator), creator_before + payout);
        assert_eq!(token_client.balance(&contract_id), 0);
    }

    #[test]
    fn reclaim_preserves_committed_payout() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 0);
        let token_client = TokenClient::new(&env, &token);

        let payout: i128 = 1_000_000;
        let budget: i128 = payout * 5;
        let id = create_funded_campaign(&env, &client, &business, &token, budget, 5);

        let creator = Address::generate(&env);
        run_to_payable(&env, &client, &business, &creator, &id, payout);

        // Business reclaims surplus before the creator claims.
        let business_before = token_client.balance(&business);
        client.reclaim_surplus(&business, &id);
        assert_eq!(
            token_client.balance(&business),
            business_before + budget - payout
        );
        assert_eq!(token_client.balance(&contract_id), payout);

        // Creator still gets paid after the reclaim.
        let creator_before = token_client.balance(&creator);
        client.claim_payment(&creator, &id);
        assert_eq!(token_client.balance(&creator), creator_before + payout);
        assert_eq!(token_client.balance(&contract_id), 0);
    }

    #[test]
    fn reclaim_surplus_with_committed_payouts_leaves_campaign_active() {
        let (env, contract_id) = setup_env();
        let (client, admin, _dispute, business, token) = bootstrap(&env, &contract_id, 0);
        let token_client = TokenClient::new(&env, &token);

        let payout: i128 = 1_000_000;
        let budget: i128 = payout * 5;
        let id = create_funded_campaign(&env, &client, &business, &token, budget, 5);

        let creator = Address::generate(&env);
        // Apply and get selected, but not paid
        client.apply_to_campaign(&creator, &id, &soroban_sdk::String::from_str(&env, "pitch"));
        client.approve_creator(&business, &id, &creator, &payout);

        // Business reclaims surplus
        client.reclaim_surplus(&business, &id);

        // Status should be Active (not Completed) because committed payouts remain
        let campaign = client.get_campaign(&id);
        assert_eq!(campaign.status, ads_bazaar_shared::CampaignStatus::Active);

        // Dispute functions should still work.
        client.freeze_for_dispute(&admin, &id, &creator);

        let app = client.get_application(&id, &creator);
        assert!(app.frozen);

        // resolve_dispute requires the evidence window to have elapsed
        // since the freeze (see MIN_EVIDENCE_WINDOW).
        advance_time(&env, crate::MIN_EVIDENCE_WINDOW);

        client.resolve_dispute(&admin, &id, &creator, &crate::DisputeResolution::PayCreator);

        // Payout should have reached creator
        assert_eq!(token_client.balance(&creator), payout);
    }

    #[test]
    fn emergency_recover_sweeps_unallocated_to_treasury() {
        let (env, contract_id) = setup_env();
        let (client, admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let token_client = TokenClient::new(&env, &token);

        let budget: i128 = 10_000_000;
        let id = create_funded_campaign(&env, &client, &business, &token, budget, 5);

        // Long past both the content deadline and the emergency-recovery
        // grace period — the business is treated as unreachable.
        advance_time(&env, 604_800 + crate::EMERGENCY_RECOVERY_GRACE_PERIOD + 10);

        // Treasury defaults to `admin` at `initialize`.
        let treasury_before = token_client.balance(&admin);
        let business_before = token_client.balance(&business);
        client.emergency_recover_campaign(&admin, &id);

        // Nothing was ever committed, so the whole budget is recovered —
        // and it goes to treasury, not back to the unreachable business.
        assert_eq!(token_client.balance(&admin), treasury_before + budget);
        assert_eq!(token_client.balance(&business), business_before);
        assert_eq!(token_client.balance(&contract_id), 0);

        let campaign = client.get_campaign(&id);
        assert_eq!(
            campaign.status,
            ads_bazaar_shared::CampaignStatus::Cancelled
        );
    }

    #[test]
    fn emergency_recover_preserves_committed_payout() {
        let (env, contract_id) = setup_env();
        let (client, admin, _dispute, business, token) = bootstrap(&env, &contract_id, 0);
        let token_client = TokenClient::new(&env, &token);

        let payout: i128 = 1_000_000;
        let budget: i128 = payout * 5;
        let id = create_funded_campaign(&env, &client, &business, &token, budget, 5);

        let creator = Address::generate(&env);
        run_to_payable(&env, &client, &business, &creator, &id, payout);

        advance_time(&env, 604_800 + crate::EMERGENCY_RECOVERY_GRACE_PERIOD + 10);

        let treasury_before = token_client.balance(&admin);
        client.emergency_recover_campaign(&admin, &id);
        // Only the unallocated (budget - payout) portion goes to treasury.
        assert_eq!(
            token_client.balance(&admin),
            treasury_before + budget - payout
        );
        assert_eq!(token_client.balance(&contract_id), payout);

        // The approved creator can still claim their payout afterward —
        // exactly as with cancel_campaign/expire_campaign/reclaim_surplus.
        let creator_before = token_client.balance(&creator);
        client.claim_payment(&creator, &id);
        assert_eq!(token_client.balance(&creator), creator_before + payout);
        assert_eq!(token_client.balance(&contract_id), 0);
    }

    #[test]
    fn emergency_recover_rejects_already_cancelled_campaign() {
        let (env, contract_id) = setup_env();
        let (client, admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);
        client.cancel_campaign(&business, &id);

        advance_time(&env, 604_800 + crate::EMERGENCY_RECOVERY_GRACE_PERIOD + 10);
        let result = client.try_emergency_recover_campaign(&admin, &id);
        assert_eq!(result, Err(Ok(Error::InvalidStatus)));
    }
}

mod test_protocol_config {
    use super::test_helpers::setup_env;
    use crate::{CampaignEscrowContractClient, Error};
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::Address;

    #[test]
    fn get_protocol_config_returns_current_fee_bps() {
        let (env, contract_id) = setup_env();
        let client = CampaignEscrowContractClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let dispute_contract = Address::generate(&env);
        client.initialize(&admin, &dispute_contract, &150);

        let config = client.get_protocol_config();
        assert_eq!(config.fee_bps, 150);
        assert_eq!(config.admin, admin);
        // treasury defaults to admin — see the comment on `initialize` in lib.rs
        assert_eq!(config.treasury, admin);
    }

    #[test]
    fn get_protocol_config_fails_before_initialization() {
        let (env, contract_id) = setup_env();
        let client = CampaignEscrowContractClient::new(&env, &contract_id);

        let result = client.try_get_protocol_config();
        assert_eq!(result, Err(Ok(Error::NotInitialized)));
    }
}

mod test_admin_transfer {
    use super::test_helpers::setup_env;
    use crate::{storage, CampaignEscrowContractClient, Error};
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::Address;

    #[test]
    fn propose_admin_rejects_non_admin() {
        let (env, contract_id) = setup_env();
        let client = CampaignEscrowContractClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let dispute_contract = Address::generate(&env);
        let not_admin = Address::generate(&env);
        let new_admin = Address::generate(&env);
        client.initialize(&admin, &dispute_contract, &250);

        let result = client.try_propose_admin(&not_admin, &new_admin);

        assert_eq!(result, Err(Ok(Error::Unauthorized)));
    }

    #[test]
    fn propose_admin_stores_pending_candidate() {
        let (env, contract_id) = setup_env();
        let client = CampaignEscrowContractClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let dispute_contract = Address::generate(&env);
        let new_admin = Address::generate(&env);
        client.initialize(&admin, &dispute_contract, &250);

        client.propose_admin(&admin, &new_admin);

        let pending_admin = env.as_contract(&client.address, || storage::get_pending_admin(&env));
        assert_eq!(pending_admin, Some(new_admin));
    }

    #[test]
    fn full_two_step_transfer_replaces_admin() {
        let (env, contract_id) = setup_env();
        let client = CampaignEscrowContractClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let dispute_contract = Address::generate(&env);
        let new_admin = Address::generate(&env);
        client.initialize(&admin, &dispute_contract, &250);

        client.propose_admin(&admin, &new_admin);
        client.accept_admin(&new_admin);

        let pending_admin = env.as_contract(&client.address, || storage::get_pending_admin(&env));
        assert_eq!(pending_admin, None);
        let stored_admin = env.as_contract(&client.address, || storage::get_admin(&env));
        assert_eq!(stored_admin, Ok(new_admin.clone()));

        let old_admin_result = client.try_update_fee_bps(&admin, &100);
        assert_eq!(old_admin_result, Err(Ok(Error::Unauthorized)));

        client.update_fee_bps(&new_admin, &100);
        assert_eq!(client.get_protocol_config().fee_bps, 100);
    }

    #[test]
    fn accept_admin_rejects_wrong_address() {
        let (env, contract_id) = setup_env();
        let client = CampaignEscrowContractClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let dispute_contract = Address::generate(&env);
        let new_admin = Address::generate(&env);
        let wrong_admin = Address::generate(&env);
        client.initialize(&admin, &dispute_contract, &250);

        client.propose_admin(&admin, &new_admin);
        let result = client.try_accept_admin(&wrong_admin);

        assert_eq!(result, Err(Ok(Error::Unauthorized)));
    }

    #[test]
    fn accept_admin_before_propose_is_unauthorized() {
        let (env, contract_id) = setup_env();
        let client = CampaignEscrowContractClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let dispute_contract = Address::generate(&env);
        let new_admin = Address::generate(&env);
        client.initialize(&admin, &dispute_contract, &250);

        let result = client.try_accept_admin(&new_admin);

        assert_eq!(result, Err(Ok(Error::Unauthorized)));
    }
}

mod test_auth_failures {
    use super::test_helpers::*;
    use crate::Error;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::{Address, String};

    #[test]
    fn non_owner_cancel() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);

        let stranger = Address::generate(&env);
        let result = client.try_cancel_campaign(&stranger, &id);
        assert_eq!(result, Err(Ok(Error::NotCampaignOwner)));
    }

    #[test]
    fn non_owner_select_creator() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);

        let creator = Address::generate(&env);
        let stranger = Address::generate(&env);
        let result = client.try_approve_creator(&stranger, &id, &creator, &1_000_000);
        assert_eq!(result, Err(Ok(Error::NotCampaignOwner)));
    }

    #[test]
    fn non_owner_approve_submission() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);

        let creator = Address::generate(&env);
        client.apply_to_campaign(&creator, &id, &String::from_str(&env, "pitch"));
        client.approve_creator(&business, &id, &creator, &1_000_000);
        client.submit_proof(&creator, &id, &String::from_str(&env, "proof"));

        let stranger = Address::generate(&env);
        let result = client.try_approve_submission(&stranger, &id, &creator);
        assert_eq!(result, Err(Ok(Error::NotCampaignOwner)));
    }

    #[test]
    fn creator_claim_before_approval() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);

        let creator = Address::generate(&env);
        client.apply_to_campaign(&creator, &id, &String::from_str(&env, "pitch"));
        client.approve_creator(&business, &id, &creator, &1_000_000);
        client.submit_proof(&creator, &id, &String::from_str(&env, "proof"));

        // Proof is submitted but not yet approved by the business.
        let result = client.try_claim_payment(&creator, &id);
        assert_eq!(result, Err(Ok(Error::SubmissionNotPayable)));
    }

    #[test]
    fn double_apply_same_creator() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);

        let creator = Address::generate(&env);
        client.apply_to_campaign(&creator, &id, &String::from_str(&env, "pitch"));
        let result = client.try_apply_to_campaign(&creator, &id, &String::from_str(&env, "pitch2"));
        assert_eq!(result, Err(Ok(Error::AlreadyApplied)));
    }

    #[test]
    fn double_select_same_creator() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);

        let creator = Address::generate(&env);
        client.apply_to_campaign(&creator, &id, &String::from_str(&env, "pitch"));
        client.approve_creator(&business, &id, &creator, &1_000_000);
        let result = client.try_approve_creator(&business, &id, &creator, &1_000_000);
        assert_eq!(result, Err(Ok(Error::AlreadySelected)));
    }

    #[test]
    fn non_admin_cannot_emergency_recover() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);

        advance_time(&env, 604_800 + crate::EMERGENCY_RECOVERY_GRACE_PERIOD + 10);

        let stranger = Address::generate(&env);
        let result = client.try_emergency_recover_campaign(&stranger, &id);
        assert_eq!(result, Err(Ok(Error::Unauthorized)));
    }
}

mod test_deadline_enforcement {
    use super::test_helpers::*;
    use crate::Error;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::{Address, String};

    #[test]
    fn apply_after_application_deadline() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);

        let now = env.ledger().timestamp();
        let asset = usdc(&env, &token);
        let id = client.create_campaign(
            &business,
            &asset,
            &10_000_000,
            &5,
            &(now + 86_400),
            &(now + 604_800),
            &String::from_str(&env, "ipfs://brief"),
        );
        client.fund_campaign(&business, &id);

        // Move past the application deadline.
        advance_time(&env, 86_400 + 10);

        let creator = Address::generate(&env);
        let result = client.try_apply_to_campaign(&creator, &id, &String::from_str(&env, "pitch"));
        assert_eq!(result, Err(Ok(Error::ApplicationDeadlinePassed)));
    }

    #[test]
    fn submit_after_content_deadline() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);

        let creator = Address::generate(&env);
        client.apply_to_campaign(&creator, &id, &String::from_str(&env, "pitch"));
        client.approve_creator(&business, &id, &creator, &1_000_000);

        // Move past the content deadline.
        advance_time(&env, 604_800 + 10);

        let result = client.try_submit_proof(&creator, &id, &String::from_str(&env, "proof"));
        assert_eq!(result, Err(Ok(Error::ContentDeadlinePassed)));
    }

    #[test]
    fn reject_after_content_deadline_cannot_defeat_auto_approval() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);

        let creator = Address::generate(&env);
        client.apply_to_campaign(&creator, &id, &String::from_str(&env, "pitch"));
        client.approve_creator(&business, &id, &creator, &1_000_000);
        client.submit_proof(&creator, &id, &String::from_str(&env, "proof"));

        // Move past the content deadline: the submission is now auto-approved.
        advance_time(&env, 604_800 + 10);

        let result = client.try_reject_submission(&business, &id, &creator);
        assert_eq!(result, Err(Ok(Error::ContentDeadlinePassed)));

        // Auto-approval still lets the creator claim.
        let result = client.try_claim_payment(&creator, &id);
        assert!(result.is_ok());
    }

    #[test]
    fn create_with_past_deadline() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);

        let now = env.ledger().timestamp();
        let asset = usdc(&env, &token);
        let result = client.try_create_campaign(
            &business,
            &asset,
            &10_000_000,
            &5,
            &(now - 100),
            &(now + 604_800),
            &String::from_str(&env, "ipfs://brief"),
        );
        assert_eq!(result, Err(Ok(Error::DeadlineInPast)));
    }

    #[test]
    fn create_with_equal_deadlines() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);

        let now = env.ledger().timestamp();
        let deadline = now + 300;
        let asset = usdc(&env, &token);
        let result = client.try_create_campaign(
            &business,
            &asset,
            &10_000_000,
            &5,
            &deadline,
            &deadline,
            &String::from_str(&env, "ipfs://brief"),
        );
        assert_eq!(result, Err(Ok(Error::InvalidDeadlineOrder)));
    }

    #[test]
    fn expire_before_deadline() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);

        // Still before the content deadline.
        let result = client.try_expire_campaign(&business, &id);
        assert_eq!(result, Err(Ok(Error::DeadlineNotReached)));
    }

    #[test]
    fn emergency_recover_before_grace_period_fails() {
        let (env, contract_id) = setup_env();
        let (client, admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);

        // Not reachable at all before the content deadline.
        let result = client.try_emergency_recover_campaign(&admin, &id);
        assert_eq!(result, Err(Ok(Error::DeadlineNotReached)));

        // Past the content deadline — enough for `expire_campaign` — but
        // nowhere near the much longer emergency-recovery grace period.
        advance_time(&env, 604_800 + 10);
        let result = client.try_emergency_recover_campaign(&admin, &id);
        assert_eq!(result, Err(Ok(Error::DeadlineNotReached)));
    }
}

mod test_error_variants {
    use super::test_helpers::*;
    use crate::{CampaignEscrowContractClient, Error};
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::{Address, Env, String};

    #[test]
    fn invalid_creator_count() {
        let (env, _contract_id) = setup_env();
        let (client, _admin, _dispute, _business, _token) = bootstrap(&env, &_contract_id, 50);

        let business = Address::generate(&env);
        let token = setup_token(&env, &business, 1_000_000);
        let asset = usdc(&env, &token);

        let now = env.ledger().timestamp();
        let result = client.try_create_campaign(
            &business,
            &asset,
            &1_000_000,
            &0,
            &(now + 86_400),
            &(now + 604_800),
            &String::from_str(&env, "ipfs://brief"),
        );
        assert_eq!(result, Err(Ok(Error::InvalidCreatorCount)));
    }

    #[test]
    fn selection_limit_reached() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 0);
        let id = create_funded_campaign(&env, &client, &business, &token, 2_000_000, 1);

        let c1 = Address::generate(&env);
        let c2 = Address::generate(&env);
        client.apply_to_campaign(&c1, &id, &String::from_str(&env, "pitch1"));
        client.approve_creator(&business, &id, &c1, &1_000_000);

        client.apply_to_campaign(&c2, &id, &String::from_str(&env, "pitch2"));
        let result = client.try_approve_creator(&business, &id, &c2, &1);
        assert_eq!(result, Err(Ok(Error::MaxCreatorsReached)));
    }

    #[test]
    fn budget_below_obligations() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 0);
        let id = create_funded_campaign(&env, &client, &business, &token, 5_000_000, 5);

        let c1 = Address::generate(&env);
        client.apply_to_campaign(&c1, &id, &String::from_str(&env, "pitch1"));
        client.approve_creator(&business, &id, &c1, &4_000_000);

        let c2 = Address::generate(&env);
        client.apply_to_campaign(&c2, &id, &String::from_str(&env, "pitch2"));
        let result = client.try_approve_creator(&business, &id, &c2, &2_000_000);
        assert_eq!(result, Err(Ok(Error::InsufficientEscrowBalance)));
    }

    #[test]
    fn fee_too_high() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(crate::CampaignEscrowContract, ());
        let client = CampaignEscrowContractClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let dispute = Address::generate(&env);
        let result = client.try_initialize(
            &admin,
            &dispute,
            &(ads_bazaar_shared::BASIS_POINTS_DENOMINATOR + 1),
        );
        assert_eq!(result, Err(Ok(Error::FeeTooHigh)));
    }

    #[test]
    fn invalid_deadline_order() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);

        let now = env.ledger().timestamp();
        let deadline = now + 300;
        let asset = usdc(&env, &token);
        let result = client.try_create_campaign(
            &business,
            &asset,
            &10_000_000,
            &5,
            &deadline,
            &deadline,
            &String::from_str(&env, "ipfs://brief"),
        );
        assert_eq!(result, Err(Ok(Error::InvalidDeadlineOrder)));
    }

    #[test]
    fn deadline_in_past() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);

        let now = env.ledger().timestamp();
        let asset = usdc(&env, &token);
        let result = client.try_create_campaign(
            &business,
            &asset,
            &10_000_000,
            &5,
            &(now - 100),
            &(now + 604_800),
            &String::from_str(&env, "ipfs://brief"),
        );
        assert_eq!(result, Err(Ok(Error::DeadlineInPast)));
    }

    #[test]
    fn application_deadline_passed() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);

        let now = env.ledger().timestamp();
        let asset = usdc(&env, &token);
        let id = client.create_campaign(
            &business,
            &asset,
            &10_000_000,
            &5,
            &(now + 86_400),
            &(now + 604_800),
            &String::from_str(&env, "ipfs://brief"),
        );
        client.fund_campaign(&business, &id);

        advance_time(&env, 86_400 + 10);

        let creator = Address::generate(&env);
        let result = client.try_apply_to_campaign(&creator, &id, &String::from_str(&env, "pitch"));
        assert_eq!(result, Err(Ok(Error::ApplicationDeadlinePassed)));
    }

    #[test]
    fn content_deadline_passed() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);

        let creator = Address::generate(&env);
        client.apply_to_campaign(&creator, &id, &String::from_str(&env, "pitch"));
        client.approve_creator(&business, &id, &creator, &1_000_000);

        advance_time(&env, 604_800 + 10);

        let result = client.try_submit_proof(&creator, &id, &String::from_str(&env, "proof"));
        assert_eq!(result, Err(Ok(Error::ContentDeadlinePassed)));
    }
}

mod test_version_upgrade {
    use super::test_helpers::*;
    use crate::Error;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::{Address, BytesN, String};

    #[test]
    fn version_returns_initial_version_after_initialize() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, _business, _token) = bootstrap(&env, &contract_id, 250);
        assert_eq!(client.version(), String::from_str(&env, "0.1.0"));
    }

    #[test]
    fn version_fails_before_initialization() {
        let (env, contract_id) = setup_env();
        let client = crate::CampaignEscrowContractClient::new(&env, &contract_id);
        let result = client.try_version();
        assert_eq!(result, Err(Ok(Error::NotInitialized)));
    }

    #[test]
    fn upgrade_rejects_non_admin() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, _business, _token) = bootstrap(&env, &contract_id, 250);

        let not_admin = Address::generate(&env);
        let new_wasm_hash = BytesN::from_array(&env, &[7u8; 32]);
        let result = client.try_upgrade(&not_admin, &new_wasm_hash);
        assert_eq!(result, Err(Ok(Error::Unauthorized)));
    }
}

mod test_pause {
    use super::test_helpers::*;
    use crate::Error;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::{Address, String};

    #[test]
    fn pause_unpause_toggles_is_paused() {
        let (env, contract_id) = setup_env();
        let (client, admin, _dispute, _business, _token) = bootstrap(&env, &contract_id, 250);

        assert!(!client.is_paused());
        client.pause(&admin);
        assert!(client.is_paused());
        client.unpause(&admin);
        assert!(!client.is_paused());
    }

    #[test]
    fn non_admin_cannot_pause() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, _business, _token) = bootstrap(&env, &contract_id, 250);

        let not_admin = Address::generate(&env);
        let result = client.try_pause(&not_admin);
        assert_eq!(result, Err(Ok(Error::Unauthorized)));
    }

    #[test]
    fn non_admin_cannot_unpause() {
        let (env, contract_id) = setup_env();
        let (client, admin, _dispute, _business, _token) = bootstrap(&env, &contract_id, 250);
        client.pause(&admin);

        let not_admin = Address::generate(&env);
        let result = client.try_unpause(&not_admin);
        assert_eq!(result, Err(Ok(Error::Unauthorized)));
    }

    #[test]
    fn pause_blocks_apply_to_campaign() {
        let (env, contract_id) = setup_env();
        let (client, admin, _dispute, business, token) = bootstrap(&env, &contract_id, 250);
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);
        client.pause(&admin);

        let creator = Address::generate(&env);
        let result = client.try_apply_to_campaign(&creator, &id, &String::from_str(&env, "pitch"));
        assert_eq!(result, Err(Ok(Error::ContractPaused)));
    }

    #[test]
    fn unpause_allows_apply_to_campaign() {
        let (env, contract_id) = setup_env();
        let (client, admin, _dispute, business, token) = bootstrap(&env, &contract_id, 250);
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);
        client.pause(&admin);
        client.unpause(&admin);

        let creator = Address::generate(&env);
        client.apply_to_campaign(&creator, &id, &String::from_str(&env, "pitch"));
        let application = client.get_application(&id, &creator);
        assert_eq!(
            application.status,
            ads_bazaar_shared::ApplicationStatus::Pending
        );
    }

    #[test]
    fn view_functions_readable_while_paused() {
        let (env, contract_id) = setup_env();
        let (client, admin, _dispute, business, token) = bootstrap(&env, &contract_id, 250);
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);
        client.pause(&admin);

        let config = client.get_protocol_config();
        assert_eq!(config.admin, admin);

        let campaign = client.get_campaign(&id);
        assert_eq!(campaign.id, id);

        assert!(client.is_paused());
    }
}

mod admin_updates {
    use super::test_helpers::*;
    use crate::Error;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::token::Client as TokenClient;
    use soroban_sdk::Address;

    #[test]
    fn update_fee_and_treasury() {
        let (env, contract_id) = setup_env();
        let (client, admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let token_client = TokenClient::new(&env, &token);

        let creator = Address::generate(&env);
        let gross: i128 = 1_000_000;
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);

        // Update fee from 50 to 200 bps
        client.update_fee_bps(&admin, &200);

        // Update treasury
        let new_treasury = Address::generate(&env);
        client.update_treasury(&admin, &new_treasury);

        // Verify config updated — this reflects the live/global value used
        // by future campaigns, not this already-created one.
        let config = client.get_protocol_config();
        assert_eq!(config.fee_bps, 200);
        assert_eq!(config.treasury, new_treasury);
        assert_eq!(client.get_campaign(&id).fee_bps, 50);

        // Run through to claim
        client.apply_to_campaign(&creator, &id, &soroban_sdk::String::from_str(&env, "pitch"));
        client.approve_creator(&business, &id, &creator, &gross);
        client.submit_proof(&creator, &id, &soroban_sdk::String::from_str(&env, "proof"));
        client.approve_submission(&business, &id, &creator);

        let creator_before = token_client.balance(&creator);
        let treasury_before = token_client.balance(&new_treasury);

        client.claim_payment(&creator, &id);

        let creator_after = token_client.balance(&creator);
        let treasury_after = token_client.balance(&new_treasury);

        // The payout uses the 50 bps snapshotted at creation, not the 200
        // bps the fee was later updated to — but the fee still lands at the
        // *new* treasury address, since treasury isn't snapshotted per campaign.
        let expected_fee = (gross * 50) / 10_000;
        let expected_net = gross - expected_fee;

        assert_eq!(treasury_after - treasury_before, expected_fee);
        assert_eq!(creator_after - creator_before, expected_net);
    }

    #[test]
    fn new_campaign_uses_updated_fee() {
        let (env, contract_id) = setup_env();
        let (client, admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let token_client = TokenClient::new(&env, &token);

        client.update_fee_bps(&admin, &200);

        let creator = Address::generate(&env);
        let gross: i128 = 1_000_000;
        // Created after the update — should snapshot 200 bps, not 50.
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);
        assert_eq!(client.get_campaign(&id).fee_bps, 200);

        client.apply_to_campaign(&creator, &id, &soroban_sdk::String::from_str(&env, "pitch"));
        client.approve_creator(&business, &id, &creator, &gross);
        client.submit_proof(&creator, &id, &soroban_sdk::String::from_str(&env, "proof"));
        client.approve_submission(&business, &id, &creator);

        let creator_before = token_client.balance(&creator);
        client.claim_payment(&creator, &id);
        let creator_after = token_client.balance(&creator);

        let expected_fee = (gross * 200) / 10_000;
        assert_eq!(creator_after - creator_before, gross - expected_fee);
    }

    #[test]
    fn update_fee_unauthorized() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, _business, _token) = bootstrap(&env, &contract_id, 50);
        let unauthorized = Address::generate(&env);

        let result = client.try_update_fee_bps(&unauthorized, &200);
        assert_eq!(result, Err(Ok(Error::Unauthorized)));
    }

    #[test]
    fn update_fee_too_high() {
        let (env, contract_id) = setup_env();
        let (client, admin, _dispute, _business, _token) = bootstrap(&env, &contract_id, 50);

        let result = client.try_update_fee_bps(&admin, &1_001);
        assert_eq!(result, Err(Ok(Error::FeeTooHigh)));
    }
}

mod test_update_metadata {
    use super::test_helpers::*;
    use crate::Error;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::{Address, String};

    #[test]
    fn update_metadata_success() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);

        let now = env.ledger().timestamp();
        let asset = usdc(&env, &token);
        let id = client.create_campaign(
            &business,
            &asset,
            &10_000_000,
            &5,
            &(now + 86_400),
            &(now + 604_800),
            &String::from_str(&env, "ipfs://original-brief"),
        );

        client.update_campaign_metadata(
            &id,
            &business,
            &String::from_str(&env, "ipfs://updated-brief"),
        );

        let campaign = client.get_campaign(&id);
        assert_eq!(
            campaign.metadata_uri,
            String::from_str(&env, "ipfs://updated-brief")
        );
    }

    #[test]
    fn update_metadata_after_funding() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);

        // Create and fund — still zero applicants, so metadata update
        // should succeed when status is Funded.
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);

        client.update_campaign_metadata(
            &id,
            &business,
            &String::from_str(&env, "ipfs://updated-brief"),
        );

        let campaign = client.get_campaign(&id);
        assert_eq!(
            campaign.metadata_uri,
            String::from_str(&env, "ipfs://updated-brief")
        );
    }

    #[test]
    fn not_campaign_owner_cannot_update_metadata() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);

        let now = env.ledger().timestamp();
        let asset = usdc(&env, &token);
        let id = client.create_campaign(
            &business,
            &asset,
            &10_000_000,
            &5,
            &(now + 86_400),
            &(now + 604_800),
            &String::from_str(&env, "ipfs://original-brief"),
        );

        let stranger = Address::generate(&env);
        let result = client.try_update_campaign_metadata(
            &id,
            &stranger,
            &String::from_str(&env, "ipfs://hijacked-brief"),
        );
        assert_eq!(result, Err(Ok(Error::NotCampaignOwner)));
    }

    #[test]
    fn applications_exist_blocks_metadata_update() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);

        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);

        let creator = Address::generate(&env);
        client.apply_to_campaign(&creator, &id, &String::from_str(&env, "pitch"));

        let result = client.try_update_campaign_metadata(
            &id,
            &business,
            &String::from_str(&env, "ipfs://updated-brief"),
        );
        assert_eq!(result, Err(Ok(Error::ApplicationsExist)));
    }

    #[test]
    fn empty_metadata_rejected() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);

        let now = env.ledger().timestamp();
        let asset = usdc(&env, &token);
        let id = client.create_campaign(
            &business,
            &asset,
            &10_000_000,
            &5,
            &(now + 86_400),
            &(now + 604_800),
            &String::from_str(&env, "ipfs://original-brief"),
        );

        let result =
            client.try_update_campaign_metadata(&id, &business, &String::from_str(&env, ""));
        assert_eq!(result, Err(Ok(Error::InvalidMetadata)));
    }

    #[test]
    fn cancelled_campaign_rejects_metadata_update() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);

        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);
        client.cancel_campaign(&business, &id);

        let result = client.try_update_campaign_metadata(
            &id,
            &business,
            &String::from_str(&env, "ipfs://updated-brief"),
        );
        assert_eq!(result, Err(Ok(Error::InvalidStatus)));
    }

    #[test]
    fn metadata_not_changed_on_failure() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);

        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);

        let creator = Address::generate(&env);
        client.apply_to_campaign(&creator, &id, &String::from_str(&env, "pitch"));

        // Attempt update fails because an applicant exists.
        let _ = client.try_update_campaign_metadata(
            &id,
            &business,
            &String::from_str(&env, "ipfs://should-not-persist"),
        );

        // Metadata must still be the original.
        let campaign = client.get_campaign(&id);
        assert_eq!(
            campaign.metadata_uri,
            String::from_str(&env, "ipfs://brief")
        );
    }

    #[test]
    fn metadata_update_blocked_when_paused() {
        let (env, contract_id) = setup_env();
        let (client, admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);

        let now = env.ledger().timestamp();
        let asset = usdc(&env, &token);
        let id = client.create_campaign(
            &business,
            &asset,
            &10_000_000,
            &5,
            &(now + 86_400),
            &(now + 604_800),
            &String::from_str(&env, "ipfs://original-brief"),
        );

        client.pause(&admin);

        let result = client.try_update_campaign_metadata(
            &id,
            &business,
            &String::from_str(&env, "ipfs://updated-brief"),
        );
        assert_eq!(result, Err(Ok(Error::ContractPaused)));
    }

    #[test]
    fn completed_campaign_rejects_metadata_update() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 0);
        let _token_client = soroban_sdk::token::Client::new(&env, &token);

        let payout: i128 = 1_000_000;
        let id = create_funded_campaign(&env, &client, &business, &token, payout, 1);

        let creator = Address::generate(&env);
        client.apply_to_campaign(&creator, &id, &String::from_str(&env, "pitch"));
        client.approve_creator(&business, &id, &creator, &payout);
        client.submit_proof(&creator, &id, &String::from_str(&env, "proof"));
        client.approve_submission(&business, &id, &creator);
        client.claim_payment(&creator, &id);

        // Campaign is now Completed — metadata update should be rejected.
        let result = client.try_update_campaign_metadata(
            &id,
            &business,
            &String::from_str(&env, "ipfs://updated-brief"),
        );
        assert_eq!(result, Err(Ok(Error::InvalidStatus)));
    }

    /// Applying to a campaign must stay O(1) in storage-write cost
    /// regardless of how many creators already applied — the applicant
    /// tracking is a counter, not a growing list. Apply with a large number
    /// of prior applicants, then confirm the write cost of a later apply is
    /// no larger than an early one, and that the lock-after-first-apply
    /// behavior from `applications_exist_blocks_metadata_update` still holds.
    #[test]
    fn applying_with_many_prior_applicants_does_not_regress_write_cost() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);

        // max_creators is a cap on approved creators, not applicants, so a
        // low cap here doesn't limit how many creators can apply.
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 1);

        let first_creator = Address::generate(&env);
        client.apply_to_campaign(&first_creator, &id, &String::from_str(&env, "pitch"));
        let first_apply_write_bytes = env.cost_estimate().resources().write_bytes;

        // A large number of additional creators apply to the same campaign.
        const N: u32 = 200;
        for _ in 0..N {
            let creator = Address::generate(&env);
            client.apply_to_campaign(&creator, &id, &String::from_str(&env, "pitch"));
        }

        let last_creator = Address::generate(&env);
        client.apply_to_campaign(&last_creator, &id, &String::from_str(&env, "pitch"));
        let last_apply_write_bytes = env.cost_estimate().resources().write_bytes;

        // The write cost of applying must not grow with the number of prior
        // applicants — an ever-growing Vec would regress this.
        assert_eq!(first_apply_write_bytes, last_apply_write_bytes);

        // The brief is still locked once any creator has applied, exactly
        // as in `applications_exist_blocks_metadata_update`.
        let result = client.try_update_campaign_metadata(
            &id,
            &business,
            &String::from_str(&env, "ipfs://updated-brief"),
        );
        assert_eq!(result, Err(Ok(Error::ApplicationsExist)));
    }
}

mod test_resolve_dispute {
    use super::test_helpers::*;
    use crate::{CampaignEscrowContractClient, DisputeResolution, Error, MIN_EVIDENCE_WINDOW};
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::token::Client as TokenClient;
    use soroban_sdk::Address;

    /// Open a dispute over `creator`'s payout and let the evidence window run
    /// all the way out, leaving `resolve_dispute` callable. Mirrors the real
    /// flow: a freeze (from `raise_dispute` or from admin directly) starts
    /// the clock, and settlement only unlocks `MIN_EVIDENCE_WINDOW` later.
    ///
    /// Advances by exactly the window, so every test built on this also
    /// pins the inclusive boundary — resolution is legal the instant the
    /// window closes, not a second after.
    fn open_dispute_and_wait_out_window(
        env: &soroban_sdk::Env,
        client: &CampaignEscrowContractClient,
        dispute: &Address,
        campaign_id: u64,
        creator: &Address,
    ) {
        client.freeze_for_dispute(dispute, &campaign_id, creator);
        advance_time(env, MIN_EVIDENCE_WINDOW);
    }

    #[test]
    fn pay_creator_resolution_pays_creator_minus_fee() {
        let (env, contract_id) = setup_env();
        let (client, admin, dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let token_client = TokenClient::new(&env, &token);

        let payout: i128 = 1_000_000;
        let id = create_funded_campaign(&env, &client, &business, &token, payout, 5);
        let creator = Address::generate(&env);
        client.apply_to_campaign(&creator, &id, &soroban_sdk::String::from_str(&env, "pitch"));
        client.approve_creator(&business, &id, &creator, &payout);
        open_dispute_and_wait_out_window(&env, &client, &dispute, id, &creator);

        let creator_before = token_client.balance(&creator);
        let business_before = token_client.balance(&business);
        let treasury_before = token_client.balance(&admin);

        client.resolve_dispute(&admin, &id, &creator, &DisputeResolution::PayCreator);

        let fee = payout * 50 / ads_bazaar_shared::BASIS_POINTS_DENOMINATOR;
        assert_eq!(
            token_client.balance(&creator),
            creator_before + payout - fee
        );
        assert_eq!(token_client.balance(&admin), treasury_before + fee);
        // Business's share (nothing for this creator's slot) leaves their
        // balance unchanged from before resolution.
        assert_eq!(token_client.balance(&business), business_before);

        let application = client.get_application(&id, &creator);
        assert_eq!(
            application.status,
            ads_bazaar_shared::ApplicationStatus::Paid
        );
    }

    #[test]
    fn refund_business_resolution_returns_full_amount_no_fee() {
        let (env, contract_id) = setup_env();
        let (client, admin, dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let token_client = TokenClient::new(&env, &token);

        let payout: i128 = 1_000_000;
        let id = create_funded_campaign(&env, &client, &business, &token, payout, 5);
        let creator = Address::generate(&env);
        client.apply_to_campaign(&creator, &id, &soroban_sdk::String::from_str(&env, "pitch"));
        client.approve_creator(&business, &id, &creator, &payout);
        open_dispute_and_wait_out_window(&env, &client, &dispute, id, &creator);

        let business_before = token_client.balance(&business);
        let creator_before = token_client.balance(&creator);
        let treasury_before = token_client.balance(&admin);

        client.resolve_dispute(&admin, &id, &creator, &DisputeResolution::RefundBusiness);

        // Full amount, no fee deducted.
        assert_eq!(token_client.balance(&business), business_before + payout);
        assert_eq!(token_client.balance(&creator), creator_before);
        assert_eq!(token_client.balance(&admin), treasury_before);
    }

    #[test]
    fn split_resolution_divides_correctly() {
        let (env, contract_id) = setup_env();
        let (client, admin, dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let token_client = TokenClient::new(&env, &token);

        let payout: i128 = 1_000_000;
        let id = create_funded_campaign(&env, &client, &business, &token, payout, 5);
        let creator = Address::generate(&env);
        client.apply_to_campaign(&creator, &id, &soroban_sdk::String::from_str(&env, "pitch"));
        client.approve_creator(&business, &id, &creator, &payout);
        open_dispute_and_wait_out_window(&env, &client, &dispute, id, &creator);

        let business_before = token_client.balance(&business);
        let creator_before = token_client.balance(&creator);

        // 60% to creator, 40% to business.
        client.resolve_dispute(&admin, &id, &creator, &DisputeResolution::Split(6_000));

        let creator_gross = payout * 6_000 / 10_000;
        let fee = creator_gross * 50 / ads_bazaar_shared::BASIS_POINTS_DENOMINATOR;
        let creator_net = creator_gross - fee;
        let business_amount = payout - creator_gross;

        assert_eq!(token_client.balance(&creator), creator_before + creator_net);
        assert_eq!(
            token_client.balance(&business),
            business_before + business_amount
        );
    }

    #[test]
    fn non_admin_cannot_resolve_dispute() {
        let (env, contract_id) = setup_env();
        let (client, _admin, dispute, business, token) = bootstrap(&env, &contract_id, 50);

        let payout: i128 = 1_000_000;
        let id = create_funded_campaign(&env, &client, &business, &token, payout, 5);
        let creator = Address::generate(&env);
        client.apply_to_campaign(&creator, &id, &soroban_sdk::String::from_str(&env, "pitch"));
        client.approve_creator(&business, &id, &creator, &payout);
        // Fully resolvable for the admin, so the only thing this test's
        // caller fails on is authorization.
        open_dispute_and_wait_out_window(&env, &client, &dispute, id, &creator);

        let stranger = Address::generate(&env);
        let result =
            client.try_resolve_dispute(&stranger, &id, &creator, &DisputeResolution::PayCreator);
        assert_eq!(result, Err(Ok(Error::Unauthorized)));
    }

    #[test]
    fn resolve_dispute_rejects_already_paid() {
        let (env, contract_id) = setup_env();
        let (client, admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);

        // Two creators, so the campaign stays Active (escrow_balance > 0)
        // after the first one claims — isolating the already-Paid
        // application check from the separate Completed-campaign check.
        let payout: i128 = 1_000_000;
        let id = create_funded_campaign(&env, &client, &business, &token, payout * 2, 5);
        let creator = Address::generate(&env);
        let other_creator = Address::generate(&env);
        client.apply_to_campaign(&creator, &id, &soroban_sdk::String::from_str(&env, "pitch"));
        client.approve_creator(&business, &id, &creator, &payout);
        client.apply_to_campaign(
            &other_creator,
            &id,
            &soroban_sdk::String::from_str(&env, "pitch"),
        );
        client.approve_creator(&business, &id, &other_creator, &payout);
        client.submit_proof(&creator, &id, &soroban_sdk::String::from_str(&env, "proof"));
        client.approve_submission(&business, &id, &creator);
        client.claim_payment(&creator, &id);

        let result =
            client.try_resolve_dispute(&admin, &id, &creator, &DisputeResolution::PayCreator);
        assert_eq!(result, Err(Ok(Error::SubmissionNotPayable)));
    }

    #[test]
    fn resolve_dispute_works_on_rejected_application() {
        let (env, contract_id) = setup_env();
        let (client, admin, dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let token_client = TokenClient::new(&env, &token);

        let payout: i128 = 1_000_000;
        let id = create_funded_campaign(&env, &client, &business, &token, payout, 5);
        let creator = Address::generate(&env);
        client.apply_to_campaign(&creator, &id, &soroban_sdk::String::from_str(&env, "pitch"));
        client.approve_creator(&business, &id, &creator, &payout);
        client.submit_proof(&creator, &id, &soroban_sdk::String::from_str(&env, "proof"));
        // Business rejects — a natural point of disagreement to escalate.
        client.reject_submission(&business, &id, &creator);
        open_dispute_and_wait_out_window(&env, &client, &dispute, id, &creator);

        let creator_before = token_client.balance(&creator);
        client.resolve_dispute(&admin, &id, &creator, &DisputeResolution::PayCreator);

        let fee = payout * 50 / ads_bazaar_shared::BASIS_POINTS_DENOMINATOR;
        assert_eq!(
            token_client.balance(&creator),
            creator_before + payout - fee
        );
    }

    // ── Evidence window ───────────────────────────────────────────────────

    /// Take a creator to an approved, proof-submitted application and open a
    /// dispute over their payout, leaving the evidence window still running.
    fn creator_with_open_dispute(
        env: &soroban_sdk::Env,
        client: &CampaignEscrowContractClient,
        dispute: &Address,
        business: &Address,
        campaign_id: u64,
        payout: i128,
    ) -> Address {
        let creator = Address::generate(env);
        client.apply_to_campaign(
            &creator,
            &campaign_id,
            &soroban_sdk::String::from_str(env, "pitch"),
        );
        client.approve_creator(business, &campaign_id, &creator, &payout);
        client.submit_proof(
            &creator,
            &campaign_id,
            &soroban_sdk::String::from_str(env, "proof"),
        );
        client.freeze_for_dispute(dispute, &campaign_id, &creator);
        creator
    }

    /// The core of this guard: the admin cannot reallocate a contested payout
    /// in the same breath as the dispute being opened.
    #[test]
    fn resolve_dispute_rejected_immediately_after_dispute_opened() {
        let (env, contract_id) = setup_env();
        let (client, admin, dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let token_client = TokenClient::new(&env, &token);

        let payout: i128 = 1_000_000;
        let id = create_funded_campaign(&env, &client, &business, &token, payout, 5);
        let creator = creator_with_open_dispute(&env, &client, &dispute, &business, id, payout);

        let creator_before = token_client.balance(&creator);
        let escrow_before = token_client.balance(&contract_id);

        let result =
            client.try_resolve_dispute(&admin, &id, &creator, &DisputeResolution::PayCreator);
        assert_eq!(result, Err(Ok(Error::EvidenceWindowOpen)));

        // Rejected means rejected: no funds moved and nothing settled.
        assert_eq!(token_client.balance(&creator), creator_before);
        assert_eq!(token_client.balance(&contract_id), escrow_before);
        let application = client.get_application(&id, &creator);
        assert!(application.frozen);
        assert_ne!(
            application.status,
            ads_bazaar_shared::ApplicationStatus::Paid
        );
        assert_eq!(payout, application.payout_amount);
    }

    /// One second short of the window is still inside it. Pins the boundary
    /// from below, so an off-by-one in the comparison can't pass unnoticed.
    #[test]
    fn resolve_dispute_rejected_one_second_before_window_closes() {
        let (env, contract_id) = setup_env();
        let (client, admin, dispute, business, token) = bootstrap(&env, &contract_id, 50);

        let payout: i128 = 1_000_000;
        let id = create_funded_campaign(&env, &client, &business, &token, payout, 5);
        let creator = creator_with_open_dispute(&env, &client, &dispute, &business, id, payout);

        advance_time(&env, MIN_EVIDENCE_WINDOW - 1);
        let result =
            client.try_resolve_dispute(&admin, &id, &creator, &DisputeResolution::PayCreator);
        assert_eq!(result, Err(Ok(Error::EvidenceWindowOpen)));
    }

    /// The window is a delay, not a veto — once it closes, resolution goes
    /// through and pays out exactly as it would have without the gate.
    #[test]
    fn resolve_dispute_succeeds_once_evidence_window_elapses() {
        let (env, contract_id) = setup_env();
        let (client, admin, dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let token_client = TokenClient::new(&env, &token);

        let payout: i128 = 1_000_000;
        let id = create_funded_campaign(&env, &client, &business, &token, payout, 5);
        let creator = creator_with_open_dispute(&env, &client, &dispute, &business, id, payout);

        let creator_before = token_client.balance(&creator);
        // Exactly the window, no more: resolution unlocks the instant it closes.
        advance_time(&env, MIN_EVIDENCE_WINDOW);
        client.resolve_dispute(&admin, &id, &creator, &DisputeResolution::PayCreator);

        let fee = payout * 50 / ads_bazaar_shared::BASIS_POINTS_DENOMINATOR;
        assert_eq!(
            token_client.balance(&creator),
            creator_before + payout - fee
        );

        let application = client.get_application(&id, &creator);
        assert_eq!(
            application.status,
            ads_bazaar_shared::ApplicationStatus::Paid
        );
        // Settling clears the dispute state, so the window is not re-armed
        // against a payout that no longer has anything left to contest.
        assert!(!application.frozen);
        assert_eq!(application.dispute_opened_at, None);
    }

    /// Without a `freeze_for_dispute` there is no dispute and no window to
    /// measure from, so the admin has nothing to settle — this is the half of
    /// the gap the timestamp alone wouldn't close.
    #[test]
    fn resolve_dispute_rejects_application_with_no_dispute_open() {
        let (env, contract_id) = setup_env();
        let (client, admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);

        let payout: i128 = 1_000_000;
        let id = create_funded_campaign(&env, &client, &business, &token, payout, 5);
        let creator = Address::generate(&env);
        client.apply_to_campaign(&creator, &id, &soroban_sdk::String::from_str(&env, "pitch"));
        client.approve_creator(&business, &id, &creator, &payout);

        assert_eq!(
            client.get_application(&id, &creator).dispute_opened_at,
            None
        );
        let result =
            client.try_resolve_dispute(&admin, &id, &creator, &DisputeResolution::PayCreator);
        assert_eq!(result, Err(Ok(Error::NoDisputeOpen)));

        // Still not resolvable however long the admin waits — waiting is not
        // a substitute for a dispute actually having been raised.
        advance_time(&env, MIN_EVIDENCE_WINDOW * 10);
        let result =
            client.try_resolve_dispute(&admin, &id, &creator, &DisputeResolution::PayCreator);
        assert_eq!(result, Err(Ok(Error::NoDisputeOpen)));
    }
}

mod test_freeze_for_dispute {
    use super::test_helpers::*;
    use crate::{CampaignEscrowContractClient, Error};
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::{Address, String};

    /// Take a creator all the way to a business-approved, immediately
    /// claimable submission on a freshly funded campaign.
    fn payable_application(
        env: &soroban_sdk::Env,
        client: &CampaignEscrowContractClient,
        business: &Address,
        campaign_id: u64,
        payout: i128,
    ) -> Address {
        let creator = Address::generate(env);
        client.apply_to_campaign(&creator, &campaign_id, &String::from_str(env, "pitch"));
        client.approve_creator(business, &campaign_id, &creator, &payout);
        client.submit_proof(&creator, &campaign_id, &String::from_str(env, "proof"));
        client.approve_submission(business, &campaign_id, &creator);
        creator
    }

    #[test]
    fn freeze_marks_application_and_blocks_claim() {
        let (env, contract_id) = setup_env();
        let (client, _admin, dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);
        let creator = payable_application(&env, &client, &business, id, 1_000_000);

        let before = client.get_application(&id, &creator);
        assert!(!before.frozen);
        assert_eq!(before.dispute_opened_at, None);

        advance_time(&env, 1_234);
        client.freeze_for_dispute(&dispute, &id, &creator);

        let after = client.get_application(&id, &creator);
        assert!(after.frozen);
        // The freeze stamps the moment the dispute opened — that stamp is
        // what `resolve_dispute` measures `MIN_EVIDENCE_WINDOW` from.
        assert_eq!(after.dispute_opened_at, Some(BASE_TIME + 1_234));

        let result = client.try_claim_payment(&creator, &id);
        assert_eq!(result, Err(Ok(Error::PayoutFrozen)));
    }

    /// A second freeze is already rejected, so an admin cannot use one to
    /// push `dispute_opened_at` forward and restart the evidence window on a
    /// dispute the other party is midway through answering.
    #[test]
    fn re_freeze_cannot_restart_the_evidence_window() {
        let (env, contract_id) = setup_env();
        let (client, _admin, dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);
        let creator = payable_application(&env, &client, &business, id, 1_000_000);

        client.freeze_for_dispute(&dispute, &id, &creator);
        let opened_at = client.get_application(&id, &creator).dispute_opened_at;
        assert_eq!(opened_at, Some(BASE_TIME));

        advance_time(&env, crate::MIN_EVIDENCE_WINDOW / 2);
        assert_eq!(
            client.try_freeze_for_dispute(&dispute, &id, &creator),
            Err(Ok(Error::PayoutFrozen))
        );
        assert_eq!(
            client.get_application(&id, &creator).dispute_opened_at,
            opened_at
        );
    }

    #[test]
    fn freeze_blocks_claim_after_auto_approval_deadline() {
        let (env, contract_id) = setup_env();
        let (client, _admin, dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);

        let creator = Address::generate(&env);
        client.apply_to_campaign(&creator, &id, &String::from_str(&env, "pitch"));
        client.approve_creator(&business, &id, &creator, &1_000_000);
        client.submit_proof(&creator, &id, &String::from_str(&env, "proof"));
        client.freeze_for_dispute(&dispute, &id, &creator);

        // Past the content deadline the creator would otherwise be
        // auto-approved — this is the case a business raises a dispute for.
        advance_time(&env, 604_801);
        let result = client.try_claim_payment(&creator, &id);
        assert_eq!(result, Err(Ok(Error::PayoutFrozen)));
    }

    #[test]
    fn freeze_preserves_proof_against_business_edits() {
        let (env, contract_id) = setup_env();
        let (client, _admin, dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);

        let creator = Address::generate(&env);
        client.apply_to_campaign(&creator, &id, &String::from_str(&env, "pitch"));
        client.approve_creator(&business, &id, &creator, &1_000_000);
        client.submit_proof(&creator, &id, &String::from_str(&env, "proof"));
        client.freeze_for_dispute(&dispute, &id, &creator);

        assert_eq!(
            client.try_reject_submission(&business, &id, &creator),
            Err(Ok(Error::PayoutFrozen))
        );
        assert_eq!(
            client.try_approve_submission(&business, &id, &creator),
            Err(Ok(Error::PayoutFrozen))
        );
        assert_eq!(
            client.try_submit_proof(&creator, &id, &String::from_str(&env, "proof2")),
            Err(Ok(Error::PayoutFrozen))
        );
        assert_eq!(
            client.get_application(&id, &creator).proof_uri,
            Some(String::from_str(&env, "proof"))
        );
    }

    #[test]
    fn freeze_leaves_other_creators_claimable() {
        let (env, contract_id) = setup_env();
        let (client, _admin, dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);
        let disputed = payable_application(&env, &client, &business, id, 1_000_000);
        let uncontested = payable_application(&env, &client, &business, id, 1_000_000);

        client.freeze_for_dispute(&dispute, &id, &disputed);

        client.claim_payment(&uncontested, &id);
        assert_eq!(
            client.get_application(&id, &uncontested).status,
            ads_bazaar_shared::ApplicationStatus::Paid
        );
    }

    #[test]
    fn freeze_rejects_already_paid_application() {
        let (env, contract_id) = setup_env();
        let (client, _admin, dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);
        let creator = payable_application(&env, &client, &business, id, 1_000_000);
        client.claim_payment(&creator, &id);

        let result = client.try_freeze_for_dispute(&dispute, &id, &creator);
        assert_eq!(result, Err(Ok(Error::SubmissionNotPayable)));
    }

    #[test]
    fn freeze_rejects_creator_with_no_application() {
        let (env, contract_id) = setup_env();
        let (client, _admin, dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);

        let stranger = Address::generate(&env);
        let result = client.try_freeze_for_dispute(&dispute, &id, &stranger);
        assert_eq!(result, Err(Ok(Error::ApplicationNotFound)));
    }

    #[test]
    fn freeze_rejects_unapproved_applicant() {
        let (env, contract_id) = setup_env();
        let (client, _admin, dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);

        // Applied but never approved, so no payout is committed to freeze.
        let creator = Address::generate(&env);
        client.apply_to_campaign(&creator, &id, &String::from_str(&env, "pitch"));

        let result = client.try_freeze_for_dispute(&dispute, &id, &creator);
        assert_eq!(result, Err(Ok(Error::SubmissionNotPayable)));
    }

    #[test]
    fn freeze_twice_fails() {
        let (env, contract_id) = setup_env();
        let (client, _admin, dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);
        let creator = payable_application(&env, &client, &business, id, 1_000_000);

        client.freeze_for_dispute(&dispute, &id, &creator);
        let result = client.try_freeze_for_dispute(&dispute, &id, &creator);
        assert_eq!(result, Err(Ok(Error::PayoutFrozen)));
    }

    #[test]
    fn freeze_rejects_cancelled_campaign() {
        let (env, contract_id) = setup_env();
        let (client, _admin, dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);
        let creator = payable_application(&env, &client, &business, id, 1_000_000);
        client.cancel_campaign(&business, &id);

        let result = client.try_freeze_for_dispute(&dispute, &id, &creator);
        assert_eq!(result, Err(Ok(Error::InvalidStatus)));
    }

    #[test]
    fn freeze_rejects_uninvolved_stranger() {
        let (env, contract_id) = setup_env();
        let (client, _admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);
        let creator = payable_application(&env, &client, &business, id, 1_000_000);

        // Not the configured dispute contract and not the admin.
        let stranger = Address::generate(&env);
        let result = client.try_freeze_for_dispute(&stranger, &id, &creator);
        assert_eq!(result, Err(Ok(Error::Unauthorized)));
        assert!(!client.get_application(&id, &creator).frozen);
    }

    #[test]
    fn admin_can_freeze_directly() {
        let (env, contract_id) = setup_env();
        let (client, admin, _dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);
        let creator = payable_application(&env, &client, &business, id, 1_000_000);

        // Admin can freeze directly, without routing through the
        // dispute-resolution contract's `raise_dispute`.
        client.freeze_for_dispute(&admin, &id, &creator);
        assert!(client.get_application(&id, &creator).frozen);

        let result = client.try_claim_payment(&creator, &id);
        assert_eq!(result, Err(Ok(Error::PayoutFrozen)));
    }

    #[test]
    fn admin_resolve_dispute_settles_and_clears_freeze() {
        let (env, contract_id) = setup_env();
        let (client, admin, dispute, business, token) = bootstrap(&env, &contract_id, 50);
        let id = create_funded_campaign(&env, &client, &business, &token, 10_000_000, 5);
        let creator = payable_application(&env, &client, &business, id, 1_000_000);
        client.freeze_for_dispute(&dispute, &id, &creator);
        advance_time(&env, crate::MIN_EVIDENCE_WINDOW);

        client.resolve_dispute(&admin, &id, &creator, &crate::DisputeResolution::PayCreator);

        let application = client.get_application(&id, &creator);
        assert!(!application.frozen);
        assert_eq!(
            application.status,
            ads_bazaar_shared::ApplicationStatus::Paid
        );

        // Settling clears the freeze, but the application is now Paid, so a
        // subsequent claim attempt still fails — just via the ordinary
        // already-Paid guard, not `PayoutFrozen`.
        let result = client.try_claim_payment(&creator, &id);
        assert_eq!(result, Err(Ok(Error::SubmissionNotPayable)));
    }
}
