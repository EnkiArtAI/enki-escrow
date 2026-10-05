# Contributing

Use one topic per branch and pull request. The main branch is the reviewed release line; Kev merges it. Program, payment and security changes require the backend owner's review and Kev's release approval. Kev assigns the backend-owner role; the bootstrap CODEOWNERS file routes reviews to him until the roster is confirmed.

Keep the program separate from the frontend and server implementation in [EnkiArtAI/frontend](https://github.com/EnkiArtAI/frontend). Review how changes affect the ClickUp specification, payment accounting and the requirements test cases together.

## Development

- Use disposable local test wallets. Keep keys, environment files, binaries and build output out of Git.
- Never print secrets, their prefixes or their lengths.
- Tests must run against the in-process VM or a local validator. CI must fail when the program binary is missing.
- Live devnet RPC testing needs an agreed call budget before the first request. Metered generation APIs require Kev's explicit cost approval and the frontend's metered guard.
- New money or authority behavior needs a regression test that fails without the change. Include the actual test output in the pull request.
- Do not change, rebase or force-push another contributor's branch. Do not commit debugging output to a user interface.

## Release

Keep program work in a draft pull request until the SBF build, unit tests, VM integration tests and required mutation checks pass. Review arithmetic, account owners and canonical addresses, signatures, replay behavior, refund paths, pause behavior and rent destinations.

Mainnet release also requires an independent security review, the ticket's devnet soak, approved multisig and timelock governance, and a coordinated server/database rollout. A green CI run alone is not a release authorization.
