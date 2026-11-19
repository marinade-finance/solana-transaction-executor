use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, OnceLock,
    },
    time::Duration,
};

use solana_hash::Hash;
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction_error::TransactionError;
use tokio::{
    sync::{
        mpsc::{self, UnboundedReceiver, UnboundedSender},
        oneshot, Mutex,
    },
    time::{Instant, MissedTickBehavior},
};

use crate::{
    redact::redact_urls, SendTransactionMeta, SendTransactionProvider, SignatureStatusesProvider,
    TransactionExecutorConfig,
};

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
    Send(Queued),
}

struct Queued {
    execution: u64,
    transaction: VersionedTransaction,
    attempt: u32,
    /// `None` when the RPC took it, else its error with the endpoint's host only.
    sent: oneshot::Sender<Option<String>>,
}

#[derive(Clone)]
struct Providers {
    statuses: Arc<dyn SignatureStatusesProvider + Send + Sync>,
    send: Arc<dyn SendTransactionProvider + Send + Sync>,
}

type Cached<T> = Mutex<Option<(T, Instant)>>;

/// The executor's shared state: one cache for every execution, so parallel ones share RPC reads.
pub(crate) struct SenderCore {
    providers: Providers,
    refresh: Duration,
    status_interval: Duration,
    send_interval: Duration,
    blockhash: Cached<(Hash, u64)>,
    block_height: Cached<u64>,
    executions: AtomicU64,
    commands: OnceLock<UnboundedSender<Command>>,
}

impl SenderCore {
    pub(crate) fn new(
        config: &TransactionExecutorConfig,
        statuses: Arc<dyn SignatureStatusesProvider + Send + Sync>,
        send: Arc<dyn SendTransactionProvider + Send + Sync>,
    ) -> Self {
        Self {
            providers: Providers { statuses, send },
            refresh: config.blockhash_refresh_interval,
            status_interval: config.signature_status_check_interval,
            send_interval: Duration::from_secs(1) / config.max_sends_per_second.max(1),
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
            tokio::spawn(run(
                self.providers.clone(),
                self.status_interval,
                self.send_interval,
                receiver,
            ));
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

    /// Queued behind every earlier send of the executor; the signature is watched once sent.
    pub(crate) async fn send_transaction(
        &self,
        execution: u64,
        transaction: &VersionedTransaction,
        attempt: u32,
    ) -> Option<String> {
        let (sent, result) = oneshot::channel();
        self.send(Command::Send(Queued {
            execution,
            transaction: transaction.clone(),
            attempt,
            sent,
        }));
        result
            .await
            .unwrap_or_else(|_| Some("The sender core stopped".into()))
    }

    pub(crate) async fn latest_blockhash(&self) -> anyhow::Result<(Hash, u64)> {
        cached(
            &self.blockhash,
            self.refresh,
            self.providers.statuses.latest_blockhash(),
        )
        .await
    }

    /// A stale height is lower than the real one, so it can only hold a retry longer.
    pub(crate) async fn block_height(&self) -> anyhow::Result<u64> {
        cached(
            &self.block_height,
            self.refresh,
            self.providers.statuses.block_height(),
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
    providers: Providers,
    interval: Duration,
    send_interval: Duration,
    mut commands: UnboundedReceiver<Command>,
) {
    let mut executions: HashMap<u64, UnboundedSender<Landing>> = HashMap::new();
    let mut pending: Vec<(Signature, u64)> = Vec::new();
    let mut queue: VecDeque<Queued> = VecDeque::new();
    let mut tick = tokio::time::interval_at(Instant::now() + interval, interval);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut send_tick = tokio::time::interval(send_interval);
    send_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
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
                Some(Command::Send(send)) => queue.push_back(send),
                None => return,
            },
            _ = send_tick.tick(), if !queue.is_empty() => {
                let Some(send) = queue.pop_front() else { continue };
                let watched = (send.transaction.signatures[0], send.execution);
                if !pending.contains(&watched) {
                    pending.push(watched);
                }
                let provider = providers.send.clone();
                tokio::spawn(async move {
                    let meta = SendTransactionMeta { attempt: send.attempt };
                    let error = provider
                        .send_transaction(&send.transaction, meta)
                        .await
                        .err()
                        .map(|err| redact_urls(&err.to_string()));
                    let _ = send.sent.send(error);
                });
            }
            _ = tick.tick() => {
                executions.retain(|_, landings| !landings.is_closed());
                pending.retain(|(_, execution)| executions.contains_key(execution));
                poll(providers.statuses.as_ref(), &pending, &mut executions).await;
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
