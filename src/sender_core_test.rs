use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use async_trait::async_trait;
use solana_client::rpc_response::{Response, RpcResponseContext, RpcResult};
use solana_commitment_config::CommitmentConfig;
use solana_hash::Hash;
use solana_message::{v1, VersionedMessage};
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction_error::TransactionError;
use solana_transaction_status_client_types::{TransactionConfirmationStatus, TransactionStatus};
use tokio::time::Instant;

use crate::{
    sender_core::SenderCore, SendTransactionMeta, SendTransactionProvider,
    SignatureStatusesProvider, TransactionExecutorConfig,
};

#[derive(Default)]
struct Counting {
    block_height_reads: AtomicUsize,
    block_height_delay: Mutex<Duration>,
    fails: AtomicBool,
    status_calls: Mutex<Vec<usize>>,
    /// Signature, attempt and time of every send.
    sends: Mutex<Vec<(Signature, u32, Instant)>>,
    sends_fail: AtomicBool,
    statuses_seen: Mutex<Vec<Signature>>,
    sent_transactions: Mutex<Vec<VersionedTransaction>>,
    /// Every send after the first answers `AlreadyProcessed`.
    resends_already_processed: AtomicBool,
    landed: Mutex<HashSet<Signature>>,
    statuses_delay: Mutex<Duration>,
    statuses_fail: AtomicBool,
    statuses_panic: AtomicBool,
}

#[async_trait]
impl SendTransactionProvider for Counting {
    async fn send_transaction(
        &self,
        transaction: &VersionedTransaction,
        meta: SendTransactionMeta,
    ) -> anyhow::Result<Signature> {
        let signature = transaction.signatures[0];
        let index = {
            let mut sends = self.sends.lock().unwrap();
            sends.push((signature, meta.attempt, Instant::now()));
            sends.len() - 1
        };
        self.sent_transactions
            .lock()
            .unwrap()
            .push(transaction.clone());
        if index > 0 && self.resends_already_processed.load(Ordering::SeqCst) {
            anyhow::bail!("{}", TransactionError::AlreadyProcessed);
        }
        anyhow::ensure!(
            !self.sends_fail.load(Ordering::SeqCst),
            "error sending request for url (https://rpc.example.com/?api-key=SECRET)"
        );
        Ok(signature)
    }
}

#[async_trait]
impl SignatureStatusesProvider for Counting {
    async fn get_signature_statuses(
        &self,
        signatures: &[Signature],
    ) -> RpcResult<Vec<Option<TransactionStatus>>> {
        let delay = *self.statuses_delay.lock().unwrap();
        tokio::time::sleep(delay).await;
        assert!(
            !self.statuses_panic.load(Ordering::SeqCst),
            "scripted poll failure"
        );
        if self.statuses_fail.load(Ordering::SeqCst) {
            return Err(solana_client::client_error::ClientErrorKind::Custom("down".into()).into());
        }
        self.status_calls.lock().unwrap().push(signatures.len());
        self.statuses_seen
            .lock()
            .unwrap()
            .extend_from_slice(signatures);
        Ok(Response {
            context: RpcResponseContext {
                slot: 1,
                api_version: None,
            },
            value: {
                let landed = self.landed.lock().unwrap();
                signatures
                    .iter()
                    .map(|signature| {
                        landed.contains(signature).then_some(TransactionStatus {
                            slot: 7,
                            confirmations: None,
                            status: Ok(()),
                            err: None,
                            confirmation_status: Some(TransactionConfirmationStatus::Finalized),
                        })
                    })
                    .collect()
            },
        })
    }

    async fn get_signature_statuses_with_history(
        &self,
        _signatures: &[Signature],
    ) -> RpcResult<Vec<Option<TransactionStatus>>> {
        unreachable!("the core reads no history")
    }

    async fn latest_blockhash(&self) -> anyhow::Result<(Hash, u64)> {
        Ok((Hash::new_from_array([1; 32]), 100))
    }

    async fn block_height(&self) -> anyhow::Result<u64> {
        let delay = *self.block_height_delay.lock().unwrap();
        tokio::time::sleep(delay).await;
        let read = self.block_height_reads.fetch_add(1, Ordering::SeqCst);
        anyhow::ensure!(!self.fails.load(Ordering::SeqCst), "the node is down");
        Ok(read as u64)
    }

    fn commitment(&self) -> CommitmentConfig {
        CommitmentConfig::finalized()
    }
}

fn core(chain: &Arc<Counting>) -> SenderCore {
    core_with(chain, TransactionExecutorConfig::default())
}

fn core_with(chain: &Arc<Counting>, config: TransactionExecutorConfig) -> SenderCore {
    SenderCore::new(&config, chain.clone(), chain.clone())
}

fn transaction(signature: Signature) -> VersionedTransaction {
    let message = v1::Message::try_compile_with_config(
        &Pubkey::new_unique(),
        &[],
        Hash::new_unique(),
        v1::TransactionConfig::empty(),
    )
    .unwrap();
    VersionedTransaction {
        signatures: vec![signature],
        message: VersionedMessage::V1(message),
    }
}

fn signatures(count: usize) -> Vec<Signature> {
    (0..count)
        .map(|n| {
            let mut bytes = [0; 64];
            bytes[..8].copy_from_slice(&(n as u64 + 1).to_le_bytes());
            Signature::from(bytes)
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn block_height_reads_within_the_interval_cost_one_rpc_call() {
    let chain = Arc::new(Counting::default());
    let core = core(&chain);

    for _ in 0..50 {
        assert_eq!(core.block_height().await.unwrap(), 0);
        tokio::time::advance(Duration::from_millis(80)).await;
    }
    assert_eq!(chain.block_height_reads.load(Ordering::SeqCst), 1);

    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(core.block_height().await.unwrap(), 1);
    assert_eq!(chain.block_height_reads.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn a_failed_refresh_is_an_error_and_the_next_call_retries() {
    let chain = Arc::new(Counting::default());
    let core = core(&chain);
    chain.fails.store(true, Ordering::SeqCst);

    assert!(core.block_height().await.is_err());
    chain.fails.store(false, Ordering::SeqCst);

    assert_eq!(core.block_height().await.unwrap(), 1);
}

#[tokio::test(start_paused = true)]
async fn six_hundred_pending_signatures_are_polled_in_three_calls() {
    let chain = Arc::new(Counting::default());
    let core = core(&chain);
    let (execution, _landings) = core.register();
    for signature in signatures(600) {
        core.watch(execution, signature);
    }

    tokio::time::sleep(Duration::from_millis(1_500)).await;

    assert_eq!(*chain.status_calls.lock().unwrap(), vec![256, 256, 88]);
}

#[tokio::test(start_paused = true)]
async fn the_poll_rate_does_not_depend_on_the_pending_count() {
    let mut calls = Vec::new();
    for count in [1, 200] {
        let chain = Arc::new(Counting::default());
        let core = core(&chain);
        let (execution, _landings) = core.register();
        for signature in signatures(count) {
            core.watch(execution, signature);
        }

        tokio::time::sleep(Duration::from_millis(10_500)).await;

        let status_calls = chain.status_calls.lock().unwrap().clone();
        assert!(
            status_calls.iter().all(|size| *size == count),
            "{status_calls:?}"
        );
        calls.push(status_calls.len());
    }
    assert_eq!(calls[0], calls[1]);
    assert_eq!(calls[0], 10);
}

#[tokio::test(start_paused = true)]
async fn nothing_pending_polls_nothing() {
    let chain = Arc::new(Counting::default());
    let core = core(&chain);
    let (_execution, _landings) = core.register();

    tokio::time::sleep(Duration::from_secs(10)).await;

    assert!(chain.status_calls.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_finished_execution_is_no_longer_polled() {
    let chain = Arc::new(Counting::default());
    let core = core(&chain);
    let (execution, landings) = core.register();
    core.watch(execution, signatures(1)[0]);
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    let polled = chain.status_calls.lock().unwrap().len();
    assert_eq!(polled, 2);

    drop(landings);
    tokio::time::sleep(Duration::from_secs(10)).await;

    assert_eq!(chain.status_calls.lock().unwrap().len(), polled);
}

#[tokio::test(start_paused = true)]
async fn first_sends_are_spaced_by_the_rate() {
    let chain = Arc::new(Counting::default());
    let core = Arc::new(core(&chain));
    let sends = signatures(30).into_iter().map(|signature| {
        let core = core.clone();
        tokio::spawn(async move {
            let (execution, landings) = core.register();
            let error = core
                .send_transaction(execution, &transaction(signature), 1, 1_000)
                .await;
            drop(landings);
            error
        })
    });
    for send in futures_util::future::join_all(sends).await {
        assert_eq!(send.unwrap(), None);
    }

    let times: Vec<Instant> = chain
        .sends
        .lock()
        .unwrap()
        .iter()
        .map(|(_, _, at)| *at)
        .collect();
    assert_eq!(times.len(), 30);
    for pair in times.windows(2) {
        assert!(pair[1] - pair[0] >= Duration::from_millis(100), "{pair:?}");
    }
    for (index, start) in times.iter().enumerate() {
        let in_window = times[index..]
            .iter()
            .take_while(|at| **at - *start < Duration::from_secs(1))
            .count();
        assert!(in_window <= 10, "{in_window} sends within 1s");
    }
}

#[tokio::test(start_paused = true)]
async fn a_send_error_is_recorded_and_the_signature_is_still_watched() {
    let chain = Arc::new(Counting::default());
    chain.sends_fail.store(true, Ordering::SeqCst);
    let core = core(&chain);
    let (execution, _landings) = core.register();
    let signature = signatures(1)[0];

    let error = core
        .send_transaction(execution, &transaction(signature), 1, 1_000)
        .await;

    assert_eq!(
        error.as_deref(),
        Some("error sending request for url (https://rpc.example.com/<redacted>)")
    );
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert_eq!(*chain.statuses_seen.lock().unwrap(), vec![signature]);
}

fn rebroadcasting(rebroadcast_interval: Option<Duration>) -> TransactionExecutorConfig {
    TransactionExecutorConfig {
        rebroadcast_interval,
        block_height_refresh_interval: Duration::from_secs(1),
        ..TransactionExecutorConfig::default()
    }
}

fn send_offsets(chain: &Counting, start: Instant) -> Vec<Duration> {
    chain
        .sends
        .lock()
        .unwrap()
        .iter()
        .map(|(_, _, at)| *at - start)
        .collect()
}

#[tokio::test(start_paused = true)]
async fn an_unlanded_transaction_is_sent_again_every_two_seconds() {
    let chain = Arc::new(Counting::default());
    let core = core_with(&chain, rebroadcasting(Some(Duration::from_secs(2))));
    let (execution, _landings) = core.register();
    let start = Instant::now();

    core.send_transaction(execution, &transaction(signatures(1)[0]), 1, 1_000)
        .await;
    tokio::time::sleep(Duration::from_millis(6_500)).await;

    assert_eq!(
        send_offsets(&chain, start),
        [0, 2, 4, 6].map(Duration::from_secs)
    );
    let sent = chain.sent_transactions.lock().unwrap();
    assert!(sent.iter().all(|transaction| *transaction == sent[0]));
}

#[tokio::test(start_paused = true)]
async fn a_rebroadcast_stops_past_the_last_valid_block_height() {
    let chain = Arc::new(Counting::default());
    let core = core_with(&chain, rebroadcasting(Some(Duration::from_secs(2))));
    let (execution, _landings) = core.register();
    let start = Instant::now();

    core.send_transaction(execution, &transaction(signatures(1)[0]), 1, 1)
        .await;
    tokio::time::sleep(Duration::from_secs(20)).await;

    assert_eq!(
        send_offsets(&chain, start),
        [0, 2, 4].map(Duration::from_secs)
    );
}

#[tokio::test(start_paused = true)]
async fn rebroadcast_off_sends_once() {
    let chain = Arc::new(Counting::default());
    let core = core_with(&chain, rebroadcasting(None));
    let (execution, _landings) = core.register();

    core.send_transaction(execution, &transaction(signatures(1)[0]), 1, 1_000)
        .await;
    tokio::time::sleep(Duration::from_secs(20)).await;

    assert_eq!(chain.sends.lock().unwrap().len(), 1);
    assert_eq!(chain.block_height_reads.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn rebroadcasts_count_against_the_rate() {
    let chain = Arc::new(Counting::default());
    let core = Arc::new(core_with(
        &chain,
        TransactionExecutorConfig {
            max_sends_per_second: 5,
            ..rebroadcasting(Some(Duration::from_secs(2)))
        },
    ));
    let mut receivers = Vec::new();
    for signature in signatures(20) {
        let (execution, landings) = core.register();
        receivers.push(landings);
        let core = core.clone();
        tokio::spawn(async move {
            core.send_transaction(execution, &transaction(signature), 1, 1_000)
                .await;
        });
    }

    tokio::time::sleep(Duration::from_secs(20)).await;

    let times = send_offsets(&chain, Instant::now() - Duration::from_secs(20));
    assert!(times.len() > 40, "{} sends", times.len());
    for (index, start) in times.iter().enumerate() {
        let in_window = times[index..]
            .iter()
            .take_while(|at| **at - *start < Duration::from_secs(1))
            .count();
        assert!(in_window <= 5, "{in_window} sends within 1s");
    }
}

#[tokio::test(start_paused = true)]
async fn an_already_processed_rebroadcast_is_landed_once_the_status_shows_it() {
    let chain = Arc::new(Counting::default());
    chain
        .resends_already_processed
        .store(true, Ordering::SeqCst);
    let core = core_with(&chain, rebroadcasting(Some(Duration::from_secs(2))));
    let (execution, mut landings) = core.register();
    let signature = signatures(1)[0];

    core.send_transaction(execution, &transaction(signature), 1, 1_000)
        .await;
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    assert_eq!(chain.sends.lock().unwrap().len(), 2);
    chain.landed.lock().unwrap().insert(signature);

    let landing = landings.recv().await.unwrap();
    assert_eq!(landing.signature, signature);
    assert_eq!(landing.slot, 7);
}

#[tokio::test(start_paused = true)]
async fn a_rebroadcast_repeats_the_attempt_number() {
    let chain = Arc::new(Counting::default());
    let core = core_with(&chain, rebroadcasting(Some(Duration::from_secs(2))));
    let (execution, _landings) = core.register();

    core.send_transaction(execution, &transaction(signatures(1)[0]), 3, 1_000)
        .await;
    tokio::time::sleep(Duration::from_secs(7)).await;

    let attempts: Vec<u32> = chain
        .sends
        .lock()
        .unwrap()
        .iter()
        .map(|(_, attempt, _)| *attempt)
        .collect();
    assert_eq!(attempts, vec![3; 4]);
}

fn signature_sends(chain: &Counting, signature: Signature) -> usize {
    chain
        .sends
        .lock()
        .unwrap()
        .iter()
        .filter(|(sent, _, _)| *sent == signature)
        .count()
}

#[tokio::test(start_paused = true)]
async fn a_slow_status_poll_does_not_stall_the_sends() {
    let chain = Arc::new(Counting::default());
    *chain.statuses_delay.lock().unwrap() = Duration::from_millis(800);
    let core = Arc::new(core_with(&chain, rebroadcasting(None)));
    let start = Instant::now();
    let mut receivers = Vec::new();
    for signature in signatures(30) {
        let (execution, landings) = core.register();
        receivers.push(landings);
        let core = core.clone();
        tokio::spawn(async move {
            core.send_transaction(execution, &transaction(signature), 1, 1_000)
                .await;
        });
    }

    tokio::time::sleep(Duration::from_secs(5)).await;

    let offsets = send_offsets(&chain, start);
    assert_eq!(offsets.len(), 30);
    assert!(offsets[29] < Duration::from_secs(3), "{offsets:?}");
}

#[tokio::test(start_paused = true)]
async fn a_queued_send_of_a_cancelled_execution_is_not_broadcast() {
    let chain = Arc::new(Counting::default());
    let core = Arc::new(core_with(
        &chain,
        TransactionExecutorConfig {
            max_sends_per_second: 1,
            ..rebroadcasting(None)
        },
    ));
    let [first, cancelled] = signatures(2).try_into().unwrap();
    let (execution, _landings) = core.register();
    core.send_transaction(execution, &transaction(first), 1, 1_000)
        .await;
    let queued = {
        let core = core.clone();
        tokio::spawn(async move {
            core.send_transaction(execution, &transaction(cancelled), 2, 1_000)
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    queued.abort();

    tokio::time::sleep(Duration::from_secs(3)).await;

    assert_eq!(signature_sends(&chain, first), 1);
    assert_eq!(signature_sends(&chain, cancelled), 0);
}

#[tokio::test(start_paused = true)]
async fn zero_and_huge_periods_do_not_stop_the_core() {
    let chain = Arc::new(Counting::default());
    let core = core_with(
        &chain,
        TransactionExecutorConfig {
            signature_status_check_interval: Duration::ZERO,
            max_sends_per_second: u32::MAX,
            ..rebroadcasting(None)
        },
    );
    let (execution, _landings) = core.register();
    let signature = signatures(1)[0];

    let error = core
        .send_transaction(execution, &transaction(signature), 1, 1_000)
        .await;
    tokio::time::sleep(Duration::from_millis(10)).await;

    assert_eq!(error, None);
    assert!(chain.statuses_seen.lock().unwrap().contains(&signature));
    drop(core);

    let core = core_with(
        &chain,
        TransactionExecutorConfig {
            signature_status_check_interval: Duration::MAX,
            ..rebroadcasting(None)
        },
    );
    let (execution, mut landings) = core.register();
    let signature = signatures(2)[1];
    chain.landed.lock().unwrap().insert(signature);

    let error = core
        .send_transaction(execution, &transaction(signature), 1, 1_000)
        .await;

    assert_eq!(error, None);
    assert_eq!(landings.recv().await.unwrap().signature, signature);
}

#[tokio::test(start_paused = true)]
async fn a_zero_send_rate_counts_as_one_per_second() {
    let chain = Arc::new(Counting::default());
    let core = core_with(
        &chain,
        TransactionExecutorConfig {
            max_sends_per_second: 0,
            ..rebroadcasting(None)
        },
    );
    let (execution, _landings) = core.register();
    let start = Instant::now();

    for signature in signatures(2) {
        core.send_transaction(execution, &transaction(signature), 1, 1_000)
            .await;
    }

    assert_eq!(send_offsets(&chain, start), [0, 1].map(Duration::from_secs));
}

#[tokio::test(start_paused = true)]
async fn a_panicking_poll_ends_only_its_executions() {
    let chain = Arc::new(Counting::default());
    chain.statuses_panic.store(true, Ordering::SeqCst);
    let core = core_with(&chain, rebroadcasting(None));
    let (execution, mut stopped) = core.register();
    core.watch(execution, signatures(1)[0]);

    assert!(stopped.recv().await.is_none());

    chain.statuses_panic.store(false, Ordering::SeqCst);
    let signature = signatures(2)[1];
    chain.landed.lock().unwrap().insert(signature);
    let (execution, mut landings) = core.register();
    core.send_transaction(execution, &transaction(signature), 1, 1_000)
        .await;
    assert_eq!(landings.recv().await.unwrap().signature, signature);
}

#[tokio::test(start_paused = true)]
async fn a_failed_status_call_is_retried_on_the_next_tick() {
    let chain = Arc::new(Counting::default());
    chain.statuses_fail.store(true, Ordering::SeqCst);
    let core = core_with(&chain, rebroadcasting(None));
    let (execution, mut landings) = core.register();
    let signature = signatures(1)[0];
    chain.landed.lock().unwrap().insert(signature);
    core.watch(execution, signature);

    tokio::time::sleep(Duration::from_millis(2_500)).await;
    assert!(landings.try_recv().is_err());
    chain.statuses_fail.store(false, Ordering::SeqCst);

    assert_eq!(landings.recv().await.unwrap().signature, signature);
}

#[tokio::test(start_paused = true)]
async fn a_failed_height_read_skips_the_rebroadcast() {
    let chain = Arc::new(Counting::default());
    let core = core_with(&chain, rebroadcasting(Some(Duration::from_secs(2))));
    let (execution, _landings) = core.register();
    chain.fails.store(true, Ordering::SeqCst);

    core.send_transaction(execution, &transaction(signatures(1)[0]), 1, 1_000)
        .await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(chain.sends.lock().unwrap().len(), 1);

    chain.fails.store(false, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(chain.sends.lock().unwrap().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn first_sends_go_before_rebroadcasts() {
    let chain = Arc::new(Counting::default());
    let core = Arc::new(core_with(
        &chain,
        TransactionExecutorConfig {
            max_sends_per_second: 1,
            ..rebroadcasting(Some(Duration::from_secs(2)))
        },
    ));
    let all = signatures(4);
    let mut receivers = Vec::new();
    for signature in all.clone() {
        let (execution, landings) = core.register();
        receivers.push(landings);
        let core = core.clone();
        tokio::spawn(async move {
            core.send_transaction(execution, &transaction(signature), 1, 1_000)
                .await;
        });
    }

    tokio::time::sleep(Duration::from_millis(4_500)).await;

    let order: Vec<Signature> = chain
        .sends
        .lock()
        .unwrap()
        .iter()
        .map(|(signature, _, _)| *signature)
        .collect();
    assert_eq!(order, vec![all[0], all[1], all[2], all[3], all[0]]);
}

#[tokio::test(start_paused = true)]
async fn a_queued_rebroadcast_past_its_validity_is_dropped() {
    let chain = Arc::new(Counting::default());
    let core = Arc::new(core_with(
        &chain,
        TransactionExecutorConfig {
            max_sends_per_second: 1,
            ..rebroadcasting(Some(Duration::from_secs(2)))
        },
    ));
    let all = signatures(6);
    let mut receivers = Vec::new();
    for (index, signature) in all.iter().copied().enumerate() {
        let (execution, landings) = core.register();
        receivers.push(landings);
        let last_valid = if index == 0 { 1 } else { 1_000 };
        let core = core.clone();
        tokio::spawn(async move {
            core.send_transaction(execution, &transaction(signature), 1, last_valid)
                .await;
        });
    }

    tokio::time::sleep(Duration::from_secs(10)).await;

    assert_eq!(signature_sends(&chain, all[0]), 1);
    assert!(signature_sends(&chain, all[1]) >= 2);
}

#[tokio::test(start_paused = true)]
async fn a_hung_statuses_call_does_not_stop_landing_detection() {
    let chain = Arc::new(Counting::default());
    *chain.statuses_delay.lock().unwrap() = Duration::from_secs(3600);
    let core = core_with(
        &chain,
        TransactionExecutorConfig {
            signature_status_check_interval: Duration::from_millis(100),
            ..TransactionExecutorConfig::default()
        },
    );
    let (execution, mut landings) = core.register();
    let signature = signatures(1)[0];

    core.send_transaction(execution, &transaction(signature), 1, 1_000)
        .await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    *chain.statuses_delay.lock().unwrap() = Duration::ZERO;
    chain.landed.lock().unwrap().insert(signature);

    let landing = tokio::time::timeout(Duration::from_secs(60), landings.recv())
        .await
        .expect("a landing before the hung call returns");
    assert_eq!(landing.unwrap().signature, signature);
}

#[tokio::test(start_paused = true)]
async fn a_hung_block_height_read_fails_and_frees_the_cache() {
    let chain = Arc::new(Counting::default());
    *chain.block_height_delay.lock().unwrap() = Duration::from_secs(3600);
    let core = core(&chain);

    let hung = tokio::time::timeout(Duration::from_secs(60), core.block_height())
        .await
        .expect("the read gives up before the provider returns");
    assert!(hung.is_err());

    *chain.block_height_delay.lock().unwrap() = Duration::ZERO;
    assert!(core.block_height().await.is_ok());
}

#[tokio::test(start_paused = true)]
async fn concurrent_reads_of_a_hung_block_height_fail_within_one_timeout() {
    let chain = Arc::new(Counting::default());
    *chain.block_height_delay.lock().unwrap() = Duration::from_secs(3600);
    let core = core(&chain);

    let (first, second, third) = tokio::time::timeout(Duration::from_secs(45), async {
        tokio::join!(
            core.block_height(),
            core.block_height(),
            core.block_height()
        )
    })
    .await
    .expect("every waiter gives up within one call timeout");
    assert!(first.is_err() && second.is_err() && third.is_err());
}

#[tokio::test(start_paused = true)]
async fn a_queued_rebroadcast_of_a_finished_execution_is_not_sent() {
    let chain = Arc::new(Counting::default());
    let core = core_with(
        &chain,
        TransactionExecutorConfig {
            signature_status_check_interval: Duration::from_millis(100),
            max_sends_per_second: 1,
            ..rebroadcasting(Some(Duration::from_millis(200)))
        },
    );
    let (execution, landings) = core.register();
    let signature = signatures(1)[0];

    core.send_transaction(execution, &transaction(signature), 1, 1_000)
        .await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    drop(landings);
    tokio::time::sleep(Duration::from_secs(3)).await;

    assert_eq!(signature_sends(&chain, signature), 1);
}
