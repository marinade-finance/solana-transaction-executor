use std::{future::Future, sync::Arc, time::Duration};

use solana_hash::Hash;
use tokio::{sync::Mutex, time::Instant};

use crate::SignatureStatusesProvider;

type Cached<T> = Mutex<Option<(T, Instant)>>;

/// The executor's shared state: one cache for every execution, so parallel ones share RPC reads.
pub(crate) struct SenderCore {
    statuses: Arc<dyn SignatureStatusesProvider + Send + Sync>,
    refresh: Duration,
    blockhash: Cached<(Hash, u64)>,
    block_height: Cached<u64>,
}

impl SenderCore {
    pub(crate) fn new(
        statuses: Arc<dyn SignatureStatusesProvider + Send + Sync>,
        refresh: Duration,
    ) -> Self {
        Self {
            statuses,
            refresh,
            blockhash: Mutex::new(None),
            block_height: Mutex::new(None),
        }
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
