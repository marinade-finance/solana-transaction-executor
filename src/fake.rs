use async_trait::async_trait;
use solana_client::rpc_response::{
    Response, RpcResponseContext, RpcResult, RpcSimulateTransactionResult,
};
use solana_hash::Hash;
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction_status_client_types::TransactionStatus;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use crate::{
    PrioritizationFeesProvider, SendTransactionMeta, SendTransactionProvider,
    SignatureStatusesProvider, SimulateTransactionProvider, TransactionExecutor,
    TransactionExecutorConfig,
};

pub(crate) struct Fake {
    simulation: Option<serde_json::Value>,
    fees: Result<Vec<u64>, String>,
    pub fee_calls: AtomicUsize,
}

impl Fake {
    pub fn simulating(simulation: serde_json::Value) -> Arc<Self> {
        Arc::new(Self {
            simulation: Some(simulation),
            fees: Ok(vec![]),
            fee_calls: AtomicUsize::new(0),
        })
    }

    pub fn with_fees(fees: Result<Vec<u64>, String>) -> Arc<Self> {
        Arc::new(Self {
            simulation: None,
            fees,
            fee_calls: AtomicUsize::new(0),
        })
    }

    pub fn executor(self: Arc<Self>) -> TransactionExecutor {
        TransactionExecutor::new(
            TransactionExecutorConfig::default(),
            self.clone(),
            self.clone(),
            self.clone(),
            self,
        )
    }
}

#[async_trait]
impl SimulateTransactionProvider for Fake {
    async fn simulate_transaction(
        &self,
        _transaction: &VersionedTransaction,
    ) -> RpcResult<RpcSimulateTransactionResult> {
        unreachable!("no test sends a signed attempt through the fake")
    }

    async fn simulate_unsigned_transaction(
        &self,
        _transaction: &VersionedTransaction,
    ) -> RpcResult<RpcSimulateTransactionResult> {
        let simulation = self.simulation.clone().expect("the fake has no simulation");
        Ok(Response {
            context: RpcResponseContext {
                slot: 1,
                api_version: None,
            },
            value: serde_json::from_value(simulation).unwrap(),
        })
    }
}

#[async_trait]
impl PrioritizationFeesProvider for Fake {
    async fn recent_prioritization_fees(&self, _writable: &[Pubkey]) -> anyhow::Result<Vec<u64>> {
        self.fee_calls.fetch_add(1, Ordering::SeqCst);
        self.fees.clone().map_err(anyhow::Error::msg)
    }
}

#[async_trait]
impl SignatureStatusesProvider for Fake {
    async fn get_signature_statuses(
        &self,
        _signatures: &[Signature],
    ) -> RpcResult<Vec<Option<TransactionStatus>>> {
        unreachable!("no test reads a status through the fake")
    }

    async fn is_blockhash_valid(&self, _blockhash: &Hash) -> anyhow::Result<bool> {
        unreachable!("no test reads a blockhash through the fake")
    }
}

#[async_trait]
impl SendTransactionProvider for Fake {
    async fn send_transaction(
        &self,
        _transaction: &VersionedTransaction,
        _meta: SendTransactionMeta,
    ) -> anyhow::Result<Signature> {
        unreachable!("no test sends through the fake")
    }
}
