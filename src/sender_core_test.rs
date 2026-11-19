use std::{
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
use solana_transaction_status_client_types::TransactionStatus;
use tokio::time::Instant;

use crate::{
    sender_core::SenderCore, SendTransactionMeta, SendTransactionProvider,
    SignatureStatusesProvider, TransactionExecutorConfig,
};

#[derive(Default)]
struct Counting {
    blockhash_reads: AtomicUsize,
    block_height_reads: AtomicUsize,
    fails: AtomicBool,
    status_calls: Mutex<Vec<usize>>,
    /// Signature, attempt and time of every send.
    sends: Mutex<Vec<(Signature, u32, Instant)>>,
    sends_fail: AtomicBool,
    statuses_seen: Mutex<Vec<Signature>>,
}

#[async_trait]
impl SendTransactionProvider for Counting {
    async fn send_transaction(
        &self,
        transaction: &VersionedTransaction,
        meta: SendTransactionMeta,
    ) -> anyhow::Result<Signature> {
        let signature = transaction.signatures[0];
        self.sends
            .lock()
            .unwrap()
            .push((signature, meta.attempt, Instant::now()));
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
            value: vec![None; signatures.len()],
        })
    }

    async fn get_signature_statuses_with_history(
        &self,
        _signatures: &[Signature],
    ) -> RpcResult<Vec<Option<TransactionStatus>>> {
        unreachable!("the core reads no history")
    }

    async fn latest_blockhash(&self) -> anyhow::Result<(Hash, u64)> {
        let read = self.blockhash_reads.fetch_add(1, Ordering::SeqCst);
        anyhow::ensure!(!self.fails.load(Ordering::SeqCst), "the node is down");
        Ok((Hash::new_from_array([1; 32]), 100 + read as u64))
    }

    async fn block_height(&self) -> anyhow::Result<u64> {
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
async fn blockhash_reads_within_the_interval_cost_one_rpc_call() {
    let chain = Arc::new(Counting::default());
    let core = core(&chain);

    for _ in 0..50 {
        assert_eq!(core.latest_blockhash().await.unwrap().1, 100);
        assert_eq!(core.block_height().await.unwrap(), 0);
        tokio::time::advance(Duration::from_millis(80)).await;
    }
    assert_eq!(chain.blockhash_reads.load(Ordering::SeqCst), 1);
    assert_eq!(chain.block_height_reads.load(Ordering::SeqCst), 1);

    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(core.latest_blockhash().await.unwrap().1, 101);
    assert_eq!(core.block_height().await.unwrap(), 1);
    assert_eq!(chain.blockhash_reads.load(Ordering::SeqCst), 2);
    assert_eq!(chain.block_height_reads.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn a_failed_refresh_is_an_error_and_the_next_call_retries() {
    let chain = Arc::new(Counting::default());
    let core = core(&chain);
    chain.fails.store(true, Ordering::SeqCst);

    assert!(core.latest_blockhash().await.is_err());
    assert!(core.block_height().await.is_err());
    chain.fails.store(false, Ordering::SeqCst);

    assert_eq!(core.latest_blockhash().await.unwrap().1, 101);
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
                .send_transaction(execution, &transaction(signature), 1)
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
        .send_transaction(execution, &transaction(signature), 1)
        .await;

    assert_eq!(
        error.as_deref(),
        Some("error sending request for url (https://rpc.example.com/<redacted>)")
    );
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert_eq!(*chain.statuses_seen.lock().unwrap(), vec![signature]);
}
