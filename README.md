# solana-transaction-executor

Sends one transaction until it lands or it is proven it did not. Each attempt comes from a stream
the caller builds (`AttemptTransaction`: the signed transaction, its blockhash's last valid block
height, `sent_before` and `simulated`), is simulated, then sent; one loop per executor polls the
signatures of every execution, up to 256 per call. An attempt marked `simulated` skips that
simulation unless it is `sent_before`; a re-signed attempt should leave it unset, so the changed
state is checked before a fee is paid again.
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
  from a first step of 1,000 micro-lamports per CU up to 100,000. `FeeStrategy::Dynamic` raises every
  attempt, the first too, to the chosen percentile of the recent fees over the transaction's writable
  accounts. The rate is in
  micro-lamports per CU; `priority_fee_lamports` turns it into the total a v1 `priority_fee` takes.

`TransactionExecutorConfig` defaults are slow and cheap:

| Field | Default | Meaning |
|---|---|---|
| `signature_status_check_interval` | 1s | status and expiry check period |
| `transaction_send_interval` | 20s | pause before the next attempt when not waiting for expiry |
| `waiting_time_after_last_send` | 120s | bound after which a still-valid attempt is `Unknown` |
| `max_attempts` | 10 | attempts pulled from the stream |
| `wait_for_blockhash_expiry_before_retry` | true | rebuild only after the last attempt's expiry is proven |
| `max_consecutive_blockhash_check_failures` | 5 | failed block height reads before `Unknown` |
| `blockhash_refresh_interval` | 5s | age of the cached blockhash and block height |
| `max_sends_per_second` | 10 | all sends of the executor, first sends and rebroadcasts |
| `rebroadcast_interval` | 2s | resend of the same bytes until they land or expire; `None` sends once |

`wait_for_blockhash_expiry_before_retry = false` is the fast, fee-costing mode: it re-signs every
`transaction_send_interval` with `FeePolicy::next` without waiting for expiry, so two attempts can
both land. Use it for idempotent instructions only.

Tests against a local Surfpool 1.6.1 (offline, 50 ms slots) are ignored by default:
`SURFPOOL=<binary> cargo test --test surfpool -- --ignored`.
