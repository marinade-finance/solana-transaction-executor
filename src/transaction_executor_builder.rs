use async_trait::async_trait;
use base64::{prelude::BASE64_STANDARD, Engine};
use log::{info, warn};
use serde_json::{json, Value};
use solana_client::{
    nonblocking::rpc_client::RpcClient,
    rpc_client::SerializableTransaction,
    rpc_config::{RpcSendTransactionConfig, RpcSimulateTransactionConfig},
    rpc_response::{RpcResult, RpcSimulateTransactionResult},
};
use solana_commitment_config::CommitmentConfig;
use solana_hash::Hash;
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction_status_client_types::TransactionStatus;
use std::sync::Arc;

use crate::{TipPolicy, TransactionExecutor, TransactionExecutorConfig};

#[derive(Default)]
pub struct TransactionExecutorBuilder {
    transaction_executor_config: TransactionExecutorConfig,
    signature_statuses_provider: Option<Arc<dyn SignatureStatusesProvider + Send + Sync>>,
    simulate_transaction_provider: Option<Arc<dyn SimulateTransactionProvider + Send + Sync>>,
    send_transaction_provider: Option<Arc<dyn SendTransactionProvider + Send + Sync>>,
    prioritization_fees_provider: Option<Arc<dyn PrioritizationFeesProvider + Send + Sync>>,
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
            .with_prioritization_fees_provider(DefaultProvider::new(rpc_client.clone()))
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

    pub fn with_prioritization_fees_provider(
        mut self,
        provider: impl PrioritizationFeesProvider + Send + Sync + 'static,
    ) -> Self {
        self.prioritization_fees_provider = Some(Arc::new(provider));
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
        let prioritization_fees_provider = self
            .prioritization_fees_provider
            .expect("Prioritization fees provider not set");

        TransactionExecutor::new(
            self.transaction_executor_config,
            signature_statuses_provider,
            simulate_transaction_provider,
            send_transaction_provider,
            prioritization_fees_provider,
        )
    }

    pub fn initialize(
        rpc_client: Arc<RpcClient>,
        sender_rpc_url: Option<String>,
        tip_policy: Option<TipPolicy>,
    ) -> Self {
        if let Some(rpc_url) = sender_rpc_url {
            TransactionExecutorBuilder::new()
                .with_default_providers(rpc_client)
                .with_send_transaction_provider(SendTransactionWithGrowingTipProvider {
                    rpc_url,
                    query_param: "tip".into(),
                    tip_policy: tip_policy.unwrap_or_default(),
                })
        } else {
            TransactionExecutorBuilder::new().with_default_providers(rpc_client.clone())
        }
    }
}

#[async_trait]
pub trait SignatureStatusesProvider {
    async fn get_signature_statuses(
        &self,
        signatures: &[Signature],
    ) -> RpcResult<Vec<Option<TransactionStatus>>>;
    /// Searches the ledger too, and keeps a status below the commitment.
    async fn get_signature_statuses_with_history(
        &self,
        signatures: &[Signature],
    ) -> RpcResult<Vec<Option<TransactionStatus>>>;
    async fn is_blockhash_valid(&self, blockhash: &Hash) -> anyhow::Result<bool>;
    async fn latest_blockhash(&self) -> anyhow::Result<Hash>;
    fn commitment(&self) -> CommitmentConfig;
}

#[async_trait]
pub trait SimulateTransactionProvider {
    async fn simulate_transaction(
        &self,
        transaction: &VersionedTransaction,
    ) -> RpcResult<RpcSimulateTransactionResult>;
    async fn simulate_unsigned_transaction(
        &self,
        transaction: &VersionedTransaction,
    ) -> RpcResult<RpcSimulateTransactionResult>;
}

#[async_trait]
pub trait PrioritizationFeesProvider {
    async fn recent_prioritization_fees(&self, writable: &[Pubkey]) -> anyhow::Result<Vec<u64>>;
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
                transaction_status.filter(|transaction_status| {
                    transaction_status.satisfies_commitment(self.rpc_client.commitment())
                })
            })
            .collect();

        Ok(result)
    }

    async fn is_blockhash_valid(&self, blockhash: &Hash) -> anyhow::Result<bool> {
        Ok(self
            .rpc_client
            .is_blockhash_valid(blockhash, self.rpc_client.commitment())
            .await?)
    }

    async fn latest_blockhash(&self) -> anyhow::Result<Hash> {
        Ok(self.rpc_client.get_latest_blockhash().await?)
    }

    async fn get_signature_statuses_with_history(
        &self,
        signatures: &[Signature],
    ) -> RpcResult<Vec<Option<TransactionStatus>>> {
        self.rpc_client
            .get_signature_statuses_with_history(signatures)
            .await
    }

    fn commitment(&self) -> CommitmentConfig {
        self.rpc_client.commitment()
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

    async fn simulate_unsigned_transaction(
        &self,
        transaction: &VersionedTransaction,
    ) -> RpcResult<RpcSimulateTransactionResult> {
        self.rpc_client
            .simulate_transaction_with_config(
                transaction,
                RpcSimulateTransactionConfig {
                    sig_verify: false,
                    replace_recent_blockhash: true,
                    ..Default::default()
                },
            )
            .await
    }
}

#[async_trait]
impl PrioritizationFeesProvider for DefaultProvider {
    async fn recent_prioritization_fees(&self, writable: &[Pubkey]) -> anyhow::Result<Vec<u64>> {
        Ok(self
            .rpc_client
            .get_recent_prioritization_fees(writable)
            .await?
            .into_iter()
            .map(|fee| fee.prioritization_fee)
            .collect())
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
                    encoding: Some(solana_client::rpc_config::UiTransactionEncoding::Base64),
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
                    wire_base64(transaction)?,
                    {
                        "encoding": "base64",
                        "skipPreflight": true,
                    }
                ]
            }))
            .send()
            .await?;

        let status = response.status();
        let body = response.text().await?;
        let signature = *transaction.get_signature();

        if !status.is_success() {
            anyhow::bail!(
                "Sender RPC refused {signature} sent with tip {tip}: HTTP {status}, body: {body}"
            );
        }

        let rpc_response: Value = serde_json::from_str(&body).map_err(|err| {
            anyhow::anyhow!(
                "Sender RPC response for {signature} is not a JSON: {err}, body: {body}"
            )
        })?;
        if let Some(error) = rpc_response.get("error").filter(|error| !error.is_null()) {
            anyhow::bail!("Sender RPC refused {signature} sent with tip {tip}: {error}");
        }
        // Warn, not bail: the relay's success envelope is unverified and a hard reject would fail every send.
        if rpc_response
            .get("result")
            .filter(|result| !result.is_null())
            .is_none()
        {
            warn!("Sender RPC response for {signature} has neither result nor error");
        }

        info!("Transaction {signature} sent with tip {tip}, rpc response: {status} {body}");

        Ok(signature)
    }
}

fn wire_base64(transaction: &VersionedTransaction) -> anyhow::Result<String> {
    Ok(BASE64_STANDARD.encode(wincode::serialize(transaction)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_keypair::Keypair;
    use solana_message::{v1, VersionedMessage};
    use solana_signer::Signer;

    #[test]
    fn a_v1_transaction_is_sent_in_its_wire_format() {
        let payer = Keypair::new();
        let message = v1::Message::try_compile_with_config(
            &payer.pubkey(),
            &[],
            Hash::new_unique(),
            v1::TransactionConfig::empty(),
        )
        .unwrap();
        let transaction =
            VersionedTransaction::try_new(VersionedMessage::V1(message), &[&payer]).unwrap();

        let wire = BASE64_STANDARD
            .decode(wire_base64(&transaction).unwrap())
            .unwrap();

        assert_eq!(wire[0], v1::V1_PREFIX);
        assert_eq!(
            wincode::deserialize::<VersionedTransaction>(&wire).unwrap(),
            transaction
        );
    }
}
