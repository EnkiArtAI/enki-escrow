# Enki Escrow

Solana escrow for Enki paid image-generation intents. A buyer funds a fixed batch; settlement pays only for delivered images and refunds the unused balance.

This repository contains the source for the deployed Devnet program
`2sfm76FMcfqUYCv47J4VCNFMrp6PpfD6QYpmWqBCPyJV`. The Escrow v2 work on
`codex/escrow-v2-author-shares-2026-10-14` is a proposed upgrade of that same
program, not a v2 deployment or a release approval. The upgrade preserves the
existing author-vault addresses and their USDC balances. Mainnet is outside
this rollout.

The current specification is
[docs/ESCROW-V2-SHARES.md on EnkiArtAI/enki main](https://github.com/EnkiArtAI/enki/blob/main/docs/ESCROW-V2-SHARES.md).
Frontend and server integration live in [EnkiArtAI/enki](https://github.com/EnkiArtAI/enki).
[ClickUp 123qmzqzw1w](https://app.clickup.com/t/9015834867/123qmzqzw1w) remains the
original escrow reference.

## Payment contract

- Classic SPL USDC only, with a cluster-specific mint and six decimals.
- A deposit holds 1–24 units. The sole batch cap is `Config.max_deposit_micro`
  (the 2026-10-06 specification calls for 50 USDC when Config is initialized).
  It can be changed by the admin without redeploying code. Clients and SQL must
  read this on-chain field instead of defining another policy limit.
- Four fixed positions hold the treasury, artist, referrer and X author, in
  that order. Owners and per-unit amounts are fixed by the deposit. Optional
  positions may be empty, including gaps before the author position;
  `recipient_count` is the highest populated index plus one, not the number of
  populated positions. An empty position has the default owner and zero amount.
- The signing rent payer must be the current `Config.operator`, so the server
  authorizes the deposit terms together with the buyer. Admin/upgrade governance
  and guardian roles are separate from this online quote/settlement signer.
- The authorized operator settles once for `k` delivered units, with `0 <= k <= N`.
- Settlement receives one remaining account for each position below
  `recipient_count`, including the treasury. Every populated position supplies
  its canonical USDC ATA; each empty position supplies the System Program as
  its placeholder. An unusable canonical non-treasury ATA forfeits only its own
  leg to the buyer refund while settlement still pays the treasury and other
  usable recipients. Noncanonical addresses, reordered positions and omitted
  placeholders are rejected. The treasury leg remains mandatory.
- Any caller can reclaim after settlement or expiry; refunds and account rent go to the stored destinations.
- With no recorded buyer refund due, reclaim closes without inspecting buyer ATA contents,
  even when someone has donated tokens to the vault. With a positive refund due,
  an unusable buyer ATA preserves the escrow until restored;
  a missing ATA retains the existing recreation/fee rules. Operator rotation
  does not change the stored rent payer of earlier deposits.
- Escrow records `refund_due`: the deposit total, less the recipient
  amounts actually paid at settlement. Reclaim applies the existing refund and
  missing-ATA fee rules to `min(vault.amount, refund_due)` and sweeps any surplus
  to the stored treasury. Donations never increase the buyer's claim, purchased
  units, or missing-ATA fee branch. Escrow version 2 is 343 bytes including its
  discriminator: expanding the two owner/amount slots to four adds 80 bytes to
  the 231-byte v1 layout, and `paid_amounts[4]` adds 32 bytes. The actual paid
  vector is needed to verify each settled leg when an optional ATA was unusable;
  the proposed 311-byte layout cannot carry that proof. Clients require the
  compiled v2 interface. Existing v1 accounts must be drained before upgrade;
  there is no automatic account migration.
- A guardian can pause new deposits or revoke the operator. Restoring authority requires the admin.

## Author vault claims

An author's vault authority is the PDA with seeds `["author", x_user_id]`, where
the X user ID is a `u64` encoded in little-endian order. Its canonical USDC ATA
can already receive direct transfers, including from the separate x402 rail.
`claim_author_vault(x_user_id, claimant)` requires the current operator's
signature as the attestation that the claimant owns that X account. It transfers
the vault ATA's whole balance to the claimant's canonical USDC ATA; there is no
operator-selected withdrawal amount, balance ledger or vault closure.

The first claim additionally requires the claimant wallet's signature before
the permanent binding is written, including when the vault is empty. A missing
signature returns `ClaimantMustSign`. Subsequent sweeps to the already-bound
wallet require only the current operator's signature. The wallet account stays
unchecked in the generated interface because its signature requirement is
conditional: the client must mark `claimant_wallet` as a readonly signer and
collect that wallet's signature for a first claim. Account order and sizes are
unchanged.

The first successful claim creates a 58-byte `AuthorClaim` account at seeds
`["claim", x_user_id]`: discriminator, version, bump, X user ID, claimant wallet
and claimed slot. That wallet binding is immutable, including after operator
rotation. The same wallet may sweep funds that arrive later; another wallet is
refused with `AlreadyClaimed`, even if the current operator signs. Claimant
wallet rotation is deferred and there is no re-attestation override. The
treasury, operator, default public key and author vault itself are forbidden
claimants. The application must verify X-account ownership and the exact wallet
binding before requesting the operator attestation, then confirm the chain receipt and
read the on-chain binding before marking a claim complete in its database.

The operator attests identity; the program cannot prove X-account ownership.
It checks the operator signature, canonical PDA/ATA addresses and first-wallet
binding, and rejects the current treasury/operator as claimants. The operator's
off-chain identity check and the program's upgrade authority remain trusted.

Tests use an in-process Solana VM and disposable wallets. They require no Enki secrets, live RPC connection or paid generation API.

## Build and test

The program branch pins Anchor 0.32.1, host Rust 1.90.0, Agave 2.3.0 and SBF platform-tools v1.57. CI uses GitHub's standard `ubuntu-latest` x64 runner and verifies the tool archives' SHA-256 before extraction.

With that toolchain configured:

```sh
cargo fmt --all -- --check
cargo build-sbf --tools-version v1.57 -- --locked
cargo test --workspace --locked -- --test-threads=1
```

The VM tests require `target/deploy/enki_escrow.so` and fail when it is absent.
The default build accepts Devnet USDC. The program ID is the deployed Devnet
address; a passing local test or build does not establish which binary is live.
The separate mainnet feature requires its own review and is not used by this
rollout.

The host VM tests use optimization level 1 with overflow checks and debug assertions enabled. The 100,000-sequence test reports progress every 10,000 sequences in CI.

After the original program passes, Linux CI compiles deliberately faulty
programs and requires the selected VM regression tests to fail. It retains the
ten v1 mutations for recipient failures, signer binding, refund/donation rules,
initialization, deposit caps, repeat settlement and delivered-unit accounting.
The v2 mutations cover the claim operator signature, forbidden claimants,
first-claim wallet signature, immutable claimant binding, canonical author-vault accounts and the sparse
author leg. `scripts/check-mutations.py` restores the original source and SBF
binary in a `finally` block. A compilation failure does not count as mutation
proof. The following command checks mutation targets and exact test selectors
without compiling or executing Rust:

```sh
python3 scripts/check-mutations.py --check
```

CI also exports the Anchor IDL compiled from the Rust source as `enki-escrow-idl`, with its SHA-256 and source commit. The server client must be generated from this artifact. The pinned Anchor 0.32.1 CLI is checksum-verified and `anchor idl build` runs without a wallet or RPC connection.

After mutation checks restore the original SBF binary and the IDL build
succeeds, CI separately exports `enki-escrow-devnet-program`, containing
`enki_escrow.so`, `program.sha256` and `program-source-commit.txt`. Verify the
checksum and approved source commit before using that exact checked binary for
the owner-approved Devnet upgrade. This artifact avoids requiring a local
Rust/SBF build; CI does not deploy it.

The v2 SBF build, VM suite, mutation results and four-leg compute measurement
are pending CI. Local Rust/SBF execution remains paused while the Norton issue
is unresolved. Live Devnet acceptance also remains pending an owner-approved
RPC and generation-call budget; CI uses no live RPC or paid generation calls.
See [the v2 rollout checklist](docs/ESCROW-V2-ROLLOUT.md) for the coordinated
program, client and application cutover.

The pending `update_config` governance fix belongs to a separate mainnet release
and is not changed by this Devnet v2 work.

See [CONTRIBUTING.md](CONTRIBUTING.md) for review and release rules. Licensed under [Apache-2.0](LICENSE).
