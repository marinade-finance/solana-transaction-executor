use std::{
    net::TcpListener,
    process::{Child, Command, Stdio},
    sync::Arc,
    time::Duration,
};

use futures_util::stream;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_hash::Hash;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_message::{v1, VersionedMessage};
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction_error::TransactionError;
use solana_transaction_executor::{
    priority_fee_lamports, AttemptTransaction, DefaultProvider, NotLanded, Outcome,
    SimulateTransactionProvider, TransactionExecutor, TransactionExecutorBuilder,
    TransactionExecutorConfig,
};

const VERSION: &str = "surfpool 1.6.1";
const SLOT_MS: u64 = 50;
const TRANSFER_LAMPORTS: u64 = 1_000_000;
const COMPUTE_UNIT_LIMIT: u32 = 10_000;
const LOADED_BYTES_LIMIT: u32 = 128 * 1024;
const RATE_MICRO_LAMPORTS_PER_CU: u64 = 5_000;

struct Surfpool {
    child: Child,
    rpc: Arc<RpcClient>,
    payer: Keypair,
}

impl Drop for Surfpool {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn binary() -> String {
    std::env::var("SURFPOOL").unwrap_or_else(|_| "surfpool".into())
}

fn free_port_pair() -> u16 {
    loop {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        if port < u16::MAX && TcpListener::bind(("127.0.0.1", port + 1)).is_ok() {
            return port;
        }
    }
}

async fn start() -> Surfpool {
    let version = Command::new(binary()).arg("--version").output().unwrap();
    assert_eq!(String::from_utf8_lossy(&version.stdout).trim(), VERSION);

    let payer = Keypair::new();
    let port = free_port_pair();
    let child = Command::new(binary())
        .args(["start", "--offline", "--no-deploy", "--yes", "--ci"])
        .args(["--db", ":memory:", "--host", "127.0.0.1"])
        .args([
            "--port",
            &port.to_string(),
            "--ws-port",
            &(port + 1).to_string(),
        ])
        .args(["--slot-time", &SLOT_MS.to_string()])
        .args(["--airdrop", &payer.pubkey().to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let rpc = Arc::new(RpcClient::new_with_commitment(
        format!("http://127.0.0.1:{port}"),
        CommitmentConfig::confirmed(),
    ));
    let surfpool = Surfpool { child, rpc, payer };
    for _ in 0..200 {
        if surfpool
            .rpc
            .get_balance(&surfpool.payer.pubkey())
            .await
            .unwrap_or(0)
            > 0
        {
            return surfpool;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("Surfpool on port {port} did not start with the payer funded");
}

fn executor(surfpool: &Surfpool) -> TransactionExecutor {
    TransactionExecutorBuilder::new()
        .with_default_providers(surfpool.rpc.clone())
        .with_transaction_executor_config(TransactionExecutorConfig {
            signature_status_check_interval: Duration::from_millis(100),
            transaction_send_interval: Duration::from_secs(1),
            waiting_time_after_last_send: Duration::from_secs(60),
            max_attempts: 3,
            ..TransactionExecutorConfig::default()
        })
        .build()
}

fn transfer(from: &Pubkey, to: &Pubkey) -> Instruction {
    let mut data = 2u32.to_le_bytes().to_vec();
    data.extend_from_slice(&TRANSFER_LAMPORTS.to_le_bytes());
    Instruction::new_with_bytes(
        Pubkey::default(),
        &data,
        vec![AccountMeta::new(*from, true), AccountMeta::new(*to, false)],
    )
}

fn message(
    payer: &Keypair,
    to: &Pubkey,
    blockhash: Hash,
    config: v1::TransactionConfig,
) -> v1::Message {
    v1::Message::try_compile_with_config(
        &payer.pubkey(),
        &[transfer(&payer.pubkey(), to)],
        blockhash,
        config,
    )
    .unwrap()
}

async fn signed_transfer(
    surfpool: &Surfpool,
    executor: &TransactionExecutor,
    to: &Pubkey,
) -> AttemptTransaction {
    let (blockhash, last_valid_block_height) = executor.latest_blockhash().await.unwrap();
    let config = v1::TransactionConfig::empty()
        .with_compute_unit_limit(COMPUTE_UNIT_LIMIT)
        .with_loaded_accounts_data_size_limit(LOADED_BYTES_LIMIT)
        .with_priority_fee(priority_fee_lamports(
            RATE_MICRO_LAMPORTS_PER_CU,
            COMPUTE_UNIT_LIMIT,
        ));
    let message = message(&surfpool.payer, to, blockhash, config);
    AttemptTransaction {
        transaction: VersionedTransaction::try_new(
            VersionedMessage::V1(message),
            &[&surfpool.payer],
        )
        .unwrap(),
        last_valid_block_height,
        sent_before: false,
    }
}

async fn wait_past(surfpool: &Surfpool, block_height: u64) {
    while surfpool.rpc.get_block_height().await.unwrap() <= block_height {
        tokio::time::sleep(Duration::from_millis(SLOT_MS)).await;
    }
}

#[tokio::test]
#[ignore = "needs Surfpool 1.6.1: SURFPOOL=<binary> cargo test --test surfpool -- --ignored"]
async fn a_v1_transfer_lands_and_pays_its_priority_fee() {
    let surfpool = start().await;
    let executor = executor(&surfpool);
    let to = Pubkey::new_unique();
    let attempt = signed_transfer(&surfpool, &executor, &to).await;
    let signature = attempt.transaction.signatures[0];
    let before = surfpool
        .rpc
        .get_balance(&surfpool.payer.pubkey())
        .await
        .unwrap();

    let execution = executor
        .execute_transaction(stream::iter([Ok(attempt)]))
        .await;

    assert!(
        matches!(execution.outcome, Outcome::Landed { signature: landed, .. } if landed == signature),
        "{execution:?}"
    );
    assert_eq!(
        surfpool.rpc.get_balance(&to).await.unwrap(),
        TRANSFER_LAMPORTS
    );
    let paid = before
        - surfpool
            .rpc
            .get_balance(&surfpool.payer.pubkey())
            .await
            .unwrap()
        - TRANSFER_LAMPORTS;
    let priority_fee = priority_fee_lamports(RATE_MICRO_LAMPORTS_PER_CU, COMPUTE_UNIT_LIMIT);
    assert_eq!(
        paid,
        5_000 + priority_fee,
        "base fee of one signature plus the priority fee"
    );
}

#[tokio::test]
#[ignore = "needs Surfpool 1.6.1: SURFPOOL=<binary> cargo test --test surfpool -- --ignored"]
async fn an_unsigned_v1_transfer_without_limits_is_sized_by_the_simulation() {
    let surfpool = start().await;
    let executor = executor(&surfpool);
    let unlimited = VersionedTransaction {
        signatures: vec![Default::default()],
        message: VersionedMessage::V1(message(
            &surfpool.payer,
            &Pubkey::new_unique(),
            Hash::default(),
            v1::TransactionConfig::empty(),
        )),
    };

    let raw = DefaultProvider::new(surfpool.rpc.clone())
        .simulate_unsigned_transaction(&unlimited)
        .await
        .unwrap()
        .value;
    let simulation = executor.simulate(&unlimited).await.unwrap();

    assert_eq!(
        raw.err.map(TransactionError::from),
        Some(TransactionError::MaxLoadedAccountsDataSizeExceeded)
    );
    assert_eq!(simulation.error, None, "{simulation:?}");
    assert!(
        simulation.units_consumed.is_some_and(|units| units > 0),
        "{simulation:?}"
    );
}

#[tokio::test]
#[ignore = "needs Surfpool 1.6.1: SURFPOOL=<binary> cargo test --test surfpool -- --ignored"]
async fn a_sent_before_attempt_that_landed_and_expired_is_found_and_no_fresh_one_is_sent() {
    let surfpool = start().await;
    let executor = executor(&surfpool);
    let to = Pubkey::new_unique();
    let first = signed_transfer(&surfpool, &executor, &to).await;
    let signature = first.transaction.signatures[0];
    let resumed = AttemptTransaction {
        transaction: first.transaction.clone(),
        last_valid_block_height: first.last_valid_block_height,
        sent_before: true,
    };
    let landed = executor
        .execute_transaction(stream::iter([Ok(first)]))
        .await;
    assert!(
        matches!(landed.outcome, Outcome::Landed { .. }),
        "{landed:?}"
    );
    wait_past(&surfpool, resumed.last_valid_block_height).await;
    let fresh = signed_transfer(&surfpool, &executor, &to).await;

    let execution = executor
        .execute_transaction(stream::iter([Ok(resumed), Ok(fresh)]))
        .await;

    assert!(
        matches!(execution.outcome, Outcome::Landed { signature: landed, .. } if landed == signature),
        "{execution:?}"
    );
    assert_eq!(execution.attempts.len(), 1);
    assert_eq!(
        surfpool.rpc.get_balance(&to).await.unwrap(),
        TRANSFER_LAMPORTS
    );
}

#[tokio::test]
#[ignore = "needs Surfpool 1.6.1: SURFPOOL=<binary> cargo test --test surfpool -- --ignored"]
async fn without_sent_before_a_landed_and_expired_attempt_is_sent_twice() {
    let surfpool = start().await;
    let executor = executor(&surfpool);
    let to = Pubkey::new_unique();
    let first = signed_transfer(&surfpool, &executor, &to).await;
    let unmarked = AttemptTransaction {
        transaction: first.transaction.clone(),
        last_valid_block_height: first.last_valid_block_height,
        sent_before: false,
    };
    executor
        .execute_transaction(stream::iter([Ok(first)]))
        .await;
    wait_past(&surfpool, unmarked.last_valid_block_height).await;
    let fresh = signed_transfer(&surfpool, &executor, &to).await;

    executor
        .execute_transaction(stream::iter([Ok(unmarked), Ok(fresh)]))
        .await;

    assert_eq!(
        surfpool.rpc.get_balance(&to).await.unwrap(),
        2 * TRANSFER_LAMPORTS
    );
}

#[tokio::test]
#[ignore = "needs Surfpool 1.6.1: SURFPOOL=<binary> cargo test --test surfpool -- --ignored"]
async fn a_sent_before_attempt_that_expired_unsent_is_proven_then_replaced() {
    let surfpool = start().await;
    let executor = executor(&surfpool);
    let to = Pubkey::new_unique();
    let never_sent = AttemptTransaction {
        sent_before: true,
        ..signed_transfer(&surfpool, &executor, &to).await
    };
    let stale = never_sent.transaction.signatures[0];
    wait_past(&surfpool, never_sent.last_valid_block_height).await;
    let fresh = signed_transfer(&surfpool, &executor, &to).await;
    let replacement = fresh.transaction.signatures[0];

    let execution = executor
        .execute_transaction(stream::iter([Ok(never_sent), Ok(fresh)]))
        .await;

    assert!(
        matches!(execution.outcome, Outcome::Landed { signature, .. } if signature == replacement),
        "{execution:?}"
    );
    assert_eq!(execution.attempts.len(), 2);
    assert_eq!(execution.attempts[0].signature, stale);
    assert!(execution.attempts[0].sent);
    assert_eq!(
        surfpool.rpc.get_balance(&to).await.unwrap(),
        TRANSFER_LAMPORTS
    );
}

#[tokio::test]
#[ignore = "needs Surfpool 1.6.1: SURFPOOL=<binary> cargo test --test surfpool -- --ignored"]
async fn a_refused_attempt_is_not_landed_and_not_sent() {
    let surfpool = start().await;
    let executor = executor(&surfpool);
    let (blockhash, last_valid_block_height) = executor.latest_blockhash().await.unwrap();
    let broke = Keypair::new();
    let config = v1::TransactionConfig::empty()
        .with_compute_unit_limit(COMPUTE_UNIT_LIMIT)
        .with_loaded_accounts_data_size_limit(LOADED_BYTES_LIMIT);
    let message = message(&broke, &Pubkey::new_unique(), blockhash, config);
    let attempt = AttemptTransaction {
        transaction: VersionedTransaction::try_new(VersionedMessage::V1(message), &[&broke])
            .unwrap(),
        last_valid_block_height,
        sent_before: false,
    };

    let execution = executor
        .execute_transaction(stream::iter([Ok(attempt)]))
        .await;

    assert!(
        matches!(
            execution.outcome,
            Outcome::NotLanded(NotLanded::Refused { .. })
        ),
        "{execution:?}"
    );
    assert!(!execution.attempts[0].sent);
}
