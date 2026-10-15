use log::warn;
use solana_pubkey::Pubkey;

use crate::TransactionExecutor;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PriorityLevel {
    Low,
    Medium,
    High,
    VeryHigh,
    UnsafeMax,
}

impl PriorityLevel {
    fn basis_points(self) -> u64 {
        match self {
            PriorityLevel::Low => 2_500,
            PriorityLevel::Medium => 5_000,
            PriorityLevel::High => 7_500,
            PriorityLevel::VeryHigh => 9_000,
            PriorityLevel::UnsafeMax => 10_000,
        }
    }

    pub fn of(self, fees: &[u64]) -> Option<u64> {
        let mut paid: Vec<u64> = fees.iter().copied().filter(|fee| *fee > 0).collect();
        paid.sort_unstable();
        let last = u64::try_from(paid.len().checked_sub(1)?).ok()?;
        let index = (last * self.basis_points() + 5_000) / 10_000;
        paid.get(usize::try_from(index).ok()?).copied()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FeeStrategy {
    #[default]
    Static,
    Dynamic {
        level: PriorityLevel,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeePolicy {
    pub strategy: FeeStrategy,
    pub start_micro_lamports_per_cu: u64,
    pub first_step_micro_lamports_per_cu: u64,
    pub multiplier: u64,
    pub max_micro_lamports_per_cu: u64,
}

impl Default for FeePolicy {
    fn default() -> Self {
        Self {
            strategy: FeeStrategy::Static,
            start_micro_lamports_per_cu: 0,
            first_step_micro_lamports_per_cu: 1_000,
            multiplier: 2,
            max_micro_lamports_per_cu: 100_000,
        }
    }
}

impl FeePolicy {
    pub fn first(&self) -> u64 {
        self.start_micro_lamports_per_cu
    }

    pub fn next(&self, previous: u64, network: Option<u64>) -> u64 {
        let step = if previous == 0 {
            self.first_step_micro_lamports_per_cu
        } else {
            previous.saturating_mul(self.multiplier)
        };
        step.max(network.unwrap_or(0))
            .min(self.max_micro_lamports_per_cu)
            .max(previous)
    }
}

/// The v1 `priority_fee` in lamports for a micro-lamports/CU rate, rounded as a legacy price is.
pub fn priority_fee_lamports(micro_lamports_per_cu: u64, compute_unit_limit: u32) -> u64 {
    let micro_lamports = u128::from(micro_lamports_per_cu) * u128::from(compute_unit_limit);
    u64::try_from(micro_lamports.div_ceil(1_000_000)).unwrap_or(u64::MAX)
}

impl TransactionExecutor {
    pub async fn next_rate(&self, policy: &FeePolicy, previous: u64, writable: &[Pubkey]) -> u64 {
        let network = match policy.strategy {
            FeeStrategy::Static => None,
            FeeStrategy::Dynamic { level } => match self
                .prioritization_fees_provider
                .recent_prioritization_fees(writable)
                .await
            {
                Ok(fees) => level.of(&fees),
                Err(err) => {
                    warn!("Recent prioritization fees unavailable, escalating without them: {err}");
                    None
                }
            },
        };
        policy.next(previous, network)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::Fake;
    use std::sync::atomic::Ordering;

    fn dynamic(level: PriorityLevel) -> FeePolicy {
        FeePolicy {
            strategy: FeeStrategy::Dynamic { level },
            ..FeePolicy::default()
        }
    }

    #[test]
    fn the_default_pays_nothing_until_a_retry_and_then_doubles_to_the_cap() {
        let policy = FeePolicy::default();
        let mut rates = vec![policy.first()];
        for _ in 0..9 {
            rates.push(policy.next(*rates.last().unwrap(), None));
        }
        assert_eq!(
            rates,
            vec![0, 1_000, 2_000, 4_000, 8_000, 16_000, 32_000, 64_000, 100_000, 100_000]
        );
    }

    #[test]
    fn a_set_start_rate_is_the_first_rate() {
        let policy = FeePolicy {
            start_micro_lamports_per_cu: 5_000,
            ..FeePolicy::default()
        };
        assert_eq!(policy.first(), 5_000);
        assert_eq!(policy.next(5_000, None), 10_000);
    }

    #[test]
    fn a_start_rate_above_the_cap_is_kept_not_lowered() {
        let policy = FeePolicy {
            start_micro_lamports_per_cu: 200_000,
            ..FeePolicy::default()
        };
        assert_eq!(policy.next(policy.first(), None), 200_000);
    }

    #[test]
    fn a_level_picks_its_percentile_of_the_paid_fees() {
        let fees = [9_000, 0, 500, 3_000];
        assert_eq!(PriorityLevel::Low.of(&fees), Some(3_000));
        assert_eq!(PriorityLevel::Medium.of(&fees), Some(3_000));
        assert_eq!(PriorityLevel::UnsafeMax.of(&fees), Some(9_000));
        assert_eq!(PriorityLevel::Medium.of(&[0, 0]), None);
        assert_eq!(PriorityLevel::Medium.of(&[]), None);
    }

    #[test]
    fn a_rate_over_a_limit_is_its_total_in_lamports_rounded_up() {
        assert_eq!(priority_fee_lamports(1_000, 200_000), 200);
        assert_eq!(priority_fee_lamports(1, 1), 1);
        assert_eq!(priority_fee_lamports(1_500_000, 3), 5);
        assert_eq!(priority_fee_lamports(0, 1_400_000), 0);
        assert_eq!(priority_fee_lamports(100_000, 0), 0);
        assert_eq!(priority_fee_lamports(u64::MAX, u32::MAX), u64::MAX);
    }

    #[tokio::test]
    async fn a_dynamic_retry_takes_the_larger_of_the_network_and_the_step() {
        let fake = Fake::with_fees(Ok(vec![0, 500, 3_000, 9_000]));
        let executor = fake.clone().executor();
        let policy = dynamic(PriorityLevel::Medium);

        assert_eq!(executor.next_rate(&policy, 0, &[]).await, 3_000);
        assert_eq!(executor.next_rate(&policy, 4_000, &[]).await, 8_000);
        assert_eq!(fake.fee_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_dynamic_retry_without_network_fees_escalates_statically() {
        let policy = dynamic(PriorityLevel::High);

        let failing = Fake::with_fees(Err("node down".to_string())).executor();
        assert_eq!(failing.next_rate(&policy, 0, &[]).await, 1_000);

        let empty = Fake::with_fees(Ok(vec![])).executor();
        assert_eq!(empty.next_rate(&policy, 2_000, &[]).await, 4_000);
    }

    #[tokio::test]
    async fn a_static_retry_never_asks_the_network() {
        let fake = Fake::with_fees(Ok(vec![50_000]));
        let executor = fake.clone().executor();

        assert_eq!(
            executor.next_rate(&FeePolicy::default(), 0, &[]).await,
            1_000
        );
        assert_eq!(fake.fee_calls.load(Ordering::SeqCst), 0);
    }
}
