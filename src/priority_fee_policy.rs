use solana_sdk::{compute_budget::ComputeBudgetInstruction, instruction::Instruction};

#[derive(Clone, Debug, Default)]
pub struct PriorityFeeConfiguration {
    pub micro_lamports_per_cu: u64,
}

impl PriorityFeeConfiguration {
    pub fn build_pre_instructions(&self) -> Vec<Instruction> {
        vec![ComputeBudgetInstruction::set_compute_unit_price(
            self.micro_lamports_per_cu,
        )]
    }
}

#[derive(Clone, Debug, Default)]
pub struct PriorityFeePolicy {
    pub micro_lamports_per_cu_min: u64,
    pub micro_lamports_per_cu_max: u64,
    pub multiplier_per_attempt: u64,
}

impl PriorityFeePolicy {
    pub fn iter_priority_fee_configuration(
        &self,
    ) -> impl Iterator<Item = PriorityFeeConfiguration> + '_ {
        std::iter::successors(
            Some(PriorityFeeConfiguration {
                micro_lamports_per_cu: self.micro_lamports_per_cu_min,
            }),
            |prev| {
                Some(PriorityFeeConfiguration {
                    micro_lamports_per_cu: prev
                        .micro_lamports_per_cu
                        .saturating_mul(self.multiplier_per_attempt)
                        .min(self.micro_lamports_per_cu_max),
                })
            },
        )
    }
}
