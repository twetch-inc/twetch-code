//! WhatsOnChain: deposits, who spent a coin, the BSV price, and sending a withdraw.

use crate::model::Outpoint;
use anyhow::{ensure, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;

pub struct Chain {
    http: reqwest::Client,
    base: String,
}

#[derive(Deserialize)]
struct Row {
    tx_hash: String,
    tx_pos: u32,
    value: u64,
    #[serde(default, rename = "isSpentInMempoolTx")]
    spent: bool,
}

#[derive(Deserialize)]
struct Page {
    #[serde(default)]
    result: Option<Vec<Row>>,
    #[serde(default, rename = "nextPageToken")]
    next: Option<String>,
}

#[derive(Deserialize)]
struct Spender {
    txid: String,
}

#[derive(Deserialize)]
struct Rate {
    rate: f64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Sent {
    Accepted,
    /// The network says an input is gone.
    Conflict(String),
    Refused(String),
}

impl Chain {
    /// # Errors
    /// When the HTTP client cannot be made.
    pub fn new(base: &str) -> Result<Self> {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(20)).build()?;
        Ok(Self { http, base: base.trim_end_matches('/').to_string() })
    }

    /// Every unspent coin of an address, confirmed or not.
    ///
    /// # Errors
    /// When any page cannot be read: a partial list is never returned.
    pub async fn unspent(&self, address: &str) -> Result<HashMap<Outpoint, u64>> {
        let mut out = HashMap::new();
        let mut token = String::new();
        loop {
            let mut req = self.http.get(format!("{}/address/{address}/unspent/all", self.base));
            if !token.is_empty() {
                req = req.query(&[("token", token.as_str())]);
            }
            let res = req.send().await?;
            if res.status() == reqwest::StatusCode::NOT_FOUND {
                return Ok(out);
            }
            let page: Page = res.error_for_status()?.json().await?;
            for r in page.result.unwrap_or_default() {
                if !r.spent {
                    out.insert((r.tx_hash, r.tx_pos), r.value);
                }
            }
            match page.next {
                Some(t) if !t.is_empty() && t != token => token = t,
                _ => return Ok(out),
            }
        }
    }

    /// The transaction that spends a coin, if the chain knows one.
    ///
    /// # Errors
    /// When the provider cannot be asked.
    pub async fn spender(&self, op: &Outpoint) -> Result<Option<String>> {
        let res = self.http.get(format!("{}/tx/{}/{}/spent", self.base, op.0, op.1)).send().await?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let res = res.error_for_status()?;
        let body = res.bytes().await?;
        Ok(serde_json::from_slice::<Spender>(&body).ok().map(|s| s.txid).filter(|t| t.len() == 64))
    }

    /// # Errors
    /// When the provider cannot be asked or the price makes no sense.
    pub async fn bsv_usd(&self) -> Result<f64> {
        let rate: Rate = self.http.get(format!("{}/exchangerate", self.base)).send().await?.error_for_status()?.json().await?;
        ensure!(rate.rate.is_finite() && rate.rate > 0.0, "bad price");
        Ok(rate.rate)
    }

    /// # Errors
    /// When no answer came: the transaction may or may not be out.
    pub async fn send(&self, tx_hex: &str) -> Result<Sent> {
        let res = self.http.post(format!("{}/tx/raw", self.base)).json(&serde_json::json!({ "txhex": tx_hex })).send().await?;
        let status = res.status();
        let text = res.text().await?;
        if status.is_success() {
            return Ok(Sent::Accepted);
        }
        ensure!(!status.is_server_error() && status.as_u16() != 429, "the network node answered {status}");
        let low = text.to_lowercase();
        if low.contains("already-known") || low.contains("already in the mempool") || low.contains("already-in-mempool") {
            Ok(Sent::Accepted)
        } else if low.contains("conflict") || low.contains("missing inputs") || low.contains("missingorspent") || low.contains("double spend") {
            Ok(Sent::Conflict(text))
        } else {
            Ok(Sent::Refused(text))
        }
    }
}
