#[derive(Clone, Debug, Default)]
pub struct TipConfiguration {
    pub tip: u64,
}

#[derive(Clone, Debug)]
pub struct TipPolicy {
    pub tip_min: u64,
    pub tip_max: u64,
    pub multiplier_per_attempt: u64,
}

impl Default for TipPolicy {
    fn default() -> Self {
        Self {
            tip_min: 1_000,
            tip_max: 200_000,
            multiplier_per_attempt: 4,
        }
    }
}

impl TipPolicy {
    /// The first attempt goes untipped; `tip_min` is the tip of the first retry.
    pub fn get_tip_configuration(&self, attempt: u32) -> TipConfiguration {
        let tip = attempt.checked_sub(2).map_or(0, |retry| {
            self.tip_min
                .saturating_mul(self.multiplier_per_attempt.saturating_pow(retry))
                .min(self.tip_max)
        });

        TipConfiguration { tip }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_tips_nothing_until_a_retry_and_then_quadruples_to_the_cap() {
        let policy = TipPolicy::default();
        let tips: Vec<u64> = (1..=7)
            .map(|attempt| policy.get_tip_configuration(attempt).tip)
            .collect();
        assert_eq!(
            tips,
            vec![0, 1_000, 4_000, 16_000, 64_000, 200_000, 200_000]
        );
    }
}
