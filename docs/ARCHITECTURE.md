# Architecture

AdsBazaar's on-chain layer is a Cargo workspace of Soroban (Stellar smart
contracts) crates:

```
contracts/
├── shared/               ads-bazaar-shared     — common types/enums, no #[contract]
├── campaign-escrow/      ads-bazaar-campaign-escrow — holds & releases campaign funds
└── dispute-resolution/   ads-bazaar-dispute-resolution — arbitrates contested payouts
```

`shared` has no contract entry points of its own — it exists purely so the
two contracts (and any future ones) agree on the same `CampaignStatus`,
`ApplicationStatus`, `DisputeStatus`, `DisputeOutcome` and `PayoutAsset`
types instead of drifting apart.

## Why two contracts instead of one

- **Escrow correctness is the highest-stakes code in this repo** — it holds
  real business funds. Keeping it free of arbitration logic keeps its
  surface area (and audit surface) as small as possible.
- **Arbitration is the least-settled design space.** Whether disputes are
  resolved by a single trusted arbiter, a staked jury, or an oracle is an
  open question (see `dispute-resolution/src/lib.rs`). Isolating it in its
  own contract means that design can iterate — or even be redeployed —
  without touching escrow.
- Cross-contract calls between them go through an explicit, narrow
  interface: `campaign-escrow::freeze_for_dispute` /
  `resolve_dispute_payout`, callable only by the configured
  `dispute_contract` address, and `dispute-resolution` calling back into
  escrow once a dispute resolves. `freeze_for_dispute` is implemented (a
  per-application freeze wired up by `raise_dispute`); `resolve_dispute_payout`
  is still `todo!()`.

### The evidence window

`freeze_for_dispute` is the point at which escrow learns a dispute exists: it
stamps `Application::dispute_opened_at` and emits `DisputeFrozen`. The
admin-resolved shortcut `campaign-escrow::resolve_dispute` will not settle an
application that has no such stamp, and will not settle one until
`MIN_EVIDENCE_WINDOW` (72h) has elapsed since it.

This is a deliberate trust-minimization bound on the admin key rather than an
implementation detail. `resolve_dispute` can move a committed payout in any
direction, so without the window a compromised admin key could reallocate a
disputed payout in the same block the dispute is raised — before the other
party could answer it, or even see it. Requiring the freeze first is what
makes the window meaningful: it guarantees a public `DisputeFrozen` event
precedes every settlement, so there is something for the counterparty to
notice. The constant is `pub` so a frontend can render the countdown instead
of duplicating the number.

## Multi-currency design

A campaign is funded in whatever Stellar asset the business already holds —
represented by `PayoutAsset { token: Address, symbol: String }`, where
`token` is any SEP-41-compatible token contract address (a classic Stellar
Asset Contract wrapping XLM/a Naira-pegged stablecoin/USDC, or a native
Soroban token). The escrow contract is asset-agnostic: it's expected to move
funds via the standard `soroban_sdk::token::Client` rather than special-
casing any one currency. This is what lets a Lagos business fund a campaign
in a Naira-denominated asset and a Nairobi creator withdraw through a
mobile-money-connected anchor without either side touching a different
contract.

## Current state of this scaffold

Every function in `campaign-escrow` and `dispute-resolution` is implemented
enough to compile and export correctly, but the actual state-transition
logic for the core flows is left as `todo!()`:

| Area | Status |
|---|---|
| Storage schema, error types, event types | Implemented |
| `initialize` (both contracts) | Implemented |
| Read-only getters (`get_campaign`, `get_application`, `get_dispute`) | Implemented |
| `create_campaign`, `fund_campaign` | `todo!()` |
| `apply_to_campaign`, `approve_creator`, `submit_proof` | `todo!()` |
| `release_payment`, `cancel_campaign` | `todo!()` |
| `freeze_for_dispute` | Implemented |
| `resolve_dispute_payout` | `todo!()` |
| `raise_dispute` | Implemented |
| `assign_arbiter`, `resolve_dispute` (dispute-resolution) | `todo!()` |

Each `todo!()` has a doc comment directly above it describing the intended
behavior and the open design questions it depends on — start there.

## Testing strategy

Testing is split into two layers, each with different goals:

### Unit tests (fast, mock environment)

Run via `cargo test --workspace`. These use `soroban_sdk::testutils::Env` — a
mocked, in-process host environment that lets you verify contract logic
quickly without network dependency.

**Coverage:** `initialize`, read-only getters, error cases, state transitions
for functions that are already implemented.

**Limitation:** Unit tests don't exercise real network semantics — actual
transaction submission, real Stellar Asset Contract behavior, ledger time
progression, TTL/rent behavior, or the actual CLI that users will invoke.

### End-to-end testnet smoke test (real network, pre-release verification)

Run via `./scripts/testnet-smoke-test.sh` or trigger via GitHub Actions.

This drives a complete campaign lifecycle against a **real testnet
deployment**:

- Funds testnet accounts via Friendbot
- Orchestrates the full flow: `create_campaign` → `fund_campaign` →
  `apply_to_campaign` → `approve_creator` → `submit_proof` →
  `approve_submission` → `claim_payment`
- Asserts on-chain state at each step, failing with clear errors if
  anything goes wrong
- Exercises the actual `stellar contract invoke` CLI that production users
  will depend on

**When to run:** Before a mainnet deployment (part of the pre-release
checklist), or whenever you want to verify the real deployed artifact works
end-to-end. Not intended to run on every CI push — it has testnet dependency,
funded account setup, and network flakiness.

**How to run:**

```bash
# Against existing testnet deployment (assumes contracts already deployed)
./scripts/testnet-smoke-test.sh

# Deploy fresh contracts first
./scripts/testnet-smoke-test.sh --deploy

# Manually trigger from GitHub Actions
# Go to Actions → Testnet Smoke Test → Run workflow
```

See [`README.md`](../README.md#end-to-end-testnet-smoke-test) for full details.

## Open design questions for contributors

1. **Proof-of-work verification** (`submit_proof`): off-chain URI only, an
   on-chain hash commitment, an oracle attestation? This is probably the
   single biggest open question in the repo.
2. **Payout sizing** (`approve_creator`): business sets `payout_amount` per
   creator at approval time (current sketch), vs. an even split of
   `total_budget / max_creators`, vs. milestone-based partial payouts.
3. **Arbitration model** (`dispute-resolution`): single trusted arbiter
   (simplest, most centralized) vs. staked juror voting vs. an oracle feed.
4. **Fee collection** (`release_payment`): transfer the platform fee to
   `admin` on every release, or accrue it for a periodic sweep?
5. **Release trigger**: does `release_payment` require an explicit business
   call, or should there be an auto-release timeout after proof submission
   to protect creators from an unresponsive business?

## Known scaffold quirks

- `stellar contract build` prints warnings like `type 'CampaignId' ... is not
  defined in the spec`. This is cosmetic — `CampaignId`/`DisputeId` are
  plain `u64` type aliases (see `contracts/shared/src/lib.rs`) and the
  spec-generation tooling doesn't register a name for bare aliases used
  across a crate boundary. The build still succeeds and the wasm is
  correct.
- `contracts/shared/Cargo.toml` pins `ed25519-dalek = "=2.2.0"` as a
  dev-dependency. This works around `soroban-env-host` declaring an
  unbounded `ed25519-dalek = ">=2.0.0"` dependency, which otherwise resolves
  to a newer semver-major release that breaks `cargo test`'s `testutils`
  build. It's dev-only so it never touches the release/wasm build graph.
  Safe to remove once upstream (`stellar/rs-soroban-env`) tightens that
  bound.
