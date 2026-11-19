use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use solana_client::rpc_response::{Response, RpcResponseContext, RpcResult};
use solana_commitment_config::CommitmentConfig;
use solana_hash::Hash;
use solana_signature::Signature;
use solana_transaction_status_client_types::TransactionStatus;

use crate::{sender_core::SenderCore, SignatureStatusesProvider};

#[derive(Default)]
struct Counting {
    blockhash_reads: AtomicUsize,
    block_height_reads: AtomicUsize,
    fails: AtomicBool,
}

#[async_trait]
impl SignatureStatusesProvider for Counting {
    async fn get_signature_statuses(
        &self,
        signatures: &[Signature],
    ) -> RpcResult<Vec<Option<TransactionStatus>>> {
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
        signatures: &[Signature],
    ) -> RpcResult<Vec<Option<TransactionStatus>>> {
        self.get_signature_statuses(signatures).await
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
    SenderCore::new(chain.clone(), Duration::from_secs(5))
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
