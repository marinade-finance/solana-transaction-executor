use solana_transaction::versioned::VersionedTransaction;
use solana_transaction_error::TransactionError;

use crate::TransactionExecutor;

#[derive(Clone, Debug, PartialEq)]
pub struct Simulation {
    pub units_consumed: Option<u64>,
    pub loaded_accounts_data_size: Option<u32>,
    pub error: Option<TransactionError>,
    pub logs: Vec<String>,
}

impl TransactionExecutor {
    pub async fn latest_blockhash(&self) -> anyhow::Result<solana_hash::Hash> {
        self.signature_statuses_provider.latest_blockhash().await
    }

    /// Simulates without signatures, against the cluster's latest blockhash.
    pub async fn simulate(&self, transaction: &VersionedTransaction) -> anyhow::Result<Simulation> {
        let result = self
            .simulate_transaction_provider
            .simulate_unsigned_transaction(transaction)
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
        let payer = Keypair::new();
        let message = v1::Message::try_compile(&payer.pubkey(), &[], Hash::default()).unwrap();
        VersionedTransaction {
            signatures: vec![Signature::default()],
            message: VersionedMessage::V1(message),
        }
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
