# Enki Escrow

Solana escrow for Enki paid image-generation intents. A buyer funds a fixed batch; settlement pays only for delivered images and refunds the unused balance.

This repository is being initialized. The program implementation is developed on `codex/123qmzqzw1w-program` and reviewed through a draft pull request. There is no deployment from this repository yet. Production use requires successful program tests, an independent security review and the team's release approval.

The specification is [ClickUp 123qmzqzw1w](https://app.clickup.com/t/9015834867/123qmzqzw1w). Frontend and server integration live in [EnkiArtAI/frontend](https://github.com/EnkiArtAI/frontend).

## Payment contract

- Classic SPL USDC only, with a cluster-specific mint and six decimals.
- A deposit holds 1–24 units. The sole batch cap is `Config.max_deposit_micro`
  (the 2026-10-06 specification calls for 50 USDC when Config is initialized).
  It can be changed by the admin without redeploying code. Clients and SQL must
  read this on-chain field instead of defining another policy limit.
- The treasury and optional artist amounts per unit are fixed by the deposit.
- The authorized operator settles once for `k` delivered units, with `0 <= k <= N`.
- Any caller can reclaim after settlement or expiry; refunds and account rent go to the stored destinations.
- A guardian can pause new deposits or revoke the operator. Restoring authority requires the admin.

Tests use an in-process Solana VM and disposable wallets. They require no Enki secrets, live RPC connection or paid generation API.

## Build and test

The program branch pins Anchor 0.32.1, host Rust 1.90.0, Agave 2.3.0 and SBF platform-tools v1.57. CI uses GitHub's standard `ubuntu-latest` x64 runner and verifies the tool archives' SHA-256 before extraction.

With that toolchain configured:

```sh
cargo fmt --all -- --check
cargo build-sbf --tools-version v1.57 -- --locked
cargo test --workspace --locked -- --test-threads=1
```

The VM tests require `target/deploy/enki_escrow.so` and fail when it is absent. The default build accepts devnet USDC. A mainnet build requires `--no-default-features --features mainnet` and a separate release review. The program address in the draft is a test address, not evidence of a deployment.

The host VM tests use optimization level 1 with overflow checks and debug assertions enabled. The 100,000-sequence test reports progress every 10,000 sequences in CI.

After the original program passes, Linux CI compiles four deliberately faulty programs and requires the matching VM tests to fail: restoring a second fixed deposit cap, allowing repeated settlement, charging for undelivered units, and refunding without deducting the missing-ATA fee. `scripts/check-mutations.py` restores the original source and SBF binary in a `finally` block. A compilation failure does not count as mutation proof.

CI also exports the Anchor IDL compiled from the Rust source as `enki-escrow-idl`, with its SHA-256 and source commit. The server client must be generated from this artifact. The pinned Anchor 0.32.1 CLI is checksum-verified and `anchor idl build` runs without a wallet or RPC connection.

See [CONTRIBUTING.md](CONTRIBUTING.md) for review and release rules. Licensed under [Apache-2.0](LICENSE).
