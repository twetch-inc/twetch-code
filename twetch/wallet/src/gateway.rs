//! The inference gateway's API (its contract is `inference/API.md`).

use crate::chain::Sent;
use crate::model::Outpoint;
use crate::secrets::{SecretStore, GATEWAY_KEY};
use crate::signer::Signer;
use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

const KEY_NAME: &str = "TwetchCode";

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct AccountInfo {
    pub balance_sats: i64,
    #[serde(default)]
    pub reserved_sats: i64,
    #[serde(default)]
    pub credit_sats: i64,
    #[serde(default)]
    pub available_sats: i64,
    #[serde(default)]
    pub paid_sats: i64,
    #[serde(default)]
    pub spent_sats: i64,
    pub pay_to: String,
    pub bsv_usd: f64,
    #[serde(default)]
    pub min_topup_sats: u64,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Model {
    pub id: String,
    pub name: Option<String>,
    pub context_length: Option<u64>,
    pub max_output: Option<u64>,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default)]
    pub default: bool,
    pub pricing: Option<Pricing>,
}

/// USD per token.
#[derive(Deserialize, Clone, Debug)]
pub struct Pricing {
    pub prompt: Option<String>,
    pub completion: Option<String>,
    pub cached_prompt: Option<String>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Models {
    pub data: Vec<Model>,
    pub bsv_usd: Option<f64>,
}

/// What a 402 asks for.
#[derive(Deserialize, Clone, Debug)]
pub struct Demand {
    pub pay_to: String,
    pub sats_required: u64,
    #[serde(default)]
    pub available_sats: i64,
}

#[derive(Debug)]
pub enum Submit {
    Credited {
        txid: String,
    },
    Pending,
    Conflict {
        named: Vec<Outpoint>,
        retry: bool,
        body: String,
    },
    /// The same bytes may work later.
    Later(String),
    /// Not a payment for this account: never will be.
    Invalid(String),
}

#[derive(Deserialize)]
struct Credited {
    txid: String,
}

#[derive(Deserialize, Default)]
struct Refusal {
    #[serde(default)]
    retry: Option<bool>,
    #[serde(default)]
    spent: Vec<serde_json::Value>,
    #[serde(default)]
    detail: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct UsageRow {
    cost_sats: u64,
}

#[derive(Serialize)]
struct MintBody<'a> {
    pubkey: &'a str,
    ts: i64,
    name: &'a str,
    sig: &'a str,
}

#[derive(Deserialize)]
struct Minted {
    key: String,
}

#[derive(Deserialize)]
struct Payment402 {
    payment: Option<Demand>,
    error: Option<Nested402>,
}

#[derive(Deserialize)]
struct Nested402 {
    payment: Option<Demand>,
}

pub struct Gateway {
    /// Calls with a deadline.
    http: reqwest::Client,
    /// Model answers: a long one is not a stuck one.
    stream: reqwest::Client,
    base: String,
    secrets: Arc<dyn SecretStore>,
    signer: Signer,
    key: tokio::sync::Mutex<Option<String>>,
}

impl Gateway {
    /// # Errors
    /// When the HTTP clients cannot be made.
    pub fn new(base: &str, secrets: Arc<dyn SecretStore>, signer: Signer) -> Result<Self> {
        let build = || reqwest::Client::builder().connect_timeout(Duration::from_secs(15)).tcp_nodelay(true).redirect(reqwest::redirect::Policy::none());
        Ok(Self {
            http: build().timeout(Duration::from_secs(45)).build()?,
            stream: build().build()?,
            base: base.trim_end_matches('/').to_string(),
            secrets,
            signer,
            key: tokio::sync::Mutex::new(None),
        })
    }

    async fn mint(&self) -> Result<String> {
        let ts = crate::model::now_ms();
        let pubkey = self.signer.account().pubkey;
        let sig = self.signer.key_message(KEY_NAME, ts).await?;
        let res = self.http.post(format!("{}/v1/keys", self.base)).json(&MintBody { pubkey: &pubkey, ts, name: KEY_NAME, sig: &sig }).send().await?;
        if !res.status().is_success() {
            bail!("the gateway refused a new key ({})", res.status());
        }
        Ok(res.json::<Minted>().await?.key)
    }

    /// The gateway key, minted on first use. `stale` is one the gateway refused.
    ///
    /// # Errors
    /// When the keychain, the wallet or the gateway refuses.
    pub async fn key(&self, stale: Option<&str>) -> Result<String> {
        let mut held = self.key.lock().await;
        if held.is_none() {
            *held = self.secrets.get(GATEWAY_KEY)?.map(|k| k.to_string());
        }
        if let Some(k) = held.as_ref() {
            if stale != Some(k.as_str()) {
                return Ok(k.clone());
            }
        }
        let fresh = self.mint().await?;
        self.secrets.set(GATEWAY_KEY, &fresh)?;
        *held = Some(fresh.clone());
        Ok(fresh)
    }

    /// The next call reads the keychain again (the wallet was replaced).
    pub async fn forget_key(&self) {
        *self.key.lock().await = None;
    }

    /// A new key, and the old one revoked.
    ///
    /// # Errors
    /// When a new key cannot be made; the old one then stays.
    pub async fn rotate(&self) -> Result<()> {
        let old = self.key(None).await?;
        let mut held = self.key.lock().await;
        let fresh = self.mint().await?;
        self.secrets.set(GATEWAY_KEY, &fresh)?;
        *held = Some(fresh);
        drop(held);
        let res = self.http.delete(format!("{}/v1/keys/current", self.base)).bearer_auth(&old).send().await?;
        if !res.status().is_success() && res.status() != StatusCode::UNAUTHORIZED {
            bail!("the new key works, but the old one was not revoked ({})", res.status());
        }
        Ok(())
    }

    /// Send with the key; mint a new one once if it is refused.
    async fn authed(&self, client: &reqwest::Client, make: impl Fn(&reqwest::Client) -> reqwest::RequestBuilder) -> Result<reqwest::Response> {
        let key = self.key(None).await?;
        let res = make(client).bearer_auth(&key).send().await?;
        if res.status() != StatusCode::UNAUTHORIZED {
            return Ok(res);
        }
        let key = self.key(Some(&key)).await?;
        Ok(make(client).bearer_auth(&key).send().await?)
    }

    /// # Errors
    /// When the gateway cannot be reached.
    pub async fn chat(&self, body: Bytes) -> Result<reqwest::Response> {
        let url = format!("{}/v1/chat/completions", self.base);
        self.authed(&self.stream, |c| c.post(&url).header("content-type", "application/json").body(body.clone())).await
    }

    /// The models, as the gateway sent them.
    ///
    /// # Errors
    /// When the gateway cannot be reached or answers an error.
    pub async fn models_raw(&self) -> Result<Bytes> {
        let res = self.http.get(format!("{}/v1/models", self.base)).timeout(Duration::from_secs(8)).send().await?;
        Ok(res.error_for_status()?.bytes().await?)
    }

    /// # Errors
    /// When the gateway cannot be reached or answers an error.
    pub async fn account(&self) -> Result<AccountInfo> {
        let url = format!("{}/v1/account", self.base);
        Ok(self.authed(&self.http, |c| c.get(&url)).await?.error_for_status()?.json().await?)
    }

    /// What a request cost, once the gateway has settled it.
    ///
    /// # Errors
    /// When the gateway cannot be reached or answers an error.
    pub async fn request_cost(&self, id: &str) -> Result<Option<u64>> {
        let url = format!("{}/v1/requests/{id}", self.base);
        let res = self.authed(&self.http, |c| c.get(&url)).await?;
        if res.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(res.error_for_status()?.json::<UsageRow>().await?.cost_sats))
    }

    /// Spendable coins for this wallet from Twetch's P2P ledger (1-sat outputs excluded).
    ///
    /// # Errors
    /// When the gateway cannot be reached or refuses the request.
    pub async fn unspent(&self, sync: bool) -> Result<HashMap<Outpoint, u64>> {
        #[derive(Deserialize)]
        struct CoinRow {
            txid: String,
            vout: u32,
            satoshis: u64,
        }
        #[derive(Deserialize)]
        struct Body {
            #[serde(default)]
            coins: Vec<CoinRow>,
        }
        let url = if sync { format!("{}/v1/wallet/unspent?sync=1", self.base) } else { format!("{}/v1/wallet/unspent", self.base) };
        let res = self.authed(&self.http, |c| c.get(&url)).await?;
        let body: Body = res.error_for_status()?.json().await?;
        let mut out = HashMap::with_capacity(body.coins.len());
        for c in body.coins {
            if c.satoshis > 1 {
                out.insert((c.txid, c.vout), c.satoshis);
            }
        }
        Ok(out)
    }

    /// The transaction recorded by Twetch's ledger as spending `op`, if any.
    ///
    /// # Errors
    /// When the gateway cannot be reached.
    pub async fn spender(&self, op: &Outpoint) -> Result<Option<String>> {
        #[derive(Deserialize)]
        struct Body {
            txid: String,
        }
        let url = format!("{}/v1/wallet/spender/{}/{}", self.base, op.0, op.1);
        let res = self.authed(&self.http, |c| c.get(&url)).await?;
        if res.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let body: Body = res.error_for_status()?.json().await?;
        Ok((body.txid.len() == 64).then_some(body.txid))
    }

    /// Broadcast a withdrawal transaction through Twetch's P2P relay.
    ///
    /// # Errors
    /// When no definitive answer came back from the relay.
    pub async fn broadcast(&self, tx_hex: &str) -> Result<Sent> {
        let url = format!("{}/v1/wallet/broadcast", self.base);
        let body = serde_json::json!({ "tx_hex": tx_hex });
        let res = self.authed(&self.http, |c| c.post(&url).json(&body)).await?;
        let status = res.status();
        let text = res.text().await?;
        if status.is_success() || status == StatusCode::ACCEPTED {
            return Ok(Sent::Accepted);
        }
        anyhow::ensure!(!status.is_server_error() && status != StatusCode::TOO_MANY_REQUESTS, "the relay answered {status}");
        let low = text.to_lowercase();
        if status == StatusCode::CONFLICT
            || low.contains("conflict")
            || low.contains("missing inputs")
            || low.contains("missingorspent")
            || low.contains("double spend")
            || low.contains("spent")
        {
            Ok(Sent::Conflict(text))
        } else if low.contains("already-known") || low.contains("already in the mempool") || low.contains("already-in-mempool") {
            Ok(Sent::Accepted)
        } else {
            Ok(Sent::Refused(text))
        }
    }

    /// BSV/USD rate from `/v1/models`.
    ///
    /// # Errors
    /// When the gateway cannot be reached or has no price.
    pub async fn bsv_usd(&self) -> Result<f64> {
        let raw = self.models_raw().await?;
        let m: Models = serde_json::from_slice(&raw)?;
        let usd = m.bsv_usd.ok_or_else(|| anyhow!("no BSV price in /v1/models"))?;
        anyhow::ensure!(usd.is_finite() && usd > 0.0, "bad price");
        Ok(usd)
    }

    /// Hand in a signed payment. Safe to repeat with the same bytes.
    ///
    /// # Errors
    /// When no answer came: the payment may or may not have arrived.
    pub async fn submit(&self, tx_hex: &str) -> Result<Submit> {
        let url = format!("{}/v1/payments", self.base);
        let body = serde_json::json!({ "tx_hex": tx_hex });
        let res = self.authed(&self.http, |c| c.post(&url).json(&body)).await?;
        let status = res.status();
        let bytes = res.bytes().await?;
        match status.as_u16() {
            200 => Ok(Submit::Credited { txid: serde_json::from_slice::<Credited>(&bytes).context("bad payment answer")?.txid }),
            202 => Ok(Submit::Pending),
            409 => {
                let r: Refusal = serde_json::from_slice(&bytes).unwrap_or_default();
                let mut named: Vec<Outpoint> = r.spent.iter().filter_map(spent_coin).collect();
                for v in [&r.detail, &r.error].into_iter().flatten() {
                    named_in(&v.to_string(), &mut named);
                }
                Ok(Submit::Conflict { named, retry: r.retry != Some(false), body: clip(&bytes) })
            }
            400 | 413 | 422 => Ok(Submit::Invalid(clip(&bytes))),
            _ => Ok(Submit::Later(format!("{status}: {}", clip(&bytes)))),
        }
    }
}

/// The relay names a coin as `{txid, vout}` or as `"txid:vout"`.
fn spent_coin(v: &serde_json::Value) -> Option<Outpoint> {
    if let Some(text) = v.as_str() {
        return crate::model::parse_outpoint(&text.to_lowercase());
    }
    Some((v.get("txid")?.as_str()?.to_lowercase(), u32::try_from(v.get("vout")?.as_u64()?).ok()?))
}

/// `input <txid>:<vout>` in a refusal's own sentence.
fn named_in(text: &str, out: &mut Vec<Outpoint>) {
    for part in text.split("input ").skip(1) {
        let Some((txid, rest)) = part.split_once(':') else { continue };
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if txid.len() == 64 && txid.bytes().all(|b| b.is_ascii_hexdigit()) {
            if let Ok(vout) = digits.parse() {
                let op = (txid.to_lowercase(), vout);
                if !out.contains(&op) {
                    out.push(op);
                }
            }
        }
    }
}

fn clip(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).chars().take(300).collect()
}

/// The payment a 402 body asks for.
///
/// # Errors
/// When the body names none.
pub fn demand(body: &[u8]) -> Result<Demand> {
    let b: Payment402 = serde_json::from_slice(body)?;
    b.payment.or(b.error.and_then(|e| e.payment)).ok_or_else(|| anyhow!("the gateway asked for a payment without saying how much"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusal_names_coins_either_way() {
        let t = "ab".repeat(32);
        let mut out = Vec::new();
        named_in(&format!("broadcast rejected [spent-conflict]: input {}:3 already spent, input {t}:3", t.to_uppercase()), &mut out);
        assert_eq!(out, [(t, 3)]);
        named_in("input nope:1 input :", &mut out);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn refusal_lists_coins_either_way() {
        let t = "ab".repeat(32);
        for body in [serde_json::json!({"txid": t, "vout": 2}), serde_json::json!(format!("{t}:2"))] {
            assert_eq!(spent_coin(&body), Some((t.clone(), 2)));
        }
        assert_eq!(spent_coin(&serde_json::json!("x")), None);
    }

    #[test]
    fn demand_is_found_at_either_level() {
        let inner = r#"{"pay_to":"p","sats_required":5,"balance_sats":0,"available_sats":-3}"#;
        for body in [format!(r#"{{"error":{{"message":"m"}},"payment":{inner}}}"#), format!(r#"{{"error":{{"message":"m","payment":{inner}}}}}"#)] {
            let d = demand(body.as_bytes()).unwrap();
            assert_eq!((d.pay_to.as_str(), d.sats_required, d.available_sats), ("p", 5, -3));
        }
        assert!(demand(br#"{"error":{"message":"m"}}"#).is_err());
    }
}
