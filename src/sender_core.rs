use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, OnceLock,
    },
    time::Duration,
};

use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction_error::TransactionError;
use tokio::{
    sync::{
        mpsc::{self, UnboundedReceiver, UnboundedSender},
        oneshot, Mutex,
    },
    task::JoinHandle,
    time::{Instant, MissedTickBehavior},
};

use crate::{
    redact::redact_urls, SendTransactionMeta, SendTransactionProvider, SignatureStatusesProvider,
    TransactionExecutorConfig,
};

/// The protocol maximum of one `getSignatureStatuses` call.
const MAX_STATUSES_PER_CALL: usize = 256;
const MIN_PERIOD: Duration = Duration::from_millis(1);
/// A custom statuses provider may have no HTTP timeout; one hung call stalls every execution.
const STATUS_CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Keeps `Instant::now() + interval` from overflowing.
const MAX_STATUS_INTERVAL: Duration = Duration::from_secs(60 * 60);

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
    last_valid_block_height: u64,
    /// `None` when the RPC took it, else its error with the endpoint's host only.
    sent: oneshot::Sender<Option<String>>,
}

struct Broadcast {
    transaction: VersionedTransaction,
    attempt: u32,
    last_valid_block_height: u64,
    last_sent: Instant,
    queued: bool,
}

struct Height {
    statuses: Arc<dyn SignatureStatusesProvider + Send + Sync>,
    refresh: Duration,
    cached: Mutex<Option<(u64, Instant)>>,
}

impl Height {
    async fn read(&self) -> anyhow::Result<u64> {
        tokio::time::timeout(STATUS_CALL_TIMEOUT, async {
            let mut cached = self.cached.lock().await;
            if let Some((height, read_at)) = *cached {
                if read_at.elapsed() < self.refresh {
                    return Ok(height);
                }
            }
            let height = self.statuses.block_height().await?;
            *cached = Some((height, Instant::now()));
            Ok(height)
        })
        .await
        .map_err(|_| anyhow::anyhow!("The block height read timed out"))?
    }
}

struct Polled {
    landed: Vec<(u64, Landing)>,
    height: Option<u64>,
}

struct Loop {
    height: Arc<Height>,
    send: Arc<dyn SendTransactionProvider + Send + Sync>,
    status_interval: Duration,
    send_interval: Duration,
    rebroadcast_interval: Option<Duration>,
}

/// The executor's shared state: one loop and one block height for every execution.
pub(crate) struct SenderCore {
    core_loop: OnceLock<UnboundedSender<Command>>,
    height: Arc<Height>,
    send: Arc<dyn SendTransactionProvider + Send + Sync>,
    status_interval: Duration,
    send_interval: Duration,
    rebroadcast_interval: Option<Duration>,
    executions: AtomicU64,
}

impl SenderCore {
    pub(crate) fn new(
        config: &TransactionExecutorConfig,
        statuses: Arc<dyn SignatureStatusesProvider + Send + Sync>,
        send: Arc<dyn SendTransactionProvider + Send + Sync>,
    ) -> Self {
        Self {
            core_loop: OnceLock::new(),
            height: Arc::new(Height {
                statuses,
                refresh: config.block_height_refresh_interval,
                cached: Mutex::new(None),
            }),
            send,
            status_interval: config
                .signature_status_check_interval
                .clamp(MIN_PERIOD, MAX_STATUS_INTERVAL),
            send_interval: (Duration::from_secs(1) / config.max_sends_per_second.max(1))
                .max(MIN_PERIOD),
            rebroadcast_interval: config.rebroadcast_interval,
            executions: AtomicU64::new(0),
        }
    }

    /// The loop starts on the first command.
    fn send(&self, command: Command) {
        let commands = self.core_loop.get_or_init(|| {
            let (commands, receiver) = mpsc::unbounded_channel();
            let core_loop = Loop {
                height: self.height.clone(),
                send: self.send.clone(),
                status_interval: self.status_interval,
                send_interval: self.send_interval,
                rebroadcast_interval: self.rebroadcast_interval,
            };
            tokio::spawn(core_loop.run(receiver));
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
        last_valid_block_height: u64,
    ) -> Option<String> {
        let (sent, result) = oneshot::channel();
        self.send(Command::Send(Queued {
            execution,
            transaction: transaction.clone(),
            attempt,
            last_valid_block_height,
            sent,
        }));
        result
            .await
            .unwrap_or_else(|_| Some("The sender core stopped".into()))
    }

    /// A stale height is lower than the real one, so it can only hold a retry longer.
    pub(crate) async fn block_height(&self) -> anyhow::Result<u64> {
        self.height.read().await
    }
}

fn prune(
    executions: &HashMap<u64, UnboundedSender<Landing>>,
    pending: &mut Vec<(Signature, u64)>,
    broadcasts: &mut HashMap<Signature, Broadcast>,
    rebroadcasts: &mut VecDeque<Signature>,
) {
    pending.retain(|(_, execution)| executions.contains_key(execution));
    broadcasts.retain(|signature, _| pending.iter().any(|(pending, _)| pending == signature));
    rebroadcasts.retain(|signature| broadcasts.contains_key(signature));
}

fn watch(pending: &mut Vec<(Signature, u64)>, signature: Signature, execution: u64) {
    if !pending.contains(&(signature, execution)) {
        pending.push((signature, execution));
    }
}

impl Loop {
    async fn run(self, mut commands: UnboundedReceiver<Command>) {
        let mut executions: HashMap<u64, UnboundedSender<Landing>> = HashMap::new();
        let mut pending: Vec<(Signature, u64)> = Vec::new();
        let mut broadcasts: HashMap<Signature, Broadcast> = HashMap::new();
        let mut first_sends: VecDeque<Queued> = VecDeque::new();
        let mut rebroadcasts: VecDeque<Signature> = VecDeque::new();
        let mut polling: Option<JoinHandle<Polled>> = None;
        let mut polled_executions: Vec<u64> = Vec::new();
        let interval = self.status_interval;
        let mut tick = tokio::time::interval_at(Instant::now() + interval, interval);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut send_tick = tokio::time::interval(self.send_interval);
        send_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            let sendable = !first_sends.is_empty() || !rebroadcasts.is_empty();
            tokio::select! {
                biased;
                command = commands.recv() => match command {
                    Some(Command::Register { execution, landings }) => {
                        executions.insert(execution, landings);
                    }
                    Some(Command::Watch { execution, signature }) => {
                        watch(&mut pending, signature, execution);
                    }
                    Some(Command::Send(send)) => first_sends.push_back(send),
                    None => return,
                },
                _ = send_tick.tick(), if sendable => {
                    first_sends.retain(|send| !send.sent.is_closed());
                    if let Some(send) = first_sends.pop_front() {
                        let signature = send.transaction.signatures[0];
                        watch(&mut pending, signature, send.execution);
                        broadcasts.insert(signature, Broadcast {
                            transaction: send.transaction.clone(),
                            attempt: send.attempt,
                            last_valid_block_height: send.last_valid_block_height,
                            last_sent: Instant::now(),
                            queued: false,
                        });
                        let provider = self.send.clone();
                        tokio::spawn(async move {
                            let meta = SendTransactionMeta { attempt: send.attempt };
                            let error = provider
                                .send_transaction(&send.transaction, meta)
                                .await
                                .err()
                                .map(|err| redact_urls(&err.to_string()));
                            let _ = send.sent.send(error);
                        });
                    } else if let Some(signature) = rebroadcasts.pop_front() {
                        if let Some(broadcast) = broadcasts.get_mut(&signature) {
                            broadcast.last_sent = Instant::now();
                            broadcast.queued = false;
                            let transaction = broadcast.transaction.clone();
                            let meta = SendTransactionMeta { attempt: broadcast.attempt };
                            let provider = self.send.clone();
                            tokio::spawn(async move {
                                if let Err(err) = provider.send_transaction(&transaction, meta).await {
                                    log::debug!(
                                        "Rebroadcast of {signature} failed: {}",
                                        redact_urls(&err.to_string())
                                    );
                                }
                            });
                        }
                    }
                }
                joined = async { polling.as_mut().expect("guarded by is_some").await }, if polling.is_some() => {
                    polling = None;
                    let polled = match joined {
                        Ok(polled) => polled,
                        Err(err) => {
                            log::error!("The status poll stopped: {err}");
                            for execution in polled_executions.drain(..) {
                                executions.remove(&execution);
                            }
                            continue;
                        }
                    };
                    for (execution, landing) in polled.landed {
                        report(&mut executions, execution, landing);
                    }
                    prune(&executions, &mut pending, &mut broadcasts, &mut rebroadcasts);
                    if let Some(height) = polled.height {
                        self.queue_rebroadcasts(height, &mut broadcasts, &mut rebroadcasts);
                    }
                    polled_executions.clear();
                }
                _ = tick.tick(), if polling.is_none() => {
                    executions.retain(|_, landings| !landings.is_closed());
                    prune(&executions, &mut pending, &mut broadcasts, &mut rebroadcasts);
                    if pending.is_empty() {
                        continue;
                    }
                    polled_executions = pending.iter().map(|(_, execution)| *execution).collect();
                    let read_height = self.rebroadcast_interval.is_some_and(|every| {
                        broadcasts.values().any(|broadcast| broadcast.last_sent.elapsed() >= every)
                    });
                    polling = Some(tokio::spawn(poll(
                        self.height.clone(),
                        pending.clone(),
                        read_height,
                    )));
                }
            }
        }
    }

    /// Drops queued rebroadcasts past their validity and queues the due ones still valid.
    fn queue_rebroadcasts(
        &self,
        height: u64,
        broadcasts: &mut HashMap<Signature, Broadcast>,
        rebroadcasts: &mut VecDeque<Signature>,
    ) {
        let Some(every) = self.rebroadcast_interval else {
            return;
        };
        rebroadcasts.retain(|signature| {
            let broadcast = broadcasts
                .get_mut(signature)
                .expect("retained with broadcasts");
            broadcast.queued = height <= broadcast.last_valid_block_height;
            broadcast.queued
        });
        for (signature, broadcast) in broadcasts.iter_mut() {
            if !broadcast.queued
                && broadcast.last_sent.elapsed() >= every
                && height <= broadcast.last_valid_block_height
            {
                broadcast.queued = true;
                rebroadcasts.push_back(*signature);
            }
        }
    }
}

fn report(
    executions: &mut HashMap<u64, UnboundedSender<Landing>>,
    execution: u64,
    landing: Landing,
) {
    let Some(landings) = executions.remove(&execution) else {
        return;
    };
    let signature = landing.signature;
    match &landing.result {
        Ok(()) => log::info!(
            "Transaction {signature} landed in the slot {}",
            landing.slot
        ),
        Err(err) => log::error!(
            "Transaction {signature} landed in the slot {} and failed: {err}",
            landing.slot
        ),
    }
    if landings.send(landing).is_err() {
        log::warn!("Channel closed before {signature} was reported");
    }
}

async fn poll(height: Arc<Height>, pending: Vec<(Signature, u64)>, read_height: bool) -> Polled {
    let mut landed = Vec::new();
    for chunk in pending.chunks(MAX_STATUSES_PER_CALL) {
        let signatures: Vec<Signature> = chunk.iter().map(|(signature, _)| *signature).collect();
        let call = height.statuses.get_signature_statuses(&signatures);
        let found = match tokio::time::timeout(STATUS_CALL_TIMEOUT, call).await {
            Ok(Ok(response)) => response.value,
            Err(_) => {
                log::error!("Failed to get transaction signatures statuses: timed out");
                continue;
            }
            Ok(Err(err)) => {
                log::error!(
                    "Failed to get transaction signatures statuses: {}",
                    redact_urls(&err.to_string())
                );
                continue;
            }
        };
        for (status, (signature, execution)) in found.into_iter().zip(chunk) {
            if let Some(status) = status {
                let landing = Landing {
                    signature: *signature,
                    slot: status.slot,
                    result: status.status,
                };
                landed.push((*execution, landing));
            }
        }
    }
    let height = match read_height {
        true => match height.read().await {
            Ok(height) => Some(height),
            Err(err) => {
                log::warn!(
                    "No rebroadcast this round, the block height read failed: {}",
                    redact_urls(&err.to_string())
                );
                None
            }
        },
        false => None,
    };
    Polled { landed, height }
}
