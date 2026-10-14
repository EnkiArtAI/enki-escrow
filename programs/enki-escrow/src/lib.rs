use anchor_lang::prelude::*;
use anchor_lang::solana_program::{program_option::COption, program_pack::Pack};
use anchor_spl::{
    associated_token::{self, get_associated_token_address, AssociatedToken, Create},
    token::{self, CloseAccount, Mint, Token, TokenAccount, TransferChecked},
};

declare_id!("2sfm76FMcfqUYCv47J4VCNFMrp6PpfD6QYpmWqBCPyJV");

#[cfg(all(feature = "devnet", feature = "mainnet"))]
compile_error!("Select exactly one cluster: devnet or mainnet.");
#[cfg(not(any(feature = "devnet", feature = "mainnet")))]
compile_error!("Select a cluster: devnet or mainnet.");

#[cfg(feature = "mainnet")]
pub const USDC_MINT: Pubkey = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
#[cfg(not(feature = "mainnet"))]
pub const USDC_MINT: Pubkey = pubkey!("4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU");

pub const MAX_UNITS: u8 = 24;
pub const MIN_TTL_S: i64 = 600;
pub const MAX_TTL_S: i64 = 1_800;
pub const MAX_REFUND_ATA_FEE_MICRO: u64 = 1_000_000;
pub const USDC_DECIMALS: u8 = 6;
pub const ESCROW_VERSION: u8 = 2;
pub const RECIPIENT_SLOTS: usize = 4;

#[program]
pub mod enki_escrow {
    use super::*;

    pub fn init_config(ctx: Context<InitConfig>, args: ConfigArgs) -> Result<()> {
        args.validate()?;
        ctx.accounts
            .config
            .set_inner(args.into_config(ctx.bumps.config));
        Ok(())
    }

    pub fn update_config(ctx: Context<UpdateConfig>, args: ConfigArgs) -> Result<()> {
        args.validate()?;
        let bump = ctx.accounts.config.bump;
        ctx.accounts.config.set_inner(args.into_config(bump));
        Ok(())
    }

    // Guardian actions only reduce authority. Unpausing or replacing an operator needs admin.
    pub fn pause(ctx: Context<EmergencyControl>) -> Result<()> {
        ctx.accounts.config.paused = true;
        Ok(())
    }

    pub fn revoke_operator(ctx: Context<EmergencyControl>) -> Result<()> {
        ctx.accounts.config.operator = Pubkey::default();
        Ok(())
    }

    pub fn deposit(
        ctx: Context<Deposit>,
        intent_id: [u8; 16],
        units: u8,
        recipient_owners: [Pubkey; 4],
        unit_amounts: [u64; 4],
        expires_at: i64,
    ) -> Result<()> {
        let config = &ctx.accounts.config;
        require!(!config.paused, EscrowError::Paused);
        require_keys_eq!(
            recipient_owners[0],
            config.treasury_owner,
            EscrowError::WrongTreasury
        );
        let amount = deposit_amount(units, unit_amounts, config.max_deposit_micro)?;
        let recipient_count = validate_recipients(recipient_owners, unit_amounts)?;
        let created_at = Clock::get()?.unix_timestamp;
        validate_expiry(created_at, expires_at, config.min_ttl_s, config.max_ttl_s)?;
        require!(
            ctx.accounts.buyer_ata.delegate == COption::None,
            EscrowError::DelegatedSource
        );

        token::transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                TransferChecked {
                    from: ctx.accounts.buyer_ata.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.vault.to_account_info(),
                    authority: ctx.accounts.buyer.to_account_info(),
                },
            ),
            amount,
            USDC_DECIMALS,
        )?;
        ctx.accounts.escrow.set_inner(Escrow {
            version: ESCROW_VERSION,
            state: EscrowState::Funded,
            bump: ctx.bumps.escrow,
            vault_bump: ctx.bumps.vault,
            intent_id,
            buyer: ctx.accounts.buyer.key(),
            mint: ctx.accounts.mint.key(),
            rent_payer: ctx.accounts.rent_payer.key(),
            recipient_count,
            recipient_owners,
            unit_amounts,
            units,
            settled_units: 0,
            created_at,
            expires_at,
            refund_due: amount,
            paid_amounts: [0; RECIPIENT_SLOTS],
        });
        emit!(Deposited {
            escrow: ctx.accounts.escrow.key(),
            buyer: ctx.accounts.buyer.key(),
            intent_id,
            units,
            amount,
            expires_at,
        });
        Ok(())
    }

    pub fn settle<'info>(ctx: Context<'_, '_, '_, 'info, Settle<'info>>, k: u8) -> Result<()> {
        let escrow = &ctx.accounts.escrow;
        require!(escrow.version == ESCROW_VERSION, EscrowError::WrongVersion);
        require!(
            escrow.state == EscrowState::Funded,
            EscrowError::AlreadySettled
        );
        require!(
            Clock::get()?.unix_timestamp < escrow.expires_at,
            EscrowError::Expired
        );
        require!(k <= escrow.units, EscrowError::TooManyDeliveredUnits);
        let count = validate_recipients(escrow.recipient_owners, escrow.unit_amounts)?;
        require!(
            count == escrow.recipient_count,
            EscrowError::RecipientMismatch
        );
        require!(
            ctx.remaining_accounts.len() == usize::from(count),
            EscrowError::RecipientMismatch
        );
        let treasury_paid = checked_product(k, escrow.unit_amounts[0])?;
        let mut paid_amounts = [0; RECIPIENT_SLOTS];
        paid_amounts[0] = treasury_paid;
        for (i, recipient) in ctx.remaining_accounts.iter().enumerate() {
            let owner = escrow.recipient_owners[i];
            if escrow.unit_amounts[i] == 0 {
                // Sparse slots retain their position, with the System Program as placeholder.
                require_keys_eq!(
                    *recipient.key,
                    Pubkey::default(),
                    EscrowError::RecipientMismatch
                );
                continue;
            }
            require_keys_eq!(
                *recipient.key,
                get_associated_token_address(&owner, &escrow.mint),
                EscrowError::NonCanonicalAta
            );
            require!(recipient.is_writable, EscrowError::RecipientMismatch);
            if i == 0 {
                require!(
                    canonical_token_status(recipient, owner, escrow.mint)? == TokenStatus::Ready,
                    EscrowError::FrozenTreasuryAta
                );
            } else {
                // An unusable recipient forfeits only its own leg, never the provider payment.
                let available = canonical_token_status(recipient, owner, escrow.mint)
                    .unwrap_or(TokenStatus::Frozen)
                    == TokenStatus::Ready;
                if available {
                    paid_amounts[i] = checked_product(k, escrow.unit_amounts[i])?;
                }
            }
        }
        let paid = paid_amounts.iter().try_fold(0_u64, |total, amount| {
            total.checked_add(*amount).ok_or(EscrowError::Overflow)
        })?;
        let refund_due = escrow
            .refund_due
            .checked_sub(paid)
            .ok_or(EscrowError::Overflow)?;
        require!(
            ctx.accounts.vault.amount >= paid,
            EscrowError::InsufficientVault
        );

        let buyer = escrow.buyer;
        let intent_id = escrow.intent_id;
        let bump = [escrow.bump];
        let seeds: &[&[u8]] = &[b"escrow", buyer.as_ref(), &intent_id, &bump];
        let signer = &[seeds];
        for (recipient, amount) in ctx.remaining_accounts.iter().zip(paid_amounts) {
            transfer_from_vault(
                ctx.accounts.token_program.to_account_info(),
                ctx.accounts.vault.to_account_info(),
                ctx.accounts.mint.to_account_info(),
                recipient.clone(),
                ctx.accounts.escrow.to_account_info(),
                signer,
                amount,
            )?;
        }
        let escrow = &mut ctx.accounts.escrow;
        escrow.state = EscrowState::Settled;
        escrow.settled_units = k;
        escrow.refund_due = refund_due;
        escrow.paid_amounts = paid_amounts;
        emit!(Settled {
            escrow: escrow.key(),
            k,
            paid: paid_amounts
        });
        Ok(())
    }

    pub fn reclaim(ctx: Context<Reclaim>) -> Result<()> {
        let escrow = &ctx.accounts.escrow;
        require!(escrow.version == ESCROW_VERSION, EscrowError::WrongVersion);
        require!(
            escrow.state == EscrowState::Settled
                || Clock::get()?.unix_timestamp >= escrow.expires_at,
            EscrowError::NotReclaimable
        );
        let remaining = ctx.accounts.vault.amount.min(escrow.refund_due);
        let surplus = ctx
            .accounts
            .vault
            .amount
            .checked_sub(remaining)
            .ok_or(EscrowError::Overflow)?;
        let missing = if escrow.refund_due > 0 {
            let buyer_status = canonical_token_status(
                &ctx.accounts.buyer_ata.to_account_info(),
                escrow.buyer,
                escrow.mint,
            )?;
            require!(
                buyer_status != TokenStatus::Frozen,
                EscrowError::FrozenBuyerAta
            );
            buyer_status == TokenStatus::Missing
        } else {
            false
        };
        if remaining > 0 && missing {
            require!(
                ctx.accounts.rent_payer.is_signer,
                EscrowError::RentPayerMustSign
            );
        }
        let (refund, ata_fee, create_ata) =
            refund_amounts(remaining, missing, ctx.accounts.config.refund_ata_fee_micro);
        let treasury_refund = ata_fee.checked_add(surplus).ok_or(EscrowError::Overflow)?;
        if create_ata {
            associated_token::create(CpiContext::new(
                ctx.accounts.associated_token_program.to_account_info(),
                Create {
                    payer: ctx.accounts.rent_payer.to_account_info(),
                    associated_token: ctx.accounts.buyer_ata.to_account_info(),
                    authority: ctx.accounts.buyer.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    system_program: ctx.accounts.system_program.to_account_info(),
                    token_program: ctx.accounts.token_program.to_account_info(),
                },
            ))?;
        }
        let buyer = escrow.buyer;
        let intent_id = escrow.intent_id;
        let bump = [escrow.bump];
        let seeds: &[&[u8]] = &[b"escrow", buyer.as_ref(), &intent_id, &bump];
        let signer = &[seeds];
        transfer_from_vault(
            ctx.accounts.token_program.to_account_info(),
            ctx.accounts.vault.to_account_info(),
            ctx.accounts.mint.to_account_info(),
            ctx.accounts.treasury_ata.to_account_info(),
            ctx.accounts.escrow.to_account_info(),
            signer,
            treasury_refund,
        )?;
        transfer_from_vault(
            ctx.accounts.token_program.to_account_info(),
            ctx.accounts.vault.to_account_info(),
            ctx.accounts.mint.to_account_info(),
            ctx.accounts.buyer_ata.to_account_info(),
            ctx.accounts.escrow.to_account_info(),
            signer,
            refund,
        )?;
        token::close_account(CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            CloseAccount {
                account: ctx.accounts.vault.to_account_info(),
                destination: ctx.accounts.rent_payer.to_account_info(),
                authority: ctx.accounts.escrow.to_account_info(),
            },
            signer,
        ))?;
        emit!(Reclaimed {
            escrow: escrow.key(),
            refund,
            ata_fee
        });
        Ok(())
    }

    pub fn claim_author_vault(
        ctx: Context<ClaimAuthorVault>,
        x_user_id: u64,
        claimant: Pubkey,
    ) -> Result<()> {
        require!(
            claimant != ctx.accounts.config.treasury_owner
                && claimant != ctx.accounts.config.operator
                && claimant != Pubkey::default()
                && claimant != ctx.accounts.author.key(),
            EscrowError::VaultToTreasury
        );
        let claim = &mut ctx.accounts.author_claim;
        if claim.claimant == Pubkey::default() {
            require!(
                ctx.accounts.claimant_wallet.is_signer,
                EscrowError::ClaimantMustSign
            );
            // This record has no close or rebind instruction. init_if_needed may only
            // initialize the empty record; subsequent calls preserve the first wallet.
            claim.version = 1;
            claim.bump = ctx.bumps.author_claim;
            claim.x_user_id = x_user_id;
            claim.claimant = claimant;
            claim.claimed_slot = Clock::get()?.slot;
        } else {
            require!(
                claim.version == 1
                    && claim.bump == ctx.bumps.author_claim
                    && claim.x_user_id == x_user_id,
                EscrowError::WrongVersion
            );
            require_keys_eq!(claim.claimant, claimant, EscrowError::AlreadyClaimed);
        }
        let amount = ctx.accounts.author_ata.amount;
        let id_bytes = x_user_id.to_le_bytes();
        let bump = [ctx.bumps.author];
        let seeds: &[&[u8]] = &[b"author", &id_bytes, &bump];
        transfer_from_vault(
            ctx.accounts.token_program.to_account_info(),
            ctx.accounts.author_ata.to_account_info(),
            ctx.accounts.mint.to_account_info(),
            ctx.accounts.claimant_ata.to_account_info(),
            ctx.accounts.author.to_account_info(),
            &[seeds],
            amount,
        )?;
        emit!(AuthorVaultClaimed {
            x_user_id,
            author: ctx.accounts.author.key(),
            claimant,
            amount,
        });
        Ok(())
    }
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct ConfigArgs {
    pub admin: Pubkey,
    pub guardian: Pubkey,
    pub operator: Pubkey,
    pub treasury_owner: Pubkey,
    pub paused: bool,
    pub max_deposit_micro: u64,
    pub min_ttl_s: i64,
    pub max_ttl_s: i64,
    pub refund_ata_fee_micro: u64,
}

impl ConfigArgs {
    fn validate(&self) -> Result<()> {
        require!(self.admin != Pubkey::default(), EscrowError::InvalidRole);
        require!(self.guardian != Pubkey::default(), EscrowError::InvalidRole);
        require!(
            self.treasury_owner != Pubkey::default(),
            EscrowError::InvalidRole
        );
        require!(self.max_deposit_micro > 0, EscrowError::DepositCap);
        require!(
            self.min_ttl_s >= MIN_TTL_S
                && self.max_ttl_s <= MAX_TTL_S
                && self.min_ttl_s <= self.max_ttl_s,
            EscrowError::InvalidTtl
        );
        require!(
            self.refund_ata_fee_micro <= MAX_REFUND_ATA_FEE_MICRO,
            EscrowError::RefundFeeCap
        );
        Ok(())
    }

    fn into_config(self, bump: u8) -> Config {
        Config {
            admin: self.admin,
            guardian: self.guardian,
            operator: self.operator,
            treasury_owner: self.treasury_owner,
            paused: self.paused,
            max_deposit_micro: self.max_deposit_micro,
            min_ttl_s: self.min_ttl_s,
            max_ttl_s: self.max_ttl_s,
            refund_ata_fee_micro: self.refund_ata_fee_micro,
            bump,
        }
    }
}

#[account]
#[derive(InitSpace)]
pub struct Config {
    pub admin: Pubkey,
    pub guardian: Pubkey,
    pub operator: Pubkey,
    pub treasury_owner: Pubkey,
    pub paused: bool,
    pub max_deposit_micro: u64,
    pub min_ttl_s: i64,
    pub max_ttl_s: i64,
    pub refund_ata_fee_micro: u64,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct Escrow {
    pub version: u8,
    pub state: EscrowState,
    pub bump: u8,
    pub vault_bump: u8,
    pub intent_id: [u8; 16],
    pub buyer: Pubkey,
    pub mint: Pubkey,
    pub rent_payer: Pubkey,
    pub recipient_count: u8,
    pub recipient_owners: [Pubkey; 4],
    pub unit_amounts: [u64; 4],
    pub units: u8,
    pub settled_units: u8,
    pub created_at: i64,
    pub expires_at: i64,
    pub refund_due: u64,
    pub paid_amounts: [u64; 4],
}

#[account]
#[derive(InitSpace)]
pub struct AuthorClaim {
    pub version: u8,
    pub bump: u8,
    pub x_user_id: u64,
    pub claimant: Pubkey,
    pub claimed_slot: u64,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, InitSpace)]
pub enum EscrowState {
    Funded,
    Settled,
}

#[derive(Accounts)]
pub struct InitConfig<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(init, payer = authority, space = 8 + Config::INIT_SPACE, seeds = [b"config"], bump)]
    pub config: Account<'info, Config>,
    #[account(constraint = program.programdata_address()? == Some(program_data.key()) @ EscrowError::WrongProgramData)]
    pub program: Program<'info, crate::program::EnkiEscrow>,
    #[account(constraint = program_data.upgrade_authority_address == Some(authority.key()) @ EscrowError::Unauthorized)]
    pub program_data: Account<'info, ProgramData>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct UpdateConfig<'info> {
    pub admin: Signer<'info>,
    #[account(mut, seeds = [b"config"], bump = config.bump, has_one = admin @ EscrowError::Unauthorized)]
    pub config: Account<'info, Config>,
}

#[derive(Accounts)]
pub struct EmergencyControl<'info> {
    pub authority: Signer<'info>,
    #[account(
        mut, seeds = [b"config"], bump = config.bump,
        constraint = authority.key() == config.admin || authority.key() == config.guardian @ EscrowError::Unauthorized
    )]
    pub config: Account<'info, Config>,
}

#[derive(Accounts)]
#[instruction(intent_id: [u8; 16])]
pub struct Deposit<'info> {
    #[account(seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, Config>,
    pub buyer: Signer<'info>,
    #[account(mut, address = config.operator @ EscrowError::Unauthorized)]
    pub rent_payer: Signer<'info>,
    #[account(address = USDC_MINT @ EscrowError::WrongMint, mint::decimals = USDC_DECIMALS)]
    pub mint: Account<'info, Mint>,
    #[account(mut, associated_token::mint = mint, associated_token::authority = buyer, associated_token::token_program = token_program)]
    pub buyer_ata: Account<'info, TokenAccount>,
    #[account(init, payer = rent_payer, space = 8 + Escrow::INIT_SPACE, seeds = [b"escrow", buyer.key().as_ref(), &intent_id], bump)]
    pub escrow: Account<'info, Escrow>,
    #[account(init, payer = rent_payer, seeds = [b"vault", escrow.key().as_ref()], bump, token::mint = mint, token::authority = escrow, token::token_program = token_program)]
    pub vault: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Settle<'info> {
    #[account(seeds = [b"config"], bump = config.bump, has_one = operator @ EscrowError::Unauthorized)]
    pub config: Account<'info, Config>,
    pub operator: Signer<'info>,
    #[account(mut, seeds = [b"escrow", escrow.buyer.as_ref(), &escrow.intent_id], bump = escrow.bump, has_one = mint @ EscrowError::WrongMint)]
    pub escrow: Account<'info, Escrow>,
    #[account(mut, seeds = [b"vault", escrow.key().as_ref()], bump = escrow.vault_bump, token::mint = mint, token::authority = escrow, token::token_program = token_program)]
    pub vault: Account<'info, TokenAccount>,
    #[account(address = USDC_MINT @ EscrowError::WrongMint, mint::decimals = USDC_DECIMALS)]
    pub mint: Account<'info, Mint>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
#[instruction(x_user_id: u64, claimant: Pubkey)]
pub struct ClaimAuthorVault<'info> {
    #[account(seeds = [b"config"], bump = config.bump, has_one = operator @ EscrowError::Unauthorized)]
    pub config: Account<'info, Config>,
    #[account(mut)]
    pub operator: Signer<'info>,
    #[account(address = USDC_MINT @ EscrowError::WrongMint, mint::decimals = USDC_DECIMALS)]
    pub mint: Account<'info, Mint>,
    /// CHECK: PDA seeds bind the token authority; the PDA never stores an account balance ledger.
    #[account(seeds = [b"author", &x_user_id.to_le_bytes()], bump)]
    pub author: UncheckedAccount<'info>,
    #[account(mut, associated_token::mint = mint, associated_token::authority = author, associated_token::token_program = token_program)]
    pub author_ata: Account<'info, TokenAccount>,
    /// CHECK: address and ATA constraints bind the beneficiary. The first claim also
    /// requires its signature in the handler; later sweeps retain the recorded wallet.
    #[account(address = claimant @ EscrowError::RecipientMismatch)]
    pub claimant_wallet: UncheckedAccount<'info>,
    #[account(init_if_needed, payer = operator, associated_token::mint = mint, associated_token::authority = claimant_wallet, associated_token::token_program = token_program)]
    pub claimant_ata: Account<'info, TokenAccount>,
    #[account(init_if_needed, payer = operator, space = 8 + AuthorClaim::INIT_SPACE, seeds = [b"claim".as_ref(), &x_user_id.to_le_bytes()], bump)]
    pub author_claim: Account<'info, AuthorClaim>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Reclaim<'info> {
    pub caller: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, Config>,
    /// CHECK: immutable address from the escrow; signs only when a missing buyer ATA requires it.
    #[account(mut, address = escrow.rent_payer @ EscrowError::WrongRentPayer)]
    pub rent_payer: UncheckedAccount<'info>,
    /// CHECK: immutable buyer address; used only as the ATA authority during creation.
    #[account(address = escrow.buyer @ EscrowError::WrongTokenOwner)]
    pub buyer: UncheckedAccount<'info>,
    #[account(mut, close = rent_payer, seeds = [b"escrow", escrow.buyer.as_ref(), &escrow.intent_id], bump = escrow.bump, has_one = mint @ EscrowError::WrongMint)]
    pub escrow: Account<'info, Escrow>,
    #[account(mut, seeds = [b"vault", escrow.key().as_ref()], bump = escrow.vault_bump, token::mint = mint, token::authority = escrow, token::token_program = token_program)]
    pub vault: Account<'info, TokenAccount>,
    #[account(address = USDC_MINT @ EscrowError::WrongMint, mint::decimals = USDC_DECIMALS)]
    pub mint: Account<'info, Mint>,
    /// CHECK: canonical address is constrained; manually validated before transfer or creation.
    #[account(mut, constraint = buyer_ata.key() == get_associated_token_address(&escrow.buyer, &escrow.mint) @ EscrowError::NonCanonicalAta)]
    pub buyer_ata: UncheckedAccount<'info>,
    #[account(mut, constraint = treasury_ata.key() == get_associated_token_address(&escrow.recipient_owners[0], &escrow.mint) @ EscrowError::NonCanonicalAta, token::mint = mint, constraint = treasury_ata.owner == escrow.recipient_owners[0] @ EscrowError::WrongTokenOwner)]
    pub treasury_ata: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

fn validate_recipients(
    owners: [Pubkey; RECIPIENT_SLOTS],
    amounts: [u64; RECIPIENT_SLOTS],
) -> Result<u8> {
    let mut count = 0;
    for i in 0..RECIPIENT_SLOTS {
        require!(
            (owners[i] == Pubkey::default()) == (amounts[i] == 0),
            EscrowError::RecipientMismatch
        );
        if amounts[i] != 0 {
            count = (i + 1) as u8;
        }
    }
    require!(amounts[0] != 0, EscrowError::RecipientMismatch);
    Ok(count)
}

fn checked_product(units: u8, amount: u64) -> Result<u64> {
    u64::from(units)
        .checked_mul(amount)
        .ok_or_else(|| error!(EscrowError::Overflow))
}

fn deposit_amount<const N: usize>(units: u8, amounts: [u64; N], cap: u64) -> Result<u64> {
    require!(units > 0 && units <= MAX_UNITS, EscrowError::InvalidUnits);
    let per_unit = amounts.iter().try_fold(0_u64, |total, amount| {
        total.checked_add(*amount).ok_or(EscrowError::Overflow)
    })?;
    require!(per_unit > 0, EscrowError::ZeroAmount);
    let total = checked_product(units, per_unit)?;
    require!(total <= cap, EscrowError::DepositCap);
    Ok(total)
}

fn validate_expiry(now: i64, expiry: i64, min_ttl: i64, max_ttl: i64) -> Result<()> {
    let ttl = expiry.checked_sub(now).ok_or(EscrowError::Overflow)?;
    require!(ttl >= min_ttl && ttl <= max_ttl, EscrowError::InvalidTtl);
    Ok(())
}

#[derive(PartialEq, Eq)]
enum TokenStatus {
    Missing,
    Ready,
    Frozen,
}

fn canonical_token_status(info: &AccountInfo, owner: Pubkey, mint: Pubkey) -> Result<TokenStatus> {
    require_keys_eq!(
        *info.key,
        get_associated_token_address(&owner, &mint),
        EscrowError::NonCanonicalAta
    );
    if *info.owner == System::id() && info.data_is_empty() {
        return Ok(TokenStatus::Missing);
    }
    require_keys_eq!(*info.owner, token::ID, EscrowError::WrongTokenProgram);
    let data = info.try_borrow_data()?;
    let account = token::spl_token::state::Account::unpack(&data)
        .map_err(|_| error!(EscrowError::InvalidTokenAccount))?;
    require_keys_eq!(account.owner, owner, EscrowError::WrongTokenOwner);
    require_keys_eq!(account.mint, mint, EscrowError::WrongMint);
    Ok(
        if account.state == token::spl_token::state::AccountState::Frozen {
            TokenStatus::Frozen
        } else {
            TokenStatus::Ready
        },
    )
}

fn refund_amounts(remaining: u64, missing: bool, fee: u64) -> (u64, u64, bool) {
    if remaining == 0 || !missing {
        (remaining, 0, false)
    } else if remaining < fee {
        (0, remaining, false)
    } else {
        (remaining - fee, fee, true)
    }
}

fn transfer_from_vault<'info>(
    program: AccountInfo<'info>,
    vault: AccountInfo<'info>,
    mint: AccountInfo<'info>,
    destination: AccountInfo<'info>,
    authority: AccountInfo<'info>,
    signer: &[&[&[u8]]],
    amount: u64,
) -> Result<()> {
    if amount != 0 {
        token::transfer_checked(
            CpiContext::new_with_signer(
                program,
                TransferChecked {
                    from: vault,
                    mint,
                    to: destination,
                    authority,
                },
                signer,
            ),
            amount,
            USDC_DECIMALS,
        )?;
    }
    Ok(())
}

#[event]
pub struct Deposited {
    pub escrow: Pubkey,
    pub buyer: Pubkey,
    pub intent_id: [u8; 16],
    pub units: u8,
    pub amount: u64,
    pub expires_at: i64,
}

#[event]
pub struct Settled {
    pub escrow: Pubkey,
    pub k: u8,
    pub paid: [u64; 4],
}

#[event]
pub struct AuthorVaultClaimed {
    pub x_user_id: u64,
    pub author: Pubkey,
    pub claimant: Pubkey,
    pub amount: u64,
}

#[event]
pub struct Reclaimed {
    pub escrow: Pubkey,
    pub refund: u64,
    pub ata_fee: u64,
}

#[error_code]
pub enum EscrowError {
    #[msg("Deposits are paused.")]
    Paused,
    #[msg("This signer is not authorized.")]
    Unauthorized,
    #[msg("The upgrade authority does not belong to this program.")]
    WrongProgramData,
    #[msg("An admin, guardian or treasury address is missing.")]
    InvalidRole,
    #[msg("The deposit exceeds the configured cap.")]
    DepositCap,
    #[msg("The expiry is outside the allowed lifetime.")]
    InvalidTtl,
    #[msg("The ATA recovery fee exceeds one USDC.")]
    RefundFeeCap,
    #[msg("Only the configured cluster's classic USDC mint is accepted.")]
    WrongMint,
    #[msg("The treasury recipient does not match the configuration.")]
    WrongTreasury,
    #[msg("The artist recipient does not match its amount.")]
    InvalidRecipient,
    #[msg("A deposit must contain between one and 24 units.")]
    InvalidUnits,
    #[msg("The per-unit amount must be positive.")]
    ZeroAmount,
    #[msg("The amount or expiry calculation overflowed.")]
    Overflow,
    #[msg("The buyer's deposit account must have no delegate.")]
    DelegatedSource,
    #[msg("The escrow has already been settled.")]
    AlreadySettled,
    #[msg("The escrow has expired.")]
    Expired,
    #[msg("Delivered units exceed the funded units.")]
    TooManyDeliveredUnits,
    #[msg("The vault cannot cover this settlement.")]
    InsufficientVault,
    #[msg("The token account is not the canonical associated token account.")]
    NonCanonicalAta,
    #[msg("The token account has the wrong owner.")]
    WrongTokenOwner,
    #[msg("Only the classic SPL Token program is accepted.")]
    WrongTokenProgram,
    #[msg("The token account is invalid.")]
    InvalidTokenAccount,
    #[msg("A funded escrow cannot be reclaimed before expiry.")]
    NotReclaimable,
    #[msg("The buyer's token account is frozen.")]
    FrozenBuyerAta,
    #[msg("The stored rent payer must sign to recover a missing buyer account.")]
    RentPayerMustSign,
    #[msg("Rent must return to the original rent payer.")]
    WrongRentPayer,
    #[msg("A recipient slot does not match its amount or token account.")]
    RecipientMismatch,
    #[msg("The escrow account version is unsupported.")]
    WrongVersion,
    #[msg("The treasury token account is frozen.")]
    FrozenTreasuryAta,
    #[msg("An author vault cannot pay the treasury, operator or an invalid beneficiary.")]
    VaultToTreasury,
    #[msg("This author is already bound to another payout wallet.")]
    AlreadyClaimed,
    #[msg("The payout wallet must sign its first author claim.")]
    ClaimantMustSign,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_layout_matches_ticket() {
        assert_eq!(8 + Config::INIT_SPACE, 170);
        assert_eq!(8 + Escrow::INIT_SPACE, 343);
        assert_eq!(8 + AuthorClaim::INIT_SPACE, 58);
    }

    #[test]
    fn deposits_reject_empty_oversized_zero_and_overflowing_amounts() {
        let config_cap = 50_000_000;
        assert!(deposit_amount(0, [1, 0], config_cap).is_err());
        assert!(deposit_amount(25, [1, 0], config_cap).is_err());
        assert!(deposit_amount(1, [0, 0], config_cap).is_err());
        assert!(deposit_amount(1, [u64::MAX, 1], config_cap).is_err());
        assert!(deposit_amount(24, [u64::MAX / 2, 0], u64::MAX).is_err());
        assert!(deposit_amount(1, [config_cap + 1, 0], config_cap).is_err());
        assert_eq!(
            deposit_amount(1, [60_000_000, 0], 60_000_000).unwrap(),
            60_000_000
        );
        assert_eq!(
            deposit_amount(4, [12_500_000, 0], config_cap).unwrap(),
            config_cap
        );
        assert_eq!(
            deposit_amount(24, [900_000, 100_000], config_cap).unwrap(),
            24_000_000
        );
        assert_eq!(
            deposit_amount(4, [1_000_000, 200_000, 25_000, 25_000], config_cap).unwrap(),
            5_000_000
        );
        assert!(deposit_amount(1, [u64::MAX - 2, 1, 1, 1], u64::MAX).is_err());
    }

    #[test]
    fn recipient_span_preserves_free_and_claimed_sparse_slots() {
        let treasury = Pubkey::new_unique();
        let referrer = Pubkey::new_unique();
        let author = Pubkey::new_unique();
        let empty = Pubkey::default();
        assert_eq!(
            validate_recipients([treasury, empty, empty, empty], [1, 0, 0, 0]).unwrap(),
            1
        );
        assert_eq!(
            validate_recipients([treasury, empty, referrer, author], [100, 0, 25, 25]).unwrap(),
            4
        );
        assert_eq!(
            validate_recipients([treasury, empty, empty, author], [100, 0, 0, 50]).unwrap(),
            4
        );
        assert!(validate_recipients([treasury, empty, referrer, empty], [100, 0, 0, 0]).is_err());
        assert!(validate_recipients([treasury, empty, empty, empty], [100, 0, 25, 0]).is_err());
        assert!(validate_recipients([empty; 4], [0; 4]).is_err());
    }

    #[test]
    fn expiry_boundaries_use_chain_time_and_checked_subtraction() {
        for ttl in [600, 900, 1_800] {
            assert!(validate_expiry(1_000, 1_000 + ttl, 600, 1_800).is_ok());
        }
        for ttl in [-1, 0, 599, 1_801] {
            assert!(validate_expiry(1_000, 1_000 + ttl, 600, 1_800).is_err());
        }
        assert!(validate_expiry(i64::MIN, i64::MAX, 600, 1_800).is_err());
    }

    #[test]
    fn missing_buyer_account_refund_rules_include_equality_and_zero() {
        assert_eq!(
            refund_amounts(1_500_000, false, 1_000_000),
            (1_500_000, 0, false)
        );
        assert_eq!(
            refund_amounts(1_500_000, true, 1_000_000),
            (500_000, 1_000_000, true)
        );
        assert_eq!(
            refund_amounts(1_000_000, true, 1_000_000),
            (0, 1_000_000, true)
        );
        assert_eq!(
            refund_amounts(999_999, true, 1_000_000),
            (0, 999_999, false)
        );
        assert_eq!(refund_amounts(0, true, 1_000_000), (0, 0, false));
        assert_eq!(refund_amounts(1, true, 0), (1, 0, true));
    }
}
