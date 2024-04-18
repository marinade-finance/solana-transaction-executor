use async_trait::async_trait;
use log::info;
use serde_json::json;
use solana_client::{
    nonblocking::rpc_client::RpcClient,
    rpc_client::SerializableTransaction,
    rpc_config::RpcSendTransactionConfig,
    rpc_response::{RpcResult, RpcSimulateTransactionResult},
};
use solana_sdk::{signature::Signature, transaction::VersionedTransaction};
use solana_transaction_status::TransactionStatus;
use std::sync::Arc;

use crate::{TipPolicy, TransactionExecutor, TransactionExecutorConfig};

#[derive(Default)]
pub struct TransactionExecutorBuilder {
    transaction_executor_config: TransactionExecutorConfig,
    signature_statuses_provider: Option<Arc<dyn SignatureStatusesProvider + Send + Sync>>,
    simulate_transaction_provider: Option<Arc<dyn SimulateTransactionProvider + Send + Sync>>,
    send_transaction_provider: Option<Arc<dyn SendTransactionProvider + Send + Sync>>,
}

impl TransactionExecutorBuilder {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn with_transaction_executor_config(
        mut self,
        transaction_executor_config: TransactionExecutorConfig,
    ) -> Self {
        self.transaction_executor_config = transaction_executor_config;
        self
    }

    pub fn with_default_providers(self, rpc_client: Arc<RpcClient>) -> Self {
        self.with_signature_statuses_provider(DefaultProvider::new(rpc_client.clone()))
            .with_simulate_transaction_provider(DefaultProvider::new(rpc_client.clone()))
            .with_send_transaction_provider(DefaultProvider::new(rpc_client.clone()))
    }

    pub fn with_signature_statuses_provider(
        mut self,
        provider: impl SignatureStatusesProvider + Send + Sync + 'static,
    ) -> Self {
        self.signature_statuses_provider = Some(Arc::new(provider));
        self
    }

    pub fn with_simulate_transaction_provider(
        mut self,
        provider: impl SimulateTransactionProvider + Send + Sync + 'static,
    ) -> Self {
        self.simulate_transaction_provider = Some(Arc::new(provider));
        self
    }

    pub fn with_send_transaction_provider(
        mut self,
        provider: impl SendTransactionProvider + Send + Sync + 'static,
    ) -> Self {
        self.send_transaction_provider = Some(Arc::new(provider));
        self
    }

    pub fn build(self) -> TransactionExecutor {
        let signature_statuses_provider = self
            .signature_statuses_provider
            .expect("Signature statuses provider not set");
        let simulate_transaction_provider = self
            .simulate_transaction_provider
            .expect("Simulate transaction provider not set");
        let send_transaction_provider = self
            .send_transaction_provider
            .expect("Send transaction provider not set");

        TransactionExecutor::new(
            self.transaction_executor_config,
            signature_statuses_provider,
            simulate_transaction_provider,
            send_transaction_provider,
        )
    }
}

#[async_trait]
pub trait SignatureStatusesProvider {
    async fn get_signature_statuses(
        &self,
        signatures: &[Signature],
    ) -> RpcResult<Vec<Option<TransactionStatus>>>;
}

#[async_trait]
pub trait SimulateTransactionProvider {
    async fn simulate_transaction(
        &self,
        transaction: &VersionedTransaction,
    ) -> RpcResult<RpcSimulateTransactionResult>;
}

pub struct SendTransactionMeta {
    pub attempt: u32,
}

#[async_trait]
pub trait SendTransactionProvider {
    async fn send_transaction(
        &self,
        transaction: &VersionedTransaction,
        meta: SendTransactionMeta,
    ) -> anyhow::Result<Signature>;
}

pub struct DefaultProvider {
    rpc_client: Arc<RpcClient>,
}

impl DefaultProvider {
    pub fn new(rpc_client: Arc<RpcClient>) -> Self {
        Self { rpc_client }
    }
}

#[async_trait]
impl SignatureStatusesProvider for DefaultProvider {
    async fn get_signature_statuses(
        &self,
        signatures: &[Signature],
    ) -> RpcResult<Vec<Option<TransactionStatus>>> {
        let mut result = self.rpc_client.get_signature_statuses(signatures).await?;

        result.value = result
            .value
            .into_iter()
            .map(|transaction_status| {
                transaction_status.and_then(|transaction_status| {
                    if transaction_status.satisfies_commitment(self.rpc_client.commitment()) {
                        Some(transaction_status)
                    } else {
                        None
                    }
                })
            })
            .collect();

        Ok(result)
    }
}

#[async_trait]
impl SimulateTransactionProvider for DefaultProvider {
    async fn simulate_transaction(
        &self,
        transaction: &VersionedTransaction,
    ) -> RpcResult<RpcSimulateTransactionResult> {
        self.rpc_client.simulate_transaction(transaction).await
    }
}

#[async_trait]
impl SendTransactionProvider for DefaultProvider {
    async fn send_transaction(
        &self,
        transaction: &VersionedTransaction,
        _meta: SendTransactionMeta,
    ) -> anyhow::Result<Signature> {
        Ok(self
            .rpc_client
            .send_transaction_with_config(
                transaction,
                RpcSendTransactionConfig {
                    skip_preflight: true,
                    encoding: Some(solana_transaction_status::UiTransactionEncoding::Base64),
                    ..Default::default()
                },
            )
            .await?)
    }
}

pub struct SendTransactionWithGrowingTipProvider {
    pub rpc_url: String,
    pub tip_policy: TipPolicy,
    pub query_param: String,
}

impl Default for SendTransactionWithGrowingTipProvider {
    fn default() -> Self {
        Self {
            rpc_url: Default::default(),
            tip_policy: Default::default(),
            query_param: "tip".into(),
        }
    }
}

#[async_trait]
impl SendTransactionProvider for SendTransactionWithGrowingTipProvider {
    async fn send_transaction(
        &self,
        transaction: &VersionedTransaction,
        meta: SendTransactionMeta,
    ) -> anyhow::Result<Signature> {
        let tip = self.tip_policy.get_tip_configuration(meta.attempt).tip;

        let response = reqwest::Client::new()
            .post(self.rpc_url.clone())
            .query(&[(&self.query_param, tip)])
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "sendTransaction",
                "params": [
                    base64::encode(bincode::serialize(transaction)?),
                    {
                        "encoding": "base64",
                        "skipPreflight": true,
                    }
                ]
            }))
            .send()
            .await?;

        let status = response.status();
        let body = response.text().await;

        info!("Transaction sent with tip {tip}, rpc response: {status} {body:?}");

        Ok(*transaction.get_signature())
    }
}
