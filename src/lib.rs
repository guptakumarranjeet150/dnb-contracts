#![no_std]
//! DeenBridge On-Chain Giving — conditional, non-custodial USDC escrow on Soroban.
//!
//! Donors fund a campaign (a scholarship or a community event). Funds are held by
//! the contract and released to the organizer ONLY when the platform has approved
//! the campaign and its funding goal is met. If the campaign is cancelled, or its
//! deadline passes without approval/goal, every donor can reclaim 100% of their
//! contribution. The platform can approve or cancel — it can never move escrowed
//! funds to itself (no admin custody path). USDC is handled via its Stellar Asset
//! Contract (the SEP-41 token interface).

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env,
};

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum Error {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    CampaignNotFound = 3,
    NotActive = 4,
    DeadlinePassed = 5,
    InvalidAmount = 6,
    InvalidGoal = 7,
    InvalidDeadline = 8,
    NotApproved = 9,
    AlreadyApproved = 10,
    GoalNotMet = 11,
    RefundNotAllowed = 12,
    NothingToRefund = 13,
    WrongStatus = 14,
}

/// A campaign is either a scholarship fund or a community-event fund.
#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CampaignKind {
    Scholarship,
    Event,
}

/// Lifecycle: Active (accepting donations) -> Released (paid organizer) or
/// Cancelled (refunds open). `approved` is a separate platform gate.
#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Active,
    Released,
    Cancelled,
}

#[contracttype]
#[derive(Clone)]
pub struct Config {
    pub admin: Address,
    pub usdc: Address,
    pub platform: Address,
}

#[contracttype]
#[derive(Clone)]
pub struct Campaign {
    pub id: u64,
    pub kind: CampaignKind,
    pub organizer: Address,
    pub goal: i128,
    pub deadline: u64,
    pub raised: i128,
    pub status: Status,
    pub approved: bool,
}

#[contracttype]
pub enum DataKey {
    Config,
    Counter,
    Campaign(u64),
    Contribution(u64, Address),
}

#[contract]
pub struct GivingEscrow;

#[contractimpl]
impl GivingEscrow {
    /// One-time setup. `platform` is the approver (ideally a multisig); `usdc` is
    /// the USDC Stellar Asset Contract address. Requires the admin's signature.
    pub fn initialize(
        env: Env,
        admin: Address,
        usdc: Address,
        platform: Address,
    ) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Config) {
            return Err(Error::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(
            &DataKey::Config,
            &Config {
                admin,
                usdc,
                platform,
            },
        );
        env.storage().instance().set(&DataKey::Counter, &0u64);
        Ok(())
    }

    /// Open a new campaign. Only the platform may create campaigns (it vets the
    /// organizer off-chain first). Returns the new campaign id.
    pub fn create_campaign(
        env: Env,
        kind: CampaignKind,
        organizer: Address,
        goal: i128,
        deadline: u64,
    ) -> Result<u64, Error> {
        let config = Self::load_config(&env)?;
        config.platform.require_auth();
        if goal <= 0 {
            return Err(Error::InvalidGoal);
        }
        if deadline <= env.ledger().timestamp() {
            return Err(Error::InvalidDeadline);
        }
        let id: u64 = env.storage().instance().get(&DataKey::Counter).unwrap_or(0) + 1;
        let campaign = Campaign {
            id,
            kind,
            organizer,
            goal,
            deadline,
            raised: 0,
            status: Status::Active,
            approved: false,
        };
        env.storage()
            .persistent()
            .set(&DataKey::Campaign(id), &campaign);
        env.storage().instance().set(&DataKey::Counter, &id);
        env.events().publish(
            (symbol_short!("created"), id),
            (campaign.kind, campaign.goal),
        );
        Ok(id)
    }

    /// Donate USDC into a campaign's escrow. Pulls `amount` from the donor (who
    /// must authorize) into the contract and records their contribution.
    pub fn donate(env: Env, campaign_id: u64, donor: Address, amount: i128) -> Result<(), Error> {
        donor.require_auth();
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }
        let config = Self::load_config(&env)?;
        let mut c = Self::get_campaign(env.clone(), campaign_id)?;
        if c.status != Status::Active {
            return Err(Error::NotActive);
        }
        if env.ledger().timestamp() >= c.deadline {
            return Err(Error::DeadlinePassed);
        }

        token::Client::new(&env, &config.usdc).transfer(
            &donor,
            &env.current_contract_address(),
            &amount,
        );

        let key = DataKey::Contribution(campaign_id, donor.clone());
        let prev: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        env.storage().persistent().set(&key, &(prev + amount));

        c.raised += amount;
        env.storage()
            .persistent()
            .set(&DataKey::Campaign(campaign_id), &c);
        env.events()
            .publish((symbol_short!("donation"), campaign_id), (donor, amount));
        Ok(())
    }

    /// Platform approves the campaign as legitimate. Donations continue; release
    /// remains gated on the goal being met.
    pub fn approve_campaign(env: Env, campaign_id: u64) -> Result<(), Error> {
        let config = Self::load_config(&env)?;
        config.platform.require_auth();
        let mut c = Self::get_campaign(env.clone(), campaign_id)?;
        if c.status != Status::Active {
            return Err(Error::WrongStatus);
        }
        if c.approved {
            return Err(Error::AlreadyApproved);
        }
        c.approved = true;
        env.storage()
            .persistent()
            .set(&DataKey::Campaign(campaign_id), &c);
        env.events()
            .publish((symbol_short!("approved"), campaign_id), ());
        Ok(())
    }

    /// Release the full escrow to the organizer. Requires platform auth, prior
    /// approval, and that the funding goal is met. Pays the organizer only.
    pub fn release(env: Env, campaign_id: u64) -> Result<(), Error> {
        let config = Self::load_config(&env)?;
        config.platform.require_auth();
        let mut c = Self::get_campaign(env.clone(), campaign_id)?;
        if c.status != Status::Active {
            return Err(Error::WrongStatus);
        }
        if !c.approved {
            return Err(Error::NotApproved);
        }
        if c.raised < c.goal {
            return Err(Error::GoalNotMet);
        }
        let amount = c.raised;
        c.status = Status::Released;
        env.storage()
            .persistent()
            .set(&DataKey::Campaign(campaign_id), &c);

        token::Client::new(&env, &config.usdc).transfer(
            &env.current_contract_address(),
            &c.organizer,
            &amount,
        );
        env.events().publish(
            (symbol_short!("released"), campaign_id),
            (c.organizer, amount),
        );
        Ok(())
    }

    /// Platform cancels a campaign, opening refunds for all donors.
    pub fn cancel_campaign(env: Env, campaign_id: u64) -> Result<(), Error> {
        let config = Self::load_config(&env)?;
        config.platform.require_auth();
        let mut c = Self::get_campaign(env.clone(), campaign_id)?;
        if c.status != Status::Active {
            return Err(Error::WrongStatus);
        }
        c.status = Status::Cancelled;
        env.storage()
            .persistent()
            .set(&DataKey::Campaign(campaign_id), &c);
        env.events()
            .publish((symbol_short!("cancelled"), campaign_id), ());
        Ok(())
    }

    /// Reclaim a donor's full contribution. Allowed when the campaign is cancelled,
    /// or its deadline has passed without meeting the goal or without approval.
    /// Anyone may trigger it — funds only ever return to the donor. Idempotent.
    pub fn refund(env: Env, campaign_id: u64, donor: Address) -> Result<(), Error> {
        let config = Self::load_config(&env)?;
        let c = Self::get_campaign(env.clone(), campaign_id)?;

        let expired = env.ledger().timestamp() >= c.deadline;
        let failed_open =
            c.status == Status::Active && expired && (c.raised < c.goal || !c.approved);
        let cancelled = c.status == Status::Cancelled;
        if !(failed_open || cancelled) {
            return Err(Error::RefundNotAllowed);
        }

        let key = DataKey::Contribution(campaign_id, donor.clone());
        let amount: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        if amount <= 0 {
            return Err(Error::NothingToRefund);
        }
        // Clear before transfer so a repeated call refunds nothing (idempotent).
        env.storage().persistent().remove(&key);

        token::Client::new(&env, &config.usdc).transfer(
            &env.current_contract_address(),
            &donor,
            &amount,
        );
        env.events()
            .publish((symbol_short!("refunded"), campaign_id), (donor, amount));
        Ok(())
    }

    // ---- views ----

    pub fn get_campaign(env: Env, campaign_id: u64) -> Result<Campaign, Error> {
        env.storage()
            .persistent()
            .get(&DataKey::Campaign(campaign_id))
            .ok_or(Error::CampaignNotFound)
    }

    pub fn get_contribution(env: Env, campaign_id: u64, donor: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::Contribution(campaign_id, donor))
            .unwrap_or(0)
    }

    pub fn get_config(env: Env) -> Result<Config, Error> {
        Self::load_config(&env)
    }

    fn load_config(env: &Env) -> Result<Config, Error> {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(Error::NotInitialized)
    }
}

#[cfg(test)]
mod test;
