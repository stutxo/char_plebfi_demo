use anyhow::{Context, Result, anyhow};
use bitcoin::consensus::encode::VarInt;
use bitcoin::consensus::{Decodable, Encodable};
use bitcoind_async_client::Client;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Cursor, Read};
use tracing::{error, info};

#[derive(Deserialize, Debug)]
pub struct DecisionRollResponse {
    pub found: bool,
    pub decision_roll: Option<DecisionRollInner>,
}

#[derive(Deserialize, Debug)]
pub struct DecisionRollInner {
    pub data: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CharRpc {
    pub client: Client,
}

impl CharRpc {
    pub fn new(datadir: &str, rpc_url: &str) -> Result<Self> {
        let cookie_path = format!("{}/.cookie", datadir);
        let cookie_content = fs::read_to_string(&cookie_path)
            .with_context(|| format!("Failed to read cookie file at {}", cookie_path))?;

        let (user, password) = cookie_content
            .trim()
            .split_once(':')
            .ok_or_else(|| anyhow!("Invalid cookie format"))?;

        let client = Client::new(
            rpc_url.to_string(),
            user.to_string(),
            password.to_string(),
            None,
            None,
        )
        .map_err(|e| anyhow!("Failed to create client: {}", e))?;

        info!("Connected to char regtest via bitcoind-async-client");
        Ok(Self { client })
    }

    pub async fn add_bamboo_kv(
        &self,
        key_hex: String,
        notes: Vec<String>,
        ballot_number: u64,
    ) -> Result<Value> {
        let serialized_payload =
            serde_json::to_string(&notes).context("Failed to serialize notes")?;

        let payload_bytes = serialized_payload.as_bytes();

        let hex_value = encode_referendum_vote(ballot_number, payload_bytes);
        let mut map = Map::new();
        map.insert(key_hex, Value::String(hex_value));

        let json_array_of_notes = Value::Array(vec![Value::Object(map)]);

        let params = vec![json_array_of_notes, json!(false)];

        self.client
            .call_raw::<Value>("addbambookv", &params)
            .await
            .map_err(|e| anyhow!("RPC Error: {}", e))
    }

    pub async fn get_referendum_decision_roll(
        &self,
        domain_hex: &str,
        ballot_number: u64,
    ) -> Result<Option<Value>> {
        let params = vec![json!(domain_hex), json!(ballot_number), json!(1)];

        let response: DecisionRollResponse = match self
            .client
            .call_raw("getreferendumdecisionroll", &params)
            .await
        {
            Ok(v) => v,
            Err(e) => return Err(anyhow!("RPC Error: {}", e)),
        };

        if !response.found {
            return Ok(None);
        }

        if let Some(inner) = response.decision_roll {
            if let Some(hex_data) = inner.data {
                let (decoded_ballot, payload_bytes) = decode_referendum_vote_hex(&hex_data)?;

                if decoded_ballot != ballot_number {
                    error!(
                        "Warning: Decoded ballot {} does not match requested {}",
                        decoded_ballot, ballot_number
                    );
                }

                if payload_bytes.is_empty() {
                    return Ok(Some(serde_json::json!([])));
                }

                let result_json: Value = serde_json::from_slice(&payload_bytes)?;
                return Ok(Some(result_json));
            }
        }

        Ok(None)
    }

    pub async fn check_for_active_bonds(&self) -> Result<bool> {
        let params = vec![json!(0)];

        let bonds: Vec<Value> = self
            .client
            .call_raw("getallcharbonds", &params)
            .await
            .map_err(|e| anyhow!("RPC Error: {}", e))?;

        println!("Active bonds: {:?}", bonds);

        Ok(!bonds.is_empty())
    }
}

pub fn encode_referendum_vote(ballot_number: u64, payload_bytes: &[u8]) -> String {
    let mut buf = Vec::new();
    // 1. LeafType::REFERENDUM_VOTE == 0
    buf.push(0x00);
    // 2. Ballot Number (CVarInt)
    buf.extend(encode_varint(ballot_number));
    // 3. Payload Length (CompactSize)
    VarInt(payload_bytes.len() as u64)
        .consensus_encode(&mut buf)
        .unwrap();
    // 4. Payload
    buf.extend_from_slice(payload_bytes);
    hex::encode(buf)
}

pub fn decode_referendum_vote_hex(vote_hex: &str) -> Result<(u64, Vec<u8>)> {
    let bytes = hex::decode(vote_hex)?;
    let mut reader = Cursor::new(bytes);

    let mut leaf_buf = [0u8; 1];
    reader.read_exact(&mut leaf_buf)?;
    if leaf_buf[0] != 0x00 {
        return Err(anyhow!(
            "Invalid LeafType: expected 0x00, got 0x{:02x}",
            leaf_buf[0]
        ));
    }

    let ballot = decode_varint(&mut reader)?;

    let payload_len = VarInt::consensus_decode(&mut reader).unwrap().0;

    let mut payload = vec![0u8; payload_len as usize];
    reader.read_exact(&mut payload)?;

    Ok((ballot, payload))
}

pub fn decode_leader_payload(payload: &[u8]) -> Result<(u64, [u8; 32])> {
    let mut cursor = Cursor::new(payload);
    let ballot = decode_varint(&mut cursor)?;
    let mut domain = [0u8; 32];
    cursor.read_exact(&mut domain)?;
    Ok((ballot, domain))
}

pub fn domain_to_char_hash(domain_hex: &str) -> Result<[u8; 32]> {
    let domain_bytes = hex::decode(domain_hex)?;
    let mut buf = Vec::new();
    VarInt(domain_bytes.len() as u64)
        .consensus_encode(&mut buf)
        .expect("writing to Vec cannot fail");
    buf.extend_from_slice(&domain_bytes);
    let hash = Sha256::digest(&buf);
    Ok(hash.into())
}

fn encode_varint(mut n: u64) -> Vec<u8> {
    let mut tmp = Vec::new();
    let mut len = 0;
    loop {
        let byte = (n & 0x7f) as u8 | (if len > 0 { 0x80 } else { 0x00 });
        tmp.push(byte);
        if n <= 0x7f {
            break;
        }
        n = (n >> 7) - 1;
        len += 1;
    }
    tmp.reverse();
    tmp
}

fn decode_varint<R: Read>(reader: &mut R) -> Result<u64> {
    let mut n: u64 = 0;
    loop {
        let mut buf = [0u8; 1];
        reader.read_exact(&mut buf)?;
        let dat = buf[0];
        n = (n << 7) | (dat as u64 & 0x7f);
        if (dat & 0x80) != 0 {
            n += 1;
        } else {
            return Ok(n);
        }
    }
}
