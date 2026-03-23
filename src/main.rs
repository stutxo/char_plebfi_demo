use std::collections::HashSet;
use std::error::Error as StdError;
use std::io::Read;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use char_sdk::{
    BitcoindAsyncTransport, CharBallotHandlers, CharRpcTransport, DomainId, ReconcileRequest,
    TransportError,
};
use clap::Parser;
use nostr_sdk::prelude::*;
use sha2::{Digest, Sha256};
use tracing::{error, info, warn};

use crate::dns::update_a_record;

mod dns;

const BALLOT_FILENAME: &str = "ballot.dat";
const NOSTR_KIND: Kind = Kind::Custom(60067);

#[derive(Parser, Debug)]
struct Cli {
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

    let last_ballot = load_ballot_num_from_disk(&ballot_path);
    info!("Starting Charter with last ballot number: {}", last_ballot);

    let cookie_path = Path::new(&args.datadir).join(".cookie");

    // 1. Connect to Char RPC
    let char_rpc =
        BitcoindAsyncTransport::from_cookie_file(&args.bitcoinrpc, &cookie_path).unwrap();
    // See if there is a bond loaded in wallet
    let bond = match char_rpc.get_all_char_bonds(0).await {
        Ok(bonds) if bonds.is_empty() => {
            error!("No active bonds detected or wallet loaded, exiting.");
            return;
        }
        Err(e) => {
            error!("Failed to get bonds: {:?}", e);
            return;
        }
        Ok(bonds) => bonds.into_iter().next().unwrap(),
    };

    // 2. Compute the app key for the char_dns app
    let kv_key_hex = hex::encode(Sha256::digest(b"char_dns"));
    info!("char dns key hex: {}", kv_key_hex);

    let target_app = DomainId::from_preimage_hex(&kv_key_hex).unwrap();

    match char_rpc
        .domain_registry_schedule(&kv_key_hex, "char_dns")
        .await
    {
        Ok(result) if result.success => {
            info!("Scheduled char_dns domain in the node registry.");
        }
        Ok(_) => {
            error!("Scheduling char_dns domain returned success=false.");
            return;
        }
        Err(e) => {
            error!("Failed to schedule char_dns domain: {:?}", e);
            return;
        }
    }

    let domain_tip = match char_rpc.get_domain_info(&kv_key_hex).await {
        Ok(info) => Some(info.tip_height),
        Err(e) if is_empty_domain_error(&e) => {
            if last_ballot == 0 {
                info!("No decided ballots yet for this domain. Skipping startup reconcile.");
            } else {
                warn!(
                    "Domain has no decided ballots yet, but local state is at ballot {}. Continuing without reconcile.",
                    last_ballot
                );
            }
            None
        }
        Err(e) => {
            error!("Failed to get domain info: {:?}", e);
            return;
        }
    };

    if let Some(domain_tip) = domain_tip {
        let reconcile_result = char_sdk::reconcile(
            &char_rpc,
            &kv_key_hex,
            ReconcileRequest {
                domain: target_app,
                from_ballot: last_ballot,
                to_ballot: domain_tip,
                max_fetch: 100,
            },
        )
        .await;
        match reconcile_result {
            Ok(result) => {
                info!("Reconcile completed. Next ballot: {}", result.next_ballot);
                save_ballot_num_to_disk(&ballot_path, result.next_ballot);
            }
            Err(e) => error!("Reconcile failed: {:?}", e),
        }
    }
    let nostr_pool: NostrPool = Arc::new(Mutex::new(Vec::new()));

    // 3. Listen for dns events from nostr
    let nostr_task = tokio::spawn(nostr_task(nostr_pool.clone()));

    let mut app = Charter::new(
        nostr_pool.clone(),
        Path::new(&args.datadir).join(BALLOT_FILENAME),
        dns_api_key,
    );

    //4. Run the ZMQ char daemon to participate in char consensus and submit votes
    if let Err(e) = char_sdk::run_zmq(
        &char_rpc,
        kv_key_hex.as_str(),
        target_app,
        &bond.txid,
        &mut app,
    )
    .await
    {
        error!("ZMQ task failed: {:?}", e);
    }

    let _ = nostr_task.await;
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
                    if let Ok(mut pool) = nostr_pool.lock() {
                        pool.push(event.as_json());
                    }
                }
            }
            Ok(false)
        })
        .await
        .unwrap();
}

pub struct Charter {
    nostr_pool: NostrPool,
    ballot_path: PathBuf,
    dns_api_key: String,
}

impl Charter {
    pub fn new(nostr_pool: NostrPool, ballot_path: PathBuf, dns_api_key: String) -> Self {
        Self {
            nostr_pool,
            ballot_path,
            dns_api_key,
        }
    }
}

impl CharBallotHandlers for Charter {
    fn produce_payload(&mut self, ballot: u64) -> Vec<u8> {
        info!("Waiting for ZMQ events...");

        let mut events = Vec::new();
        if let Ok(mut pool) = self.nostr_pool.lock() {
            std::mem::swap(&mut *pool, &mut events);
        }

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
        serde_json::to_vec(&events).unwrap_or_default()
    }

    fn on_roll_observed(
        &mut self,
        ballot: u64,
        payload: &[u8],
    ) -> Result<(), Box<dyn StdError + Send + Sync>> {
        let json: serde_json::Value = serde_json::from_slice(payload)?;
        let events_arr = json.as_array();

        if events_arr.map_or(true, |a| a.is_empty()) {
            save_ballot_num_to_disk(&self.ballot_path, ballot + 1);
            return Ok(());
        }

        info!("[ROLL] Decision roll found for ballot: {}", ballot);

        let events: Vec<Event> = events_arr
            .unwrap()
            .iter()
            .filter_map(|i| {
                i.as_str()
                    .and_then(|s| serde_json::from_str::<Event>(s).ok())
            })
            .collect();

        let ids: HashSet<_> = events.iter().map(|e| e.id.to_string()).collect();

        if !ids.is_empty() {
            if let Ok(mut pool) = self.nostr_pool.lock() {
                pool.retain(|s| {
                    serde_json::from_str::<Event>(s)
                        .map_or(true, |e| !ids.contains(&e.id.to_string()))
                });
            }
        }

        for event in events {
            process_dns_event(&event, &self.dns_api_key);
        }

        save_ballot_num_to_disk(&self.ballot_path, ballot + 1);

        Ok(())
    }
}

fn process_dns_event(event: &Event, dns_api_key: &str) {
    // verify signature for nostr note
    if event.verify().is_err() {
        return error!("[DNS] Skip {}: Invalid Sig", event.id);
    }
    match (Ipv4Addr::from_str(&event.content), event.pubkey.to_bech32()) {
        (Ok(ip), Ok(label)) => {
            info!("[DNS] Updating key: {} with ip: {}", label, ip);
            // reqwest::blocking must run in Tokio's blocking section when invoked from the ZMQ runtime.
            let update_result = tokio::task::block_in_place(|| {
                update_a_record(&label, &ip.to_string(), dns_api_key)
            });
            if let Err(e) = update_result {
                error!("Failed to update A record: {:?}", e);
            }
        }
        _ => error!("[DNS] Invalid IP or Pubkey for event {}", event.id),
    }
}

fn load_ballot_num_from_disk(path: &Path) -> u64 {
    if let Ok(mut f) = std::fs::File::open(path) {
        let mut buf = [0u8; 8];
        if f.read_exact(&mut buf).is_ok() {
            return u64::from_le_bytes(buf);
        }
    }
    0
}

fn save_ballot_num_to_disk(path: &Path, num: u64) -> u64 {
    if let Err(e) = std::fs::write(path, num.to_le_bytes()) {
        error!("[DISK] Failed to save ballot: {:?}", e);
    }
    num
}

fn is_empty_domain_error(err: &TransportError) -> bool {
    matches!(
        err,
        TransportError::Rpc { code: -1, message }
            if message == "No decided ballots for this domain"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_empty_domain_rpc_error() {
        let err = TransportError::Rpc {
            code: -1,
            message: "No decided ballots for this domain".into(),
        };
        assert!(is_empty_domain_error(&err));
    }

    #[test]
    fn ignores_other_transport_errors() {
        let err = TransportError::Rpc {
            code: -1,
            message: "some other rpc error".into(),
        };
        assert!(!is_empty_domain_error(&err));
    }
}
