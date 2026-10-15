use solana_client::nonblocking::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;

use crate::redact::redact_urls;

/// Finalized once Alpenglow is active; before that it costs 12.8 s a send, so confirmed.
pub async fn alpenglow_commitment(rpc_client: &RpcClient) -> CommitmentConfig {
    match rpc_client.get_ag_genesis_cert().await {
        Ok(Some(_)) => CommitmentConfig::finalized(),
        Ok(None) => CommitmentConfig::confirmed(),
        Err(err) => {
            log::warn!(
                "Alpenglow activation is unknown, confirming at confirmed: {}",
                redact_urls(&err.to_string())
            );
            CommitmentConfig::confirmed()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_active_alpenglow_confirms_at_finalized() {
        let rpc_client = RpcClient::new_mock("succeeds".to_string());
        assert_eq!(
            alpenglow_commitment(&rpc_client).await,
            CommitmentConfig::finalized()
        );
    }

    #[tokio::test]
    async fn no_genesis_certificate_confirms_at_confirmed() {
        let rpc_client = RpcClient::new_mock("fails".to_string());
        assert_eq!(
            alpenglow_commitment(&rpc_client).await,
            CommitmentConfig::confirmed()
        );
    }

    #[tokio::test]
    async fn a_node_that_cannot_answer_confirms_at_confirmed() {
        let rpc_client = RpcClient::new("http://127.0.0.1:1".to_string());
        assert_eq!(
            alpenglow_commitment(&rpc_client).await,
            CommitmentConfig::confirmed()
        );
    }
}
