#[derive(Clone, Debug, Default)]
pub struct TipConfiguration {
    pub tip: u64,
}

#[derive(Clone, Debug, Default)]
pub struct TipPolicy {
    pub tip_min: u64,
    pub tip_max: u64,
    pub multiplier_per_attempt: u64,
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
