use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::{
    bpf_loader_upgradeable::{self, UpgradeableLoaderState},
    clock::Clock,
    instruction::Instruction,
    program_option::COption,
    program_pack::Pack,
};
use anchor_lang::{AccountDeserialize, InstructionData, ToAccountMetas};
use anchor_spl::{
    associated_token::get_associated_token_address,
    token::{self, spl_token},
};
use enki_escrow::{accounts, instruction, Config, ConfigArgs, Escrow, ID, USDC_MINT};
use litesvm::{types::TransactionResult, LiteSVM};
use solana_account::Account;
use solana_keypair::Keypair;
use solana_message::{Message, VersionedMessage};
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;

const NOW: i64 = 1_750_000_000;
const BUYER_TOKENS: u64 = 5_000_000_000_000;

struct Fixture {
    svm: LiteSVM,
    admin: Keypair,
    guardian: Keypair,
    operator: Keypair,
    payer: Keypair,
    buyer: Keypair,
    stranger: Keypair,
    artist: Pubkey,
    treasury: Pubkey,
    config: Pubkey,
    buyer_ata: Pubkey,
    treasury_ata: Pubkey,
    artist_ata: Pubkey,
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
        let payer = Keypair::new();
        let buyer = Keypair::new();
        let stranger = Keypair::new();
        for key in [&admin, &guardian, &operator, &payer, &buyer, &stranger] {
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
        let buyer_ata = get_associated_token_address(&buyer.pubkey(), &USDC_MINT);
        let treasury_ata = get_associated_token_address(&treasury, &USDC_MINT);
        let artist_ata = get_associated_token_address(&artist, &USDC_MINT);
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
            config: Pubkey::find_program_address(&[b"config"], &ID).0,
            buyer_ata,
            treasury_ata,
            artist_ata,
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

    fn deposit_ix(
        &self,
        intent_id: [u8; 16],
        units: u8,
        amounts: [u64; 2],
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
                recipient_owners: [
                    self.treasury,
                    if amounts[1] == 0 {
                        Pubkey::default()
                    } else {
                        self.artist
                    },
                ],
                unit_amounts: amounts,
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

    fn settle_ix(&self, id: [u8; 16], k: u8, operator: Pubkey) -> Instruction {
        let (escrow, vault) = self.pdas(id);
        let artist_owner = self.escrow(id).recipient_owners[1];
        Instruction {
            program_id: ID,
            accounts: accounts::Settle {
                config: self.config,
                operator,
                escrow,
                vault,
                mint: USDC_MINT,
                treasury_ata: self.treasury_ata,
                artist_ata: get_associated_token_address(&artist_owner, &USDC_MINT),
                token_program: token::ID,
            }
            .to_account_metas(None),
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
fn partial_delivery_pays_exact_units_refunds_rest_and_returns_rent() {
    let mut f = Fixture::new();
    f.init();
    let id = [1; 16];
    let before = f.tokens(f.buyer_ata);
    f.deposit(id, 4, [900_000, 100_000]);
    let (escrow, vault) = f.pdas(id);
    assert_eq!(f.svm.get_account(&escrow).unwrap().data.len(), 223);
    assert_eq!(f.tokens(vault), 4_000_000);
    assert_eq!(f.tokens(f.buyer_ata), before - 4_000_000);
    let immutable = f.svm.get_account(&escrow).unwrap().data;
    let settled = f.settle(id, 2).unwrap();
    assert!(settled.compute_units_consumed < 200_000);
    assert_eq!(f.tokens(f.treasury_ata), 1_800_000);
    assert_eq!(f.tokens(f.artist_ata), 200_000);
    assert_eq!(f.tokens(vault), 2_000_000);
    let after = f.svm.get_account(&escrow).unwrap().data;
    for index in 0..immutable.len() {
        if index != 9 && index != 206 {
            assert_eq!(
                immutable[index], after[index],
                "deposit field changed at {index}"
            );
        }
    }
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
        recipient_owners: [f.stranger.pubkey(), Pubkey::default()],
        unit_amounts: [1, 0],
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
    let mut ix = f.deposit_ix([3; 16], 1, [1, 0], NOW + 900);
    ix.accounts[1].is_signer = false;
    assert!(f.send(ix, &[]).is_err());
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
    ix.accounts[5].pubkey = f.artist_ata;
    rejects(
        f.send(ix, &[Role::Operator]),
        code(enki_escrow::EscrowError::NonCanonicalAta),
    );
    let mut ix = f.settle_ix(id, 2, f.operator.pubkey());
    ix.accounts[6].pubkey = f.treasury_ata;
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
        let k = (random.rotate_left(41) % (u64::from(units) + 1)) as u8;
        f.settle(id, k).unwrap();
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
