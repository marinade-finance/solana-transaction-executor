use solana_hash::Hash;
use solana_signature::Signature;
use solana_transaction_error::TransactionError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Landed {
        signature: Signature,
        slot: u64,
    },
    LandedWithError {
        signature: Signature,
        slot: u64,
        error: TransactionError,
    },
    /// Provably not landed: nothing was sent, or every sent attempt's blockhash expired unfound.
    NotLanded(NotLanded),
    /// An attempt may still land; rebuilding now could land it twice.
    Unknown {
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotLanded {
    Refused {
        error: TransactionError,
        logs: Vec<String>,
    },
    NotSent {
        error: String,
    },
    Expired,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attempt {
    pub signature: Signature,
    pub blockhash: Hash,
    pub last_valid_block_height: u64,
    pub sent: bool,
    pub send_error: Option<String>,
    pub refused: Option<TransactionError>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Execution {
    pub outcome: Outcome,
    pub attempts: Vec<Attempt>,
}
