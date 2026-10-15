use std::time::Duration;

pub struct TransactionExecutorConfig {
    pub signature_status_check_interval: Duration,
    pub transaction_send_interval: Duration,
    pub waiting_time_after_last_send: Duration,
    pub max_attempts: u32,
    pub wait_for_blockhash_expiry_before_retry: bool,
    pub max_consecutive_blockhash_check_failures: u32,
}

impl Default for TransactionExecutorConfig {
    fn default() -> Self {
        Self {
            signature_status_check_interval: Duration::from_secs(1),
            transaction_send_interval: Duration::from_secs(20),
            waiting_time_after_last_send: Duration::from_secs(60),
            max_attempts: 10,
            wait_for_blockhash_expiry_before_retry: true,
            max_consecutive_blockhash_check_failures: 5,
        }
    }
}
