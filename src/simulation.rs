use solana_transaction::{versioned::VersionedTransaction, VersionedMessage};
use solana_transaction_error::TransactionError;

use crate::TransactionExecutor;

// The protocol maxima, as in solana-program-runtime's execution_budget.
pub const MAX_COMPUTE_UNIT_LIMIT: u32 = 1_400_000;
pub const MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES: u32 = 64 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct Simulation {
    pub units_consumed: Option<u64>,
    pub loaded_accounts_data_size: Option<u32>,
    pub error: Option<TransactionError>,
    pub logs: Vec<String>,
}

impl TransactionExecutor {
    pub async fn latest_blockhash(&self) -> anyhow::Result<(solana_hash::Hash, u64)> {
        self.signature_statuses_provider.latest_blockhash().await
    }

    /// Simulates without signatures, against the latest blockhash; an unset v1 limit runs at the max.
    pub async fn simulate(&self, transaction: &VersionedTransaction) -> anyhow::Result<Simulation> {
        let result = self
            .simulate_transaction_provider
            .simulate_unsigned_transaction(&with_unset_limits_at_max(transaction))
            .await?
            .value;
        Ok(Simulation {
            units_consumed: result.units_consumed,
            loaded_accounts_data_size: result.loaded_accounts_data_size,
            error: result.err.map(TransactionError::from),
            logs: result.logs.unwrap_or_default(),
        })
    }
}

// A v1 limit left unset is 0, which would fail the simulation before it measures anything.
fn with_unset_limits_at_max(transaction: &VersionedTransaction) -> VersionedTransaction {
    let mut transaction = transaction.clone();
    if let VersionedMessage::V1(message) = &mut transaction.message {
        let config = &mut message.config;
        config
            .compute_unit_limit
            .get_or_insert(MAX_COMPUTE_UNIT_LIMIT);
        config
            .loaded_accounts_data_size_limit
            .get_or_insert(MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES);
    }
    transaction
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::Fake;
    use solana_hash::Hash;
    use solana_instruction::error::InstructionError;
    use solana_keypair::Keypair;
    use solana_message::{v1, VersionedMessage};
    use solana_signature::Signature;
    use solana_signer::Signer;

    fn executor(result: serde_json::Value) -> TransactionExecutor {
        Fake::simulating(result).executor()
    }

    fn transaction() -> VersionedTransaction {
        transaction_with(v1::TransactionConfig::empty())
    }

    fn transaction_with(config: v1::TransactionConfig) -> VersionedTransaction {
        let payer = Keypair::new();
        let message =
            v1::Message::try_compile_with_config(&payer.pubkey(), &[], Hash::default(), config)
                .unwrap();
        VersionedTransaction {
            signatures: vec![Signature::default()],
            message: VersionedMessage::V1(message),
        }
    }

    async fn simulated_config(transaction: VersionedTransaction) -> v1::TransactionConfig {
        let fake = Fake::simulating(serde_json::json!({ "err": null }));
        fake.clone()
            .executor()
            .simulate(&transaction)
            .await
            .unwrap();
        let simulated = fake.simulated.lock().unwrap().pop().unwrap();
        match simulated.message {
            VersionedMessage::V1(message) => message.config,
            other => panic!("simulated a non-v1 message: {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_unset_v1_limit_is_simulated_at_the_protocol_maximum() {
        let config = simulated_config(transaction()).await;

        assert_eq!(config.compute_unit_limit, Some(MAX_COMPUTE_UNIT_LIMIT));
        assert_eq!(
            config.loaded_accounts_data_size_limit,
            Some(MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES)
        );
        assert_eq!(config.priority_fee, None);
        assert_eq!(config.heap_size, None);
    }

    #[tokio::test]
    async fn a_set_v1_limit_is_simulated_as_set() {
        let set = v1::TransactionConfig::empty()
            .with_compute_unit_limit(50_000)
            .with_loaded_accounts_data_size_limit(1_024)
            .with_priority_fee(7);

        assert_eq!(simulated_config(transaction_with(set)).await, set);
    }

    #[tokio::test]
    async fn a_simulation_reports_the_units_and_the_loaded_bytes() {
        let simulation = executor(serde_json::json!({
            "err": null,
            "logs": ["Program log: ok"],
            "unitsConsumed": 12_345,
            "loadedAccountsDataSize": 67_890,
        }))
        .simulate(&transaction())
        .await
        .unwrap();

        assert_eq!(
            simulation,
            Simulation {
                units_consumed: Some(12_345),
                loaded_accounts_data_size: Some(67_890),
                error: None,
                logs: vec!["Program log: ok".to_string()],
            }
        );
    }

    #[tokio::test]
    async fn a_failed_simulation_names_the_failing_instruction() {
        let simulation = executor(serde_json::json!({
            "err": { "InstructionError": [1, { "Custom": 6001 }] },
            "logs": null,
            "unitsConsumed": 900,
        }))
        .simulate(&transaction())
        .await
        .unwrap();

        assert_eq!(
            simulation.error,
            Some(TransactionError::InstructionError(
                1,
                InstructionError::Custom(6001)
            ))
        );
        assert_eq!(simulation.units_consumed, Some(900));
        assert!(simulation.logs.is_empty());
    }
}
