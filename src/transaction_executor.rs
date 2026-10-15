use futures_core::stream::Stream;
use futures_util::stream::StreamExt;
use solana_client::rpc_client::SerializableTransaction;
use solana_commitment_config::CommitmentConfig;
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction_error::TransactionError;
use solana_transaction_status_client_types::TransactionStatus;
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::{
    sync::RwLock,
    time::{sleep, Instant},
};

use crate::{
    redact::redact_urls, Attempt, Execution, NotLanded, Outcome, PrioritizationFeesProvider,
    SendTransactionMeta, SendTransactionProvider, SignatureStatusesProvider,
    SimulateTransactionProvider, TransactionExecutorConfig,
};

pub type TransactionBuilderResult =
    Pin<Box<dyn Future<Output = anyhow::Result<VersionedTransaction>> + Send>>;

pub struct AttemptTransaction {
    pub transaction: VersionedTransaction,
    pub last_valid_block_height: u64,
}

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
    Expired,
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
        transaction_builder: impl Stream<Item = anyhow::Result<AttemptTransaction>>,
    ) -> Execution {
        tokio::pin!(transaction_builder);
        let signatures = Arc::new(RwLock::new(Vec::<Signature>::new()));
        let (sender_transaction_landed, mut receiver_transaction_landed) =
            tokio::sync::mpsc::unbounded_channel::<Landing>();

        self.spawn_any_signature_watcher(signatures.clone(), sender_transaction_landed);

        let mut attempts = Vec::<Attempt>::new();
        let mut stopped: Option<NotLanded> = None;
        let mut last_sent_at = Instant::now();
        while let Some(transaction_build_result) = transaction_builder.next().await {
            let attempt = u32::try_from(attempts.len()).unwrap_or(u32::MAX) + 1;
            if attempt > self.transaction_executor_config.max_attempts {
                log::info!("Maximum attempts reached");
                break;
            }

            let AttemptTransaction {
                transaction,
                last_valid_block_height,
            } = match transaction_build_result {
                Ok(built) => built,
                Err(err) => {
                    stopped = Some(NotLanded::NotSent {
                        error: redact_urls(&format!("Failed to build attempt #{attempt}: {err}")),
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
                last_valid_block_height,
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
                        error: redact_urls(&format!("Failed to simulate {signature}: {err}")),
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
                        last_sent_at = Instant::now();
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
                record.send_error = Some(redact_urls(&err.to_string()));
            }
            record.sent = true;
            attempts.push(record);
            signatures.write().await.push(signature);
            last_sent_at = Instant::now();

            // Hold retry until previous attempt's blockhash is invalid, see "Orphaned Revoke Tokens"
            let sent = signatures.read().await.clone();
            let last = &attempts[attempts.len() - 1];
            match self
                .wait_for_outcome(
                    &sent,
                    last,
                    last_sent_at,
                    self.transaction_executor_config
                        .wait_for_blockhash_expiry_before_retry,
                    self.transaction_executor_config.transaction_send_interval,
                    &mut receiver_transaction_landed,
                )
                .await
            {
                Waited::Landed(landing) => {
                    return Execution {
                        outcome: landing.into(),
                        attempts,
                    }
                }
                Waited::Expired => {}
                Waited::Unknown(reason) => {
                    return Execution {
                        outcome: Outcome::Unknown { reason },
                        attempts,
                    }
                }
            }
        }

        let sent = signatures.read().await.clone();
        let outcome = self
            .settle(
                &attempts,
                &sent,
                stopped,
                last_sent_at,
                &mut receiver_transaction_landed,
            )
            .await;
        Execution { outcome, attempts }
    }

    async fn settle(
        &self,
        attempts: &[Attempt],
        sent: &[Signature],
        stopped: Option<NotLanded>,
        last_sent_at: Instant,
        receiver_transaction_landed: &mut UnboundedReceiver<Landing>,
    ) -> Outcome {
        let not_landed = stopped.unwrap_or(NotLanded::Expired);
        let Some(last) = attempts.iter().rev().find(|attempt| attempt.sent) else {
            return Outcome::NotLanded(match not_landed {
                NotLanded::Expired => NotLanded::NotSent {
                    error: "No attempt was sent".into(),
                },
                reason => reason,
            });
        };

        match self
            .wait_for_outcome(
                sent,
                last,
                last_sent_at,
                true,
                self.transaction_executor_config
                    .signature_status_check_interval,
                receiver_transaction_landed,
            )
            .await
        {
            Waited::Landed(landing) => landing.into(),
            Waited::Expired => Outcome::NotLanded(not_landed),
            Waited::Unknown(reason) => Outcome::Unknown { reason },
        }
    }

    /// Ends on a landing, or once `last` expired and the ledger knows none of `sent`.
    async fn wait_for_outcome(
        &self,
        sent: &[Signature],
        last: &Attempt,
        sent_at: Instant,
        hold_until_expiry: bool,
        interval: Duration,
        receiver_transaction_landed: &mut UnboundedReceiver<Landing>,
    ) -> Waited {
        let config = &self.transaction_executor_config;
        let bound = config.waiting_time_after_last_send;
        let mut block_height_failures: u32 = 0;
        loop {
            tokio::select! {
                landing = receiver_transaction_landed.recv() => match landing {
                    Some(landing) => return Waited::Landed(landing),
                    None => return Waited::Unknown("The status watcher stopped".into()),
                },
                _ = sleep(interval) => {}
            }
            if hold_until_expiry {
                match self.signature_statuses_provider.block_height().await {
                    Ok(height) if height <= last.last_valid_block_height => {
                        block_height_failures = 0;
                        if sent_at.elapsed() >= bound {
                            return Waited::Unknown(format!(
                                "The blockhash of {} is still valid {bound:?} after its send",
                                last.signature
                            ));
                        }
                        log::info!(
                            "Attempt {} not confirmed yet but its blockhash is still valid; holding to avoid a duplicate landing",
                            last.signature
                        );
                        continue;
                    }
                    Ok(_) => log::info!("The blockhash of {} expired", last.signature),
                    Err(err) => {
                        block_height_failures += 1;
                        let budget = config.max_consecutive_blockhash_check_failures;
                        if block_height_failures >= budget {
                            return Waited::Unknown(redact_urls(&format!(
                                "The block height read failed {block_height_failures} consecutive times for {}: {err}",
                                last.signature
                            )));
                        }
                        log::warn!(
                            "The block height read failed for {} ({block_height_failures}/{budget}): {err}; holding rather than rebuilding to be safe",
                            last.signature
                        );
                        continue;
                    }
                }
            }

            let statuses = match self
                .signature_statuses_provider
                .get_signature_statuses_with_history(sent)
                .await
            {
                Ok(response) => response.value,
                Err(err) => {
                    return Waited::Unknown(redact_urls(&format!(
                        "The history of the sent attempts cannot be read: {err}"
                    )))
                }
            };
            let commitment = self.signature_statuses_provider.commitment();
            let Some((status, signature)) = best_status(statuses, sent, commitment) else {
                return Waited::Expired;
            };
            if status.satisfies_commitment(commitment) {
                return Waited::Landed(Landing {
                    signature,
                    slot: status.slot,
                    result: status.status,
                });
            }
            if sent_at.elapsed() >= bound {
                return Waited::Unknown(format!(
                    "{signature} landed in the slot {} but has not reached {:?} within {bound:?}",
                    status.slot, commitment.commitment
                ));
            }
        }
    }
}

/// A status at the commitment if any has one, else the first status found.
pub(crate) fn best_status(
    statuses: Vec<Option<TransactionStatus>>,
    sent: &[Signature],
    commitment: CommitmentConfig,
) -> Option<(TransactionStatus, Signature)> {
    let found: Vec<(TransactionStatus, Signature)> = statuses
        .into_iter()
        .zip(sent.iter().copied())
        .filter_map(|(status, signature)| Some((status?, signature)))
        .collect();
    let at_commitment = found
        .iter()
        .position(|(status, _)| status.satisfies_commitment(commitment));
    found.into_iter().nth(at_commitment.unwrap_or(0))
}
