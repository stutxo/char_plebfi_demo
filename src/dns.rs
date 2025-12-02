use reqwest::Client;
use serde_json::json;

const PDNS_BASE_URL: &str = "http://127.0.0.1:8081/api/v1";
const PDNS_SERVER_ID: &str = "localhost";
const ZONE_NAME: &str = "nostr.";

pub async fn update_a_record(
    label: &str,
    ip: &str,
    api_key: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::new();

    let record_name = format!("{}.{}", label, ZONE_NAME);

    let url = format!(
        "{base}/servers/{server_id}/zones/{zone_id}",
        base = PDNS_BASE_URL,
        server_id = PDNS_SERVER_ID,
        zone_id = ZONE_NAME,
    );

    let body = json!({
        "rrsets": [
            {
                "name": record_name,
                "type": "A",
                "changetype": "REPLACE",
                "ttl": 300,
                "records": [
                    {
                        "content": ip,
                        "disabled": false
                    }
                ]
            }
        ]
    });

    let resp: reqwest::Response = client
        .patch(&url)
        .header("X-API-Key", api_key)
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await?;
        eprintln!("PowerDNS error: HTTP {}: {}", status, text);
        return Err(format!("PDNS API request failed: {}", status).into());
    }

    Ok(())
}
