use std::{
    collections::HashMap,
    future::Future,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, OnceLock,
    },
    time::Duration,
};

use solana_hash::Hash;
use solana_signature::Signature;
use solana_transaction_error::TransactionError;
use tokio::{
    sync::{
        mpsc::{self, UnboundedReceiver, UnboundedSender},
        Mutex,
    },
    time::{Instant, MissedTickBehavior},
};

use crate::{redact::redact_urls, SignatureStatusesProvider, TransactionExecutorConfig};

/// The protocol maximum of one `getSignatureStatuses` call.
const MAX_STATUSES_PER_CALL: usize = 256;

pub(crate) struct Landing {
    pub(crate) signature: Signature,
    pub(crate) slot: u64,
    pub(crate) result: Result<(), TransactionError>,
}

enum Command {
    Register {
        execution: u64,
        landings: UnboundedSender<Landing>,
    },
    Watch {
        execution: u64,
        signature: Signature,
    },
}

type Cached<T> = Mutex<Option<(T, Instant)>>;

/// The executor's shared state: one cache for every execution, so parallel ones share RPC reads.
pub(crate) struct SenderCore {
    statuses: Arc<dyn SignatureStatusesProvider + Send + Sync>,
    refresh: Duration,
    status_interval: Duration,
    blockhash: Cached<(Hash, u64)>,
    block_height: Cached<u64>,
    executions: AtomicU64,
    commands: OnceLock<UnboundedSender<Command>>,
}

impl SenderCore {
    pub(crate) fn new(
        config: &TransactionExecutorConfig,
        statuses: Arc<dyn SignatureStatusesProvider + Send + Sync>,
    ) -> Self {
        Self {
            statuses,
            refresh: config.blockhash_refresh_interval,
            status_interval: config.signature_status_check_interval,
            blockhash: Mutex::new(None),
            block_height: Mutex::new(None),
            executions: AtomicU64::new(0),
            commands: OnceLock::new(),
        }
    }

    /// The loop starts on the first command; a stopped loop closes every execution's landings.
    fn send(&self, command: Command) {
        let commands = self.commands.get_or_init(|| {
            let (commands, receiver) = mpsc::unbounded_channel();
            tokio::spawn(run(self.statuses.clone(), self.status_interval, receiver));
            commands
        });
        let _ = commands.send(command);
    }

    /// The first landing found among the execution's watched signatures.
    pub(crate) fn register(&self) -> (u64, UnboundedReceiver<Landing>) {
        let execution = self.executions.fetch_add(1, Ordering::SeqCst);
        let (landings, receiver) = mpsc::unbounded_channel();
        self.send(Command::Register {
            execution,
            landings,
        });
        (execution, receiver)
    }

    pub(crate) fn watch(&self, execution: u64, signature: Signature) {
        self.send(Command::Watch {
            execution,
            signature,
        });
    }

    pub(crate) async fn latest_blockhash(&self) -> anyhow::Result<(Hash, u64)> {
        cached(
            &self.blockhash,
            self.refresh,
            self.statuses.latest_blockhash(),
        )
        .await
    }

    /// A stale height is lower than the real one, so it can only hold a retry longer.
    pub(crate) async fn block_height(&self) -> anyhow::Result<u64> {
        cached(
            &self.block_height,
            self.refresh,
            self.statuses.block_height(),
        )
        .await
    }
}

async fn cached<T: Copy>(
    slot: &Cached<T>,
    refresh: Duration,
    read: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    let mut cached = slot.lock().await;
    if let Some((value, read_at)) = *cached {
        if read_at.elapsed() < refresh {
            return Ok(value);
        }
    }
    let value = read.await?;
    *cached = Some((value, Instant::now()));
    Ok(value)
}

async fn run(
    statuses: Arc<dyn SignatureStatusesProvider + Send + Sync>,
    interval: Duration,
    mut commands: UnboundedReceiver<Command>,
) {
    let mut executions: HashMap<u64, UnboundedSender<Landing>> = HashMap::new();
    let mut pending: Vec<(Signature, u64)> = Vec::new();
    let mut tick = tokio::time::interval_at(Instant::now() + interval, interval);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            command = commands.recv() => match command {
                Some(Command::Register { execution, landings }) => {
                    executions.insert(execution, landings);
                }
                Some(Command::Watch { execution, signature }) => {
                    if !pending.contains(&(signature, execution)) {
                        pending.push((signature, execution));
                    }
                }
                None => return,
            },
            _ = tick.tick() => {
                executions.retain(|_, landings| !landings.is_closed());
                pending.retain(|(_, execution)| executions.contains_key(execution));
                poll(statuses.as_ref(), &pending, &mut executions).await;
                pending.retain(|(_, execution)| executions.contains_key(execution));
            }
        }
    }
}

async fn poll(
    statuses: &(dyn SignatureStatusesProvider + Send + Sync),
    pending: &[(Signature, u64)],
    executions: &mut HashMap<u64, UnboundedSender<Landing>>,
) {
    for chunk in pending.chunks(MAX_STATUSES_PER_CALL) {
        let signatures: Vec<Signature> = chunk.iter().map(|(signature, _)| *signature).collect();
        let found = match statuses.get_signature_statuses(&signatures).await {
            Ok(response) => response.value,
            Err(err) => {
                log::error!(
                    "Failed to get transaction signatures statuses: {}",
                    redact_urls(&err.to_string())
                );
                continue;
            }
        };
        for (status, (signature, execution)) in found.into_iter().zip(chunk) {
            let Some(status) = status else { continue };
            let Some(landings) = executions.remove(execution) else {
                continue;
            };
            match &status.status {
                Ok(()) => log::info!("Transaction {signature} landed in the slot {}", status.slot),
                Err(err) => log::error!(
                    "Transaction {signature} landed in the slot {} and failed: {err}",
                    status.slot
                ),
            }
            let landing = Landing {
                signature: *signature,
                slot: status.slot,
                result: status.status,
            };
            if landings.send(landing).is_err() {
                log::warn!("Channel closed before {signature} was reported");
            }
        }
    }
}
