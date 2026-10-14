#!/usr/bin/env python3
"""Prove payment regression tests reject faulty SBF programs, without RPC."""

import argparse
import os
from pathlib import Path
import re
import subprocess
import sys


ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "programs/enki-escrow/src/lib.rs"
BINARY = ROOT / "target/deploy/enki_escrow.so"
TESTS = ROOT / "programs/enki-escrow/tests/escrow.rs"
PARTIAL = "partial_delivery_pays_exact_units_refunds_rest_and_returns_rent"
SPARSE = "sparse_author_slot_settles_and_repeat_settlement_is_rejected"
MUTATIONS = (
    (
        "unusable-artist-blocks-treasury",
        b".unwrap_or(TokenStatus::Frozen)",
        b"?",
        "unusable_artist_ata_cannot_block_treasury_settlement",
    ),
    (
        "unbound-deposit-server",
        b"mut, address = config.operator @ EscrowError::Unauthorized",
        b"mut",
        "deposit_requires_configured_operator_even_for_buyer_chosen_terms",
    ),
    (
        "check-buyer-ata-with-zero-refund",
        b"let missing = if escrow.refund_due > 0 {",
        b"let missing = if true {",
        "zero_refund_closes_with_unusable_buyer_ata_and_returns_stored_rent",
    ),
    (
        "donated-dust-gates-buyer-validation",
        b"let missing = if escrow.refund_due > 0 {",
        b"let missing = if ctx.accounts.vault.amount > 0 {",
        "donated_dust_cannot_lock_zero_refund_or_stored_rent",
    ),
    (
        "unsigned-settlement-operator",
        b"pub operator: Signer<'info>,",
        b"/// CHECK: deliberately remove the settlement signature requirement.\n    pub operator: UncheckedAccount<'info>,",
        "settle_requires_operator_signature_even_when_stranger_pays_transaction_fee",
    ),
    (
        "unbound-initialization-program-data",
        b"#[account(constraint = program.programdata_address()? == Some(program_data.key()) @ EscrowError::WrongProgramData)]",
        b"",
        "init_rejects_another_programs_program_data_even_when_its_authority_signs",
    ),
    (
        "restore-second-deposit-cap",
        b"total <= cap, EscrowError::DepositCap",
        b"total <= cap && total <= 25_000_000, EscrowError::DepositCap",
        "config_alone_controls_new_deposits_and_lowering_it_does_not_block_refunds",
    ),
    (
        "repeat-settlement",
        b"escrow.state == EscrowState::Funded",
        b"true",
        PARTIAL,
    ),
    (
        "charge-undelivered-units",
        b"checked_product(k, escrow.unit_amounts[0])?",
        b"checked_product(escrow.units, escrow.unit_amounts[0])?",
        PARTIAL,
    ),
    (
        "refund-without-deducting-ata-fee",
        b"(remaining - fee, fee, true)",
        b"(remaining, fee, true)",
        "missing_buyer_ata_fee_rules_are_exact_and_rent_destination_cannot_change",
    ),
    (
        "unsigned-claim-operator",
        b"pub operator: Signer<'info>,",
        b"/// CHECK: deliberately remove the claim signature requirement.\n    pub operator: UncheckedAccount<'info>,",
        "claim_requires_operator_signature_even_when_stranger_pays_transaction_fee",
    ),
    (
        "claim-allows-forbidden-beneficiary",
        b"claimant != ctx.accounts.config.treasury_owner\n"
        b"                && claimant != ctx.accounts.config.operator\n"
        b"                && claimant != Pubkey::default()\n"
        b"                && claimant != ctx.accounts.author.key()",
        b"true",
        "claim_rejects_treasury_operator_and_default_claimants",
    ),
    (
        "claim-rebinds-first-wallet",
        b"require_keys_eq!(claim.claimant, claimant, EscrowError::AlreadyClaimed);",
        b"let _ = claimant;",
        "claim_binds_first_wallet_and_same_wallet_resweeps_fresh_funds",
    ),
    (
        "claim-allows-noncanonical-author-ata",
        b"#[account(mut, associated_token::mint = mint, associated_token::authority = author, associated_token::token_program = token_program)]",
        b"#[account(mut, token::mint = mint, token::authority = author, token::token_program = token_program)]",
        "claim_rejects_noncanonical_author_pda_and_token_accounts",
    ),
    (
        "settlement-loses-sparse-author-leg",
        b"paid_amounts[i] = checked_product(k, escrow.unit_amounts[i])?;",
        b"if i != 3 { paid_amounts[i] = checked_product(k, escrow.unit_amounts[i])?; }",
        SPARSE,
    ),
    (
        "repeat-sparse-settlement",
        b"escrow.state == EscrowState::Funded",
        b"true",
        SPARSE,
    ),
)
SCOPES = {
    "unsigned-settlement-operator": b"pub struct Settle<'info> {",
    "unsigned-claim-operator": b"pub struct ClaimAuthorVault<'info> {",
}
ANSI = re.compile(r"\x1b\[[0-9;]*m")


def mutated_source(original, name, needle, replacement):
    """Require one exact target, scoped when account structs share a field."""
    marker = SCOPES.get(name)
    start, end = 0, len(original)
    if marker is not None:
        if original.count(marker) != 1:
            raise RuntimeError(f"{name}: expected exactly one mutation scope")
        start = original.index(marker)
        closing = original.find(b"\n}", start)
        if closing == -1:
            raise RuntimeError(f"{name}: mutation scope has no closing brace")
        end = closing + len(b"\n}")
    selected = original[start:end]
    if selected.count(needle) != 1:
        raise RuntimeError(f"{name}: expected exactly one mutation target")
    if needle == replacement:
        raise RuntimeError(f"{name}: mutation must change the source")
    return original[:start] + selected.replace(needle, replacement, 1) + original[end:]


def check_inventory(original, test_source):
    names = [name for name, _, _, _ in MUTATIONS]
    if len(names) != len(set(names)):
        raise RuntimeError("Mutation names must be unique")
    for name, needle, replacement, test in MUTATIONS:
        mutated_source(original, name, needle, replacement)
        selector = re.compile(rf"(?m)^fn {re.escape(test)}\(")
        if len(selector.findall(test_source)) != 1:
            raise RuntimeError(f"{name}: expected exactly one VM test named {test}")


def run(command, env, log, timeout):
    result = subprocess.run(
        command,
        cwd=ROOT,
        env=env,
        capture_output=True,
        text=True,
        timeout=timeout,
        check=False,
    )
    output = ANSI.sub("", result.stdout + result.stderr)
    log.write_text(output, encoding="utf-8")
    return result.returncode, output


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check", action="store_true",
        help="Check mutation targets and VM test selectors without building or running Rust.",
    )
    args = parser.parse_args()
    if not args.check and sys.platform != "linux":
        raise RuntimeError("Run mutation checks on the Linux CI runner.")
    original_source = SOURCE.read_bytes()
    check_inventory(original_source, TESTS.read_text(encoding="utf-8"))
    if args.check:
        print(f"Mutation inventory: {len(MUTATIONS)} exact targets and VM test selectors; no Rust executed.")
        return
    original_binary = BINARY.read_bytes()
    host_env = os.environ.copy()
    build_env = host_env.copy()
    rust_bin = ROOT / ".ci-tools/platform-tools/rust/bin"
    solana_bin = ROOT / ".ci-tools/solana-release/bin"
    build_env["PATH"] = f"{rust_bin}:{solana_bin}:{host_env['PATH']}"
    build_env["RUSTC"] = str(rust_bin / "rustc")
    logs = ROOT / "target/mutation-logs"
    logs.mkdir(parents=True, exist_ok=True)
    try:
        for name, needle, replacement, test in MUTATIONS:
            SOURCE.write_bytes(mutated_source(original_source, name, needle, replacement))
            status, output = run(
                [
                    "cargo", "build-sbf", "--skip-tools-install",
                    "--no-rustup-override", "--jobs", "2",
                    "--sbf-out-dir", "target/deploy", "--", "--locked",
                ],
                build_env, logs / f"{name}-build.log", 600,
            )
            if status != 0:
                raise RuntimeError(f"{name}: mutant must compile\n{output[-4000:]}")
            status, output = run(
                [
                    "cargo", "test", "--package", "enki-escrow", "--locked",
                    "--test", "escrow", test, "--", "--exact",
                    "--test-threads=1",
                ],
                host_env, logs / f"{name}-test.log", 120,
            )
            if (
                status != 101
                or f"test {test} ... FAILED" not in output
                or "test result: FAILED. 0 passed; 1 failed;" not in output
            ):
                raise RuntimeError(f"{name}: expected a failed regression test\n{output[-4000:]}")
            for line in output.splitlines():
                if line == f"test {test} ... FAILED" or line.startswith("test result: FAILED."):
                    print(line, flush=True)
            print(f"PASS {name}: compiled mutant rejected by {test}", flush=True)
    finally:
        SOURCE.write_bytes(original_source)
        BINARY.write_bytes(original_binary)
    if SOURCE.read_bytes() != original_source or BINARY.read_bytes() != original_binary:
        raise RuntimeError("Original source and SBF binary were not restored")
    print(f"Mutation checks: {len(MUTATIONS)}/{len(MUTATIONS)}; original source and SBF binary restored.")


if __name__ == "__main__":
    main()
