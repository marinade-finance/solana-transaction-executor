use anyhow::anyhow;
use futures_core::stream::Stream;
use futures_core::{self};
use futures_util::stream::StreamExt;
use solana_client::rpc_client::SerializableTransaction;
use solana_sdk::{
    signature::Signature,
    transaction::{TransactionError, VersionedTransaction},
};
use solana_transaction_status::TransactionStatus;
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};
use tokio::sync::mpsc::UnboundedSender;
use tokio::{sync::RwLock, time::sleep};

use crate::{
    SendTransactionMeta, SendTransactionProvider, SignatureStatusesProvider,
    SimulateTransactionProvider, TransactionExecutorConfig,
};

pub type TransactionBuilderResult =
    Pin<Box<dyn Future<Output = anyhow::Result<VersionedTransaction>> + Send>>;

pub struct RetryPolicy {}

impl RetryPolicy {}

pub struct TransactionExecutor {
    pub transaction_executor_config: TransactionExecutorConfig,
    pub signature_statuses_provider: Arc<dyn SignatureStatusesProvider + Send + Sync>,
    pub simulate_transaction_provider: Arc<dyn SimulateTransactionProvider + Send + Sync>,
    pub send_transaction_provider: Arc<dyn SendTransactionProvider + Send + Sync>,
}

impl TransactionExecutor {
    pub fn new(
        transaction_executor_config: TransactionExecutorConfig,
        signature_statuses_provider: Arc<dyn SignatureStatusesProvider + Send + Sync>,
        simulate_transaction_provider: Arc<dyn SimulateTransactionProvider + Send + Sync>,
        send_transaction_provider: Arc<dyn SendTransactionProvider + Send + Sync>,
    ) -> Self {
        Self {
            transaction_executor_config,
            signature_statuses_provider,
            simulate_transaction_provider,
            send_transaction_provider,
        }
    }

    fn spawn_any_signature_watcher(
        &self,
        signatures: Arc<RwLock<Vec<Signature>>>,
        sender_transaction_exectuted: UnboundedSender<Signature>,
    ) {
        let signature_statuses_provider = self.signature_statuses_provider.clone();
        tokio::spawn(async move {
            while !sender_transaction_exectuted.is_closed() {
                sleep(Duration::from_secs(1)).await;

                let mut failed_signatures = vec![];
                let pending_signatures = signatures.read().await.clone();
                let statuses = match signature_statuses_provider
                    .get_signature_statuses(&pending_signatures)
                    .await
                {
                    Ok(result) => result.value,
                    Err(err) => {
                        log::error!("Failed to get transaction signatures statuses: {err}");
                        continue;
                    }
                };

                for (
                    TransactionStatus {
                        slot,
                        status,
                        confirmation_status,
                        ..
                    },
                    signature,
                ) in statuses
                    .into_iter()
                    .zip(pending_signatures)
                    .filter_map(|(status, signature)| status.map(|status| (status, signature)))
                {
                    match status {
                        Ok(_) => {
                            log::info!("Transaction executed: {signature} in the slot {slot}, confirmation status {confirmation_status:?}");
                            if let Err(err) = sender_transaction_exectuted.send(signature) {
                                log::warn!("Channel closed {signature}: {err}");
                            }
                            return;
                        }
                        Err(err) => {
                            log::error!("Transaction {signature} error: {err}");
                            failed_signatures.push(signature);
                        }
                    }
                }

                signatures
                    .write()
                    .await
                    .retain(|pending_signature| !failed_signatures.contains(pending_signature));
            }
        });
    }

    pub async fn execute_transaction(
        &self,
        transaction_builder: impl Stream<Item = anyhow::Result<VersionedTransaction>>,
    ) -> anyhow::Result<Signature> {
        tokio::pin!(transaction_builder);
        let signatures = Arc::new(RwLock::new(Vec::<Signature>::new()));
        let (sender_transaction_exectuted, mut receiver_transaction_executed) =
            tokio::sync::mpsc::unbounded_channel::<Signature>();

        self.spawn_any_signature_watcher(signatures.clone(), sender_transaction_exectuted);

        let mut attempt = 0;
        while let Some(transaction_build_result) = transaction_builder.next().await {
            attempt += 1;
            if attempt > self.transaction_executor_config.max_attempts {
                log::info!("Maximum attempts reached");
                break;
            }

            let transaction = match transaction_build_result {
                Ok(transaction) => transaction,
                Err(err) => anyhow::bail!("Failed to build transaction: {err}"),
            };
            let attempt_signature = transaction.get_signature();
            log::info!("Execution attempt #{attempt} {attempt_signature}");

            let simulation_response = self
                .simulate_transaction_provider
                .simulate_transaction(&transaction)
                .await
                .map_err(|err| {
                    anyhow!("Failed to get simulation result for {attempt_signature}: {err}")
                })?
                .value;

            if let Some(err) = simulation_response.err {
                match err {
                    TransactionError::AlreadyProcessed => {
                        log::info!("Transaction {attempt_signature} has already been processed!");
                        return Ok(*attempt_signature);
                    }
                    TransactionError::BlockhashNotFound => {
                        log::info!("Simulation of {attempt_signature} failed because blockhash was not found");
                        continue;
                    }
                    _ => {
                        if signatures.read().await.is_empty() {
                            anyhow::bail!(
                                "Simulation of the first transaction attempt {attempt_signature} failed: {err}, logs: {:?}",
                                simulation_response.logs
                            );
                        } else {
                            log::error!(
                                "Simulation of a transaction attempt {attempt_signature} failed: {err}, logs: {:?}",
                                simulation_response.logs
                            );
                            break;
                        }
                    }
                };
            } else {
                if let Err(err) = self
                    .send_transaction_provider
                    .send_transaction(&transaction, SendTransactionMeta { attempt })
                    .await
                {
                    log::error!(
                        "RPC error while sending the transaction {attempt_signature}: {err}"
                    );
                }
                signatures.write().await.push(*transaction.get_signature());
            }

            tokio::select! {
                executed_signature = receiver_transaction_executed.recv() => {
                    if let Some(executed_signature) = executed_signature {
                        log::info!("Transaction {executed_signature} successfully executed!");
                        return Ok(executed_signature)
                    } else {
                        log::info!("No transaction has been verified");
                    }
                }
                _ = sleep(self.transaction_executor_config.transaction_send_interval) => {
                    log::info!("Attempt {attempt_signature} timeout");
                    continue
                }
            }
        }

        log::info!("No additional transaction retries planned");

        if signatures.read().await.is_empty() {
            anyhow::bail!("No transaction execution attempt has been made");
        }

        tokio::select! {
            executed_signature = receiver_transaction_executed.recv() => {
                if let Some(executed_signature) = executed_signature {
                    log::info!("Transaction {executed_signature} successfully executed!");
                    Ok(executed_signature)
                } else {
                    anyhow::bail!("No transaction executed")
                }
            }
            _ = sleep(self.transaction_executor_config.waiting_time_after_last_send) => anyhow::bail!("No transaction executed up until now, timeouting")
        }
    }
}
