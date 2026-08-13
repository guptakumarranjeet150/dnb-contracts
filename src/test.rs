use super::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    token, Address, Env,
};

const NOW: u64 = 1_000;
const DEADLINE: u64 = 2_000;

struct Fixture {
    env: Env,
    contract: Address,
    usdc: Address,
    platform: Address,
}

fn setup() -> Fixture {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(NOW);

    let admin = Address::generate(&env);
    let platform = Address::generate(&env);
    let issuer = Address::generate(&env);
    let usdc = env.register_stellar_asset_contract_v2(issuer).address();

    let contract = env.register(GivingEscrow, ());
    GivingEscrowClient::new(&env, &contract).initialize(&admin, &usdc, &platform);

    Fixture {
        env,
        contract,
        usdc,
        platform,
    }
}

fn client<'a>(f: &Fixture) -> GivingEscrowClient<'a> {
    GivingEscrowClient::new(&f.env, &f.contract)
}

fn mint(f: &Fixture, to: &Address, amount: i128) {
    token::StellarAssetClient::new(&f.env, &f.usdc).mint(to, &amount);
}

fn balance(f: &Fixture, who: &Address) -> i128 {
    token::Client::new(&f.env, &f.usdc).balance(who)
}

#[test]
fn full_flow_donate_approve_release() {
    let f = setup();
    let c = client(&f);
    let organizer = Address::generate(&f.env);
    let d1 = Address::generate(&f.env);
    let d2 = Address::generate(&f.env);
    mint(&f, &d1, 100);
    mint(&f, &d2, 200);

    let id = c.create_campaign(&CampaignKind::Event, &organizer, &250, &DEADLINE);
    c.donate(&id, &d1, &100);
    c.donate(&id, &d2, &200);

    let camp = c.get_campaign(&id);
    assert_eq!(camp.raised, 300);
    assert_eq!(balance(&f, &f.contract), 300); // escrow holds the funds

    c.approve_campaign(&id);
    c.release(&id);

    assert_eq!(c.get_campaign(&id).status, Status::Released);
    assert_eq!(balance(&f, &organizer), 300); // organizer paid the full raise
    assert_eq!(balance(&f, &f.contract), 0); // escrow emptied
    assert_eq!(balance(&f, &f.platform), 0); // platform never takes custody
    assert_eq!(balance(&f, &d1), 0);
}

#[test]
fn refund_when_deadline_passes_under_goal() {
    let f = setup();
    let c = client(&f);
    let organizer = Address::generate(&f.env);
    let d1 = Address::generate(&f.env);
    mint(&f, &d1, 100);

    let id = c.create_campaign(&CampaignKind::Scholarship, &organizer, &500, &DEADLINE);
    c.donate(&id, &d1, &100);
    assert_eq!(balance(&f, &d1), 0);

    f.env.ledger().set_timestamp(DEADLINE + 1); // expired, goal not met
    c.refund(&id, &d1);

    assert_eq!(balance(&f, &d1), 100); // donor made whole
    assert_eq!(balance(&f, &f.contract), 0);
    assert_eq!(c.get_contribution(&id, &d1), 0);
}

#[test]
fn refund_after_cancel() {
    let f = setup();
    let c = client(&f);
    let organizer = Address::generate(&f.env);
    let d1 = Address::generate(&f.env);
    mint(&f, &d1, 150);

    let id = c.create_campaign(&CampaignKind::Event, &organizer, &500, &DEADLINE);
    c.donate(&id, &d1, &150);
    c.cancel_campaign(&id);
    assert_eq!(c.get_campaign(&id).status, Status::Cancelled);

    c.refund(&id, &d1);
    assert_eq!(balance(&f, &d1), 150);
    assert_eq!(balance(&f, &f.contract), 0);
}

#[test]
fn release_fails_without_approval() {
    let f = setup();
    let c = client(&f);
    let organizer = Address::generate(&f.env);
    let d1 = Address::generate(&f.env);
    mint(&f, &d1, 500);

    let id = c.create_campaign(&CampaignKind::Event, &organizer, &500, &DEADLINE);
    c.donate(&id, &d1, &500); // goal met, but not approved
    assert_eq!(c.try_release(&id), Err(Ok(Error::NotApproved)));
}

#[test]
fn release_fails_when_goal_not_met() {
    let f = setup();
    let c = client(&f);
    let organizer = Address::generate(&f.env);
    let d1 = Address::generate(&f.env);
    mint(&f, &d1, 100);

    let id = c.create_campaign(&CampaignKind::Event, &organizer, &500, &DEADLINE);
    c.donate(&id, &d1, &100);
    c.approve_campaign(&id); // approved, but under goal
    assert_eq!(c.try_release(&id), Err(Ok(Error::GoalNotMet)));
}

#[test]
fn refund_is_idempotent() {
    let f = setup();
    let c = client(&f);
    let organizer = Address::generate(&f.env);
    let d1 = Address::generate(&f.env);
    mint(&f, &d1, 100);

    let id = c.create_campaign(&CampaignKind::Event, &organizer, &500, &DEADLINE);
    c.donate(&id, &d1, &100);
    c.cancel_campaign(&id);

    c.refund(&id, &d1);
    assert_eq!(balance(&f, &d1), 100);
    // Second refund returns nothing (contribution already cleared).
    assert_eq!(c.try_refund(&id, &d1), Err(Ok(Error::NothingToRefund)));
    assert_eq!(balance(&f, &d1), 100);
}

#[test]
fn donate_after_deadline_fails() {
    let f = setup();
    let c = client(&f);
    let organizer = Address::generate(&f.env);
    let d1 = Address::generate(&f.env);
    mint(&f, &d1, 100);

    let id = c.create_campaign(&CampaignKind::Event, &organizer, &500, &DEADLINE);
    f.env.ledger().set_timestamp(DEADLINE + 1);
    assert_eq!(c.try_donate(&id, &d1, &100), Err(Ok(Error::DeadlinePassed)));
}

#[test]
fn cannot_refund_active_before_deadline() {
    let f = setup();
    let c = client(&f);
    let organizer = Address::generate(&f.env);
    let d1 = Address::generate(&f.env);
    mint(&f, &d1, 100);

    let id = c.create_campaign(&CampaignKind::Event, &organizer, &500, &DEADLINE);
    c.donate(&id, &d1, &100);
    // Still active, not expired, not cancelled -> refunds are not open.
    assert_eq!(c.try_refund(&id, &d1), Err(Ok(Error::RefundNotAllowed)));
}

#[test]
fn double_initialize_fails() {
    let f = setup();
    let c = client(&f);
    let a = Address::generate(&f.env);
    assert_eq!(
        c.try_initialize(&a, &f.usdc, &f.platform),
        Err(Ok(Error::AlreadyInitialized))
    );
}
