# solana-transaction-executor

Sends one transaction until it lands or it is proven it did not. Each attempt comes from a stream
the caller builds (`AttemptTransaction`: the signed transaction, its blockhash's last valid block
height and `sent_before`), is simulated, then sent; a watcher polls the signatures of every attempt.
A caller resuming after a crash yields its persisted attempts first with `sent_before`: each is
watched, resent as is while its blockhash is valid, and held until its expiry is proven before the
next attempt is pulled, so it is never replaced while it may still land. The
`solana-to-4.x` line uses the Solana 4.x crates and sends legacy and v1 transactions;
`solana-3.0.x` stays for its users.

- **Retry after expiry** — an unconfirmed attempt is rebuilt only once the block height passed its
  last valid block height and the ledger history knows none of the sent attempts, so two attempts
  of a non-idempotent operation never both land, and a landing with an error ends the send.
- **`Execution`** — every attempt (signature, blockhash, last valid block height, sent, send error,
  refusal) and one `Outcome`. RPC error text in it keeps only the endpoint's host:
  - `Landed` / `LandedWithError` — the signature, its slot and the on-chain error.
  - `NotLanded` — proven: `Refused` by a simulation, `NotSent` because no attempt could be built or
    simulated, or `Expired` when every sent attempt's blockhash expired and the ledger knows none.
    Planning the operation again is safe.
  - `Unknown` — an attempt may still land: the RPC kept failing, the status watcher stopped, or a
    blockhash was still valid `waiting_time_after_last_send` after its attempt was sent. Planning
    the operation again could run it twice.
- **Providers** — `SignatureStatusesProvider` (statuses at the commitment and with history, the
  latest blockhash with its last valid block height, the block height, the commitment),
  `SimulateTransactionProvider` (signed, and unsigned against the latest blockhash),
  `SendTransactionProvider` and `PrioritizationFeesProvider`. `with_default_providers` serves all four from one `RpcClient`,
  whose commitment they use; `SendTransactionWithGrowingTipProvider` sends through a tip relay.
  Its default `TipPolicy` tips nothing on the first attempt, then 1,000 ×4 per retry up to 200,000.
- **`alpenglow_commitment`** — finalized once `getAgGenesisCert` returns a certificate, confirmed
  before. A long-running caller asks on each pass and builds its `RpcClient` with the answer.
- **`simulate`** — the consumed units, loaded account bytes, typed error and logs of an unsigned
  transaction. A v1 limit left unset counts as 0, so it is simulated at `MAX_COMPUTE_UNIT_LIMIT`
  and `MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES`; the signed attempt must set its own.
- **`FeePolicy`** — the first attempt pays the start rate, 0 by default. A retry doubles the rate
  from a first step of 1,000 micro-lamports per CU up to 100,000. `FeeStrategy::Dynamic` also takes
  the chosen percentile of the recent fees over the transaction's writable accounts. The rate is in
  micro-lamports per CU; `priority_fee_lamports` turns it into the total a v1 `priority_fee` takes.

Tests against a local Surfpool 1.6.1 (offline, 50 ms slots) are ignored by default:
`SURFPOOL=<binary> cargo test --test surfpool -- --ignored`.
