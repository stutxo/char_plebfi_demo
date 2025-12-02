use std::collections::HashSet;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::thread;

use clap::Parser;
use nostr_sdk::prelude::*;
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio::sync::{Mutex, mpsc};
use tracing::{error, info};

use crate::char_rpc::{CharRpc, decode_leader_payload, domain_to_char_hash};
use crate::dns::update_a_record;

mod char_rpc;
mod dns;

const BALLOT_FILENAME: &str = "ballot.dat";
const NOSTR_KIND: Kind = Kind::Custom(60067);

#[derive(Parser, Debug)]
struct Cli {
    #[arg(long)]
    zmq: String,
    #[arg(long)]
    bitcoinrpc: String,
    #[arg(long)]
    datadir: String,
}

type NostrPool = Arc<Mutex<Vec<String>>>;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().without_time().init();
    let args = Cli::parse();
    let dns_api_key = std::env::var("PDNS_API_KEY").expect("PDNS_API_KEY not set");
    let ballot_path = Path::new(&args.datadir).join(BALLOT_FILENAME);

    // 1. Connect to Char RPC
    let rpc = CharRpc::new(&args.datadir, &args.bitcoinrpc).unwrap();
    if !rpc.check_for_active_bonds().await.unwrap_or(false) {
        error!("No active bonds detected or wallet loaded, exiting.");
        return;
    }

    // 2. Compute the app key for the char_dns app
    let kv_key_hex = hex::encode(Sha256::digest(b"char_dns"));
    info!("char dns key hex: {}", kv_key_hex);

    let target_app = domain_to_char_hash(&kv_key_hex).expect("Hash failed");
    let nostr_pool: NostrPool = Arc::new(Mutex::new(Vec::new()));
    let (tx, rx) = mpsc::channel(100);

    // 3. Listen for dns events from nostr
    let nostr_task = tokio::spawn(nostr_task(nostr_pool.clone()));

    let zmq_args = args.zmq.clone();
    // 4. Wait for leader notifications over ZMQ to data in nostr pool
    thread::spawn(move || zmq_task(&zmq_args, target_app, tx));

    // 5. When leader, submit data from nostr pool to char via add_bamboo_kv rpc
    let leader_task = tokio::spawn(leader_task(
        rpc.clone(),
        kv_key_hex.clone(),
        nostr_pool.clone(),
        rx,
    ));

    // 6. Find decision rolls, processs any dns events found
    let roll_task = tokio::spawn(roll_task(
        rpc.clone(),
        kv_key_hex,
        nostr_pool.clone(),
        ballot_path,
        dns_api_key.clone(),
    ));

    if let Err(e) = tokio::try_join!(leader_task, roll_task, nostr_task) {
        error!("Task exited unexpectedly: {:?}", e);
    }
}

async fn nostr_task(nostr_pool: NostrPool) {
    let client = Client::new(Keys::generate());
    client.add_relay("wss://relay.damus.io").await.unwrap();
    client.add_relay("wss://nostr.wine").await.unwrap();
    client.add_relay("wss://nos.lol").await.unwrap();
    client.connect().await;

    let filter = Filter::new().kind(NOSTR_KIND).since(Timestamp::now());
    let sub_id = client.subscribe(filter, None).await.unwrap().val;

    client
        .handle_notifications(|n| async {
            if let RelayPoolNotification::Event {
                subscription_id,
                event,
                ..
            } = n
            {
                if subscription_id == sub_id {
                    info!("Nostr event received: {}", event.id);
                    nostr_pool.lock().await.push(event.as_json());
                }
            }
            Ok(false)
        })
        .await
        .unwrap();
}

// Char needs to be build with `cmake -DWITH_ZMQ=ON -B build`, `zmqpubleader=tcp://127.0.0.1:28332` and app scheduled with
// bitcoin-cli config_app_registry_schedule 17f24e073d4eb05ef1779b2ea4c9895e7ad0d25e08d188ef5818aa9e1112f5ef true
fn zmq_task(endpoint: &str, target_app: [u8; 32], tx: mpsc::Sender<(u64, Vec<u8>)>) {
    let ctx = zmq::Context::new();
    let socket = ctx.socket(zmq::SUB).expect("ZMQ socket failed");
    socket.connect(endpoint).expect("ZMQ connect failed");
    socket
        .set_subscribe(b"leader")
        .expect("ZMQ subscribe failed");

    info!("[ZMQ] Listening...");
    loop {
        if let Ok(Ok(topic)) = socket.recv_string(0) {
            if topic == "leader" {
                if let Ok(payload) = socket.recv_bytes(0) {
                    let _ = socket.recv_bytes(0); // seq
                    if let Ok((ballot, domain)) = decode_leader_payload(&payload) {
                        if domain == target_app {
                            let _ = tx.blocking_send((ballot, domain.to_vec()));
                        }
                    }
                }
            }
        }
    }
}

async fn leader_task(
    rpc: CharRpc,
    key_hex: String,
    nostr_pool: NostrPool,
    mut rx: mpsc::Receiver<(u64, Vec<u8>)>,
) {
    info!("Waiting for ZMQ events...");
    let mut last_ballot = None;

    while let Some((ballot, _)) = rx.recv().await {
        if last_ballot == Some(ballot) {
            continue;
        }
        last_ballot = Some(ballot);

        let mut events = Vec::new();
        std::mem::swap(&mut *nostr_pool.lock().await, &mut events);

        info!(
            "You are the leader for ballot {}, Submitting vote with {} events",
            ballot,
            events.len()
        );
        if !events.is_empty() {
            info!("[ELECTED LEADER] Submitting Event IDs:");
            for e in &events {
                if let Ok(ev) = serde_json::from_str::<Event>(e) {
                    info!(" - {}", ev.id);
                }
            }
        }
        if let Err(e) = rpc.add_bamboo_kv(key_hex.clone(), events, ballot).await {
            error!("Failed to send KV: {:?}", e);
        }
        info!("Waiting for next leader election...");
    }
}

async fn roll_task(
    rpc: CharRpc,
    key_hex: String,
    nostr_pool: NostrPool,
    ballot_path: PathBuf,
    dns_api_key: String,
) {
    let mut ballot = load_ballot_num_from_disk(&ballot_path).await;
    info!("[INIT] Resuming from ballot: {}", ballot);

    loop {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        match rpc.get_referendum_decision_roll(&key_hex, ballot).await {
            Ok(Some(json)) => {
                let events_arr = json.as_array();
                if events_arr.map_or(true, |a| a.is_empty()) {
                    // Write ballot to disk so we can start up again from here
                    ballot = save_ballot_num_to_disk(&ballot_path, ballot + 1).await;
                    continue;
                }

                info!("[ROLL] Decision roll found for ballot: {}", ballot);

                let events: Vec<Event> = events_arr
                    .unwrap()
                    .iter()
                    .filter_map(|i| i.as_str().and_then(|s| serde_json::from_str(s).ok()))
                    .collect();

                let ids: HashSet<_> = events.iter().map(|e| e.id.to_string()).collect();
                if !ids.is_empty() {
                    let mut p = nostr_pool.lock().await;
                    // Remove any events from our nostr pool that have been processed by this decision roll
                    p.retain(|s| {
                        serde_json::from_str::<Event>(s)
                            .map_or(true, |e| !ids.contains(&e.id.to_string()))
                    });
                }

                // Add records to DNS
                for event in events {
                    process_dns_event(&event, &dns_api_key).await;
                }

                ballot = save_ballot_num_to_disk(&ballot_path, ballot + 1).await;
            }
            Ok(None) => {}
            Err(e) => error!("RPC Error on ballot {}: {:?}", ballot, e),
        }
    }
}

async fn process_dns_event(event: &Event, dns_api_key: &str) {
    // verify signature for nostr note
    if event.verify().is_err() {
        return error!("[DNS] Skip {}: Invalid Sig", event.id);
    }
    match (Ipv4Addr::from_str(&event.content), event.pubkey.to_bech32()) {
        (Ok(ip), Ok(label)) => {
            info!("[DNS] Updating key: {} with ip: {}", label, ip);
            // update the A record for the requested npub, using the provided IP
            if let Err(e) = update_a_record(&label, &ip.to_string(), &dns_api_key).await {
                error!("Failed to update A record: {:?}", e);
            }
        }
        _ => error!("[DNS] Invalid IP or Pubkey for event {}", event.id),
    }
}

async fn load_ballot_num_from_disk(path: &Path) -> u64 {
    if let Ok(mut f) = tokio::fs::File::open(path).await {
        let mut buf = [0u8; 8];
        if f.read_exact(&mut buf).await.is_ok() {
            return u64::from_le_bytes(buf);
        }
    }
    0
}

async fn save_ballot_num_to_disk(path: &Path, num: u64) -> u64 {
    if let Err(e) = tokio::fs::write(path, num.to_le_bytes()).await {
        error!("[DISK] Failed to save ballot: {:?}", e);
    }
    num
}
