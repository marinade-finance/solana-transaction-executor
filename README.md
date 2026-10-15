# solana-transaction-executor

Sends one transaction until it confirms. Each attempt comes from a stream the caller builds, is
simulated, then sent; a watcher polls the signatures of every attempt. The `solana-to-4.x` line
uses the Solana 4.x crates and sends legacy and v1 transactions; `solana-3.0.x` stays for its
users.

- **Retry after expiry** — an unconfirmed attempt is rebuilt only once its blockhash has expired,
  so two attempts of a non-idempotent operation never both land.
- **Providers** — `SignatureStatusesProvider` (statuses, blockhash validity, latest blockhash),
  `SimulateTransactionProvider` (signed, and unsigned against the latest blockhash),
  `SendTransactionProvider` and `PrioritizationFeesProvider`. `with_default_providers` serves all
  four from one `RpcClient`; `SendTransactionWithGrowingTipProvider` sends through a tip relay.
- **`simulate`** — the consumed units, loaded account bytes, typed error and logs of an unsigned
  transaction.
- **`FeePolicy`** — the first attempt pays the start rate, 0 by default. A retry doubles the rate
  from a first step of 1,000 micro-lamports per CU up to 100,000. `FeeStrategy::Dynamic` also takes
  the chosen percentile of the recent fees over the transaction's writable accounts.
