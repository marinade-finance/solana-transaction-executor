use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use futures_util::stream;
use solana_client::{
    client_error::ClientErrorKind,
    rpc_response::{Response, RpcResponseContext, RpcResult, RpcSimulateTransactionResult},
};
use solana_commitment_config::CommitmentConfig;
use solana_hash::Hash;
use solana_instruction::error::InstructionError;
use solana_message::{v1, VersionedMessage};
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction_error::TransactionError;
use solana_transaction_status_client_types::{
    TransactionConfirmationStatus, TransactionStatus, UiTransactionError,
};

use crate::{
    transaction_executor::best_status, AttemptTransaction, Execution, NotLanded, Outcome,
    PrioritizationFeesProvider, SendTransactionMeta, SendTransactionProvider,
    SignatureStatusesProvider, SimulateTransactionProvider, TransactionExecutor,
    TransactionExecutorConfig,
};

/// What the chain answers per attempt, in order; an empty queue leaves the attempt unanswered.
#[derive(Default)]
struct Script {
    refusals: VecDeque<Option<TransactionError>>,
    simulation_errors: VecDeque<Option<String>>,
    landings: VecDeque<Option<Result<(), TransactionError>>>,
    block_heights: VecDeque<Result<u64, String>>,
    history: HashMap<Signature, TransactionStatus>,
    watcher_panics: bool,
}

#[derive(Default)]
struct Scripted {
    script: Mutex<Script>,
    sent: Mutex<Vec<Signature>>,
    landed: Mutex<HashMap<Signature, TransactionStatus>>,
    sends_at_block_height_reads: Mutex<Vec<usize>>,
}

fn response<T>(value: T) -> RpcResult<T> {
    Ok(Response {
        context: RpcResponseContext {
            slot: 1,
            api_version: None,
        },
        value,
    })
}

fn finalized(slot: u64, status: Result<(), TransactionError>) -> TransactionStatus {
    TransactionStatus {
        slot,
        confirmations: None,
        err: status.clone().err(),
        status,
        confirmation_status: Some(TransactionConfirmationStatus::Finalized),
    }
}

fn confirmed(slot: u64) -> TransactionStatus {
    TransactionStatus {
        slot,
        confirmations: Some(1),
        status: Ok(()),
        err: None,
        confirmation_status: Some(TransactionConfirmationStatus::Confirmed),
    }
}

fn custom(code: u32) -> TransactionError {
    TransactionError::InstructionError(0, InstructionError::Custom(code))
}

#[async_trait]
impl SimulateTransactionProvider for Scripted {
    async fn simulate_transaction(
        &self,
        _transaction: &VersionedTransaction,
    ) -> RpcResult<RpcSimulateTransactionResult> {
        let (refusal, error) = {
            let mut script = self.script.lock().unwrap();
            let error = script.simulation_errors.pop_front().flatten();
            (script.refusals.pop_front().flatten(), error)
        };
        if let Some(error) = error {
            return Err(ClientErrorKind::Custom(error).into());
        }
        response(RpcSimulateTransactionResult {
            err: refusal.map(UiTransactionError::from),
            logs: Some(vec!["Program log: scripted".into()]),
            accounts: None,
            units_consumed: None,
            loaded_accounts_data_size: None,
            return_data: None,
            inner_instructions: None,
            replacement_blockhash: None,
            fee: None,
            pre_balances: None,
            post_balances: None,
            pre_token_balances: None,
            post_token_balances: None,
            loaded_addresses: None,
        })
    }

    async fn simulate_unsigned_transaction(
        &self,
        transaction: &VersionedTransaction,
    ) -> RpcResult<RpcSimulateTransactionResult> {
        self.simulate_transaction(transaction).await
    }
}

#[async_trait]
impl SendTransactionProvider for Scripted {
    async fn send_transaction(
        &self,
        transaction: &VersionedTransaction,
        _meta: SendTransactionMeta,
    ) -> anyhow::Result<Signature> {
        let signature = transaction.signatures[0];
        let index = {
            let mut sent = self.sent.lock().unwrap();
            sent.push(signature);
            sent.len() - 1
        };
        let landing = self.script.lock().unwrap().landings.pop_front();
        if let Some(Some(result)) = landing {
            self.landed
                .lock()
                .unwrap()
                .insert(signature, finalized(10 + index as u64, result));
        }
        Ok(signature)
    }
}

#[async_trait]
impl SignatureStatusesProvider for Scripted {
    async fn get_signature_statuses(
        &self,
        signatures: &[Signature],
    ) -> RpcResult<Vec<Option<TransactionStatus>>> {
        assert!(
            !self.script.lock().unwrap().watcher_panics,
            "scripted watcher failure"
        );
        let landed = self.landed.lock().unwrap();
        response(
            signatures
                .iter()
                .map(|signature| landed.get(signature).cloned())
                .collect(),
        )
    }

    async fn get_signature_statuses_with_history(
        &self,
        signatures: &[Signature],
    ) -> RpcResult<Vec<Option<TransactionStatus>>> {
        let landed = self.landed.lock().unwrap();
        let script = self.script.lock().unwrap();
        response(
            signatures
                .iter()
                .map(|signature| {
                    landed
                        .get(signature)
                        .or_else(|| script.history.get(signature))
                        .cloned()
                })
                .collect(),
        )
    }

    async fn latest_blockhash(&self) -> anyhow::Result<(Hash, u64)> {
        Ok((Hash::new_unique(), LAST_VALID))
    }

    async fn block_height(&self) -> anyhow::Result<u64> {
        let sends = self.sent.lock().unwrap().len();
        self.sends_at_block_height_reads.lock().unwrap().push(sends);
        let next = self.script.lock().unwrap().block_heights.pop_front();
        next.unwrap_or(Ok(LAST_VALID + 1))
            .map_err(anyhow::Error::msg)
    }

    fn commitment(&self) -> CommitmentConfig {
        CommitmentConfig::finalized()
    }
}

#[async_trait]
impl PrioritizationFeesProvider for Scripted {
    async fn recent_prioritization_fees(&self, _writable: &[Pubkey]) -> anyhow::Result<Vec<u64>> {
        Ok(vec![])
    }
}

fn config() -> TransactionExecutorConfig {
    TransactionExecutorConfig {
        signature_status_check_interval: Duration::from_millis(5),
        transaction_send_interval: Duration::from_millis(20),
        waiting_time_after_last_send: Duration::from_millis(300),
        max_attempts: 3,
        max_consecutive_blockhash_check_failures: 2,
        ..TransactionExecutorConfig::default()
    }
}

const LAST_VALID: u64 = 100;

fn attempt(n: u8) -> AttemptTransaction {
    let payer = Pubkey::new_unique();
    let message = v1::Message::try_compile_with_config(
        &payer,
        &[],
        Hash::new_unique(),
        v1::TransactionConfig::empty(),
    )
    .unwrap();
    AttemptTransaction {
        transaction: VersionedTransaction {
            signatures: vec![Signature::from([n; 64])],
            message: VersionedMessage::V1(message),
        },
        last_valid_block_height: LAST_VALID,
    }
}

async fn execute(script: Script, config: TransactionExecutorConfig) -> (Execution, Arc<Scripted>) {
    execute_stream(script, config, (1..=3).map(|n| Ok(attempt(n))).collect()).await
}

async fn execute_stream(
    script: Script,
    config: TransactionExecutorConfig,
    attempts: Vec<anyhow::Result<AttemptTransaction>>,
) -> (Execution, Arc<Scripted>) {
    let chain = Arc::new(Scripted {
        script: Mutex::new(script),
        ..Scripted::default()
    });
    let executor = TransactionExecutor::new(
        config,
        chain.clone(),
        chain.clone(),
        chain.clone(),
        chain.clone(),
    );
    let execution = executor.execute_transaction(stream::iter(attempts)).await;
    (execution, chain)
}

#[tokio::test]
async fn the_first_attempt_lands() {
    let (execution, chain) = execute(
        Script {
            landings: VecDeque::from([Some(Ok(()))]),
            ..Script::default()
        },
        config(),
    )
    .await;

    assert_eq!(
        execution.outcome,
        Outcome::Landed {
            signature: Signature::from([1; 64]),
            slot: 10,
        }
    );
    assert_eq!(execution.attempts.len(), 1);
    assert_eq!(chain.sent.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_landed_error_ends_the_execution_without_another_attempt() {
    let (execution, chain) = execute(
        Script {
            landings: VecDeque::from([Some(Err(custom(7)))]),
            ..Script::default()
        },
        config(),
    )
    .await;

    assert_eq!(
        execution.outcome,
        Outcome::LandedWithError {
            signature: Signature::from([1; 64]),
            slot: 10,
            error: custom(7),
        }
    );
    assert_eq!(execution.attempts.len(), 1);
    assert_eq!(chain.sent.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_refused_first_attempt_sends_nothing() {
    let (execution, chain) = execute(
        Script {
            refusals: VecDeque::from([Some(custom(1))]),
            ..Script::default()
        },
        config(),
    )
    .await;

    assert_eq!(
        execution.outcome,
        Outcome::NotLanded(NotLanded::Refused {
            error: custom(1),
            logs: vec!["Program log: scripted".into()],
        })
    );
    assert_eq!(execution.attempts.len(), 1);
    assert_eq!(execution.attempts[0].refused, Some(custom(1)));
    assert!(!execution.attempts[0].sent);
    assert!(chain.sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_attempt_that_cannot_be_built_is_not_sent() {
    let (execution, chain) = execute_stream(
        Script::default(),
        config(),
        vec![Err(anyhow::anyhow!("no blockhash"))],
    )
    .await;

    assert!(matches!(
        execution.outcome,
        Outcome::NotLanded(NotLanded::NotSent { .. })
    ));
    assert!(execution.attempts.is_empty());
    assert!(chain.sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_rpc_error_in_the_execution_carries_no_api_key() {
    let (execution, _) = execute_stream(
        Script::default(),
        config(),
        vec![Err(anyhow::anyhow!(
            "error sending request for url (https://rpc.example.com/?api-key=SECRET)"
        ))],
    )
    .await;

    assert_eq!(
        execution.outcome,
        Outcome::NotLanded(NotLanded::NotSent {
            error: "Failed to build attempt #1: error sending request for url (https://rpc.example.com/<redacted>)".into(),
        })
    );
}

#[tokio::test]
async fn attempts_that_all_expire_unfound_did_not_land() {
    let (execution, chain) = execute(Script::default(), config()).await;

    assert_eq!(execution.outcome, Outcome::NotLanded(NotLanded::Expired));
    assert_eq!(execution.attempts.len(), 3);
    assert!(execution.attempts.iter().all(|attempt| attempt.sent));
    assert_eq!(chain.sent.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn a_landing_only_the_ledger_knows_is_found_before_the_next_attempt() {
    let (execution, chain) = execute(
        Script {
            history: HashMap::from([(Signature::from([2; 64]), finalized(42, Ok(())))]),
            ..Script::default()
        },
        config(),
    )
    .await;

    assert_eq!(
        execution.outcome,
        Outcome::Landed {
            signature: Signature::from([2; 64]),
            slot: 42,
        }
    );
    assert_eq!(execution.attempts.len(), 2);
    assert_eq!(chain.sent.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn a_landing_below_the_commitment_is_unknown_past_the_bound() {
    let (execution, _) = execute(
        Script {
            history: HashMap::from([(Signature::from([3; 64]), confirmed(43))]),
            ..Script::default()
        },
        config(),
    )
    .await;

    assert!(
        matches!(&execution.outcome, Outcome::Unknown { reason } if reason.contains("has not reached")),
        "{execution:?}"
    );
}

#[tokio::test]
async fn a_blockhash_valid_past_the_bound_is_unknown() {
    let (execution, _) = execute(
        Script {
            block_heights: VecDeque::from(vec![Ok(LAST_VALID); 1_000]),
            ..Script::default()
        },
        TransactionExecutorConfig {
            max_attempts: 1,
            wait_for_blockhash_expiry_before_retry: false,
            ..config()
        },
    )
    .await;

    assert!(
        matches!(&execution.outcome, Outcome::Unknown { reason } if reason.contains("still valid")),
        "{execution:?}"
    );
    assert_eq!(execution.attempts.len(), 1);
}

#[tokio::test]
async fn a_blockhash_check_that_keeps_failing_is_unknown() {
    let (execution, chain) = execute(
        Script {
            block_heights: VecDeque::from(vec![Err("node down".to_string()); 2]),
            ..Script::default()
        },
        config(),
    )
    .await;

    assert!(
        matches!(&execution.outcome, Outcome::Unknown { reason } if reason.contains("node down")),
        "{execution:?}"
    );
    assert_eq!(chain.sent.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn an_already_processed_attempt_is_resolved_from_the_ledger() {
    let (execution, chain) = execute(
        Script {
            refusals: VecDeque::from([Some(TransactionError::AlreadyProcessed)]),
            history: HashMap::from([(Signature::from([1; 64]), finalized(5, Ok(())))]),
            ..Script::default()
        },
        config(),
    )
    .await;

    assert_eq!(
        execution.outcome,
        Outcome::Landed {
            signature: Signature::from([1; 64]),
            slot: 5,
        }
    );
    assert_eq!(execution.attempts.len(), 1);
    assert!(execution.attempts[0].sent);
    assert!(chain.sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_landing_the_watcher_missed_stops_the_next_attempt() {
    let (execution, chain) = execute(
        Script {
            history: HashMap::from([(Signature::from([1; 64]), finalized(7, Ok(())))]),
            ..Script::default()
        },
        config(),
    )
    .await;

    assert_eq!(
        execution.outcome,
        Outcome::Landed {
            signature: Signature::from([1; 64]),
            slot: 7,
        }
    );
    assert_eq!(execution.attempts.len(), 1);
    assert_eq!(chain.sent.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn an_attempt_held_at_its_last_valid_height_is_unknown_past_the_bound() {
    let (execution, chain) = execute(
        Script {
            block_heights: VecDeque::from(vec![Ok(LAST_VALID); 1_000]),
            ..Script::default()
        },
        config(),
    )
    .await;

    assert!(
        matches!(&execution.outcome, Outcome::Unknown { reason } if reason.contains("still valid")),
        "{execution:?}"
    );
    assert_eq!(execution.attempts.len(), 1);
    assert_eq!(chain.sent.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_retry_waits_until_the_block_height_passes_the_last_valid_one() {
    let (execution, chain) = execute(
        Script {
            block_heights: VecDeque::from([Ok(LAST_VALID - 1), Ok(LAST_VALID)]),
            ..Script::default()
        },
        TransactionExecutorConfig {
            max_attempts: 2,
            ..config()
        },
    )
    .await;

    assert_eq!(execution.outcome, Outcome::NotLanded(NotLanded::Expired));
    assert_eq!(
        *chain.sends_at_block_height_reads.lock().unwrap(),
        vec![1, 1, 1, 2, 2]
    );
    assert_eq!(chain.sent.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn a_stopped_status_watcher_is_unknown() {
    let (execution, chain) = execute(
        Script {
            block_heights: VecDeque::from(vec![Ok(LAST_VALID); 1_000]),
            watcher_panics: true,
            ..Script::default()
        },
        config(),
    )
    .await;

    assert_eq!(
        execution.outcome,
        Outcome::Unknown {
            reason: "The status watcher stopped".into(),
        }
    );
    assert_eq!(chain.sent.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn an_attempt_whose_blockhash_is_not_found_is_skipped_unsent() {
    let (execution, chain) = execute(
        Script {
            refusals: VecDeque::from([Some(TransactionError::BlockhashNotFound)]),
            ..Script::default()
        },
        config(),
    )
    .await;

    assert_eq!(execution.outcome, Outcome::NotLanded(NotLanded::Expired));
    assert_eq!(execution.attempts.len(), 3);
    assert!(!execution.attempts[0].sent);
    assert_eq!(
        *chain.sent.lock().unwrap(),
        vec![Signature::from([2; 64]), Signature::from([3; 64])]
    );
}

#[tokio::test]
async fn a_simulation_error_after_an_expired_send_is_not_sent() {
    let (execution, chain) = execute(
        Script {
            simulation_errors: VecDeque::from([None, Some("node down".to_string())]),
            ..Script::default()
        },
        config(),
    )
    .await;

    assert!(
        matches!(&execution.outcome, Outcome::NotLanded(NotLanded::NotSent { error }) if error.contains("node down")),
        "{execution:?}"
    );
    assert_eq!(execution.attempts.len(), 2);
    assert!(execution.attempts[0].sent);
    assert!(!execution.attempts[1].sent);
    assert_eq!(chain.sent.lock().unwrap().len(), 1);
}

#[test]
fn a_status_at_the_commitment_wins_over_an_earlier_one_below_it() {
    let sent = [Signature::from([1; 64]), Signature::from([2; 64])];
    let finalized_later = finalized(9, Ok(()));

    assert_eq!(
        best_status(
            vec![Some(confirmed(8)), Some(finalized_later.clone())],
            &sent,
            CommitmentConfig::finalized()
        ),
        Some((finalized_later, sent[1]))
    );
    assert_eq!(
        best_status(
            vec![None, Some(confirmed(8))],
            &sent,
            CommitmentConfig::finalized()
        ),
        Some((confirmed(8), sent[1]))
    );
    assert_eq!(
        best_status(vec![None, None], &sent, CommitmentConfig::finalized()),
        None
    );
}
