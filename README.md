# Enki Escrow

Solana escrow for Enki paid image-generation intents. A buyer funds a fixed batch; settlement pays only for delivered images and refunds the unused balance.

This repository is being initialized. The program implementation is developed on `codex/123qmzqzw1w-program` and reviewed through a draft pull request. There is no deployment from this repository yet. Production use requires successful program tests, an independent security review and the team's release approval.

The specification is [ClickUp 123qmzqzw1w](https://app.clickup.com/t/9015834867/123qmzqzw1w). Frontend and server integration live in [EnkiArtAI/frontend](https://github.com/EnkiArtAI/frontend).

## Payment contract

- Classic SPL USDC only, with a cluster-specific mint and six decimals.
- A deposit holds 1–24 units, capped at 25 USDC across the batch.
- The treasury and optional artist amounts per unit are fixed by the deposit.
- The authorized operator settles once for `k` delivered units, with `0 <= k <= N`.
- Any caller can reclaim after settlement or expiry; refunds and account rent go to the stored destinations.
- A guardian can pause new deposits or revoke the operator. Restoring authority requires the admin.

Tests use an in-process Solana VM and disposable wallets. They require no Enki secrets, live RPC connection or paid generation API.

See [CONTRIBUTING.md](CONTRIBUTING.md) for review and release rules. Licensed under [Apache-2.0](LICENSE).
