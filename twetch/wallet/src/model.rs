//! What the wallet stores.

use serde::{Deserialize, Serialize};

pub type Outpoint = (String, u32);

#[must_use]
pub fn outpoint_key(op: &Outpoint) -> String {
    format!("{}:{}", op.0, op.1)
}

#[must_use]
pub fn parse_outpoint(key: &str) -> Option<Outpoint> {
    let (txid, vout) = key.split_once(':')?;
    Some((txid.to_string(), vout.parse().ok()?))
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Request,
    Topup,
    Withdraw,
}

/// Where a payment is. `Signed` is written with its bytes before they are sent,
/// so a restart replays them and never signs a second time.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// A request is running or ended without saying what it cost.
    Open,
    /// The cost is known; nothing is signed.
    Owed,
    /// Over a spending limit: the user approves it or raises the limit.
    Held,
    /// Signed and stored; not accepted yet.
    Signed,
    /// Sent on chain, not credited yet.
    Pending,
    Paid,
    /// The request cost nothing.
    Free,
    /// The gateway never reported this request.
    Gone,
    /// Needs the user: refused for good, or the chain gives no clear answer.
    Failed,
    /// The user released the coins of a failed payment.
    Canceled,
}

impl State {
    #[must_use]
    pub fn is_active(self) -> bool {
        matches!(self, Self::Open | Self::Owed | Self::Held | Self::Signed | Self::Pending)
    }

    /// The bytes have left this machine and were accepted.
    #[must_use]
    pub fn is_accepted(self) -> bool {
        matches!(self, Self::Pending | Self::Paid)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Item {
    /// `req:<request id>`, `top:<n>` or `out:<n>`.
    pub id: String,
    pub kind: Kind,
    pub seq: u64,
    pub state: State,
    pub created_ms: i64,
    pub model: String,
    pub sats: u64,
    pub usd_micros: u64,
    pub to: String,
    pub txid: String,
    pub tx_hex: String,
    pub fee: u64,
    pub inputs: Vec<Outpoint>,
    pub change: u64,
    pub change_vout: u32,
    /// The day its sats count against, once signed.
    pub day: String,
    /// The user approved this payment over the limit.
    pub approved: bool,
    /// A dead transaction was replaced once already.
    pub rebuilt: bool,
    pub old_txids: Vec<String>,
    pub note: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CoinState {
    Available,
    /// Spent by one of our own transactions. Kept, so a lagging provider
    /// cannot hand the coin out again.
    Spent,
    /// Spent by someone else, or named dead by the relay.
    Dropped,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Coin {
    pub sats: u64,
    pub state: CoinState,
    /// The provider has listed it at least once.
    pub seen: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct Limits {
    pub per_payment_usd: f64,
    pub per_day_usd: f64,
}

impl Default for Limits {
    fn default() -> Self {
        Self { per_payment_usd: 1.0, per_day_usd: 10.0 }
    }
}

#[must_use]
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// The user's own calendar day.
#[must_use]
pub fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

#[must_use]
pub fn usd_micros(sats: u64, bsv_usd: f64) -> u64 {
    // 1 BSV = 1e8 sats and 1 USD = 1e6 micros, so sats * rate / 100.
    (sats as f64 * bsv_usd / 100.0).ceil() as u64
}
