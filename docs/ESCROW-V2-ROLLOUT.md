# Escrow v2 rollout

This checklist covers the Devnet upgrade of
`2sfm76FMcfqUYCv47J4VCNFMrp6PpfD6QYpmWqBCPyJV` for treasury, artist, referrer
and X-author shares. The existing v1 deployment is not evidence that this v2
source or its client is deployed. Backend-owner review and Kev's release
approval are required by [CONTRIBUTING.md](../CONTRIBUTING.md).

The canonical specification is
[docs/ESCROW-V2-SHARES.md on EnkiArtAI/enki main](https://github.com/EnkiArtAI/enki/blob/main/docs/ESCROW-V2-SHARES.md),
and the application integration belongs to [EnkiArtAI/enki](https://github.com/EnkiArtAI/enki).

## Interface and account contracts

| Account or interface | v2 contract |
| --- | --- |
| Escrow | Version 2; 343 bytes including discriminator; four owners, four unit amounts and four actual paid amounts. |
| Recipient positions | 0 treasury, 1 artist, 2 referrer, 3 author. Empty gaps remain in place. `recipient_count` is the highest populated index plus one. |
| Settle accounts | One remaining account per position below `recipient_count`. Populated positions use canonical USDC ATAs; empty positions use the System Program placeholder. |
| Optional settlement failure | An unusable canonical non-treasury ATA leaves only that leg in the buyer refund. Treasury payment and usable optional legs continue. Wrong addresses or positions fail. |
| Author vault | Authority PDA `["author", x_user_id.to_le_bytes()]` under the existing program ID; canonical classic SPL USDC ATA. Existing ATA balances remain reachable after upgrade. |
| AuthorClaim | Version 1; 58 bytes including discriminator; version, bump, `x_user_id: u64`, claimant public key and claimed slot. PDA `["claim", x_user_id.to_le_bytes()]`. |
| Author claim | `claim_author_vault(x_user_id, claimant)`, signed by the current operator and, on the first claim, the claimant wallet; transfer the whole vault balance to the claimant's canonical USDC ATA, without closing the vault or keeping a balance ledger. |
| Claimant binding | First successful claim fixes the wallet permanently, including after operator rotation. That wallet may sweep later funds. A different wallet fails with `AlreadyClaimed`; treasury, operator, default public key and author vault itself are forbidden. |

Four owner/amount positions alone would grow the 231-byte v1 Escrow to the
proposed 311 bytes. The additional `paid_amounts[4]` occupies 32 bytes, bringing
v2 to 343. It records what each leg actually received, so confirmation can
distinguish an intentionally forfeited optional leg from a paid leg. Checking
only the aggregate payment or recomputing every nominal amount cannot provide
that per-leg proof. Frontend decoders, account-size checks and confirmation must
all use the compiled 343-byte interface.

The chosen contract resolves the specification's optional first-claim record
in favor of an on-chain immutable binding. An operator change or another
off-chain ownership attestation cannot rebind it. Claimant wallet rotation is
explicitly deferred; this release has no re-attestation override or account
close/recreate path. Sparse slots use the highest populated index plus one so
an author in position 3 survives empty artist/referrer positions.

The operator is the identity-attestation boundary: it verifies X-account control
off chain and signs the exact X user ID and claimant wallet. The program checks
that signature, canonical PDA/ATA addresses and the first-wallet binding, and
rejects the current treasury/operator as claimants. It cannot itself prove
X-account ownership. Both the operator's identity check and the program's
upgrade authority remain trusted.

## Build and review evidence

1. Keep the change in a draft program PR until the SBF build, unit tests, VM
   integration tests and all required compiled mutations pass on the exact PR
   source commit. Run `python3 scripts/check-mutations.py --check` first to check
   every source target and exact VM test selector without Rust execution.
2. Review four-leg arithmetic and sparse positions, zero-rounded shares,
   canonical token addresses, partial delivery, donated tokens, recipient
   failures, replay prevention, refund/rent destinations and pause/revocation.
   Claim tests must cover operator signature, first-wallet binding, repeat
   sweeps, forbidden claimants, and wrong author/claim PDA or ATA inputs.
3. Measure the complete four-leg settlement and claim path in the VM. Confirm
   that the application's compute-unit allocation covers the actual path,
   including ATA creation where supported. Do not infer the four-leg budget
   from two-leg tests. Record consumed units and the selected allocation with
   the release evidence.
4. Obtain `enki-escrow-idl` from a successful CI run for the exact approved source
   commit. Verify `program-source-commit.txt` and `idl.sha256` against the
   artifact. Regenerate the application client from that compiled IDL and run
   its byte-for-byte generation check. Do not hand-edit the IDL, generated
   account layout or instruction builders.
5. Download the separate `enki-escrow-devnet-program` artifact from the same
   successful run. It contains the restored original `enki_escrow.so`,
   `program.sha256` and `program-source-commit.txt`, written after the mutation
   checks and IDL build succeed. Verify the binary checksum and exact approved
   source commit. Use that checked binary for the approved Devnet upgrade;
   a local build is not required. The existing `enki-escrow-idl` artifact remains
   the client-generation input.

Local Rust/SBF builds and VM execution are paused while Norton remains
unresolved. No v2 SBF, complete VM-suite, mutation or live acceptance result is
claimed by this document. CI uses disposable VM wallets and no live RPC,
generation-provider calls, database writes or deployment command.

## Drain v1 before the program upgrade

1. Freeze new v1 escrow admission and finish or cancel admitted generation
   work. Preserve the v1 application/program client for its existing accounts.
2. Settle delivered v1 units, reclaim unused funds and rent, and reconcile every
   stored deposit, submitted transaction and lifecycle attempt against its
   confirmed chain result. Resolve old nonce-rail cases using the existing
   cutover process too. Do not treat database status or absent RPC responses as
   closure proof.
3. Inventory v1 accounts and require a verified empty in-flight set before
   upgrading. V2 does not reallocate, reinterpret or automatically upgrade a
   231-byte v1 Escrow. Never send its decoder or instructions to leftover v1
   accounts. If draining cannot be proved, keep admission stopped and resolve
   the accounts with the v1 release before proceeding.
4. Record existing author-vault PDA/ATA addresses and their balances at the
   approved checkpoint. Upgrade the same program ID with the approved binary
   and governance. Do not redeploy to a new ID, reseed author PDAs, close the
   author ATAs or move their accumulated funds as a migration shortcut.
5. Verify the deployed program identity/binary and Config, including the current
   operator and treasury. Publish the approved compiled IDL/client pin together
   with deployment evidence, then activate the matching v2 server, database
   contracts and browser checkout in a coordinated release.

No upgrade, key-governance change or live transaction is authorized by this
checklist itself. Mainnet rollout is a separate decision.
The pending `update_config` governance fix is separate mainnet release work and
is not changed by this Devnet v2 rollout.

The zero-v1-account gate is mandatory for both Devnet and Mainnet. There is no
`reclaim_v1` instruction in this release. Require `Config.paused = true` on chain,
stop v1 admission and finish in-flight work, then recheck all
program-owned Escrow accounts immediately before the upgrade, and abort if any
v1 account remains, including settled, expired or orphaned accounts. A previous
zero result is not sufficient if new v1 deposits can still be admitted. Use a
finalized inventory by the Escrow discriminator as well as the 231-byte v1
layout check so unexpected account sizes are not silently excluded. Mainnet
requires this gate's recorded evidence and the separate release approvals.

## Application claim and payment integration

- Read the four server-calculated shares into the deposit plan, preserving slot
  positions and empty gaps. Confirmation and lifecycle reconciliation must
  verify all four actual paid legs and the corresponding buyer refund.
- Kev's off-chain author-claim flow must verify control of the exact X account
  and its claimant-wallet binding before the operator signs the exact
  `x_user_id` and wallet. Never take treasury/operator/default destinations from
  a caller.
- Treat claim submission as pending until its successful confirmed chain
  receipt and matching on-chain `AuthorClaim` binding are read back. A signature,
  transport acknowledgement or database write is not claim completion. Bind
  every retry to the same X user ID and wallet, and recover ambiguous responses
  from chain evidence before offering another action.
- For a first claim, mark fixed account `claimant_wallet` (index 5) as a readonly
  signer and obtain both its signature and the current operator's signature
  over the same transaction. This also applies to an empty vault. The generated
  IDL cannot encode this conditional requirement; an unmodified account-meta
  builder is insufficient for first claims. The handler rejects a missing
  signature with `ClaimantMustSign` before writing the permanent binding.
  Later sweeps to the same bound wallet may omit its signature. Re-pin the new
  compiled IDL and program hash even though argument shapes and account sizes
  remain unchanged.
- The author vault ATA must exist before a claim. If it has never received a
  share, create its canonical ATA with the application's fee payer before
  submitting the first claim.
- Use the immutable on-chain claimant when routing later author shares. A
  same-wallet sweep transfers newly arrived vault funds; it does not replace
  the binding or create a debt balance. Keep the referrer payment in the buyer's
  transaction, including the case where the referrer has no Solana wallet.
- Preserve the independent x402 direct-transfer rail and existing author ATAs.
  Do not create a custody balance ledger or silently replace this rollout with
  another payment protocol.

The application now omits referrer and claimed-author legs when their wallet
has no USDC ATA at quote time and retains those shares in the treasury leg.
That policy is decided before the signed deposit; the unnamed slot must use
the default owner and zero amount. The author-vault ATA is still prepared by
the application. After deposit, if a named optional ATA becomes unusable, the
program's existing per-leg buyer-refund fallback still applies. Do not gate an
already-funded settlement on an off-chain ATA lookup or redirect its frozen
terms to the treasury.

## Pending Devnet acceptance

The backend owner and Kev must set an RPC request budget and, if image
generation is used, approve the provider, model, call count and estimated cost
before the first live call. Run paid calls through the application's existing
metered guard. No such budget or paid-call approval is assumed here.

The acceptance sequence is one imported prompt rendered before an author
claim, after the claim, and with a referrer who has no Solana wallet. For each
step, verify buyer, treasury, artist, referrer and author-vault/wallet deltas,
delivered-unit count, all four paid amounts and refund. Verify existing
author-vault funds reach only the bound claimant, fresh funds can be swept by
the same wallet, and a second wallet cannot claim. Include a sparse author
position, each unusable optional canonical ATA, and a rejected repeated
settlement. Record receipts and the exact application, program and IDL commits.

Acceptance and owner approval remain pending until that evidence exists.
