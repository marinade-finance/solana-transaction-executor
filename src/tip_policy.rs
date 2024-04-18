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
            tip_min: 1000,
            tip_max: 200000,
            multiplier_per_attempt: 4,
        }
    }
}

impl TipPolicy {
    pub fn get_tip_configuration(&self, attempt: u32) -> TipConfiguration {
        let tip = self
            .tip_min
            .saturating_mul(self.multiplier_per_attempt.saturating_pow(attempt))
            .min(self.tip_max);

        TipConfiguration { tip }
    }
}
