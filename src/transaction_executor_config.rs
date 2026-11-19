use std::time::Duration;

pub struct TransactionExecutorConfig {
    pub signature_status_check_interval: Duration,
    pub transaction_send_interval: Duration,
    pub waiting_time_after_last_send: Duration,
    pub max_attempts: u32,
    pub wait_for_blockhash_expiry_before_retry: bool,
    pub max_consecutive_blockhash_check_failures: u32,
    /// How long a read blockhash and block height are served from the cache.
    pub blockhash_refresh_interval: Duration,
    /// All sends of the executor, first sends and rebroadcasts; 0 counts as 1.
    pub max_sends_per_second: u32,
    /// Sends the same signed bytes again until they land or expire; `None` sends once.
    pub rebroadcast_interval: Option<Duration>,
}

impl Default for TransactionExecutorConfig {
    fn default() -> Self {
        Self {
            signature_status_check_interval: Duration::from_secs(1),
            transaction_send_interval: Duration::from_secs(20),
            waiting_time_after_last_send: Duration::from_secs(120),
            max_attempts: 10,
            wait_for_blockhash_expiry_before_retry: true,
            max_consecutive_blockhash_check_failures: 5,
            blockhash_refresh_interval: Duration::from_secs(5),
            max_sends_per_second: 10,
            rebroadcast_interval: Some(Duration::from_secs(2)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_slow_and_cheap() {
        let config = TransactionExecutorConfig::default();

        assert_eq!(
            config.signature_status_check_interval,
            Duration::from_secs(1)
        );
        assert_eq!(config.transaction_send_interval, Duration::from_secs(20));
        assert_eq!(
            config.waiting_time_after_last_send,
            Duration::from_secs(120)
        );
        assert_eq!(config.max_attempts, 10);
        assert!(config.wait_for_blockhash_expiry_before_retry);
        assert_eq!(config.max_consecutive_blockhash_check_failures, 5);
        assert_eq!(config.blockhash_refresh_interval, Duration::from_secs(5));
        assert_eq!(config.max_sends_per_second, 10);
        assert_eq!(config.rebroadcast_interval, Some(Duration::from_secs(2)));
    }
}
