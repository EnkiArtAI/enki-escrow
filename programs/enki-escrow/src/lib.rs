use anchor_lang::prelude::*;
use anchor_lang::solana_program::{program_option::COption, program_pack::Pack};
use anchor_spl::{
    associated_token::{self, get_associated_token_address, AssociatedToken, Create},
    token::{self, CloseAccount, Mint, Token, TokenAccount, TransferChecked},
};

declare_id!("hNNT7mTmthTVvnw4NYo6jYoBTDazRgweWTBHBmwgcak");

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
        recipient_owners: [Pubkey; 2],
        unit_amounts: [u64; 2],
        expires_at: i64,
    ) -> Result<()> {
        let config = &ctx.accounts.config;
        require!(!config.paused, EscrowError::Paused);
        require_keys_eq!(
            recipient_owners[0],
            config.treasury_owner,
            EscrowError::WrongTreasury
        );
        let recipient_count = validate_recipients(recipient_owners, unit_amounts)?;
        let amount = deposit_amount(units, unit_amounts, config.max_deposit_micro)?;
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
            version: 1,
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

    pub fn settle(ctx: Context<Settle>, k: u8) -> Result<()> {
        let escrow = &ctx.accounts.escrow;
        require!(
            escrow.state == EscrowState::Funded,
            EscrowError::AlreadySettled
        );
        require!(
            Clock::get()?.unix_timestamp < escrow.expires_at,
            EscrowError::Expired
        );
        require!(k <= escrow.units, EscrowError::TooManyDeliveredUnits);
        let treasury_paid = checked_product(k, escrow.unit_amounts[0])?;
        let artist_due = checked_product(k, escrow.unit_amounts[1])?;
        let artist_available = if artist_due == 0 {
            false
        } else {
            // The account constraint enforces its address; unusable contents forfeit this leg.
            canonical_token_status(
                &ctx.accounts.artist_ata.to_account_info(),
                escrow.recipient_owners[1],
                escrow.mint,
            )
            .unwrap_or(TokenStatus::Frozen)
                == TokenStatus::Ready
        };
        let artist_paid = if artist_available { artist_due } else { 0 };
        let paid = treasury_paid
            .checked_add(artist_paid)
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
        transfer_from_vault(
            ctx.accounts.token_program.to_account_info(),
            ctx.accounts.vault.to_account_info(),
            ctx.accounts.mint.to_account_info(),
            ctx.accounts.treasury_ata.to_account_info(),
            ctx.accounts.escrow.to_account_info(),
            signer,
            treasury_paid,
        )?;
        transfer_from_vault(
            ctx.accounts.token_program.to_account_info(),
            ctx.accounts.vault.to_account_info(),
            ctx.accounts.mint.to_account_info(),
            ctx.accounts.artist_ata.to_account_info(),
            ctx.accounts.escrow.to_account_info(),
            signer,
            artist_paid,
        )?;
        let escrow = &mut ctx.accounts.escrow;
        escrow.state = EscrowState::Settled;
        escrow.settled_units = k;
        emit!(Settled {
            escrow: escrow.key(),
            k,
            paid: [treasury_paid, artist_paid]
        });
        Ok(())
    }

    pub fn reclaim(ctx: Context<Reclaim>) -> Result<()> {
        let escrow = &ctx.accounts.escrow;
        require!(
            escrow.state == EscrowState::Settled
                || Clock::get()?.unix_timestamp >= escrow.expires_at,
            EscrowError::NotReclaimable
        );
        let remaining = ctx.accounts.vault.amount;
        let missing = if remaining > 0 {
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
            ata_fee,
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
    pub recipient_owners: [Pubkey; 2],
    pub unit_amounts: [u64; 2],
    pub units: u8,
    pub settled_units: u8,
    pub created_at: i64,
    pub expires_at: i64,
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
    #[account(mut, constraint = treasury_ata.key() == get_associated_token_address(&escrow.recipient_owners[0], &escrow.mint) @ EscrowError::NonCanonicalAta, token::mint = mint, constraint = treasury_ata.owner == escrow.recipient_owners[0] @ EscrowError::WrongTokenOwner)]
    pub treasury_ata: Account<'info, TokenAccount>,
    /// CHECK: canonical address is constrained; owner, mint and state are checked before any CPI.
    #[account(mut, constraint = artist_ata.key() == get_associated_token_address(&escrow.recipient_owners[1], &escrow.mint) @ EscrowError::NonCanonicalAta)]
    pub artist_ata: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token>,
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

fn validate_recipients(owners: [Pubkey; 2], amounts: [u64; 2]) -> Result<u8> {
    if amounts[1] == 0 {
        require!(
            owners[1] == Pubkey::default(),
            EscrowError::InvalidRecipient
        );
        Ok(1)
    } else {
        require!(
            owners[1] != Pubkey::default(),
            EscrowError::InvalidRecipient
        );
        Ok(2)
    }
}

fn checked_product(units: u8, amount: u64) -> Result<u64> {
    u64::from(units)
        .checked_mul(amount)
        .ok_or_else(|| error!(EscrowError::Overflow))
}

fn deposit_amount(units: u8, amounts: [u64; 2], cap: u64) -> Result<u64> {
    require!(units > 0 && units <= MAX_UNITS, EscrowError::InvalidUnits);
    let per_unit = amounts[0]
        .checked_add(amounts[1])
        .ok_or(EscrowError::Overflow)?;
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
    pub paid: [u64; 2],
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_layout_matches_ticket() {
        assert_eq!(8 + Config::INIT_SPACE, 170);
        assert_eq!(8 + Escrow::INIT_SPACE, 223);
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
