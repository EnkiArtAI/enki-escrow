use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::{
    bpf_loader_upgradeable::{self, UpgradeableLoaderState},
    clock::Clock,
    instruction::{AccountMeta, Instruction},
    program_option::COption,
    program_pack::Pack,
};
use anchor_lang::{AccountDeserialize, AccountSerialize, InstructionData, ToAccountMetas};
use anchor_spl::{
    associated_token::get_associated_token_address,
    token::{self, spl_token},
};
use enki_escrow::{accounts, instruction, AuthorClaim, Config, ConfigArgs, Escrow, ID, USDC_MINT};
use litesvm::{types::TransactionResult, LiteSVM};
use solana_account::Account;
use solana_keypair::Keypair;
use solana_message::{Message, VersionedMessage};
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;

const NOW: i64 = 1_750_000_000;
const BUYER_TOKENS: u64 = 5_000_000_000_000;
const X_USER_ID: u64 = 0x0102_0304_0506_0708;
const SETTLE_RECIPIENT_OFFSET: usize = 6;

struct Fixture {
    svm: LiteSVM,
    admin: Keypair,
    guardian: Keypair,
    operator: Keypair,
    payer: Keypair,
    buyer: Keypair,
    stranger: Keypair,
    artist: Pubkey,
    referrer: Pubkey,
    treasury: Pubkey,
    config: Pubkey,
    buyer_ata: Pubkey,
    treasury_ata: Pubkey,
    artist_ata: Pubkey,
    referrer_ata: Pubkey,
    program_data: Pubkey,
}

impl Fixture {
    fn new() -> Self {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/deploy/enki_escrow.so");
        let bytes = std::fs::read(path)
            .expect("Build the SBF binary before running escrow integration tests.");
        let mut svm = LiteSVM::new();
        let admin = Keypair::new();
        let guardian = Keypair::new();
        let operator = Keypair::new();
        // The configured quote signer also sponsors rent; this is a disposable test key.
        let payer = operator.insecure_clone();
        let buyer = Keypair::new();
        let stranger = Keypair::new();
        for key in [&admin, &guardian, &operator, &buyer, &stranger] {
            svm.airdrop(&key.pubkey(), 10_000_000_000).unwrap();
        }
        // Reproduce an upgradeable deployment so init_config tests the actual loader authority.
        let program_data =
            Pubkey::find_program_address(&[ID.as_ref()], &bpf_loader_upgradeable::ID).0;
        let mut data = bincode::serialize(&UpgradeableLoaderState::ProgramData {
            slot: 0,
            upgrade_authority_address: Some(admin.pubkey()),
        })
        .unwrap();
        data.extend_from_slice(&bytes);
        svm.set_account(
            program_data,
            Account {
                lamports: svm.minimum_balance_for_rent_exemption(data.len()),
                data,
                owner: bpf_loader_upgradeable::ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
        let data = bincode::serialize(&UpgradeableLoaderState::Program {
            programdata_address: program_data,
        })
        .unwrap();
        svm.set_account(
            ID,
            Account {
                lamports: svm.minimum_balance_for_rent_exemption(data.len()),
                data,
                owner: bpf_loader_upgradeable::ID,
                executable: true,
                rent_epoch: 0,
            },
        )
        .unwrap();
        let mut clock = svm.get_sysvar::<Clock>();
        clock.unix_timestamp = NOW;
        svm.set_sysvar(&clock);
        let treasury = Pubkey::new_unique();
        let artist = Pubkey::new_unique();
        let referrer = Pubkey::new_unique();
        let buyer_ata = get_associated_token_address(&buyer.pubkey(), &USDC_MINT);
        let treasury_ata = get_associated_token_address(&treasury, &USDC_MINT);
        let artist_ata = get_associated_token_address(&artist, &USDC_MINT);
        let referrer_ata = get_associated_token_address(&referrer, &USDC_MINT);
        let mut f = Self {
            svm,
            admin,
            guardian,
            operator,
            payer,
            buyer,
            stranger,
            treasury,
            artist,
            referrer,
            config: Pubkey::find_program_address(&[b"config"], &ID).0,
            buyer_ata,
            treasury_ata,
            artist_ata,
            referrer_ata,
            program_data,
        };
        let mint = spl_token::state::Mint {
            mint_authority: COption::Some(f.admin.pubkey()),
            supply: BUYER_TOKENS,
            decimals: 6,
            is_initialized: true,
            freeze_authority: COption::Some(f.admin.pubkey()),
        };
        let mut data = vec![0; spl_token::state::Mint::LEN];
        spl_token::state::Mint::pack(mint, &mut data).unwrap();
        f.set_data(USDC_MINT, data, token::ID);
        f.set_tokens(
            f.buyer_ata,
            f.buyer.pubkey(),
            BUYER_TOKENS,
            spl_token::state::AccountState::Initialized,
        );
        f.set_tokens(
            f.treasury_ata,
            f.treasury,
            0,
            spl_token::state::AccountState::Initialized,
        );
        f.set_tokens(
            f.artist_ata,
            f.artist,
            0,
            spl_token::state::AccountState::Initialized,
        );
        f.set_tokens(
            f.referrer_ata,
            f.referrer,
            0,
            spl_token::state::AccountState::Initialized,
        );
        f.set_author_balance(X_USER_ID, 0);
        f
    }

    fn args(&self) -> ConfigArgs {
        ConfigArgs {
            admin: self.admin.pubkey(),
            guardian: self.guardian.pubkey(),
            operator: self.operator.pubkey(),
            treasury_owner: self.treasury,
            paused: false,
            max_deposit_micro: 50_000_000,
            min_ttl_s: 600,
            max_ttl_s: 1_800,
            refund_ata_fee_micro: 1_000_000,
        }
    }

    fn init_ix(&self, authority: Pubkey, args: ConfigArgs) -> Instruction {
        Instruction {
            program_id: ID,
            accounts: accounts::InitConfig {
                authority,
                config: self.config,
                program: ID,
                program_data: self.program_data,
                system_program: anchor_lang::system_program::ID,
            }
            .to_account_metas(None),
            data: instruction::InitConfig { args }.data(),
        }
    }

    fn init(&mut self) {
        let ix = self.init_ix(self.admin.pubkey(), self.args());
        self.send(ix, &[Role::Admin]).unwrap();
    }

    fn send(&mut self, ix: Instruction, roles: &[Role]) -> TransactionResult {
        let mut signers: Vec<&Keypair> = vec![&self.payer];
        for role in roles {
            let key = match role {
                Role::Admin => &self.admin,
                Role::Guardian => &self.guardian,
                Role::Operator => &self.operator,
                Role::Buyer => &self.buyer,
                Role::Stranger => &self.stranger,
            };
            if !signers.iter().any(|s| s.pubkey() == key.pubkey()) {
                signers.push(key);
            }
        }
        self.svm.expire_blockhash();
        let msg = Message::new_with_blockhash(
            &[ix],
            Some(&self.payer.pubkey()),
            &self.svm.latest_blockhash(),
        );
        let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &signers).unwrap();
        self.svm.send_transaction(tx)
    }

    fn pdas(&self, intent_id: [u8; 16]) -> (Pubkey, Pubkey) {
        let escrow = Pubkey::find_program_address(
            &[b"escrow", self.buyer.pubkey().as_ref(), &intent_id],
            &ID,
        )
        .0;
        (
            escrow,
            Pubkey::find_program_address(&[b"vault", escrow.as_ref()], &ID).0,
        )
    }

    fn send_as_stranger(&mut self, ix: Instruction) -> TransactionResult {
        self.svm.expire_blockhash();
        let msg = Message::new_with_blockhash(
            &[ix],
            Some(&self.stranger.pubkey()),
            &self.svm.latest_blockhash(),
        );
        let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &[&self.stranger])
            .unwrap();
        self.svm.send_transaction(tx)
    }

    fn send_as_buyer(&mut self, ix: Instruction) -> TransactionResult {
        self.svm.expire_blockhash();
        let msg = Message::new_with_blockhash(
            &[ix],
            Some(&self.buyer.pubkey()),
            &self.svm.latest_blockhash(),
        );
        let tx =
            VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &[&self.buyer]).unwrap();
        self.svm.send_transaction(tx)
    }

    fn donate(&mut self, id: [u8; 16], amount: u64) {
        let ix = spl_token::instruction::transfer_checked(
            &token::ID,
            &self.buyer_ata,
            &USDC_MINT,
            &self.pdas(id).1,
            &self.buyer.pubkey(),
            &[],
            amount,
            6,
        )
        .unwrap();
        self.send_as_buyer(ix).unwrap();
    }

    fn deposit_ix(
        &self,
        intent_id: [u8; 16],
        units: u8,
        amounts: [u64; 2],
        expiry: i64,
    ) -> Instruction {
        let unit_amounts = [amounts[0], amounts[1], 0, 0];
        self.deposit_four_ix(
            intent_id,
            units,
            self.recipient_owners(unit_amounts),
            unit_amounts,
            expiry,
        )
    }

    fn recipient_owners(&self, amounts: [u64; 4]) -> [Pubkey; 4] {
        let owners = [
            self.treasury,
            self.artist,
            self.referrer,
            Self::author_pdas(X_USER_ID).0,
        ];
        std::array::from_fn(|slot| {
            if slot != 0 && amounts[slot] == 0 {
                Pubkey::default()
            } else {
                owners[slot]
            }
        })
    }

    fn deposit_four_ix(
        &self,
        intent_id: [u8; 16],
        units: u8,
        recipient_owners: [Pubkey; 4],
        unit_amounts: [u64; 4],
        expiry: i64,
    ) -> Instruction {
        let (escrow, vault) = self.pdas(intent_id);
        Instruction {
            program_id: ID,
            accounts: accounts::Deposit {
                config: self.config,
                buyer: self.buyer.pubkey(),
                rent_payer: self.payer.pubkey(),
                mint: USDC_MINT,
                buyer_ata: self.buyer_ata,
                escrow,
                vault,
                token_program: token::ID,
                associated_token_program: anchor_spl::associated_token::ID,
                system_program: anchor_lang::system_program::ID,
            }
            .to_account_metas(None),
            data: instruction::Deposit {
                intent_id,
                units,
                recipient_owners,
                unit_amounts,
                expires_at: expiry,
            }
            .data(),
        }
    }

    fn deposit(&mut self, id: [u8; 16], units: u8, amounts: [u64; 2]) {
        self.send(
            self.deposit_ix(id, units, amounts, NOW + 900),
            &[Role::Buyer],
        )
        .unwrap();
    }

    fn deposit_four(&mut self, id: [u8; 16], units: u8, amounts: [u64; 4]) {
        self.send(
            self.deposit_four_ix(
                id,
                units,
                self.recipient_owners(amounts),
                amounts,
                NOW + 900,
            ),
            &[Role::Buyer],
        )
        .unwrap();
    }

    fn settle_ix(&self, id: [u8; 16], k: u8, operator: Pubkey) -> Instruction {
        let (escrow, vault) = self.pdas(id);
        let stored = self.escrow(id);
        let mut metas = accounts::Settle {
            config: self.config,
            operator,
            escrow,
            vault,
            mint: USDC_MINT,
            token_program: token::ID,
        }
        .to_account_metas(None);
        for slot in 0..usize::from(stored.recipient_count) {
            metas.push(if stored.unit_amounts[slot] == 0 {
                AccountMeta::new_readonly(anchor_lang::system_program::ID, false)
            } else {
                AccountMeta::new(
                    get_associated_token_address(&stored.recipient_owners[slot], &USDC_MINT),
                    false,
                )
            });
        }
        Instruction {
            program_id: ID,
            accounts: metas,
            data: instruction::Settle { k }.data(),
        }
    }

    fn settle(&mut self, id: [u8; 16], k: u8) -> TransactionResult {
        self.send(
            self.settle_ix(id, k, self.operator.pubkey()),
            &[Role::Operator],
        )
    }

    fn reclaim_ix(&self, id: [u8; 16], rent_payer: Pubkey, rent_signs: bool) -> Instruction {
        let (escrow, vault) = self.pdas(id);
        let mut metas = accounts::Reclaim {
            caller: self.stranger.pubkey(),
            config: self.config,
            rent_payer,
            buyer: self.buyer.pubkey(),
            escrow,
            vault,
            mint: USDC_MINT,
            buyer_ata: self.buyer_ata,
            treasury_ata: self.treasury_ata,
            token_program: token::ID,
            associated_token_program: anchor_spl::associated_token::ID,
            system_program: anchor_lang::system_program::ID,
        }
        .to_account_metas(None);
        metas[2].is_signer = rent_signs;
        Instruction {
            program_id: ID,
            accounts: metas,
            data: instruction::Reclaim {}.data(),
        }
    }

    fn reclaim(&mut self, id: [u8; 16]) -> TransactionResult {
        self.send(
            self.reclaim_ix(id, self.payer.pubkey(), false),
            &[Role::Stranger],
        )
    }

    fn clock(&mut self, now: i64) {
        let mut clock = self.svm.get_sysvar::<Clock>();
        clock.unix_timestamp = now;
        self.svm.set_sysvar(&clock);
    }

    fn escrow(&self, id: [u8; 16]) -> Escrow {
        let account = self.svm.get_account(&self.pdas(id).0).unwrap();
        Escrow::try_deserialize(&mut account.data.as_slice()).unwrap()
    }

    fn tokens(&self, key: Pubkey) -> u64 {
        self.svm
            .get_account(&key)
            .filter(|a| a.owner == token::ID)
            .map(|a| spl_token::state::Account::unpack(&a.data).unwrap().amount)
            .unwrap_or(0)
    }

    fn author_pdas(x_user_id: u64) -> (Pubkey, Pubkey, Pubkey) {
        let author = Pubkey::find_program_address(&[b"author", &x_user_id.to_le_bytes()], &ID).0;
        let author_ata = get_associated_token_address(&author, &USDC_MINT);
        let claim = Pubkey::find_program_address(&[b"claim", &x_user_id.to_le_bytes()], &ID).0;
        (author, author_ata, claim)
    }

    fn set_author_balance(&mut self, x_user_id: u64, amount: u64) {
        let (author, author_ata, _) = Self::author_pdas(x_user_id);
        self.set_tokens(
            author_ata,
            author,
            amount,
            spl_token::state::AccountState::Initialized,
        );
    }

    fn author_claim(&self, x_user_id: u64) -> AuthorClaim {
        let account = self
            .svm
            .get_account(&Self::author_pdas(x_user_id).2)
            .unwrap();
        AuthorClaim::try_deserialize(&mut account.data.as_slice()).unwrap()
    }

    fn claim_ix(&self, x_user_id: u64, claimant: Pubkey) -> Instruction {
        let (author, author_ata, author_claim) = Self::author_pdas(x_user_id);
        Instruction {
            program_id: ID,
            accounts: accounts::ClaimAuthorVault {
                config: self.config,
                operator: self.operator.pubkey(),
                mint: USDC_MINT,
                author,
                author_ata,
                claimant_wallet: claimant,
                claimant_ata: get_associated_token_address(&claimant, &USDC_MINT),
                author_claim,
                token_program: token::ID,
                associated_token_program: anchor_spl::associated_token::ID,
                system_program: anchor_lang::system_program::ID,
            }
            .to_account_metas(None),
            data: instruction::ClaimAuthorVault {
                x_user_id,
                claimant,
            }
            .data(),
        }
    }

    fn claim(&mut self, x_user_id: u64, claimant: Pubkey) -> TransactionResult {
        self.send(self.claim_ix(x_user_id, claimant), &[Role::Operator])
    }

    fn set_data(&mut self, key: Pubkey, data: Vec<u8>, owner: Pubkey) {
        self.svm
            .set_account(
                key,
                Account {
                    lamports: self.svm.minimum_balance_for_rent_exemption(data.len()),
                    data,
                    owner,
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .unwrap();
    }

    fn set_tokens(
        &mut self,
        key: Pubkey,
        owner: Pubkey,
        amount: u64,
        state: spl_token::state::AccountState,
    ) {
        let token = spl_token::state::Account {
            mint: USDC_MINT,
            owner,
            amount,
            delegate: COption::None,
            state,
            is_native: COption::None,
            delegated_amount: 0,
            close_authority: COption::None,
        };
        let mut data = vec![0; spl_token::state::Account::LEN];
        spl_token::state::Account::pack(token, &mut data).unwrap();
        self.set_data(key, data, token::ID);
    }

    fn remove_ata(&mut self, ata: Pubkey) {
        self.svm.set_account(ata, Account::default()).unwrap();
    }

    fn change_ata(&mut self, ata: Pubkey, change: AtaChange) {
        let mut account = self.svm.get_account(&ata).unwrap();
        match change {
            AtaChange::WrongProgram => account.owner = anchor_lang::system_program::ID,
            AtaChange::Malformed => account.data.truncate(7),
            _ => {
                let mut token = spl_token::state::Account::unpack(&account.data).unwrap();
                match change {
                    AtaChange::ReassignedOwner => token.owner = self.stranger.pubkey(),
                    AtaChange::WrongMint => token.mint = self.stranger.pubkey(),
                    AtaChange::Uninitialized => {
                        token.state = spl_token::state::AccountState::Uninitialized
                    }
                    AtaChange::Frozen => token.state = spl_token::state::AccountState::Frozen,
                    _ => unreachable!(),
                }
                spl_token::state::Account::pack(token, &mut account.data).unwrap();
            }
        }
        self.svm.set_account(ata, account).unwrap();
    }

    fn emergency(&self, authority: Pubkey, revoke: bool) -> Instruction {
        Instruction {
            program_id: ID,
            accounts: accounts::EmergencyControl {
                authority,
                config: self.config,
            }
            .to_account_metas(None),
            data: if revoke {
                instruction::RevokeOperator {}.data()
            } else {
                instruction::Pause {}.data()
            },
        }
    }

    fn update_ix(&self, admin: Pubkey, args: ConfigArgs) -> Instruction {
        Instruction {
            program_id: ID,
            accounts: accounts::UpdateConfig {
                admin,
                config: self.config,
            }
            .to_account_metas(None),
            data: instruction::UpdateConfig { args }.data(),
        }
    }
}

#[derive(Clone, Copy)]
enum Role {
    Admin,
    Guardian,
    Operator,
    Buyer,
    Stranger,
}

#[derive(Clone, Copy, Debug)]
enum AtaChange {
    ReassignedOwner,
    WrongMint,
    WrongProgram,
    Malformed,
    Uninitialized,
    Frozen,
}

const UNUSABLE_ATAS: [AtaChange; 6] = [
    AtaChange::ReassignedOwner,
    AtaChange::WrongMint,
    AtaChange::WrongProgram,
    AtaChange::Malformed,
    AtaChange::Uninitialized,
    AtaChange::Frozen,
];

fn rejects(result: TransactionResult, code: u32) {
    use anchor_lang::prelude::instruction::error::InstructionError;
    use solana_transaction_error::TransactionError;
    let error = result.unwrap_err();
    assert_eq!(
        error.err,
        TransactionError::InstructionError(0, InstructionError::Custom(code)),
        "{:?}",
        error.meta.logs
    );
}

fn code(error: enki_escrow::EscrowError) -> u32 {
    error as u32 + 6_000
}

#[test]
fn init_requires_actual_upgrade_authority_and_cannot_reinitialize() {
    let mut f = Fixture::new();
    rejects(
        f.send(f.init_ix(f.stranger.pubkey(), f.args()), &[Role::Stranger]),
        code(enki_escrow::EscrowError::Unauthorized),
    );
    f.init();
    assert!(f
        .send(f.init_ix(f.admin.pubkey(), f.args()), &[Role::Admin])
        .is_err());
    assert_eq!(f.svm.get_account(&f.config).unwrap().data.len(), 170);
}

#[test]
fn init_rejects_another_programs_program_data_even_when_its_authority_signs() {
    let mut f = Fixture::new();
    let other_program = Pubkey::new_unique();
    let other_data =
        Pubkey::find_program_address(&[other_program.as_ref()], &bpf_loader_upgradeable::ID).0;
    let data = bincode::serialize(&UpgradeableLoaderState::ProgramData {
        slot: 0,
        upgrade_authority_address: Some(f.stranger.pubkey()),
    })
    .unwrap();
    f.set_data(other_data, data, bpf_loader_upgradeable::ID);
    let mut args = f.args();
    args.admin = f.stranger.pubkey();
    let mut ix = f.init_ix(f.stranger.pubkey(), args);
    ix.accounts[3].pubkey = other_data;
    rejects(
        f.send_as_stranger(ix),
        code(enki_escrow::EscrowError::WrongProgramData),
    );
    assert!(f.svm.get_account(&f.config).is_none_or(|a| a.lamports == 0));
    f.init();
}

#[test]
fn partial_delivery_pays_exact_units_refunds_rest_and_returns_rent() {
    let mut f = Fixture::new();
    f.init();
    let id = [1; 16];
    let before = f.tokens(f.buyer_ata);
    f.deposit(id, 4, [900_000, 100_000]);
    let (escrow, vault) = f.pdas(id);
    assert_eq!(f.svm.get_account(&escrow).unwrap().data.len(), 343);
    assert_eq!(f.escrow(id).version, 2);
    assert_eq!(f.escrow(id).paid_amounts, [0; 4]);
    assert_eq!(f.escrow(id).refund_due, 4_000_000);
    assert_eq!(f.tokens(vault), 4_000_000);
    assert_eq!(f.tokens(f.buyer_ata), before - 4_000_000);
    let immutable = f.escrow(id);
    let settled = f.settle(id, 2).unwrap();
    assert!(settled.compute_units_consumed < 200_000);
    assert_eq!(f.tokens(f.treasury_ata), 1_800_000);
    assert_eq!(f.tokens(f.artist_ata), 200_000);
    assert_eq!(f.tokens(vault), 2_000_000);
    assert_eq!(f.escrow(id).refund_due, 2_000_000);
    let after = f.escrow(id);
    assert_eq!(after.version, immutable.version);
    assert_eq!(after.bump, immutable.bump);
    assert_eq!(after.vault_bump, immutable.vault_bump);
    assert_eq!(after.intent_id, immutable.intent_id);
    assert_eq!(after.buyer, immutable.buyer);
    assert_eq!(after.mint, immutable.mint);
    assert_eq!(after.rent_payer, immutable.rent_payer);
    assert_eq!(after.recipient_count, immutable.recipient_count);
    assert_eq!(after.recipient_owners, immutable.recipient_owners);
    assert_eq!(after.unit_amounts, immutable.unit_amounts);
    assert_eq!(after.units, immutable.units);
    assert_eq!(after.created_at, immutable.created_at);
    assert_eq!(after.expires_at, immutable.expires_at);
    assert_eq!(after.paid_amounts, [1_800_000, 200_000, 0, 0]);
    rejects(
        f.settle(id, 1),
        code(enki_escrow::EscrowError::AlreadySettled),
    );
    let rent = f.svm.get_balance(&escrow).unwrap() + f.svm.get_balance(&vault).unwrap();
    let payer_before = f.svm.get_balance(&f.payer.pubkey()).unwrap();
    f.reclaim(id).unwrap();
    assert_eq!(f.tokens(f.buyer_ata), before - 2_000_000);
    assert_eq!(
        f.svm.get_balance(&f.payer.pubkey()).unwrap(),
        payer_before + rent - 10_000
    );
    assert!(f.svm.get_account(&escrow).is_none_or(|a| a.lamports == 0));
    assert!(f.svm.get_account(&vault).is_none_or(|a| a.lamports == 0));
}

#[test]
fn deposit_rejects_invalid_numbers_ttl_treasury_mint_and_noncanonical_source() {
    let mut f = Fixture::new();
    f.init();
    use enki_escrow::EscrowError as E;
    for (units, amounts, expiry, error) in [
        (0, [1, 0], NOW + 900, E::InvalidUnits),
        (25, [1, 0], NOW + 900, E::InvalidUnits),
        (1, [0, 0], NOW + 900, E::ZeroAmount),
        (1, [50_000_001, 0], NOW + 900, E::DepositCap),
        (1, [u64::MAX, 1], NOW + 900, E::Overflow),
        (24, [u64::MAX / 2, 0], NOW + 900, E::Overflow),
        (1, [1, 0], NOW + 599, E::InvalidTtl),
        (1, [1, 0], NOW + 1_801, E::InvalidTtl),
    ] {
        rejects(
            f.send(
                f.deposit_ix([2; 16], units, amounts, expiry),
                &[Role::Buyer],
            ),
            code(error),
        );
        assert_eq!(f.tokens(f.buyer_ata), BUYER_TOKENS);
        assert!(f
            .svm
            .get_account(&f.pdas([2; 16]).0)
            .is_none_or(|a| a.lamports == 0));
    }
    let mut ix = f.deposit_ix([2; 16], 1, [1, 0], NOW + 900);
    ix.data = instruction::Deposit {
        intent_id: [2; 16],
        units: 1,
        recipient_owners: [
            f.stranger.pubkey(),
            Pubkey::default(),
            Pubkey::default(),
            Pubkey::default(),
        ],
        unit_amounts: [1, 0, 0, 0],
        expires_at: NOW + 900,
    }
    .data();
    rejects(f.send(ix, &[Role::Buyer]), code(E::WrongTreasury));
    let fake = Pubkey::new_unique();
    let mint = f.svm.get_account(&USDC_MINT).unwrap();
    f.svm.set_account(fake, mint).unwrap();
    let mut ix = f.deposit_ix([2; 16], 1, [1, 0], NOW + 900);
    ix.accounts[3].pubkey = fake;
    rejects(f.send(ix, &[Role::Buyer]), code(E::WrongMint));
    let fake_ata = Pubkey::new_unique();
    f.set_tokens(
        fake_ata,
        f.buyer.pubkey(),
        BUYER_TOKENS,
        spl_token::state::AccountState::Initialized,
    );
    let mut ix = f.deposit_ix([2; 16], 1, [1, 0], NOW + 900);
    ix.accounts[4].pubkey = fake_ata;
    assert!(f.send(ix, &[Role::Buyer]).is_err());
}

#[test]
fn source_delegate_or_missing_buyer_signature_cannot_deposit() {
    let mut f = Fixture::new();
    f.init();
    let mut source = f.svm.get_account(&f.buyer_ata).unwrap();
    let mut token = spl_token::state::Account::unpack(&source.data).unwrap();
    token.delegate = COption::Some(f.stranger.pubkey());
    token.delegated_amount = BUYER_TOKENS;
    spl_token::state::Account::pack(token, &mut source.data).unwrap();
    f.svm.set_account(f.buyer_ata, source).unwrap();
    rejects(
        f.send(f.deposit_ix([3; 16], 1, [1, 0], NOW + 900), &[Role::Buyer]),
        code(enki_escrow::EscrowError::DelegatedSource),
    );
    f.set_tokens(
        f.buyer_ata,
        f.buyer.pubkey(),
        BUYER_TOKENS,
        spl_token::state::AccountState::Initialized,
    );
    let mut ix = f.deposit_ix([3; 16], 1, [1, 0], NOW + 900);
    ix.accounts[1].is_signer = false;
    rejects(f.send(ix, &[]), 3010);
}

#[test]
fn settle_rejects_other_operator_excess_k_redirects_and_expiry() {
    let mut f = Fixture::new();
    f.init();
    let id = [4; 16];
    f.deposit(id, 4, [900_000, 100_000]);
    rejects(
        f.send(f.settle_ix(id, 2, f.stranger.pubkey()), &[Role::Stranger]),
        code(enki_escrow::EscrowError::Unauthorized),
    );
    rejects(
        f.settle(id, 5),
        code(enki_escrow::EscrowError::TooManyDeliveredUnits),
    );
    let mut ix = f.settle_ix(id, 2, f.operator.pubkey());
    ix.accounts[SETTLE_RECIPIENT_OFFSET].pubkey = f.artist_ata;
    rejects(
        f.send(ix, &[Role::Operator]),
        code(enki_escrow::EscrowError::NonCanonicalAta),
    );
    let mut ix = f.settle_ix(id, 2, f.operator.pubkey());
    ix.accounts[SETTLE_RECIPIENT_OFFSET + 1].pubkey = f.treasury_ata;
    rejects(
        f.send(ix, &[Role::Operator]),
        code(enki_escrow::EscrowError::NonCanonicalAta),
    );
    f.clock(NOW + 900);
    rejects(f.settle(id, 0), code(enki_escrow::EscrowError::Expired));
    assert_eq!(f.tokens(f.pdas(id).1), 4_000_000);
    assert_eq!(f.tokens(f.treasury_ata), 0);
}

#[test]
fn deposit_requires_configured_operator_even_for_buyer_chosen_terms() {
    let mut f = Fixture::new();
    f.init();
    let id = [20; 16];
    for (payer, roles) in [
        (f.buyer.pubkey(), vec![Role::Buyer]),
        (f.stranger.pubkey(), vec![Role::Buyer, Role::Stranger]),
    ] {
        let mut ix = f.deposit_ix(id, 24, [1, 1], NOW + 600);
        ix.accounts[2].pubkey = payer;
        ix.data = instruction::Deposit {
            intent_id: id,
            units: 24,
            recipient_owners: [
                f.treasury,
                f.buyer.pubkey(),
                Pubkey::default(),
                Pubkey::default(),
            ],
            unit_amounts: [1, 1, 0, 0],
            expires_at: NOW + 600,
        }
        .data();
        rejects(
            f.send(ix, &roles),
            code(enki_escrow::EscrowError::Unauthorized),
        );
        assert_eq!(f.tokens(f.buyer_ata), BUYER_TOKENS);
        for address in [f.pdas(id).0, f.pdas(id).1] {
            assert!(f.svm.get_account(&address).is_none_or(|a| a.lamports == 0));
        }
    }
    f.deposit(id, 4, [900_000, 100_000]);
    assert_eq!(f.escrow(id).rent_payer, f.operator.pubkey());
    assert_eq!(f.tokens(f.pdas(id).1), 4_000_000);
}

#[test]
fn deposit_requires_server_signature_even_when_buyer_pays_transaction_fee() {
    let mut f = Fixture::new();
    f.init();
    let id = [21; 16];
    let mut ix = f.deposit_ix(id, 1, [1_000_000, 0], NOW + 900);
    ix.accounts[2].is_signer = false;
    f.svm.expire_blockhash();
    let message =
        Message::new_with_blockhash(&[ix], Some(&f.buyer.pubkey()), &f.svm.latest_blockhash());
    let transaction =
        VersionedTransaction::try_new(VersionedMessage::Legacy(message), &[&f.buyer]).unwrap();
    rejects(f.svm.send_transaction(transaction), 3010);
    assert_eq!(f.tokens(f.buyer_ata), BUYER_TOKENS);
    assert!(f
        .svm
        .get_account(&f.pdas(id).0)
        .is_none_or(|a| a.lamports == 0));
    f.deposit(id, 1, [1_000_000, 0]);
}

#[test]
fn settle_requires_operator_signature_even_when_stranger_pays_transaction_fee() {
    let mut f = Fixture::new();
    f.init();
    let id = [27; 16];
    f.deposit(id, 4, [900_000, 100_000]);
    let (escrow, vault) = f.pdas(id);
    let before = f.svm.get_account(&escrow).unwrap();
    let mut ix = f.settle_ix(id, 2, f.operator.pubkey());
    ix.accounts[1].is_signer = false;
    rejects(f.send_as_stranger(ix), 3010);
    assert_eq!(f.svm.get_account(&escrow).unwrap(), before);
    assert_eq!(f.tokens(vault), 4_000_000);
    assert_eq!(f.tokens(f.treasury_ata), 0);
    assert_eq!(f.tokens(f.artist_ata), 0);
    f.settle(id, 2).unwrap();
    assert_eq!(f.escrow(id).refund_due, 2_000_000);
}

#[test]
fn donated_dust_cannot_lock_zero_refund_or_stored_rent() {
    for donate_after_settle in [false, true] {
        let mut f = Fixture::new();
        f.init();
        let id = [28; 16];
        f.deposit(id, 1, [1_000_000, 0]);
        if donate_after_settle {
            f.settle(id, 1).unwrap();
        }
        // Both the donation and ATA reassignment are real buyer-signed SPL instructions.
        f.donate(id, 1);
        let reassign = spl_token::instruction::set_authority(
            &token::ID,
            &f.buyer_ata,
            Some(&f.stranger.pubkey()),
            spl_token::instruction::AuthorityType::AccountOwner,
            &f.buyer.pubkey(),
            &[],
        )
        .unwrap();
        f.send_as_buyer(reassign).unwrap();
        if !donate_after_settle {
            f.settle(id, 1).unwrap();
        }
        assert_eq!(f.escrow(id).refund_due, 0);
        let (escrow, vault) = f.pdas(id);
        assert_eq!(f.tokens(vault), 1);
        let buyer_account = f.svm.get_account(&f.buyer_ata).unwrap();
        let rent = f.svm.get_balance(&escrow).unwrap() + f.svm.get_balance(&vault).unwrap();
        let payer_before = f.svm.get_balance(&f.payer.pubkey()).unwrap();
        let ix = f.reclaim_ix(id, f.payer.pubkey(), false);
        f.send_as_stranger(ix).unwrap();
        assert_eq!(f.svm.get_account(&f.buyer_ata).unwrap(), buyer_account);
        assert_eq!(f.tokens(f.treasury_ata), 1_000_001);
        assert_eq!(
            f.svm.get_balance(&f.payer.pubkey()).unwrap(),
            payer_before + rent
        );
        assert!(f.svm.get_account(&escrow).is_none_or(|a| a.lamports == 0));
        assert!(f.svm.get_account(&vault).is_none_or(|a| a.lamports == 0));
    }
}

#[test]
fn donations_do_not_increase_partial_artist_or_expired_buyer_refunds() {
    for (delivered, unavailable_artist) in [(None, false), (Some(2), false), (Some(4), true)] {
        let mut f = Fixture::new();
        f.init();
        let id = [29; 16];
        f.deposit(id, 4, [900_000, 100_000]);
        f.donate(id, 1_000_001);
        if unavailable_artist {
            f.change_ata(f.artist_ata, AtaChange::ReassignedOwner);
        }
        let k = delivered.unwrap_or(0);
        if delivered.is_some() {
            f.settle(id, k).unwrap();
        } else {
            f.clock(NOW + 900);
        }
        let treasury_paid = u64::from(k) * 900_000;
        let artist_paid = if unavailable_artist {
            0
        } else {
            u64::from(k) * 100_000
        };
        let due = 4_000_000 - treasury_paid - artist_paid;
        assert_eq!(f.escrow(id).refund_due, due);
        assert_eq!(f.tokens(f.pdas(id).1), due + 1_000_001);
        f.reclaim(id).unwrap();
        assert_eq!(
            f.tokens(f.buyer_ata),
            BUYER_TOKENS - treasury_paid - artist_paid - 1_000_001
        );
        assert_eq!(f.tokens(f.treasury_ata), treasury_paid + 1_000_001);
        assert_eq!(f.tokens(f.artist_ata), artist_paid);
        assert!(f
            .svm
            .get_account(&f.pdas(id).0)
            .is_none_or(|a| a.lamports == 0));
    }
}

#[test]
fn donations_do_not_change_missing_buyer_ata_fee_boundaries() {
    for due in [0, 999_999, 1_000_000, 1_500_000] {
        let mut f = Fixture::new();
        f.init();
        let id = [30; 16];
        f.deposit(id, 1, [due.max(1), 0]);
        f.donate(id, 2_000_000);
        f.settle(id, u8::from(due == 0)).unwrap();
        assert_eq!(f.escrow(id).refund_due, due);
        f.remove_ata(f.buyer_ata);
        let ix = f.reclaim_ix(id, f.payer.pubkey(), false);
        if due == 0 {
            f.send_as_stranger(ix).unwrap();
        } else {
            f.reclaim(id).unwrap();
        }
        assert_eq!(
            f.tokens(f.treasury_ata),
            2_000_000 + due.min(1_000_000) + u64::from(due == 0)
        );
        assert_eq!(f.tokens(f.buyer_ata), due.saturating_sub(1_000_000));
        if due >= 1_000_000 {
            assert_eq!(f.svm.get_account(&f.buyer_ata).unwrap().owner, token::ID);
        } else {
            assert!(f
                .svm
                .get_account(&f.buyer_ata)
                .is_none_or(|a| a.lamports == 0));
        }
        assert!(f
            .svm
            .get_account(&f.pdas(id).0)
            .is_none_or(|a| a.lamports == 0));
    }
}

#[test]
fn unusable_artist_ata_cannot_block_treasury_settlement() {
    for change in UNUSABLE_ATAS {
        let mut f = Fixture::new();
        f.init();
        let id = [22; 16];
        f.deposit(id, 4, [900_000, 100_000]);
        f.change_ata(f.artist_ata, change);
        let unavailable = f.svm.get_account(&f.artist_ata).unwrap();
        f.settle(id, 4).unwrap();
        assert_eq!(f.tokens(f.treasury_ata), 3_600_000, "{change:?}");
        assert_eq!(
            f.svm.get_account(&f.artist_ata).unwrap(),
            unavailable,
            "{change:?}"
        );
        assert_eq!(f.tokens(f.pdas(id).1), 400_000);
        assert_eq!(f.escrow(id).refund_due, 400_000);
        assert!(matches!(
            f.escrow(id).state,
            enki_escrow::EscrowState::Settled
        ));
        f.reclaim(id).unwrap();
        assert_eq!(f.tokens(f.buyer_ata), BUYER_TOKENS - 3_600_000);
    }
}

#[test]
fn zero_refund_closes_with_unusable_buyer_ata_and_returns_stored_rent() {
    for change in UNUSABLE_ATAS {
        let mut f = Fixture::new();
        f.init();
        let id = [23; 16];
        f.deposit(id, 1, [1_000_000, 0]);
        f.settle(id, 1).unwrap();
        f.change_ata(f.buyer_ata, change);
        let unavailable = f.svm.get_account(&f.buyer_ata).unwrap();
        let (escrow, vault) = f.pdas(id);
        let rent = f.svm.get_balance(&escrow).unwrap() + f.svm.get_balance(&vault).unwrap();
        let payer_before = f.svm.get_balance(&f.payer.pubkey()).unwrap();
        let ix = f.reclaim_ix(id, f.payer.pubkey(), false);
        f.send_as_stranger(ix).unwrap();
        assert_eq!(
            f.svm.get_account(&f.buyer_ata).unwrap(),
            unavailable,
            "{change:?}"
        );
        assert_eq!(f.tokens(f.treasury_ata), 1_000_000);
        assert_eq!(
            f.svm.get_balance(&f.payer.pubkey()).unwrap(),
            payer_before + rent
        );
        assert!(f.svm.get_account(&escrow).is_none_or(|a| a.lamports == 0));
        assert!(f.svm.get_account(&vault).is_none_or(|a| a.lamports == 0));
    }
}

#[test]
fn positive_refund_rejects_unusable_buyer_and_succeeds_after_restore() {
    use enki_escrow::EscrowError as E;
    for (change, error) in [
        (AtaChange::ReassignedOwner, E::WrongTokenOwner),
        (AtaChange::WrongMint, E::WrongMint),
        (AtaChange::WrongProgram, E::WrongTokenProgram),
        (AtaChange::Malformed, E::InvalidTokenAccount),
        (AtaChange::Uninitialized, E::InvalidTokenAccount),
        (AtaChange::Frozen, E::FrozenBuyerAta),
    ] {
        let mut f = Fixture::new();
        f.init();
        let id = [24; 16];
        f.deposit(id, 4, [1_000_000, 0]);
        f.settle(id, 2).unwrap();
        let original_buyer = f.svm.get_account(&f.buyer_ata).unwrap();
        f.change_ata(f.buyer_ata, change);
        let (escrow, vault) = f.pdas(id);
        let original_escrow = f.svm.get_account(&escrow).unwrap();
        let original_vault = f.svm.get_account(&vault).unwrap();
        rejects(f.reclaim(id), code(error));
        assert_eq!(
            f.svm.get_account(&escrow).unwrap(),
            original_escrow,
            "{change:?}"
        );
        assert_eq!(
            f.svm.get_account(&vault).unwrap(),
            original_vault,
            "{change:?}"
        );
        assert_eq!(f.tokens(vault), 2_000_000);
        assert_eq!(f.tokens(f.treasury_ata), 2_000_000);
        f.svm.set_account(f.buyer_ata, original_buyer).unwrap();
        f.reclaim(id).unwrap();
        assert_eq!(f.tokens(f.buyer_ata), BUYER_TOKENS - 2_000_000);
    }
}

#[test]
fn operator_rotation_does_not_redirect_old_rent_or_missing_ata_sponsorship() {
    let mut f = Fixture::new();
    f.init();
    let id = [25; 16];
    f.deposit(id, 1, [1_500_000, 0]);
    f.settle(id, 0).unwrap();
    let mut args = f.args();
    args.operator = f.stranger.pubkey();
    f.send(f.update_ix(f.admin.pubkey(), args), &[Role::Admin])
        .unwrap();
    rejects(
        f.send(f.deposit_ix([26; 16], 1, [1, 0], NOW + 900), &[Role::Buyer]),
        code(enki_escrow::EscrowError::Unauthorized),
    );
    f.remove_ata(f.buyer_ata);
    let ix = f.reclaim_ix(id, f.stranger.pubkey(), true);
    rejects(
        f.send(ix, &[Role::Stranger]),
        code(enki_escrow::EscrowError::WrongRentPayer),
    );
    f.reclaim(id).unwrap();
    assert_eq!(f.tokens(f.buyer_ata), 500_000);
    assert_eq!(f.tokens(f.treasury_ata), 1_000_000);
    assert!(f
        .svm
        .get_account(&f.pdas(id).0)
        .is_none_or(|a| a.lamports == 0));
}

#[test]
fn missing_or_frozen_artist_share_stays_in_buyer_refund() {
    for frozen in [false, true] {
        let mut f = Fixture::new();
        f.init();
        let id = [5; 16];
        f.deposit(id, 4, [900_000, 100_000]);
        if frozen {
            f.set_tokens(
                f.artist_ata,
                f.artist,
                0,
                spl_token::state::AccountState::Frozen,
            );
        } else {
            f.remove_ata(f.artist_ata);
        }
        f.settle(id, 4).unwrap();
        assert_eq!(f.tokens(f.treasury_ata), 3_600_000);
        assert_eq!(f.tokens(f.artist_ata), 0);
        assert_eq!(f.tokens(f.pdas(id).1), 400_000);
        f.reclaim(id).unwrap();
        assert_eq!(f.tokens(f.buyer_ata), BUYER_TOKENS - 3_600_000);
    }
}

#[test]
fn expiry_refunds_funded_escrow_and_any_caller_can_reclaim() {
    let mut f = Fixture::new();
    f.init();
    let id = [6; 16];
    f.deposit(id, 1, [900_000, 100_000]);
    let ix = f.reclaim_ix(id, f.payer.pubkey(), false);
    rejects(
        f.send_as_stranger(ix),
        code(enki_escrow::EscrowError::NotReclaimable),
    );
    f.clock(NOW + 900);
    let (escrow, vault) = f.pdas(id);
    let rent = f.svm.get_balance(&escrow).unwrap() + f.svm.get_balance(&vault).unwrap();
    let payer_before = f.svm.get_balance(&f.payer.pubkey()).unwrap();
    let ix = f.reclaim_ix(id, f.payer.pubkey(), false);
    f.send_as_stranger(ix).unwrap();
    assert_eq!(
        f.svm.get_balance(&f.payer.pubkey()).unwrap(),
        payer_before + rent
    );
    assert_eq!(f.tokens(f.buyer_ata), BUYER_TOKENS);
    assert_eq!(f.tokens(f.treasury_ata), 0);
    assert_eq!(f.tokens(f.artist_ata), 0);
}

#[test]
fn missing_buyer_ata_fee_rules_are_exact_and_rent_destination_cannot_change() {
    for remaining in [0, 999_999, 1_000_000, 1_500_000] {
        let mut f = Fixture::new();
        f.init();
        let id = [7; 16];
        f.deposit(id, 1, [remaining.max(1), 0]);
        f.settle(id, u8::from(remaining == 0)).unwrap();
        f.remove_ata(f.buyer_ata);
        let ix = f.reclaim_ix(id, f.stranger.pubkey(), false);
        rejects(
            f.send(ix, &[Role::Stranger]),
            code(enki_escrow::EscrowError::WrongRentPayer),
        );
        if remaining >= 1_000_000 {
            let result = f.reclaim(id).unwrap();
            assert!(result.compute_units_consumed < 200_000);
            assert_eq!(f.tokens(f.buyer_ata), remaining - 1_000_000);
            assert_eq!(f.tokens(f.treasury_ata), 1_000_000);
            assert_eq!(f.svm.get_account(&f.buyer_ata).unwrap().owner, token::ID);
        } else {
            if remaining == 0 {
                let ix = f.reclaim_ix(id, f.payer.pubkey(), false);
                f.send_as_stranger(ix).unwrap();
            } else {
                f.reclaim(id).unwrap();
            }
            assert!(f
                .svm
                .get_account(&f.buyer_ata)
                .is_none_or(|a| a.lamports == 0));
            assert_eq!(f.tokens(f.treasury_ata), remaining.max(1));
        }
    }
}

#[test]
fn missing_buyer_ata_requires_stored_rent_payer_signature() {
    let mut f = Fixture::new();
    f.init();
    let id = [8; 16];
    f.deposit(id, 1, [1_500_000, 0]);
    f.settle(id, 0).unwrap();
    f.remove_ata(f.buyer_ata);
    // Use a different transaction fee payer so the stored rent payer does not sign implicitly.
    let ix = f.reclaim_ix(id, f.payer.pubkey(), false);
    rejects(
        f.send_as_stranger(ix),
        code(enki_escrow::EscrowError::RentPayerMustSign),
    );
    assert_eq!(f.tokens(f.pdas(id).1), 1_500_000);
}

#[test]
fn frozen_buyer_prevents_reclaim_without_losing_money() {
    let mut f = Fixture::new();
    f.init();
    let id = [9; 16];
    f.deposit(id, 1, [1_000_000, 0]);
    f.settle(id, 0).unwrap();
    f.set_tokens(
        f.buyer_ata,
        f.buyer.pubkey(),
        BUYER_TOKENS - 1_000_000,
        spl_token::state::AccountState::Frozen,
    );
    rejects(
        f.reclaim(id),
        code(enki_escrow::EscrowError::FrozenBuyerAta),
    );
    assert_eq!(f.tokens(f.pdas(id).1), 1_000_000);
}

#[test]
fn guardian_can_only_pause_or_revoke_admin_restores_and_paused_refunds_work() {
    let mut f = Fixture::new();
    f.init();
    let id = [10; 16];
    f.deposit(id, 4, [900_000, 100_000]);
    rejects(
        f.send(f.emergency(f.stranger.pubkey(), false), &[Role::Stranger]),
        code(enki_escrow::EscrowError::Unauthorized),
    );
    f.send(f.emergency(f.guardian.pubkey(), false), &[Role::Guardian])
        .unwrap();
    rejects(
        f.send(f.deposit_ix([11; 16], 1, [1, 0], NOW + 900), &[Role::Buyer]),
        code(enki_escrow::EscrowError::Paused),
    );
    rejects(
        f.send(
            f.update_ix(f.guardian.pubkey(), f.args()),
            &[Role::Guardian],
        ),
        code(enki_escrow::EscrowError::Unauthorized),
    );
    f.settle(id, 2).unwrap();
    f.reclaim(id).unwrap();
    f.send(f.emergency(f.guardian.pubkey(), true), &[Role::Guardian])
        .unwrap();
    let config =
        Config::try_deserialize(&mut f.svm.get_account(&f.config).unwrap().data.as_slice())
            .unwrap();
    assert_eq!(config.operator, Pubkey::default());
    assert!(config.paused);
    f.send(f.update_ix(f.admin.pubkey(), f.args()), &[Role::Admin])
        .unwrap();
    f.deposit([11; 16], 1, [1, 0]);
    f.settle([11; 16], 0).unwrap();
    f.reclaim([11; 16]).unwrap();
}

#[test]
fn config_rejects_zero_cap_and_invalid_ttl_or_refund_fee_limits() {
    let mut f = Fixture::new();
    f.init();
    for kind in 0..4 {
        let mut args = f.args();
        match kind {
            0 => args.max_deposit_micro = 0,
            1 => args.refund_ata_fee_micro = 1_000_001,
            2 => args.min_ttl_s = 599,
            _ => args.max_ttl_s = 1_801,
        }
        assert!(f
            .send(f.update_ix(f.admin.pubkey(), args), &[Role::Admin])
            .is_err());
    }
}

#[test]
fn config_alone_controls_new_deposits_and_lowering_it_does_not_block_refunds() {
    let mut f = Fixture::new();
    f.init();
    let original = [12; 16];
    f.deposit(original, 4, [12_500_000, 0]);
    rejects(
        f.send(
            f.deposit_ix([13; 16], 1, [60_000_000, 0], NOW + 900),
            &[Role::Buyer],
        ),
        code(enki_escrow::EscrowError::DepositCap),
    );
    let mut args = f.args();
    args.max_deposit_micro = 60_000_000;
    f.send(f.update_ix(f.admin.pubkey(), args), &[Role::Admin])
        .unwrap();
    f.deposit([13; 16], 1, [60_000_000, 0]);
    let mut args = f.args();
    args.max_deposit_micro = 5_000_000;
    f.send(f.update_ix(f.admin.pubkey(), args), &[Role::Admin])
        .unwrap();
    rejects(
        f.send(
            f.deposit_ix([14; 16], 1, [5_000_001, 0], NOW + 900),
            &[Role::Buyer],
        ),
        code(enki_escrow::EscrowError::DepositCap),
    );
    f.settle(original, 2).unwrap();
    f.reclaim(original).unwrap();
    f.settle([13; 16], 0).unwrap();
    f.reclaim([13; 16]).unwrap();
    assert_eq!(f.tokens(f.buyer_ata), BUYER_TOKENS - 25_000_000);
}

#[test]
fn randomized_100000_program_sequences_conserve_tokens_and_never_pay_twice() {
    let mut f = Fixture::new();
    f.init();
    let cap = Config::try_deserialize(&mut f.svm.get_account(&f.config).unwrap().data.as_slice())
        .unwrap()
        .max_deposit_micro;
    let mut random = 0x1234_5678_9abc_def0_u64;
    for iteration in 0_u64..100_000 {
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        let units = (random % 24 + 1) as u8;
        let per_unit = random.rotate_left(17) % (cap / u64::from(units)) + 1;
        let artist = random.rotate_left(29) % per_unit;
        let amounts = [per_unit - artist, artist];
        let mut id = [0_u8; 16];
        id[..8].copy_from_slice(&iteration.to_le_bytes());
        let buyer_before = f.tokens(f.buyer_ata);
        let treasury_before = f.tokens(f.treasury_ata);
        let artist_before = f.tokens(f.artist_ata);
        f.deposit(id, units, amounts);
        assert_eq!(f.escrow(id).refund_due, u64::from(units) * per_unit);
        let k = (random.rotate_left(41) % (u64::from(units) + 1)) as u8;
        f.settle(id, k).unwrap();
        assert_eq!(f.escrow(id).refund_due, u64::from(units - k) * per_unit);
        rejects(
            f.settle(id, k),
            code(enki_escrow::EscrowError::AlreadySettled),
        );
        f.reclaim(id).unwrap();
        let treasury_paid = f.tokens(f.treasury_ata) - treasury_before;
        let artist_paid = f.tokens(f.artist_ata) - artist_before;
        assert_eq!(treasury_paid, u64::from(k) * amounts[0], "case {iteration}");
        assert_eq!(artist_paid, u64::from(k) * amounts[1], "case {iteration}");
        assert_eq!(
            buyer_before - f.tokens(f.buyer_ata),
            treasury_paid + artist_paid,
            "case {iteration}"
        );
        if (iteration + 1) % 10_000 == 0 {
            println!("Validated {} of 100000 escrow sequences", iteration + 1);
        }
    }
}

#[test]
fn four_recipient_partial_delivery_records_each_paid_leg_and_refunds_only_unused_units() {
    let mut f = Fixture::new();
    f.init();
    let id = [40; 16];
    let amounts = [700_000, 100_000, 50_000, 150_000];
    let author_ata = Fixture::author_pdas(X_USER_ID).1;
    f.deposit_four(id, 4, amounts);
    let deposited = f.escrow(id);
    assert_eq!(deposited.version, 2);
    assert_eq!(deposited.recipient_count, 4);
    assert_eq!(deposited.recipient_owners, f.recipient_owners(amounts));
    assert_eq!(deposited.unit_amounts, amounts);
    assert_eq!(deposited.paid_amounts, [0; 4]);
    assert_eq!(f.svm.get_account(&f.pdas(id).0).unwrap().data.len(), 343);
    assert_eq!(deposited.refund_due, 4_000_000);
    let settled = f.settle(id, 2).unwrap();
    assert!(settled.compute_units_consumed < 200_000);
    println!(
        "Four-recipient settle consumed {} compute units",
        settled.compute_units_consumed
    );
    assert_eq!(
        f.escrow(id).paid_amounts,
        [1_400_000, 200_000, 100_000, 300_000]
    );
    assert_eq!(f.escrow(id).settled_units, 2);
    assert_eq!(f.escrow(id).refund_due, 2_000_000);
    for (ata, amount) in [
        (f.treasury_ata, 1_400_000),
        (f.artist_ata, 200_000),
        (f.referrer_ata, 100_000),
        (author_ata, 300_000),
    ] {
        assert_eq!(f.tokens(ata), amount);
    }
    let stored = f.svm.get_account(&f.pdas(id).0).unwrap();
    rejects(
        f.settle(id, 4),
        code(enki_escrow::EscrowError::AlreadySettled),
    );
    assert_eq!(f.svm.get_account(&f.pdas(id).0).unwrap(), stored);
    f.reclaim(id).unwrap();
    assert_eq!(f.tokens(f.buyer_ata), BUYER_TOKENS - 2_000_000);
    assert_eq!(f.tokens(author_ata), 300_000);
    assert_eq!(f.tokens(f.referrer_ata), 100_000);
}

#[test]
fn sparse_author_slot_settles_and_repeat_settlement_is_rejected() {
    let mut f = Fixture::new();
    f.init();
    let id = [41; 16];
    f.deposit_four(id, 2, [900_000, 0, 0, 100_000]);
    let stored = f.escrow(id);
    assert_eq!(stored.recipient_count, 4);
    assert_eq!(stored.recipient_owners[1..3], [Pubkey::default(); 2]);
    let ix = f.settle_ix(id, 2, f.operator.pubkey());
    assert_eq!(ix.accounts.len(), SETTLE_RECIPIENT_OFFSET + 4);
    assert_eq!(
        ix.accounts[SETTLE_RECIPIENT_OFFSET + 1].pubkey,
        Pubkey::default()
    );
    assert_eq!(
        ix.accounts[SETTLE_RECIPIENT_OFFSET + 2].pubkey,
        Pubkey::default()
    );
    f.send(ix, &[Role::Operator]).unwrap();
    assert_eq!(f.tokens(f.treasury_ata), 1_800_000);
    assert_eq!(f.tokens(Fixture::author_pdas(X_USER_ID).1), 200_000);
    assert_eq!(f.escrow(id).paid_amounts, [1_800_000, 0, 0, 200_000]);
    assert_eq!(f.escrow(id).refund_due, 0);
    let settled = f.svm.get_account(&f.pdas(id).0).unwrap();
    rejects(
        f.settle(id, 2),
        code(enki_escrow::EscrowError::AlreadySettled),
    );
    assert_eq!(f.svm.get_account(&f.pdas(id).0).unwrap(), settled);
    assert_eq!(f.tokens(Fixture::author_pdas(X_USER_ID).1), 200_000);
    f.reclaim(id).unwrap();
}

#[test]
fn free_prompt_and_zero_rounded_slots_keep_their_fixed_positions() {
    for (amounts, count) in [
        ([950_000, 0, 25_000, 25_000], 4),
        ([975_000, 0, 25_000, 0], 3),
        ([99, 0, 0, 1], 4),
        ([1, 0, 0, 0], 1),
    ] {
        let mut f = Fixture::new();
        f.init();
        let id = [42; 16];
        f.deposit_four(id, 1, amounts);
        let stored = f.escrow(id);
        assert_eq!(stored.recipient_count, count);
        assert_eq!(stored.recipient_owners, f.recipient_owners(amounts));
        f.settle(id, 1).unwrap();
        assert_eq!(f.escrow(id).paid_amounts, amounts);
        assert_eq!(f.tokens(f.artist_ata), 0);
        assert_eq!(f.tokens(f.referrer_ata), amounts[2]);
        assert_eq!(f.tokens(Fixture::author_pdas(X_USER_ID).1), amounts[3]);
        assert_eq!(f.escrow(id).refund_due, 0);
        f.reclaim(id).unwrap();
        assert_eq!(
            f.tokens(f.buyer_ata),
            BUYER_TOKENS - amounts.iter().sum::<u64>()
        );
    }
}

#[test]
fn four_slot_deposit_rejects_owner_amount_mismatches_without_funding() {
    for slot in 1..4 {
        for positive_amount in [false, true] {
            let mut f = Fixture::new();
            f.init();
            let id = [43; 16];
            let mut amounts = [700_000, 100_000, 50_000, 150_000];
            let mut owners = f.recipient_owners(amounts);
            if positive_amount {
                owners[slot] = Pubkey::default();
            } else {
                amounts[slot] = 0;
            }
            let ix = f.deposit_four_ix(id, 1, owners, amounts, NOW + 900);
            rejects(
                f.send(ix, &[Role::Buyer]),
                code(enki_escrow::EscrowError::RecipientMismatch),
            );
            assert_eq!(f.tokens(f.buyer_ata), BUYER_TOKENS);
            assert!(f
                .svm
                .get_account(&f.pdas(id).0)
                .is_none_or(|a| a.lamports == 0));
            assert!(f
                .svm
                .get_account(&f.pdas(id).1)
                .is_none_or(|a| a.lamports == 0));
        }
    }
}

#[test]
fn each_missing_or_unusable_optional_ata_only_forfeits_its_own_leg() {
    for slot in 1..4 {
        for change in std::iter::once(None).chain(UNUSABLE_ATAS.into_iter().map(Some)) {
            let mut f = Fixture::new();
            f.init();
            let id = [44; 16];
            let amounts = [700_000, 100_000, 50_000, 150_000];
            let recipients = [
                f.treasury_ata,
                f.artist_ata,
                f.referrer_ata,
                Fixture::author_pdas(X_USER_ID).1,
            ];
            f.deposit_four(id, 4, amounts);
            if let Some(change) = change {
                f.change_ata(recipients[slot], change);
            } else {
                f.remove_ata(recipients[slot]);
            }
            let unusable = f.svm.get_account(&recipients[slot]);
            f.settle(id, 4).unwrap();
            let expected: [u64; 4] =
                std::array::from_fn(|i| if i == slot { 0 } else { 4 * amounts[i] });
            assert_eq!(
                f.escrow(id).paid_amounts,
                expected,
                "slot {slot}, {change:?}"
            );
            assert_eq!(f.escrow(id).refund_due, 4 * amounts[slot]);
            assert_eq!(f.tokens(f.pdas(id).1), 4 * amounts[slot]);
            for i in 0..4 {
                if i != slot {
                    assert_eq!(f.tokens(recipients[i]), expected[i]);
                }
            }
            if change.is_some() {
                assert_eq!(f.svm.get_account(&recipients[slot]), unusable);
            }
            f.reclaim(id).unwrap();
            assert_eq!(
                f.tokens(f.buyer_ata),
                BUYER_TOKENS - expected.iter().sum::<u64>()
            );
            assert_eq!(f.tokens(f.treasury_ata), 2_800_000);
        }
    }
}

#[test]
fn multiple_unusable_optional_atas_preserve_the_other_payments_and_exact_refund() {
    let mut f = Fixture::new();
    f.init();
    let id = [45; 16];
    f.deposit_four(id, 4, [700_000, 100_000, 50_000, 150_000]);
    f.remove_ata(f.artist_ata);
    f.change_ata(f.referrer_ata, AtaChange::Frozen);
    f.change_ata(
        Fixture::author_pdas(X_USER_ID).1,
        AtaChange::ReassignedOwner,
    );
    f.donate(id, 7);
    f.settle(id, 2).unwrap();
    assert_eq!(f.escrow(id).paid_amounts, [1_400_000, 0, 0, 0]);
    assert_eq!(f.escrow(id).refund_due, 2_600_000);
    assert_eq!(f.tokens(f.pdas(id).1), 2_600_007);
    f.reclaim(id).unwrap();
    assert_eq!(f.tokens(f.buyer_ata), BUYER_TOKENS - 1_400_000 - 7);
    assert_eq!(f.tokens(f.treasury_ata), 1_400_007);
}

#[test]
fn four_leg_dust_cannot_force_a_buyer_ata_check_after_full_payment() {
    let mut f = Fixture::new();
    f.init();
    let id = [46; 16];
    f.deposit_four(id, 1, [700_000, 100_000, 50_000, 150_000]);
    f.settle(id, 1).unwrap();
    f.donate(id, 1);
    let reassign = spl_token::instruction::set_authority(
        &token::ID,
        &f.buyer_ata,
        Some(&f.stranger.pubkey()),
        spl_token::instruction::AuthorityType::AccountOwner,
        &f.buyer.pubkey(),
        &[],
    )
    .unwrap();
    f.send_as_buyer(reassign).unwrap();
    let buyer = f.svm.get_account(&f.buyer_ata).unwrap();
    let (escrow, vault) = f.pdas(id);
    assert_eq!(f.escrow(id).refund_due, 0);
    assert_eq!(
        f.escrow(id).paid_amounts,
        [700_000, 100_000, 50_000, 150_000]
    );
    let rent = f.svm.get_balance(&escrow).unwrap() + f.svm.get_balance(&vault).unwrap();
    let rent_payer_before = f.svm.get_balance(&f.payer.pubkey()).unwrap();
    let ix = f.reclaim_ix(id, f.payer.pubkey(), false);
    f.send_as_stranger(ix).unwrap();
    assert_eq!(f.svm.get_account(&f.buyer_ata).unwrap(), buyer);
    assert_eq!(f.tokens(f.treasury_ata), 700_001);
    assert_eq!(
        f.svm.get_balance(&f.payer.pubkey()).unwrap(),
        rent_payer_before + rent
    );
    assert!(f.svm.get_account(&escrow).is_none_or(|a| a.lamports == 0));
    assert!(f.svm.get_account(&vault).is_none_or(|a| a.lamports == 0));
}

#[test]
fn settle_rejects_missing_reordered_noncanonical_and_writable_slot_metas() {
    let mut f = Fixture::new();
    f.init();
    let id = [47; 16];
    f.deposit_four(id, 1, [700_000, 100_000, 50_000, 150_000]);
    let original = f.svm.get_account(&f.pdas(id).0).unwrap();
    for kind in 0..4 {
        let mut ix = f.settle_ix(id, 1, f.operator.pubkey());
        match kind {
            0 => {
                ix.accounts.pop();
            }
            1 => ix
                .accounts
                .swap(SETTLE_RECIPIENT_OFFSET + 2, SETTLE_RECIPIENT_OFFSET + 3),
            2 => ix.accounts[SETTLE_RECIPIENT_OFFSET + 2].pubkey = f.artist_ata,
            _ => ix.accounts[SETTLE_RECIPIENT_OFFSET + 2].is_writable = false,
        }
        assert!(f.send(ix, &[Role::Operator]).is_err(), "case {kind}");
        assert_eq!(f.svm.get_account(&f.pdas(id).0).unwrap(), original);
        assert_eq!(f.tokens(f.pdas(id).1), 1_000_000);
        assert_eq!(f.tokens(f.treasury_ata), 0);
    }
    f.settle(id, 1).unwrap();
}

#[test]
fn sparse_zero_slot_requires_a_system_program_placeholder() {
    let mut f = Fixture::new();
    f.init();
    let id = [48; 16];
    f.deposit_four(id, 1, [900_000, 0, 0, 100_000]);
    let mut ix = f.settle_ix(id, 1, f.operator.pubkey());
    ix.accounts[SETTLE_RECIPIENT_OFFSET + 1].pubkey = f.artist_ata;
    rejects(
        f.send(ix, &[Role::Operator]),
        code(enki_escrow::EscrowError::RecipientMismatch),
    );
    assert_eq!(f.escrow(id).paid_amounts, [0; 4]);
    assert_eq!(f.tokens(f.pdas(id).1), 1_000_000);
    f.settle(id, 1).unwrap();
}

#[test]
fn after_author_claim_new_author_legs_pay_the_bound_wallet_directly() {
    let mut f = Fixture::new();
    f.init();
    f.set_author_balance(X_USER_ID, 100_000);
    let claimant = f.stranger.pubkey();
    f.claim(X_USER_ID, claimant).unwrap();
    let claimant_ata = get_associated_token_address(&claimant, &USDC_MINT);
    assert_eq!(f.tokens(claimant_ata), 100_000);
    let id = [49; 16];
    let ix = f.deposit_four_ix(
        id,
        1,
        [f.treasury, Pubkey::default(), Pubkey::default(), claimant],
        [900_000, 0, 0, 100_000],
        NOW + 900,
    );
    f.send(ix, &[Role::Buyer]).unwrap();
    f.settle(id, 1).unwrap();
    assert_eq!(f.escrow(id).paid_amounts, [900_000, 0, 0, 100_000]);
    assert_eq!(f.tokens(claimant_ata), 200_000);
    assert_eq!(f.tokens(Fixture::author_pdas(X_USER_ID).1), 0);
    assert_eq!(f.author_claim(X_USER_ID).claimant, claimant);
    f.reclaim(id).unwrap();
}

#[test]
fn claim_binds_first_wallet_and_same_wallet_resweeps_fresh_funds() {
    let mut f = Fixture::new();
    f.init();
    f.set_author_balance(X_USER_ID, 3_000_007);
    let mut clock = f.svm.get_sysvar::<Clock>();
    clock.slot = 12_345;
    f.svm.set_sysvar(&clock);
    let (author, author_ata, claim_address) = Fixture::author_pdas(X_USER_ID);
    let buyer = f.buyer.pubkey();
    let author_rent = f.svm.get_balance(&author_ata).unwrap();
    let operator_before = f.svm.get_balance(&f.operator.pubkey()).unwrap();
    f.claim(X_USER_ID, buyer).unwrap();
    let claim = f.author_claim(X_USER_ID);
    assert_eq!(claim.version, 1);
    assert_eq!(claim.x_user_id, X_USER_ID);
    assert_eq!(claim.claimant, buyer);
    assert_eq!(claim.claimed_slot, clock.slot);
    assert_eq!(
        claim.bump,
        Pubkey::find_program_address(&[b"claim", &X_USER_ID.to_le_bytes()], &ID).1
    );
    assert_eq!(f.svm.get_account(&claim_address).unwrap().data.len(), 58);
    assert_eq!(f.tokens(author_ata), 0);
    assert_eq!(f.tokens(f.buyer_ata), BUYER_TOKENS + 3_000_007);
    assert_eq!(f.svm.get_balance(&author_ata).unwrap(), author_rent);
    let claim_rent = f.svm.get_balance(&claim_address).unwrap();
    assert_eq!(
        f.svm.get_balance(&f.operator.pubkey()).unwrap(),
        operator_before - claim_rent - 5_000
    );
    let binding = f.svm.get_account(&claim_address).unwrap().data;
    f.claim(X_USER_ID, buyer).unwrap();
    assert_eq!(f.tokens(f.buyer_ata), BUYER_TOKENS + 3_000_007);
    assert_eq!(f.svm.get_account(&claim_address).unwrap().data, binding);
    rejects(
        f.claim(X_USER_ID, f.stranger.pubkey()),
        code(enki_escrow::EscrowError::AlreadyClaimed),
    );
    let id = [50; 16];
    f.deposit_four(id, 1, [900_000, 0, 0, 100_000]);
    f.settle(id, 1).unwrap();
    f.reclaim(id).unwrap();
    let donation = spl_token::instruction::transfer_checked(
        &token::ID,
        &f.buyer_ata,
        &USDC_MINT,
        &author_ata,
        &f.buyer.pubkey(),
        &[],
        7,
        6,
    )
    .unwrap();
    f.send_as_buyer(donation).unwrap();
    assert_eq!(f.tokens(author_ata), 100_007);
    let before_repeat = f.tokens(f.buyer_ata);
    rejects(
        f.claim(X_USER_ID, f.stranger.pubkey()),
        code(enki_escrow::EscrowError::AlreadyClaimed),
    );
    assert_eq!(f.tokens(author_ata), 100_007);
    assert_eq!(f.tokens(f.buyer_ata), before_repeat);
    assert_eq!(f.svm.get_account(&claim_address).unwrap().data, binding);
    f.claim(X_USER_ID, buyer).unwrap();
    assert_eq!(f.tokens(f.buyer_ata), before_repeat + 100_007);
    assert_eq!(f.tokens(author_ata), 0);
    assert_eq!(f.svm.get_account(&claim_address).unwrap().data, binding);
    assert_eq!(f.svm.get_balance(&author_ata).unwrap(), author_rent);
    let token =
        spl_token::state::Account::unpack(&f.svm.get_account(&author_ata).unwrap().data).unwrap();
    assert_eq!(token.owner, author);
}

#[test]
fn claim_requires_operator_signature_even_when_stranger_pays_transaction_fee() {
    let mut f = Fixture::new();
    f.init();
    let claimant = f.buyer.pubkey();
    // Both init_if_needed accounts already exist: no account-creation payer
    // check can accidentally cover a weakened Signer constraint.
    f.claim(X_USER_ID, claimant).unwrap();
    f.set_author_balance(X_USER_ID, 123_456);
    let (_, author_ata, claim_address) = Fixture::author_pdas(X_USER_ID);
    let binding = f.svm.get_account(&claim_address).unwrap();
    let buyer_before = f.tokens(f.buyer_ata);
    let mut ix = f.claim_ix(X_USER_ID, claimant);
    ix.accounts[1].is_signer = false;
    rejects(f.send_as_stranger(ix), 3010);
    assert_eq!(f.tokens(author_ata), 123_456);
    assert_eq!(f.tokens(f.buyer_ata), buyer_before);
    assert_eq!(f.svm.get_account(&claim_address).unwrap(), binding);
    f.claim(X_USER_ID, claimant).unwrap();
    assert_eq!(f.tokens(f.buyer_ata), buyer_before + 123_456);
}

#[test]
fn claim_rejects_treasury_operator_and_default_claimants() {
    for kind in 0..4 {
        let mut f = Fixture::new();
        f.init();
        f.set_author_balance(X_USER_ID, 123_456);
        let (author, author_ata, claim_address) = Fixture::author_pdas(X_USER_ID);
        let claimant = match kind {
            0 => f.treasury,
            1 => f.operator.pubkey(),
            2 => Pubkey::default(),
            _ => author,
        };
        // Pre-create the canonical destination so only the handler's claimant
        // restriction decides, including the system-program/default address.
        let destination = get_associated_token_address(&claimant, &USDC_MINT);
        if destination != author_ata {
            f.set_tokens(
                destination,
                claimant,
                0,
                spl_token::state::AccountState::Initialized,
            );
        }
        rejects(
            f.claim(X_USER_ID, claimant),
            code(enki_escrow::EscrowError::VaultToTreasury),
        );
        assert_eq!(f.tokens(author_ata), 123_456);
        assert!(f
            .svm
            .get_account(&claim_address)
            .is_none_or(|a| a.lamports == 0));
        f.claim(X_USER_ID, f.buyer.pubkey()).unwrap();
    }
}

#[test]
fn claim_rejects_noncanonical_author_pda_and_token_accounts() {
    for kind in 0..5 {
        let mut f = Fixture::new();
        f.init();
        f.set_author_balance(X_USER_ID, 123_456);
        let (author, author_ata, claim_address) = Fixture::author_pdas(X_USER_ID);
        let mut ix = f.claim_ix(X_USER_ID, f.buyer.pubkey());
        match kind {
            0 => {
                let wrong_author =
                    Pubkey::find_program_address(&[b"author", &X_USER_ID.to_be_bytes()], &ID).0;
                let wrong_ata = get_associated_token_address(&wrong_author, &USDC_MINT);
                f.set_tokens(
                    wrong_ata,
                    wrong_author,
                    123_456,
                    spl_token::state::AccountState::Initialized,
                );
                ix.accounts[3].pubkey = wrong_author;
                ix.accounts[4].pubkey = wrong_ata;
            }
            1 => {
                ix.accounts[7].pubkey =
                    Pubkey::find_program_address(&[b"claim", &X_USER_ID.to_be_bytes()], &ID).0;
            }
            2 => {
                let noncanonical = Pubkey::new_unique();
                // Correct SPL mint and PDA authority, deliberately wrong ATA
                // address: this specifically kills a canonical-source mutant.
                f.set_tokens(
                    noncanonical,
                    author,
                    77,
                    spl_token::state::AccountState::Initialized,
                );
                ix.accounts[4].pubkey = noncanonical;
            }
            3 => {
                let noncanonical = Pubkey::new_unique();
                f.set_tokens(
                    noncanonical,
                    f.buyer.pubkey(),
                    0,
                    spl_token::state::AccountState::Initialized,
                );
                ix.accounts[6].pubkey = noncanonical;
            }
            _ => {
                ix.accounts[5].pubkey = f.stranger.pubkey();
                let destination = get_associated_token_address(&f.stranger.pubkey(), &USDC_MINT);
                f.set_tokens(
                    destination,
                    f.stranger.pubkey(),
                    0,
                    spl_token::state::AccountState::Initialized,
                );
                ix.accounts[6].pubkey = destination;
            }
        }
        assert!(f.send(ix, &[Role::Operator]).is_err(), "case {kind}");
        assert_eq!(f.tokens(author_ata), 123_456);
        assert_eq!(f.tokens(f.buyer_ata), BUYER_TOKENS);
        assert!(f
            .svm
            .get_account(&claim_address)
            .is_none_or(|a| a.lamports == 0));
        f.claim(X_USER_ID, f.buyer.pubkey()).unwrap();
        assert_eq!(f.tokens(author_ata), 0);
    }
}

#[test]
fn claim_creates_missing_claimant_ata_at_operator_cost_without_closing_the_author_vault() {
    let mut f = Fixture::new();
    f.init();
    f.set_author_balance(X_USER_ID, 987_654);
    let claimant = f.stranger.pubkey();
    let destination = get_associated_token_address(&claimant, &USDC_MINT);
    let (_, author_ata, claim_address) = Fixture::author_pdas(X_USER_ID);
    assert!(f
        .svm
        .get_account(&destination)
        .is_none_or(|a| a.lamports == 0));
    let operator_before = f.svm.get_balance(&f.operator.pubkey()).unwrap();
    let claimant_before = f.svm.get_balance(&claimant).unwrap();
    let source = f.svm.get_account(&author_ata).unwrap();
    let claimed = f.claim(X_USER_ID, claimant).unwrap();
    assert!(claimed.compute_units_consumed < 200_000);
    println!(
        "Author claim with ATA and binding creation consumed {} compute units",
        claimed.compute_units_consumed
    );
    assert_eq!(f.tokens(destination), 987_654);
    assert_eq!(f.tokens(author_ata), 0);
    assert_eq!(f.svm.get_balance(&claimant).unwrap(), claimant_before);
    let new_rent =
        f.svm.get_balance(&destination).unwrap() + f.svm.get_balance(&claim_address).unwrap();
    assert_eq!(
        f.svm.get_balance(&f.operator.pubkey()).unwrap(),
        operator_before - new_rent - 5_000
    );
    let after = f.svm.get_account(&author_ata).unwrap();
    assert_eq!(after.owner, source.owner);
    assert_eq!(after.lamports, source.lamports);
    assert_eq!(after.data.len(), source.data.len());
    assert_eq!(f.author_claim(X_USER_ID).claimant, claimant);
}

#[test]
fn claim_uses_current_operator_and_is_available_while_paused_but_not_after_revoke() {
    let mut f = Fixture::new();
    f.init();
    f.set_author_balance(X_USER_ID, 100);
    f.send(f.emergency(f.guardian.pubkey(), false), &[Role::Guardian])
        .unwrap();
    f.claim(X_USER_ID, f.buyer.pubkey()).unwrap();
    f.set_author_balance(X_USER_ID, 200);
    f.send(f.emergency(f.guardian.pubkey(), true), &[Role::Guardian])
        .unwrap();
    rejects(
        f.claim(X_USER_ID, f.buyer.pubkey()),
        code(enki_escrow::EscrowError::Unauthorized),
    );
    assert_eq!(f.tokens(Fixture::author_pdas(X_USER_ID).1), 200);
    let mut args = f.args();
    args.operator = f.stranger.pubkey();
    args.paused = true;
    f.send(f.update_ix(f.admin.pubkey(), args), &[Role::Admin])
        .unwrap();
    rejects(
        f.claim(X_USER_ID, f.buyer.pubkey()),
        code(enki_escrow::EscrowError::Unauthorized),
    );
    let mut ix = f.claim_ix(X_USER_ID, f.buyer.pubkey());
    ix.accounts[1].pubkey = f.stranger.pubkey();
    f.send_as_stranger(ix).unwrap();
    assert_eq!(f.tokens(Fixture::author_pdas(X_USER_ID).1), 0);
    assert_eq!(f.tokens(f.buyer_ata), BUYER_TOKENS + 300);
}

#[test]
fn claim_rejects_wrong_mint_and_unusable_source_or_destination_without_losing_funds() {
    for source in [true, false] {
        for change in UNUSABLE_ATAS {
            let mut f = Fixture::new();
            f.init();
            f.set_author_balance(X_USER_ID, 123_456);
            let (_, author_ata, claim_address) = Fixture::author_pdas(X_USER_ID);
            let changed = if source { author_ata } else { f.buyer_ata };
            let original = f.svm.get_account(&changed).unwrap();
            f.change_ata(changed, change);
            let before = f.svm.get_account(&changed).unwrap();
            assert!(
                f.claim(X_USER_ID, f.buyer.pubkey()).is_err(),
                "{source}, {change:?}"
            );
            assert_eq!(f.svm.get_account(&changed).unwrap(), before);
            assert!(f
                .svm
                .get_account(&claim_address)
                .is_none_or(|a| a.lamports == 0));
            f.svm.set_account(changed, original).unwrap();
            f.claim(X_USER_ID, f.buyer.pubkey()).unwrap();
            assert_eq!(f.tokens(author_ata), 0);
            assert_eq!(f.tokens(f.buyer_ata), BUYER_TOKENS + 123_456);
        }
    }
    let mut f = Fixture::new();
    f.init();
    f.set_author_balance(X_USER_ID, 123_456);
    let fake_mint = Pubkey::new_unique();
    f.svm
        .set_account(fake_mint, f.svm.get_account(&USDC_MINT).unwrap())
        .unwrap();
    let mut ix = f.claim_ix(X_USER_ID, f.buyer.pubkey());
    ix.accounts[2].pubkey = fake_mint;
    rejects(
        f.send(ix, &[Role::Operator]),
        code(enki_escrow::EscrowError::WrongMint),
    );
    assert_eq!(f.tokens(Fixture::author_pdas(X_USER_ID).1), 123_456);
    f.claim(X_USER_ID, f.buyer.pubkey()).unwrap();
}

#[test]
fn author_claim_record_rejects_corrupt_version_bump_or_numeric_identity() {
    for field in 0..3 {
        let mut f = Fixture::new();
        f.init();
        let claimant = f.buyer.pubkey();
        f.claim(X_USER_ID, claimant).unwrap();
        f.set_author_balance(X_USER_ID, 123_456);
        let (_, author_ata, claim_address) = Fixture::author_pdas(X_USER_ID);
        let original = f.svm.get_account(&claim_address).unwrap();
        let mut claim = f.author_claim(X_USER_ID);
        match field {
            0 => claim.version = 2,
            1 => claim.bump = claim.bump.wrapping_add(1),
            _ => claim.x_user_id = X_USER_ID + 1,
        }
        let mut corrupt = original.clone();
        let mut data = Vec::new();
        claim.try_serialize(&mut data).unwrap();
        corrupt.data = data;
        f.svm.set_account(claim_address, corrupt.clone()).unwrap();
        rejects(
            f.claim(X_USER_ID, claimant),
            code(enki_escrow::EscrowError::WrongVersion),
        );
        assert_eq!(f.svm.get_account(&claim_address).unwrap(), corrupt);
        assert_eq!(f.tokens(author_ata), 123_456);
        assert_eq!(f.tokens(f.buyer_ata), BUYER_TOKENS);
        f.svm.set_account(claim_address, original).unwrap();
        f.claim(X_USER_ID, claimant).unwrap();
        assert_eq!(f.tokens(author_ata), 0);
    }
}

#[test]
fn unusable_treasury_cannot_partially_pay_or_settle_a_four_recipient_order() {
    for change in std::iter::once(None).chain(UNUSABLE_ATAS.into_iter().map(Some)) {
        let mut f = Fixture::new();
        f.init();
        let id = [51; 16];
        f.deposit_four(id, 1, [700_000, 100_000, 50_000, 150_000]);
        let original_treasury = f.svm.get_account(&f.treasury_ata).unwrap();
        let original_escrow = f.svm.get_account(&f.pdas(id).0).unwrap();
        if let Some(change) = change {
            f.change_ata(f.treasury_ata, change);
        } else {
            f.remove_ata(f.treasury_ata);
        }
        assert!(f.settle(id, 1).is_err(), "{change:?}");
        assert_eq!(f.svm.get_account(&f.pdas(id).0).unwrap(), original_escrow);
        assert_eq!(f.tokens(f.pdas(id).1), 1_000_000);
        assert_eq!(f.tokens(f.artist_ata), 0);
        assert_eq!(f.tokens(f.referrer_ata), 0);
        assert_eq!(f.tokens(Fixture::author_pdas(X_USER_ID).1), 0);
        f.svm
            .set_account(f.treasury_ata, original_treasury)
            .unwrap();
        f.settle(id, 1).unwrap();
        assert_eq!(
            f.escrow(id).paid_amounts,
            [700_000, 100_000, 50_000, 150_000]
        );
    }
}

#[test]
fn claim_of_an_empty_author_vault_still_binds_the_first_wallet() {
    let mut f = Fixture::new();
    f.init();
    let claimant = f.buyer.pubkey();
    let (_, author_ata, claim_address) = Fixture::author_pdas(X_USER_ID);
    f.claim(X_USER_ID, claimant).unwrap();
    assert_eq!(f.tokens(author_ata), 0);
    assert_eq!(f.author_claim(X_USER_ID).claimant, claimant);
    let binding = f.svm.get_account(&claim_address).unwrap();
    rejects(
        f.claim(X_USER_ID, f.stranger.pubkey()),
        code(enki_escrow::EscrowError::AlreadyClaimed),
    );
    f.claim(X_USER_ID, claimant).unwrap();
    assert_eq!(f.svm.get_account(&claim_address).unwrap(), binding);
    assert_eq!(f.tokens(f.buyer_ata), BUYER_TOKENS);
}
