use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::metadata::{burn_nft, BurnNft, Metadata};
use anchor_spl::token::{Mint, Token, TokenAccount};
use solana_ibc::chain::ChainData;
use solana_ibc::cpi::accounts::SetStake;
use solana_ibc::program::SolanaIbc;
use solana_ibc::{CHAIN_SEED, TRIE_SEED};

pub mod constants;
mod token;
mod validation;

use constants::{
    REWARDS_SEED, STAKING_PARAMS_SEED, TEST_SEED, VAULT_PARAMS_SEED, VAULT_SEED,
};

declare_id!("8n3FHwYxFgQCQc2FNFkwDUf9mcqupxXcCvgfHbApMLv3");

#[program]
pub mod restaking {

    use super::*;

    pub fn initialize(
        ctx: Context<Initialize>,
        whitelisted_tokens: Vec<Pubkey>,
        staking_cap: u128,
    ) -> Result<()> {
        let staking_params = &mut ctx.accounts.staking_params;

        staking_params.admin = ctx.accounts.admin.key();
        staking_params.whitelisted_tokens = whitelisted_tokens;
        staking_params.guest_chain_program_id = None;
        staking_params.staking_cap = staking_cap;
        staking_params.rewards_token_mint =
            ctx.accounts.rewards_token_mint.key();

        Ok(())
    }

    /// Stakes the amount in the vault and if guest chain is initialized, a CPI call to the service is being
    /// made to update the stake.
    ///
    /// We are sending the accounts needed for making CPI call to guest blockchain as [`remaining_accounts`]
    /// since we were running out of stack memory. Note that these accounts dont need to be sent until the
    /// guest chain is initialized since CPI calls wont be made during that period.
    /// Since remaining accounts are not named, they have to be
    /// sent in the same order as given below
    /// - Chain Data
    /// - trie
    /// - Guest blockchain program ID
    pub fn deposit<'a, 'info>(
        ctx: Context<'a, 'a, 'a, 'info, Deposit<'info>>,
        service: Service,
        amount: u64,
    ) -> Result<()> {
        let vault_params = &mut ctx.accounts.vault_params;
        let staking_params = &mut ctx.accounts.staking_params;

        msg!(
            "These are whitelisted tokens {:?} {}",
            staking_params.whitelisted_tokens,
            ctx.accounts.token_mint.key()
        );

        staking_params
            .whitelisted_tokens
            .iter()
            .find(|&&token_mint| token_mint == ctx.accounts.token_mint.key())
            .ok_or(error!(ErrorCodes::TokenNotWhitelisted))?;

        staking_params.total_deposited_amount += amount as u128;
        if staking_params.total_deposited_amount > staking_params.staking_cap {
            return Err(error!(ErrorCodes::StakingCapExceeded));
        }

        let current_time = Clock::get()?.unix_timestamp;
        let guest_chain_program_id = staking_params.guest_chain_program_id;

        vault_params.service =
            if guest_chain_program_id.is_some() { Some(service) } else { None };
        vault_params.stake_timestamp_sec = current_time;
        vault_params.stake_amount = amount;
        vault_params.stake_mint = ctx.accounts.token_mint.key();
        vault_params.last_received_rewards_height = 0;

        // Transfer tokens to escrow

        token::transfer(ctx.accounts.into(), &[], amount)?;

        // Mint receipt tokens
        token::mint_nft(ctx.accounts.into())?;

        // Call Guest chain program to update the stake if the chain is initialized
        if guest_chain_program_id.is_some() {
            let validator_key = match service {
                Service::GuestChain { validator } => validator,
            };
            let borrowed_chain_data =
                ctx.remaining_accounts[0].data.try_borrow().unwrap();
            let mut chain_data: &[u8] = &borrowed_chain_data;
            let chain =
                solana_ibc::chain::ChainData::try_deserialize(&mut chain_data)
                    .unwrap();
            let validator = chain
                .validator(validator_key)
                .map_err(|_| ErrorCodes::OperationNotAllowed)?;
            let amount = validator.map_or(u128::from(amount), |val| {
                u128::from(val.stake) + u128::from(amount)
            });
            validation::validate_remaining_accounts(
                ctx.remaining_accounts,
                &guest_chain_program_id.unwrap(),
            )?;
            core::mem::drop(borrowed_chain_data);
            let bump = ctx.bumps.staking_params;
            let seeds =
                [STAKING_PARAMS_SEED, TEST_SEED, core::slice::from_ref(&bump)];
            let seeds = seeds.as_ref();
            let seeds = core::slice::from_ref(&seeds);
            let cpi_accounts = SetStake {
                sender: ctx.accounts.depositor.to_account_info(),
                chain: ctx.remaining_accounts[0].clone(),
                trie: ctx.remaining_accounts[1].clone(),
                system_program: ctx.accounts.system_program.to_account_info(),
                instruction: ctx.accounts.instruction.to_account_info(),
            };
            let cpi_program = ctx.remaining_accounts[2].clone();
            let cpi_ctx =
                CpiContext::new_with_signer(cpi_program, cpi_accounts, seeds);
            solana_ibc::cpi::set_stake(cpi_ctx, validator_key, amount)?;
        }

        Ok(())
    }

    pub fn withdraw(ctx: Context<Withdraw>) -> Result<()> {
        let vault_params = &mut ctx.accounts.vault_params;
        let staking_params = &mut ctx.accounts.staking_params;
        let stake_token_mint = ctx.accounts.token_mint.key();

        if staking_params.guest_chain_program_id.is_none() {
            return Err(error!(ErrorCodes::OperationNotAllowed));
        }

        if stake_token_mint != vault_params.stake_mint {
            return Err(error!(ErrorCodes::InvalidTokenMint));
        }

        let chain = &ctx.accounts.guest_chain;
        let service = vault_params
            .service
            .as_ref()
            .ok_or(error!(ErrorCodes::MissingService))?;
        let validator_key = match service {
            Service::GuestChain { validator } => validator,
        };

        let amount = vault_params.stake_amount;
        staking_params.total_deposited_amount -= amount as u128;

        /*
         * Get the rewards from guest blockchain.
         */

        let (rewards, _current_height) = chain.calculate_rewards(
            vault_params.last_received_rewards_height,
            *validator_key,
            vault_params.stake_amount,
        )?;

        let bump = ctx.bumps.staking_params;
        let seeds =
            [STAKING_PARAMS_SEED, TEST_SEED, core::slice::from_ref(&bump)];
        let seeds = seeds.as_ref();
        let seeds = core::slice::from_ref(&seeds);

        // Call Guest chain to update the stake
        let validator_key = match service {
            Service::GuestChain { validator } => validator,
        };
        let validator = chain
            .candidate(*validator_key)
            .map_err(|_| ErrorCodes::OperationNotAllowed)?
            .ok_or(ErrorCodes::MissingService)?;
        let validator_stake = u128::from(validator.stake)
            .checked_sub(u128::from(amount))
            .ok_or(ErrorCodes::SubtractionOverflow)?;
        let cpi_accounts = SetStake {
            sender: ctx.accounts.withdrawer.to_account_info(),
            chain: chain.to_account_info(),
            trie: ctx.accounts.trie.to_account_info(),
            system_program: ctx.accounts.system_program.to_account_info(),
            instruction: ctx.accounts.instruction.to_account_info(),
        };
        let cpi_program = ctx.accounts.guest_chain_program.to_account_info();
        let cpi_ctx =
            CpiContext::new_with_signer(cpi_program, cpi_accounts, seeds);
        solana_ibc::cpi::set_stake(cpi_ctx, *validator_key, validator_stake)?;

        // Transfer rewards from platform wallet
        token::transfer(
            token::TransferAccounts {
                from: ctx
                    .accounts
                    .platform_rewards_token_account
                    .to_account_info(),
                to: ctx
                    .accounts
                    .depositor_rewards_token_account
                    .to_account_info(),
                authority: ctx.accounts.staking_params.to_account_info(),
                token_program: ctx.accounts.token_program.to_account_info(),
            },
            seeds,
            rewards,
        )?;

        // Transfer tokens from escrow
        token::transfer(ctx.accounts.into(), seeds, amount)?;

        // Burn receipt token
        burn_nft(
            CpiContext::new(
                ctx.accounts.metadata_program.to_account_info(),
                BurnNft {
                    metadata: ctx.accounts.nft_metadata.to_account_info(),
                    owner: ctx.accounts.withdrawer.to_account_info(),
                    spl_token: ctx.accounts.token_program.to_account_info(),
                    mint: ctx.accounts.receipt_token_mint.to_account_info(),
                    token: ctx.accounts.receipt_token_account.to_account_info(),
                    edition: ctx
                        .accounts
                        .master_edition_account
                        .to_account_info(),
                },
            ),
            None,
        )?;

        Ok(())
    }

    /// Whitelists new tokens
    ///
    /// This method checks if any of the new token mints which are to be whitelisted
    /// are already whitelisted. If they are the method fails to update the
    /// whitelisted token list.
    pub fn update_token_whitelist(
        ctx: Context<UpdateStakingParams>,
        new_token_mints: Vec<Pubkey>,
    ) -> Result<()> {
        let staking_params = &mut ctx.accounts.staking_params;

        let contains_mint = new_token_mints.iter().any(|token_mint| {
            staking_params.whitelisted_tokens.contains(token_mint)
        });

        if contains_mint {
            return Err(error!(ErrorCodes::TokenAlreadyWhitelisted));
        }

        staking_params
            .whitelisted_tokens
            .append(&mut new_token_mints.as_slice().to_vec());

        Ok(())
    }

    /// Sets guest chain program ID
    ///
    /// After this method is called, CPI calls would be made to guest chain during deposit and stake would be
    /// set to the validators. Users can also claim rewards or withdraw their stake
    /// when the chain is initialized.
    pub fn update_guest_chain_initialization(
        ctx: Context<UpdateStakingParams>,
        guest_chain_program_id: Pubkey,
    ) -> Result<()> {
        let staking_params = &mut ctx.accounts.staking_params;
        if staking_params.guest_chain_program_id.is_some() {
            return Err(error!(ErrorCodes::GuestChainAlreadyInitialized));
        }
        staking_params.guest_chain_program_id = Some(guest_chain_program_id);

        Ok(())
    }

    /// Updating admin proposal created by the existing admin. Admin would only be changed
    /// if the new admin accepts it in `accept_admin_change` instruction.
    pub fn change_admin_proposal(
        ctx: Context<UpdateStakingParams>,
        new_admin: Pubkey,
    ) -> Result<()> {
        let staking_params = &mut ctx.accounts.staking_params;
        msg!(
            "Proposal for changing Admin from {} to {}",
            staking_params.admin,
            new_admin
        );

        staking_params.new_admin_proposal = Some(new_admin);
        Ok(())
    }

    /// Accepting new admin change signed by the proposed admin. Admin would be changed if the
    /// proposed admin calls the method. Would fail if there is no proposed admin and if the
    /// signer is not the proposed admin.
    pub fn accept_admin_change(ctx: Context<UpdateAdmin>) -> Result<()> {
        let staking_params = &mut ctx.accounts.staking_params;
        let new_admin = staking_params
            .new_admin_proposal
            .ok_or(ErrorCodes::NoProposedAdmin)?;
        if new_admin != ctx.accounts.new_admin.key() {
            return Err(error!(ErrorCode::ConstraintSigner));
        }

        msg!(
            "Changing Admin from {} to {}",
            staking_params.admin,
            staking_params.new_admin_proposal.unwrap()
        );
        staking_params.admin = new_admin;

        Ok(())
    }

    pub fn claim_rewards(ctx: Context<Claim>) -> Result<()> {
        let staking_params = &ctx.accounts.staking_params;

        if staking_params.guest_chain_program_id.is_none() {
            return Err(error!(ErrorCodes::OperationNotAllowed));
        }

        let token_account = &ctx.accounts.receipt_token_account;
        if token_account.amount < 1 {
            return Err(error!(ErrorCodes::InsufficientReceiptTokenBalance));
        }

        let vault_params = &mut ctx.accounts.vault_params;
        let chain = &ctx.accounts.guest_chain;

        let service = vault_params
            .service
            .as_ref()
            .ok_or(error!(ErrorCodes::MissingService))?;
        let validator_key = match service {
            Service::GuestChain { validator } => validator,
        };
        let stake_amount = vault_params.stake_amount;
        let last_received_rewards_height =
            vault_params.last_received_rewards_height;

        /*
         * Get the rewards from guest blockchain.
         */

        let (rewards, current_height) = chain.calculate_rewards(
            last_received_rewards_height,
            *validator_key,
            stake_amount,
        )?;

        msg!(
            "Current height {}, last claimed height {}",
            current_height,
            vault_params.last_received_rewards_height
        );
        vault_params.last_received_rewards_height = current_height;

        /*
         * Get the current price of rewards token mint from the oracle
         */

        let bump = ctx.bumps.staking_params;
        let seeds =
            [STAKING_PARAMS_SEED, TEST_SEED, core::slice::from_ref(&bump)];
        let seeds = seeds.as_ref();
        let seeds = core::slice::from_ref(&seeds);

        // Transfer the tokens from the platfrom rewards token account to the user token account
        token::transfer(ctx.accounts.into(), seeds, rewards)?;

        Ok(())
    }

    /// This method sets the service for the stake which was deposited before guest chain
    /// initialization
    ///
    /// This method can only be called if the service was not set during the depositing and
    /// can only be called once. Calling otherwise would panic.
    ///
    /// The accounts for CPI are sent as remaining accounts similar to `deposit` method.
    pub fn set_service<'a, 'info>(
        ctx: Context<'a, 'a, 'a, 'info, SetService<'info>>,
        service: Service,
    ) -> Result<()> {
        let vault_params = &mut ctx.accounts.vault_params;
        let staking_params = &mut ctx.accounts.staking_params;
        let guest_chain = &ctx.remaining_accounts[0];

        let token_account = &ctx.accounts.receipt_token_account;
        if token_account.amount < 1 {
            return Err(error!(ErrorCodes::InsufficientReceiptTokenBalance));
        }
        if staking_params.guest_chain_program_id.is_none() {
            return Err(error!(ErrorCodes::OperationNotAllowed));
        }
        if vault_params.service.is_some() {
            return Err(error!(ErrorCodes::ServiceAlreadySet));
        }

        vault_params.service = Some(service);

        let guest_chain_program_id =
            staking_params.guest_chain_program_id.unwrap(); // Infallible
        let amount = vault_params.stake_amount;

        validation::validate_remaining_accounts(
            ctx.remaining_accounts,
            &guest_chain_program_id,
        )?;

        let validator_key = match service {
            Service::GuestChain { validator } => validator,
        };
        let borrowed_chain_data = guest_chain.data.try_borrow().unwrap();
        let mut chain_data: &[u8] = &borrowed_chain_data;
        let chain =
            solana_ibc::chain::ChainData::try_deserialize(&mut chain_data)
                .unwrap();
        let validator = chain
            .validator(validator_key)
            .map_err(|_| ErrorCodes::OperationNotAllowed)?;
        let amount = validator.map_or(u128::from(amount), |val| {
            u128::from(val.stake) + u128::from(amount)
        });
        // Drop refcount on chain data so we can use it in CPI call
        core::mem::drop(borrowed_chain_data);

        let bump = ctx.bumps.staking_params;
        let seeds =
            [STAKING_PARAMS_SEED, TEST_SEED, core::slice::from_ref(&bump)];
        let seeds = seeds.as_ref();
        let seeds = core::slice::from_ref(&seeds);
        let cpi_accounts = SetStake {
            sender: ctx.accounts.depositor.to_account_info(),
            chain: guest_chain.to_account_info(),
            trie: ctx.remaining_accounts[1].clone(),
            system_program: ctx.accounts.system_program.to_account_info(),
            instruction: ctx.accounts.instruction.to_account_info(),
        };
        let cpi_program = ctx.remaining_accounts[2].clone();
        let cpi_ctx =
            CpiContext::new_with_signer(cpi_program, cpi_accounts, seeds);
        solana_ibc::cpi::set_stake(cpi_ctx, validator_key, amount)?;

        Ok(())
    }

    /// This method would only be called by `Admin` to withdraw all the funds from the rewards account
    ///
    /// This would usually be called when a wrong amount of funds are transferred in the rewards account.
    /// This is a safety measure and should only be called on emergency.
    pub fn withdraw_reward_funds(
        ctx: Context<WithdrawRewardFunds>,
    ) -> Result<()> {
        msg!(
            "Transferring all the funds from rewards token account to admin \
             account"
        );

        let rewards_balance = ctx.accounts.rewards_token_account.amount;

        let bump = ctx.bumps.staking_params;
        let seeds =
            [STAKING_PARAMS_SEED, TEST_SEED, core::slice::from_ref(&bump)];
        let seeds = seeds.as_ref();
        let seeds = core::slice::from_ref(&seeds);

        token::transfer(ctx.accounts.into(), seeds, rewards_balance)?;

        Ok(())
    }

    pub fn update_staking_cap(
        ctx: Context<UpdateStakingParams>,
        new_staking_cap: u128,
    ) -> Result<()> {
        let staking_params = &mut ctx.accounts.staking_params;

        if staking_params.staking_cap >= new_staking_cap {
            return Err(error!(
                ErrorCodes::NewStakingCapShouldBeMoreThanExistingOne
            ));
        }

        staking_params.staking_cap = new_staking_cap;

        Ok(())
    }
}

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(init, payer = admin, seeds = [STAKING_PARAMS_SEED, TEST_SEED], bump, space = 1024)]
    pub staking_params: Account<'info, StakingParams>,

    pub rewards_token_mint: Account<'info, Mint>,
    #[account(init, payer = admin, seeds = [REWARDS_SEED, TEST_SEED], bump, token::mint = rewards_token_mint, token::authority = staking_params)]
    pub rewards_token_account: Account<'info, TokenAccount>,

    token_program: Program<'info, Token>,
    system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Deposit<'info> {
    #[account(mut)]
    pub depositor: Signer<'info>,

    #[account(init, payer = depositor, seeds = [VAULT_PARAMS_SEED, receipt_token_mint.key().as_ref()], bump, space = 8 + 1024)]
    pub vault_params: Box<Account<'info, Vault>>,
    #[account(mut, seeds = [STAKING_PARAMS_SEED, TEST_SEED], bump)]
    pub staking_params: Box<Account<'info, StakingParams>>,

    /// Only token mint with 9 decimals can be staked for now since
    /// the guest chain expects that.  If a whitelisted token has 6
    /// decimals, it would just be invalid.
    #[account(mut, mint::decimals = 9)]
    pub token_mint: Box<Account<'info, Mint>>,
    #[account(mut, token::mint = token_mint, token::authority = depositor.key())]
    pub depositor_token_account: Box<Account<'info, TokenAccount>>,

    #[account(init_if_needed, payer = depositor, seeds = [VAULT_SEED, token_mint.key().as_ref()], bump, token::mint = token_mint, token::authority = staking_params)]
    pub vault_token_account: Box<Account<'info, TokenAccount>>,

    #[account(
        init,
        payer = depositor,
        mint::decimals = 0,
        mint::authority = depositor,
        mint::freeze_authority = depositor,
    )]
    pub receipt_token_mint: Box<Account<'info, Mint>>,
    #[account(init, payer = depositor, associated_token::mint = receipt_token_mint, associated_token::authority = depositor)]
    pub receipt_token_account: Box<Account<'info, TokenAccount>>,

    pub metadata_program: Program<'info, Metadata>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,

    #[account(address = solana_program::sysvar::instructions::ID)]
    ///CHECK:   
    pub instruction: AccountInfo<'info>,

    #[account(
        mut,
        seeds = [
            b"metadata".as_ref(),
            metadata_program.key().as_ref(),
            receipt_token_mint.key().as_ref(),
            b"edition".as_ref(),
        ],
        bump,
        seeds::program = metadata_program.key()
    )]
    /// CHECK:
    pub master_edition_account: UncheckedAccount<'info>,
    #[account(
        mut,
        seeds = [
            b"metadata".as_ref(),
            metadata_program.key().as_ref(),
            receipt_token_mint.key().as_ref(),
        ],
        bump,
        seeds::program = metadata_program.key()
    )]
    /// CHECK:
    pub nft_metadata: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct Withdraw<'info> {
    #[account(mut)]
    pub withdrawer: Signer<'info>,

    #[account(mut, close = withdrawer, seeds = [VAULT_PARAMS_SEED, receipt_token_mint.key().as_ref()], bump)]
    pub vault_params: Box<Account<'info, Vault>>,
    #[account(mut, seeds = [STAKING_PARAMS_SEED, TEST_SEED], bump, has_one = rewards_token_mint)]
    pub staking_params: Box<Account<'info, StakingParams>>,

    #[account(mut, seeds = [CHAIN_SEED], bump, seeds::program = guest_chain_program.key())]
    pub guest_chain: Box<Account<'info, ChainData>>,
    #[account(mut, seeds = [TRIE_SEED], bump, seeds::program = guest_chain_program.key())]
    /// CHECK:
    pub trie: AccountInfo<'info>,

    pub token_mint: Box<Account<'info, Mint>>,
    #[account(mut, token::mint = token_mint, token::authority = withdrawer.key())]
    pub withdrawer_token_account: Box<Account<'info, TokenAccount>>,

    #[account(mut, seeds = [VAULT_SEED, token_mint.key().as_ref()], bump, token::mint = token_mint, token::authority = staking_params)]
    pub vault_token_account: Box<Account<'info, TokenAccount>>,

    pub rewards_token_mint: Box<Account<'info, Mint>>,
    #[account(init_if_needed, payer = withdrawer, associated_token::mint = rewards_token_mint, associated_token::authority = withdrawer)]
    pub depositor_rewards_token_account: Box<Account<'info, TokenAccount>>,

    #[account(mut, seeds = [REWARDS_SEED, TEST_SEED], bump, token::mint = rewards_token_mint, token::authority = staking_params)]
    pub platform_rewards_token_account: Box<Account<'info, TokenAccount>>,

    #[account(
        mut,
        mint::decimals = 0,
        mint::authority = master_edition_account,
        // mint::freeze_authority = withdrawer,
    )]
    pub receipt_token_mint: Box<Account<'info, Mint>>,
    #[account(mut, token::mint = receipt_token_mint, token::authority = withdrawer)]
    pub receipt_token_account: Box<Account<'info, TokenAccount>>,

    pub guest_chain_program: Program<'info, SolanaIbc>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
    pub metadata_program: Program<'info, Metadata>,
    pub rent: Sysvar<'info, Rent>,
    #[account(
        mut,
        seeds = [
            b"metadata".as_ref(),
            metadata_program.key().as_ref(),
            receipt_token_mint.key().as_ref(),
            b"edition".as_ref(),
        ],
        bump,
        seeds::program = metadata_program.key()
    )]
    /// CHECK:
    pub master_edition_account: UncheckedAccount<'info>,
    #[account(
        mut,
        seeds = [
            b"metadata".as_ref(),
            metadata_program.key().as_ref(),
            receipt_token_mint.key().as_ref(),
        ],
        bump,
        seeds::program = metadata_program.key()
    )]
    /// CHECK:
    pub nft_metadata: UncheckedAccount<'info>,

    #[account(address = solana_program::sysvar::instructions::ID)]
    /// CHECK:
    pub instruction: AccountInfo<'info>,
}

#[derive(Accounts)]
pub struct UpdateStakingParams<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(mut, seeds = [STAKING_PARAMS_SEED, TEST_SEED], bump, has_one = admin)]
    pub staking_params: Account<'info, StakingParams>,
}

#[derive(Accounts)]
pub struct UpdateAdmin<'info> {
    #[account(mut)]
    pub new_admin: Signer<'info>,

    /// Validation would be done in the method
    #[account(mut, seeds = [STAKING_PARAMS_SEED, TEST_SEED], bump)]
    pub staking_params: Account<'info, StakingParams>,
}

#[derive(Accounts)]
pub struct Claim<'info> {
    #[account(mut)]
    pub claimer: Signer<'info>,

    #[account(mut, seeds = [VAULT_PARAMS_SEED, receipt_token_mint.key().as_ref()], bump)]
    pub vault_params: Box<Account<'info, Vault>>,
    #[account(mut, seeds = [STAKING_PARAMS_SEED, TEST_SEED], bump, has_one = rewards_token_mint)]
    pub staking_params: Box<Account<'info, StakingParams>>,

    #[account(mut, seeds = [CHAIN_SEED], bump, seeds::program = guest_chain_program.key())]
    pub guest_chain: Box<Account<'info, ChainData>>,

    pub rewards_token_mint: Box<Account<'info, Mint>>,
    #[account(init_if_needed, payer = claimer, associated_token::mint = rewards_token_mint, associated_token::authority = claimer)]
    pub depositor_rewards_token_account: Box<Account<'info, TokenAccount>>,

    #[account(mut, seeds = [REWARDS_SEED, TEST_SEED], bump, token::mint = rewards_token_mint, token::authority = staking_params)]
    pub platform_rewards_token_account: Box<Account<'info, TokenAccount>>,

    #[account(mut, mint::decimals = 0)]
    pub receipt_token_mint: Box<Account<'info, Mint>>,
    #[account(mut, token::mint = receipt_token_mint, token::authority = claimer)]
    pub receipt_token_account: Box<Account<'info, TokenAccount>>,

    pub guest_chain_program: Program<'info, SolanaIbc>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct SetService<'info> {
    #[account(mut)]
    depositor: Signer<'info>,

    #[account(mut, seeds = [VAULT_PARAMS_SEED, receipt_token_mint.key().as_ref()], bump, has_one = stake_mint)]
    pub vault_params: Box<Account<'info, Vault>>,
    #[account(mut, seeds = [STAKING_PARAMS_SEED, TEST_SEED], bump)]
    pub staking_params: Box<Account<'info, StakingParams>>,

    #[account(mut, mint::decimals = 0)]
    pub receipt_token_mint: Box<Account<'info, Mint>>,
    #[account(mut, token::mint = receipt_token_mint, token::authority = depositor)]
    pub receipt_token_account: Box<Account<'info, TokenAccount>>,

    #[account(mut)]
    pub stake_mint: Account<'info, Mint>,

    ///CHECK:   
    #[account(address = solana_program::sysvar::instructions::ID)]
    pub instruction: AccountInfo<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct WithdrawRewardFunds<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(mut, seeds = [STAKING_PARAMS_SEED, TEST_SEED], bump, has_one = rewards_token_mint, has_one = admin)]
    pub staking_params: Box<Account<'info, StakingParams>>,

    pub rewards_token_mint: Account<'info, Mint>,
    #[account(mut, seeds = [REWARDS_SEED, TEST_SEED], bump, token::mint = rewards_token_mint, token::authority = staking_params)]
    pub rewards_token_account: Account<'info, TokenAccount>,

    pub admin_rewards_token_account: Account<'info, TokenAccount>,

    token_program: Program<'info, Token>,
}

#[account]
#[derive(InitSpace)]
pub struct StakingParams {
    pub admin: Pubkey,
    #[max_len(20)]
    pub whitelisted_tokens: Vec<Pubkey>,
    /// None means the guest chain is not initialized yet.
    pub guest_chain_program_id: Option<Pubkey>,
    pub rewards_token_mint: Pubkey,
    // None means there is not staking cap
    pub staking_cap: u128,
    pub total_deposited_amount: u128,
    pub new_admin_proposal: Option<Pubkey>,
}

/// Unused for now
#[derive(AnchorDeserialize, AnchorSerialize, Clone, Debug, Copy)]
pub enum Service {
    GuestChain { validator: Pubkey },
}

#[account]
pub struct Vault {
    pub stake_timestamp_sec: i64,
    // Program to which the amount is staked
    // unused for now
    pub service: Option<Service>,
    pub stake_amount: u64,
    pub stake_mint: Pubkey,
    /// is 0 initially
    pub last_received_rewards_height: u64,
}

#[error_code]
pub enum ErrorCodes {
    #[msg("Token is already whitelisted")]
    TokenAlreadyWhitelisted,
    #[msg("Can only stake whitelisted tokens")]
    TokenNotWhitelisted,
    #[msg(
        "This operation is not allowed until the guest chain is initialized"
    )]
    OperationNotAllowed,
    #[msg("Subtraction overflow")]
    SubtractionOverflow,
    #[msg("Invalid Token Mint")]
    InvalidTokenMint,
    #[msg("Insufficient receipt token balance, expected balance 1")]
    InsufficientReceiptTokenBalance,
    #[msg(
        "Service is missing. Make sure you have assigned your stake to a \
         service"
    )]
    MissingService,
    #[msg(
        "Staking cap has reached. You can stake only when the staking cap is \
         increased"
    )]
    StakingCapExceeded,
    #[msg("New staking cap should be more than existing one")]
    NewStakingCapShouldBeMoreThanExistingOne,
    #[msg("Guest chain can only be initialized once")]
    GuestChainAlreadyInitialized,
    #[msg("Account validation for CPI call to the guest chain")]
    AccountValidationFailedForCPI,
    #[msg("Service is already set.")]
    ServiceAlreadySet,
    #[msg("There is no proposal for changing admin")]
    NoProposedAdmin,
}
