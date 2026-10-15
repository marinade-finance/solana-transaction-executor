use futures_core::stream::Stream;
use futures_util::stream::StreamExt;
use solana_client::rpc_client::SerializableTransaction;
use solana_hash::Hash;
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction_error::TransactionError;
use solana_transaction_status_client_types::TransactionStatus;
use std::{future::Future, pin::Pin, sync::Arc};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::{
    sync::RwLock,
    time::{sleep, Instant},
};

use crate::{
    Attempt, Execution, NotLanded, Outcome, PrioritizationFeesProvider, SendTransactionMeta,
    SendTransactionProvider, SignatureStatusesProvider, SimulateTransactionProvider,
    TransactionExecutorConfig,
};

pub type TransactionBuilderResult =
    Pin<Box<dyn Future<Output = anyhow::Result<VersionedTransaction>> + Send>>;

pub struct RetryPolicy {}

impl RetryPolicy {}

struct Landing {
    signature: Signature,
    slot: u64,
    result: Result<(), TransactionError>,
}

impl From<Landing> for Outcome {
    fn from(landing: Landing) -> Self {
        match landing.result {
            Ok(()) => Outcome::Landed {
                signature: landing.signature,
                slot: landing.slot,
            },
            Err(error) => Outcome::LandedWithError {
                signature: landing.signature,
                slot: landing.slot,
                error,
            },
        }
    }
}

enum Waited {
    Landed(Landing),
    NextAttempt,
    Unknown(String),
}

pub struct TransactionExecutor {
    pub transaction_executor_config: TransactionExecutorConfig,
    pub signature_statuses_provider: Arc<dyn SignatureStatusesProvider + Send + Sync>,
    pub simulate_transaction_provider: Arc<dyn SimulateTransactionProvider + Send + Sync>,
    pub send_transaction_provider: Arc<dyn SendTransactionProvider + Send + Sync>,
    pub prioritization_fees_provider: Arc<dyn PrioritizationFeesProvider + Send + Sync>,
}

impl TransactionExecutor {
    pub fn new(
        transaction_executor_config: TransactionExecutorConfig,
        signature_statuses_provider: Arc<dyn SignatureStatusesProvider + Send + Sync>,
        simulate_transaction_provider: Arc<dyn SimulateTransactionProvider + Send + Sync>,
        send_transaction_provider: Arc<dyn SendTransactionProvider + Send + Sync>,
        prioritization_fees_provider: Arc<dyn PrioritizationFeesProvider + Send + Sync>,
    ) -> Self {
        Self {
            transaction_executor_config,
            signature_statuses_provider,
            simulate_transaction_provider,
            send_transaction_provider,
            prioritization_fees_provider,
        }
    }

    fn spawn_any_signature_watcher(
        &self,
        signatures: Arc<RwLock<Vec<Signature>>>,
        sender_transaction_landed: UnboundedSender<Landing>,
    ) {
        let signature_statuses_provider = self.signature_statuses_provider.clone();
        let interval = self
            .transaction_executor_config
            .signature_status_check_interval;
        tokio::spawn(async move {
            while !sender_transaction_landed.is_closed() {
                sleep(interval).await;

                let pending_signatures = signatures.read().await.clone();
                if pending_signatures.is_empty() {
                    continue;
                }
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

                if let Some((TransactionStatus { slot, status, .. }, signature)) = statuses
                    .into_iter()
                    .zip(pending_signatures)
                    .find_map(|(status, signature)| status.map(|status| (status, signature)))
                {
                    match &status {
                        Ok(()) => log::info!("Transaction {signature} landed in the slot {slot}"),
                        Err(err) => {
                            log::error!("Transaction {signature} landed in the slot {slot} and failed: {err}")
                        }
                    }
                    let landing = Landing {
                        signature,
                        slot,
                        result: status,
                    };
                    if sender_transaction_landed.send(landing).is_err() {
                        log::warn!("Channel closed before {signature} was reported");
                    }
                    return;
                }
            }
        });
    }

    /// Ends only on a landing or with proof of none, unless the bound passes or the RPC fails.
    pub async fn execute_transaction(
        &self,
        transaction_builder: impl Stream<Item = anyhow::Result<VersionedTransaction>>,
    ) -> Execution {
        tokio::pin!(transaction_builder);
        let signatures = Arc::new(RwLock::new(Vec::<Signature>::new()));
        let (sender_transaction_landed, mut receiver_transaction_landed) =
            tokio::sync::mpsc::unbounded_channel::<Landing>();

        self.spawn_any_signature_watcher(signatures.clone(), sender_transaction_landed);

        let mut attempts = Vec::<Attempt>::new();
        let mut stopped: Option<NotLanded> = None;
        while let Some(transaction_build_result) = transaction_builder.next().await {
            let attempt = u32::try_from(attempts.len()).unwrap_or(u32::MAX) + 1;
            if attempt > self.transaction_executor_config.max_attempts {
                log::info!("Maximum attempts reached");
                break;
            }

            let transaction = match transaction_build_result {
                Ok(transaction) => transaction,
                Err(err) => {
                    stopped = Some(NotLanded::NotSent {
                        error: format!("Failed to build attempt #{attempt}: {err}"),
                    });
                    break;
                }
            };
            let signature = *transaction.get_signature();
            let blockhash = *transaction.message.recent_blockhash();
            log::info!("Execution attempt #{attempt} {signature}");
            let mut record = Attempt {
                signature,
                blockhash,
                sent: false,
                send_error: None,
                refused: None,
            };

            let simulation = match self
                .simulate_transaction_provider
                .simulate_transaction(&transaction)
                .await
            {
                Ok(response) => response.value,
                Err(err) => {
                    attempts.push(record);
                    stopped = Some(NotLanded::NotSent {
                        error: format!("Failed to simulate {signature}: {err}"),
                    });
                    break;
                }
            };

            if let Some(err) = simulation.err {
                match TransactionError::from(err) {
                    TransactionError::AlreadyProcessed => {
                        log::info!("Transaction {signature} has already been processed");
                        record.sent = true;
                        attempts.push(record);
                        signatures.write().await.push(signature);
                        break;
                    }
                    TransactionError::BlockhashNotFound => {
                        log::info!(
                            "Simulation of {signature} failed because blockhash was not found"
                        );
                        attempts.push(record);
                        continue;
                    }
                    error => {
                        log::error!(
                            "Simulation of attempt {signature} refused it: {error}, logs: {:?}",
                            simulation.logs
                        );
                        record.refused = Some(error.clone());
                        attempts.push(record);
                        stopped = Some(NotLanded::Refused {
                            error,
                            logs: simulation.logs.unwrap_or_default(),
                        });
                        break;
                    }
                }
            }

            if let Err(err) = self
                .send_transaction_provider
                .send_transaction(&transaction, SendTransactionMeta { attempt })
                .await
            {
                log::error!("RPC error while sending the transaction {signature}: {err}");
                record.send_error = Some(err.to_string());
            }
            record.sent = true;
            attempts.push(record);
            signatures.write().await.push(signature);

            // Hold retry until previous attempt's blockhash is invalid, see "Orphaned Revoke Tokens"
            match self
                .wait_for_attempt_outcome(&blockhash, &signature, &mut receiver_transaction_landed)
                .await
            {
                Waited::Landed(landing) => {
                    return Execution {
                        outcome: landing.into(),
                        attempts,
                    }
                }
                Waited::NextAttempt => {}
                Waited::Unknown(reason) => {
                    return Execution {
                        outcome: Outcome::Unknown { reason },
                        attempts,
                    }
                }
            }
        }

        let outcome = self
            .settle(&attempts, stopped, &mut receiver_transaction_landed)
            .await;
        Execution { outcome, attempts }
    }

    async fn settle(
        &self,
        attempts: &[Attempt],
        stopped: Option<NotLanded>,
        receiver_transaction_landed: &mut UnboundedReceiver<Landing>,
    ) -> Outcome {
        let not_landed = stopped.unwrap_or(NotLanded::Expired);
        let sent: Vec<Signature> = attempts
            .iter()
            .filter(|attempt| attempt.sent)
            .map(|attempt| attempt.signature)
            .collect();
        let Some(last) = attempts.iter().rev().find(|attempt| attempt.sent) else {
            return Outcome::NotLanded(match not_landed {
                NotLanded::Expired => NotLanded::NotSent {
                    error: "No attempt was sent".into(),
                },
                reason => reason,
            });
        };

        let config = &self.transaction_executor_config;
        let bound = config.waiting_time_after_last_send;
        let deadline = Instant::now() + bound;
        let mut blockhash_check_failures: u32 = 0;
        loop {
            tokio::select! {
                landing = receiver_transaction_landed.recv() => {
                    if let Some(landing) = landing {
                        return landing.into();
                    }
                }
                _ = sleep(config.signature_status_check_interval) => {}
            }
            if Instant::now() >= deadline {
                return Outcome::Unknown {
                    reason: format!(
                        "The blockhash of {} is still valid {bound:?} after the last send",
                        last.signature
                    ),
                };
            }
            match self
                .signature_statuses_provider
                .is_blockhash_valid(&last.blockhash)
                .await
            {
                Ok(true) => blockhash_check_failures = 0,
                Ok(false) => break,
                Err(err) => {
                    blockhash_check_failures += 1;
                    if blockhash_check_failures >= config.max_consecutive_blockhash_check_failures {
                        return Outcome::Unknown {
                            reason: format!(
                                "is_blockhash_valid failed {blockhash_check_failures} consecutive times for {}: {err}",
                                last.signature
                            ),
                        };
                    }
                }
            }
        }

        let commitment = self.signature_statuses_provider.commitment();
        loop {
            let statuses = match self
                .signature_statuses_provider
                .get_signature_statuses_with_history(&sent)
                .await
            {
                Ok(response) => response.value,
                Err(err) => {
                    return Outcome::Unknown {
                        reason: format!("The history of the sent attempts cannot be read: {err}"),
                    }
                }
            };
            let Some((status, signature)) = statuses
                .into_iter()
                .zip(sent.iter())
                .find_map(|(status, signature)| status.map(|status| (status, *signature)))
            else {
                return Outcome::NotLanded(not_landed);
            };
            if status.satisfies_commitment(commitment) {
                return Landing {
                    signature,
                    slot: status.slot,
                    result: status.status,
                }
                .into();
            }
            if Instant::now() >= deadline {
                return Outcome::Unknown {
                    reason: format!(
                        "{signature} landed in the slot {} but has not reached {:?} within {bound:?}",
                        status.slot, commitment.commitment
                    ),
                };
            }
            sleep(config.signature_status_check_interval).await;
        }
    }

    async fn wait_for_attempt_outcome(
        &self,
        attempt_blockhash: &Hash,
        attempt_signature: &Signature,
        receiver_transaction_landed: &mut UnboundedReceiver<Landing>,
    ) -> Waited {
        let mut blockhash_check_failures: u32 = 0;
        loop {
            tokio::select! {
                landing = receiver_transaction_landed.recv() => {
                    if let Some(landing) = landing {
                        return Waited::Landed(landing);
                    } else {
                        log::info!("No transaction has been verified");
                        return Waited::NextAttempt;
                    }
                }
                _ = sleep(self.transaction_executor_config.transaction_send_interval) => {
                    if !self.transaction_executor_config.wait_for_blockhash_expiry_before_retry {
                        log::info!("Attempt {attempt_signature} timeout (race-unsafe mode, blockhash check disabled)");
                        return Waited::NextAttempt;
                    }
                    match self.signature_statuses_provider.is_blockhash_valid(attempt_blockhash).await {
                        Ok(true) => {
                            blockhash_check_failures = 0;
                            log::info!(
                                "Attempt {attempt_signature} not confirmed yet but blockhash still valid; holding before retry to avoid duplicate landing"
                            );
                        }
                        Ok(false) => {
                            log::info!("Attempt {attempt_signature} blockhash expired; safe to rebuild with a fresh blockhash");
                            return Waited::NextAttempt;
                        }
                        Err(err) => {
                            blockhash_check_failures += 1;
                            let budget = self.transaction_executor_config.max_consecutive_blockhash_check_failures;
                            if blockhash_check_failures >= budget {
                                return Waited::Unknown(format!(
                                    "is_blockhash_valid failed {blockhash_check_failures} consecutive times for {attempt_signature}: {err}"
                                ));
                            }
                            log::warn!(
                                "is_blockhash_valid check failed for {attempt_signature} ({blockhash_check_failures}/{budget}): {err}; holding rather than rebuilding to be safe"
                            );
                        }
                    }
                }
            }
        }
    }
}
