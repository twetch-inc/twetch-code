//! A local `ai.twetch.com` and `WhatsOnChain` with no money in them.
//!
//! The gateway side follows `inference/API.md` and checks every payment with
//! `twetch-crypto` and real input signatures. The chain side is an in-memory
//! coin set with a faucet. Faults are queued by the tests.

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use bitcoin::script::Instruction;
use bitcoin::secp256k1::{ecdsa::Signature, Message, PublicKey, Secp256k1};
use bitcoin::{ScriptBuf, Transaction};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use twetch_crypto::{sign, tx};

pub const PAY_TO_PUBKEY: &str = "024d9e9c9804d364c3e79c7f47dc6048b1afa054ff67ac160d44082a8482184eaf";
pub const PAY_TO: &str = "1J82Vbt4Uz8kKr2Xb3Kro9WtCh6udoctCJ";
pub const BSV_USD: f64 = 14.0;
const MAP: &[u8] = b"1PuQa7K62MiKCtssSLKy1kh56WWU7MtUR5";

type Outpoint = (String, u32);

/// What the next `POST /v1/payments` does instead of answering plainly.
#[derive(Clone, Debug)]
pub enum PayFault {
    /// Credit nothing yet: 202.
    Pending,
    /// Take the payment, then lose the answer.
    LostAnswer,
    /// 502 without taking it.
    Down,
    /// Someone else takes the payment's first coin first: 409.
    StolenCoin,
    /// 409 naming the first coin, though nobody spent it.
    FalseConflict,
    /// 400.
    NotYours,
}

/// How a chat answer goes wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatFault {
    /// The stream stops before the usage chunk; the cost settles anyway.
    CutStream,
}

#[derive(Clone)]
pub struct Coin {
    pub address_script: String,
    pub sats: u64,
    pub spent_by: Option<String>,
}

#[derive(Default)]
pub struct Account {
    pub paid: i64,
    pub spent: i64,
    pub keys: usize,
}

#[derive(Clone, Debug)]
pub struct Payment {
    pub account: String,
    pub txid: String,
    pub sats: u64,
    pub posts: usize,
}

pub struct World {
    pub coins: HashMap<Outpoint, Coin>,
    pub keys: HashMap<String, String>,
    pub accounts: HashMap<String, Account>,
    pub payments: Vec<Payment>,
    pub requests: HashMap<String, (String, u64)>,
    pub order: Vec<String>,
    pub pay_faults: VecDeque<PayFault>,
    pub chat_faults: VecDeque<ChatFault>,
    pub seen_sigs: HashSet<String>,
    /// The next answer costs this.
    pub cost: u64,
    /// A request is admitted when balance + credit covers this.
    pub estimate: i64,
    pub credit: i64,
    pub pay_to: String,
    pub chunk_delay: Duration,
    /// Hold a chat before it answers.
    pub head_delay: Duration,
    pub chats: usize,
    pub aborted: usize,
    /// Raw posts of `/tx/raw`, the withdraws.
    pub sent: Vec<String>,
    pub text: String,
    next: u64,
}

impl Default for World {
    fn default() -> Self {
        Self {
            coins: HashMap::new(),
            keys: HashMap::new(),
            accounts: HashMap::new(),
            payments: Vec::new(),
            requests: HashMap::new(),
            order: Vec::new(),
            pay_faults: VecDeque::new(),
            chat_faults: VecDeque::new(),
            seen_sigs: HashSet::new(),
            cost: 4200,
            estimate: 100_000,
            credit: 0,
            pay_to: PAY_TO.into(),
            chunk_delay: Duration::ZERO,
            head_delay: Duration::ZERO,
            chats: 0,
            aborted: 0,
            sent: Vec::new(),
            text: "Hello from the local stub gateway. This answer was paid for with one transaction.".into(),
            next: 0,
        }
    }
}

impl World {
    /// Give an address a confirmed coin.
    pub fn faucet(&mut self, address: &str, sats: u64) -> Outpoint {
        self.next += 1;
        let op = (format!("{:064x}", 0xfau64 * 1_000_000 + self.next), 0);
        let script = twetch_crypto::wallet::address_to_p2pkh_hex(address).expect("address");
        self.coins.insert(op.clone(), Coin { address_script: script, sats, spent_by: None });
        op
    }

    #[must_use]
    pub fn unspent_sats(&self, address: &str) -> u64 {
        let script = twetch_crypto::wallet::address_to_p2pkh_hex(address).expect("address");
        self.coins.values().filter(|c| c.address_script == script && c.spent_by.is_none()).map(|c| c.sats).sum()
    }

    /// Put a checked transaction on the chain.
    fn apply(&mut self, t: &Transaction) -> Result<(), Value> {
        let txid = t.compute_txid().to_string();
        let mut gone = Vec::new();
        let mut total = 0u64;
        for i in &t.input {
            let op = (i.previous_output.txid.to_string(), i.previous_output.vout);
            match self.coins.get(&op) {
                Some(c) if c.spent_by.is_none() || c.spent_by.as_deref() == Some(&txid) => total += c.sats,
                _ => gone.push(json!({"txid": op.0, "vout": op.1})),
            }
        }
        if !gone.is_empty() {
            return Err(json!({"code": "spent-conflict", "retry": true, "detail": "broadcast rejected [spent-conflict]", "spent": gone}));
        }
        verify_inputs(t, &self.coins).map_err(|e| json!({"error": e}))?;
        let out: u64 = t.output.iter().map(|o| o.value.to_sat()).sum();
        let size = bitcoin::consensus::serialize(t).len() as u64;
        if out > total || (total - out) * 1000 < size * 100 {
            return Err(json!({"error": "the fee is too low"}));
        }
        for i in &t.input {
            let op = (i.previous_output.txid.to_string(), i.previous_output.vout);
            if let Some(c) = self.coins.get_mut(&op) {
                c.spent_by = Some(txid.clone());
            }
        }
        for (vout, o) in t.output.iter().enumerate() {
            let op = (txid.clone(), u32::try_from(vout).unwrap_or(0));
            self.coins.entry(op).or_insert_with(|| Coin { address_script: hex::encode(o.script_pubkey.as_bytes()), sats: o.value.to_sat(), spent_by: None });
        }
        Ok(())
    }

    fn balance(&self, account: &str) -> i64 {
        self.accounts.get(account).map_or(0, |a| a.paid - a.spent)
    }
}

fn verify_inputs(t: &Transaction, coins: &HashMap<Outpoint, Coin>) -> Result<(), String> {
    let secp = Secp256k1::verification_only();
    for (n, i) in t.input.iter().enumerate() {
        let coin = &coins[&(i.previous_output.txid.to_string(), i.previous_output.vout)];
        let script = ScriptBuf::from_bytes(hex::decode(&coin.address_script).map_err(|e| e.to_string())?);
        let pushes: Vec<Vec<u8>> = i
            .script_sig
            .instructions()
            .filter_map(|x| match x {
                Ok(Instruction::PushBytes(b)) => Some(b.as_bytes().to_vec()),
                _ => None,
            })
            .collect();
        let [sig, key] = pushes.as_slice() else { return Err("bad unlock".into()) };
        let key = PublicKey::from_slice(key).map_err(|e| e.to_string())?;
        let owner = tx::address_from_pubkey_hex(&hex::encode(key.serialize()))?;
        if twetch_crypto::wallet::address_to_p2pkh_hex(&owner)? != coin.address_script {
            return Err("not the owner".into());
        }
        let digest = sign::forkid_sighash(t, n, &script, coin.sats);
        let sig = Signature::from_der(&sig[..sig.len() - 1]).map_err(|e| e.to_string())?;
        secp.verify_ecdsa(&Message::from_digest(digest), &sig, &key).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The contract's envelope, its author and its payout.
fn check_payment(t: &Transaction, account: &str) -> Result<u64, String> {
    let address = tx::address_from_pubkey_hex(account)?;
    if tx::aip_verified_signer_tx(t)? != address {
        return Err("the author is not this account".into());
    }
    let cells: Vec<Vec<u8>> = t.output[0]
        .script_pubkey
        .instructions()
        .filter_map(|x| match x {
            Ok(Instruction::PushBytes(b)) if !b.is_empty() => Some(b.as_bytes().to_vec()),
            _ => None,
        })
        .collect();
    let want: [&[u8]; 11] = [MAP, b"SET", b"app", b"twetch", b"type", b"pay", b"action", b"twetch/inference@0.0.1", b"account", account.as_bytes(), b"|"];
    if cells.len() < want.len() || cells.iter().zip(want).any(|(a, b)| a != b) {
        return Err("not an inference payment".into());
    }
    match tx::action_payout_to_pubkey_tx(t, PAY_TO_PUBKEY) {
        0 => Err("pays nothing to Twetch".into()),
        n => Ok(n),
    }
}

pub type Shared = Arc<Mutex<World>>;

pub fn router(world: Shared) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/models", get(models))
        .route("/v1/keys", post(keys))
        .route("/v1/keys/current", delete(revoke))
        .route("/v1/account", get(account))
        .route("/v1/payments", post(payments))
        .route("/v1/requests/{id}", get(request))
        .route("/v1/usage", get(usage))
        .route("/v1/chat/completions", post(chat))
        .route("/woc/exchangerate", get(|| async { Json(json!({"rate": BSV_USD, "currency": "USD"})) }))
        .route("/woc/address/{address}/unspent/all", get(unspent))
        .route("/woc/tx/{txid}/{vout}/spent", get(spent))
        .route("/woc/tx/raw", post(raw))
        // What the real gateway binary asks: OpenRouter and the Twetch API.
        .route("/up/endpoints/zdr", get(zdr))
        .route("/up/chat/completions", post(upstream_chat))
        .route("/up/generation", get(|| async { Json(json!({"data": {"total_cost": 0.0001, "tokens_prompt": 10, "tokens_completion": 3}})) }))
        .route("/up/v1/price", get(price))
        .route("/up/v1/wallet/broadcast", post(broadcast))
        .route("/stub/faucet", post(faucet))
        .route("/stub/state", get(state))
        .with_state(world)
}

/// A stub on a free port, for tests.
pub async fn spawn() -> (String, Shared) {
    let world: Shared = Arc::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let app = router(world.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (base, world)
}

fn err(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error": {"message": message, "type": "invalid_request_error", "code": "stub"}}))).into_response()
}

fn who(w: &World, h: &HeaderMap) -> Option<String> {
    let key = h.get("authorization")?.to_str().ok()?.strip_prefix("Bearer ")?;
    w.keys.get(key).cloned()
}

macro_rules! authed {
    ($w:expr, $h:expr) => {
        match who(&$w, &$h) {
            Some(a) => a,
            None => return err(StatusCode::UNAUTHORIZED, "bad key"),
        }
    };
}

async fn models() -> Json<Value> {
    let m = |id: &str, name: &str, default: bool| {
        json!({"id": id, "name": name, "context_length": 1_048_576, "max_output": 65_536, "reasoning": true, "default": default,
            "pricing": {"prompt": "0.000000125", "completion": "0.0000005", "cached_prompt": "0.00000003"}})
    };
    Json(json!({"object": "list", "markup_bps": 2500, "bsv_usd": BSV_USD, "data": [
        m("deepseek/deepseek-v4.1-flash-1017", "DeepSeek V4.1 Flash", true),
        m("z-ai/glm-5.3", "GLM 5.3", false),
        m("z-ai/glm-5.3-flash", "GLM 5.3 Flash", false),
        m("deepseek/deepseek-v4-pro-0813", "DeepSeek V4 Pro", false),
    ]}))
}

#[derive(Deserialize)]
struct KeyBody {
    pubkey: String,
    ts: i64,
    name: String,
    sig: String,
}

async fn keys(State(w): State<Shared>, Json(b): Json<KeyBody>) -> Response {
    let msg = format!("twetch-inference-key\n{}\n{}\n{}", b.pubkey, b.name, b.ts);
    if !sign::bsm_verify_pubkey(msg.as_bytes(), &b.sig, &b.pubkey) {
        return err(StatusCode::UNAUTHORIZED, "bad signature");
    }
    let mut w = w.lock().expect("world");
    if !w.seen_sigs.insert(b.sig) {
        return err(StatusCode::CONFLICT, "signature used");
    }
    w.next += 1;
    let key = format!("tw-ai-stub-{}", w.next);
    w.keys.insert(key.clone(), b.pubkey.clone());
    w.accounts.entry(b.pubkey.clone()).or_default().keys += 1;
    Json(json!({"key": key, "account": b.pubkey})).into_response()
}

async fn revoke(State(w): State<Shared>, h: HeaderMap) -> Response {
    let mut w = w.lock().expect("world");
    let _ = authed!(w, h);
    if let Some(k) = h.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")) {
        w.keys.remove(k);
    }
    Json(json!({"ok": true})).into_response()
}

async fn account(State(w): State<Shared>, h: HeaderMap) -> Response {
    let w = w.lock().expect("world");
    let a = authed!(w, h);
    let row = &w.accounts[&a];
    let balance = row.paid - row.spent;
    Json(json!({"account": a, "balance_sats": balance, "reserved_sats": 0, "credit_sats": w.credit,
        "available_sats": balance + w.credit, "paid_sats": row.paid, "spent_sats": row.spent,
        "pay_to": w.pay_to, "bsv_usd": BSV_USD, "min_topup_sats": 50_000}))
    .into_response()
}

#[derive(Deserialize)]
struct PayBody {
    tx_hex: String,
}

async fn payments(State(w): State<Shared>, h: HeaderMap, Json(b): Json<PayBody>) -> Response {
    let mut w = w.lock().expect("world");
    let a = authed!(w, h);
    let Some(t) = hex::decode(&b.tx_hex).ok().and_then(|r| bitcoin::consensus::deserialize::<Transaction>(&r).ok()) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "not a transaction"}))).into_response();
    };
    let txid = t.compute_txid().to_string();
    let fault = w.pay_faults.pop_front();
    let first = (t.input[0].previous_output.txid.to_string(), t.input[0].previous_output.vout);
    let named =
        json!({"code": "spent-conflict", "retry": true, "detail": "broadcast rejected [spent-conflict]", "spent": [{"txid": first.0, "vout": first.1}]});
    match fault {
        Some(PayFault::NotYours) => return (StatusCode::BAD_REQUEST, Json(json!({"error": "not a payment for this account"}))).into_response(),
        Some(PayFault::Down) => return (StatusCode::BAD_GATEWAY, Json(json!({"error": "relay timeout", "retry": true}))).into_response(),
        Some(PayFault::StolenCoin) => {
            if let Some(c) = w.coins.get_mut(&first) {
                c.spent_by = Some("ee".repeat(32));
            }
            return (StatusCode::CONFLICT, Json(named)).into_response();
        }
        Some(PayFault::FalseConflict) => return (StatusCode::CONFLICT, Json(named)).into_response(),
        _ => {}
    }
    let sats = match check_payment(&t, &a) {
        Ok(s) => s,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    };
    if let Err(body) = w.apply(&t) {
        let status = if body.get("code").is_some() { StatusCode::CONFLICT } else { StatusCode::BAD_REQUEST };
        return (status, Json(body)).into_response();
    }
    if matches!(fault, Some(PayFault::Pending)) {
        return (StatusCode::ACCEPTED, Json(json!({"txid": txid, "pending": true}))).into_response();
    }
    if let Some(p) = w.payments.iter_mut().find(|p| p.txid == txid) {
        p.posts += 1;
    } else {
        w.payments.push(Payment { account: a.clone(), txid: txid.clone(), sats, posts: 1 });
        w.accounts.entry(a.clone()).or_default().paid += i64::try_from(sats).unwrap_or(i64::MAX);
    }
    if matches!(fault, Some(PayFault::LostAnswer)) {
        return (StatusCode::GATEWAY_TIMEOUT, "lost").into_response();
    }
    let balance = w.balance(&a);
    Json(json!({"txid": txid, "credited_sats": sats, "balance_sats": balance})).into_response()
}

fn usage_row(id: &str, cost: u64) -> Value {
    json!({"id": id, "ts": 0, "model": "stub", "prompt_tokens": 10, "completion_tokens": 20, "cached_tokens": 0, "cost_sats": cost, "cost_usd": cost as f64 * BSV_USD / 1e8})
}

async fn request(State(w): State<Shared>, h: HeaderMap, Path(id): Path<String>) -> Response {
    let w = w.lock().expect("world");
    let a = authed!(w, h);
    match w.requests.get(&id) {
        Some((owner, cost)) if *owner == a => Json(usage_row(&id, *cost)).into_response(),
        _ => err(StatusCode::NOT_FOUND, "not settled"),
    }
}

async fn usage(State(w): State<Shared>, h: HeaderMap) -> Response {
    let w = w.lock().expect("world");
    let a = authed!(w, h);
    let rows: Vec<Value> = w.requests.iter().filter(|(_, r)| r.0 == a).map(|(id, r)| usage_row(id, r.1)).collect();
    Json(json!({"data": rows, "next": null})).into_response()
}

#[derive(Deserialize)]
struct ChatBody {
    #[serde(default)]
    stream: bool,
    #[serde(default)]
    model: String,
}

/// Settles a request when its answer ends, whoever ended it.
struct Settle {
    world: Shared,
    account: String,
    id: String,
    cost: u64,
    complete: bool,
    done: bool,
}

impl Settle {
    fn finished(&mut self) {
        self.complete = true;
        self.now();
    }

    fn now(&mut self) {
        if std::mem::replace(&mut self.done, true) {
            return;
        }
        let mut w = self.world.lock().expect("world");
        w.accounts.entry(self.account.clone()).or_default().spent += i64::try_from(self.cost).unwrap_or(0);
        w.requests.insert(self.id.clone(), (self.account.clone(), self.cost));
        w.order.push(self.id.clone());
        if !self.complete {
            w.aborted += 1;
        }
    }
}

impl Drop for Settle {
    fn drop(&mut self) {
        self.now();
    }
}

async fn chat(State(shared): State<Shared>, h: HeaderMap, body: Bytes) -> Response {
    let b: ChatBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => return err(StatusCode::BAD_REQUEST, &e.to_string()),
    };
    let (account, id, cost, text, delay, fault) = {
        let mut w = shared.lock().expect("world");
        let a = authed!(w, h);
        let balance = w.balance(&a);
        let available = balance + w.credit;
        if available < w.estimate {
            let body = json!({"error": {"message": "payment required", "type": "payment_required", "code": "payment_required"},
                "payment": {"pay_to": w.pay_to, "sats_required": w.estimate - available, "balance_sats": balance, "available_sats": available}});
            return (StatusCode::PAYMENT_REQUIRED, Json(body)).into_response();
        }
        w.next += 1;
        w.chats += 1;
        let id = format!("gen-{:08}", w.next);
        (a, id, w.cost, w.text.clone(), w.chunk_delay, w.chat_faults.pop_front())
    };
    let mut settle = Settle { world: shared.clone(), account: account.clone(), id: id.clone(), cost, complete: false, done: false };
    let hold = shared.lock().expect("world").head_delay;
    tokio::time::sleep(hold).await;
    let usage = |w: &World| json!({"prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30, "cost_sats": cost, "cost_usd": cost as f64 * BSV_USD / 1e8, "balance_sats": w.balance(&account)});
    if !b.stream {
        settle.finished();
        let u = usage(&shared.lock().expect("world"));
        let body = json!({"id": id, "object": "chat.completion", "model": b.model, "usage": u,
            "choices": [{"index": 0, "finish_reason": "stop", "message": {"role": "assistant", "content": text}}]});
        return ([("x-twetch-request-id", id), ("x-twetch-cost-sats", cost.to_string())], Json(body)).into_response();
    }
    let chunk = |delta: &Value, finish: Value| {
        format!(
            "data: {}\n\n",
            json!({"id": id, "object": "chat.completion.chunk", "model": b.model, "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
        )
    };
    let mut parts = vec![chunk(&json!({"role": "assistant", "content": ""}), Value::Null)];
    parts.extend(text.split_inclusive(' ').map(|word| chunk(&json!({"content": word}), Value::Null)));
    parts.push(chunk(&json!({}), json!("stop")));
    let cut = fault == Some(ChatFault::CutStream);
    if !cut {
        let mut u = usage(&shared.lock().expect("world"));
        u["balance_sats"] = json!(u["balance_sats"].as_i64().unwrap_or(0) - i64::try_from(cost).unwrap_or(0));
        parts
            .push(format!("data: {}\n\n", json!({"id": id, "request_id": id, "object": "chat.completion.chunk", "model": b.model, "choices": [], "usage": u})));
        parts.push("data: [DONE]\n\n".into());
    }
    let mut queue: VecDeque<String> = parts.into();
    if cut {
        queue.truncate(3);
    }
    let stream = futures_util::stream::unfold((queue, settle), move |(mut q, mut s)| async move {
        tokio::time::sleep(delay).await;
        let Some(part) = q.pop_front() else {
            s.finished();
            return None;
        };
        if part.contains("cost_sats") {
            s.finished();
        }
        Some((Ok::<_, std::io::Error>(Bytes::from(part)), (q, s)))
    });
    ([("content-type", "text/event-stream".to_string()), ("x-twetch-request-id", id.clone())], Body::from_stream(stream)).into_response()
}

async fn unspent(State(w): State<Shared>, Path(address): Path<String>) -> Response {
    let Ok(script) = twetch_crypto::wallet::address_to_p2pkh_hex(&address) else {
        return err(StatusCode::BAD_REQUEST, "bad address");
    };
    let w = w.lock().expect("world");
    let rows: Vec<Value> = w
        .coins
        .iter()
        .filter(|(_, c)| c.address_script == script && c.spent_by.is_none())
        .map(|(op, c)| json!({"tx_hash": op.0, "tx_pos": op.1, "value": c.sats, "isSpentInMempoolTx": false, "status": "confirmed"}))
        .collect();
    Json(json!({"address": address, "result": rows})).into_response()
}

async fn spent(State(w): State<Shared>, Path((txid, vout)): Path<(String, u32)>) -> Response {
    let w = w.lock().expect("world");
    match w.coins.get(&(txid, vout)).and_then(|c| c.spent_by.clone()) {
        Some(by) => Json(json!({"txid": by, "vin": 0, "status": "confirmed"})).into_response(),
        None => (StatusCode::NOT_FOUND, "").into_response(),
    }
}

#[derive(Deserialize)]
struct RawBody {
    txhex: String,
}

async fn raw(State(w): State<Shared>, Json(b): Json<RawBody>) -> Response {
    let Some(t) = hex::decode(&b.txhex).ok().and_then(|r| bitcoin::consensus::deserialize::<Transaction>(&r).ok()) else {
        return (StatusCode::BAD_REQUEST, "unexpected response code 500: TX decode failed").into_response();
    };
    let txid = t.compute_txid().to_string();
    let mut w = w.lock().expect("world");
    if w.sent.contains(&txid) {
        return (StatusCode::BAD_REQUEST, "unexpected response code 500: 257: txn-already-known").into_response();
    }
    match w.apply(&t) {
        Ok(()) => {
            w.sent.push(txid.clone());
            Json(json!(txid)).into_response()
        }
        Err(_) => (StatusCode::BAD_REQUEST, "unexpected response code 500: 258: txn-mempool-conflict").into_response(),
    }
}

#[derive(Deserialize)]
struct FaucetQuery {
    address: String,
    sats: u64,
}

async fn faucet(State(w): State<Shared>, Query(q): Query<FaucetQuery>) -> Json<Value> {
    let op = w.lock().expect("world").faucet(&q.address, q.sats);
    Json(json!({"txid": op.0, "vout": op.1}))
}

async fn state(State(w): State<Shared>) -> Json<Value> {
    let w = w.lock().expect("world");
    Json(json!({
        "chats": w.chats,
        "aborted": w.aborted,
        "requests": w.order.iter().map(|id| json!({"id": id, "cost_sats": w.requests[id].1})).collect::<Vec<_>>(),
        "payments": w.payments.iter().map(|p| json!({"txid": p.txid, "sats": p.sats, "account": p.account, "posts": p.posts})).collect::<Vec<_>>(),
        "accounts": w.accounts.iter().map(|(a, r)| json!({"account": a, "paid": r.paid, "spent": r.spent, "keys": r.keys})).collect::<Vec<_>>(),
        "withdraws": w.sent,
    }))
}

async fn zdr() -> Json<Value> {
    let row = |id: &str| {
        json!({"model_id": id, "tag": "stub/fp8", "context_length": 1_048_576, "max_completion_tokens": 65_536,
            "pricing": {"prompt": "0.0000001", "completion": "0.0000002", "input_cache_read": "0.00000001"},
            "uptime_last_30m": 100, "safety_retention": false, "supported_parameters": ["tools"]})
    };
    Json(json!({"data": [row("deepseek/deepseek-v4.1-flash-1017"), row("z-ai/glm-5.3"), row("z-ai/glm-5.3-flash"), row("deepseek/deepseek-v4-pro-0813")]}))
}

async fn price() -> Json<Value> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
    Json(json!({"bsvUsd": BSV_USD, "quotedAtMs": u64::try_from(now).unwrap_or(0)}))
}

async fn upstream_chat(State(w): State<Shared>, body: Bytes) -> Response {
    let stream = serde_json::from_slice::<ChatBody>(&body).is_ok_and(|b| b.stream);
    let text = w.lock().expect("world").text.clone();
    let usage = json!({"prompt_tokens": 100, "completion_tokens": 50, "cost": 0.00036, "prompt_tokens_details": {"cached_tokens": 20}});
    if !stream {
        return Json(json!({"id": "gen-1", "choices": [{"message": {"role": "assistant", "content": text}}], "usage": usage})).into_response();
    }
    let chunk =
        |delta: Value, finish: Value| format!("data: {}\n\n", json!({"id": "gen-1", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}));
    let mut out = String::from(": OPENROUTER PROCESSING\n\n");
    for word in text.split_inclusive(' ') {
        out.push_str(&chunk(json!({"content": word}), Value::Null));
    }
    out.push_str(&chunk(json!({}), json!("stop")));
    out.push_str(&format!("data: {}\n\ndata: [DONE]\n\n", json!({"id": "gen-1", "choices": [], "usage": usage})));
    ([("content-type", "text/event-stream")], out).into_response()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BroadcastBody {
    tx_hex: String,
}

/// The Twetch relay: puts a payment on the stub's chain.
async fn broadcast(State(w): State<Shared>, Json(b): Json<BroadcastBody>) -> Response {
    let Some(t) = hex::decode(&b.tx_hex).ok().and_then(|r| bitcoin::consensus::deserialize::<Transaction>(&r).ok()) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "not a transaction"}))).into_response();
    };
    let txid = t.compute_txid().to_string();
    match w.lock().expect("world").apply(&t) {
        Ok(()) => Json(json!({"txid": txid, "via": "stub"})).into_response(),
        Err(body) => {
            let status = if body.get("code").is_some() { StatusCode::CONFLICT } else { StatusCode::BAD_GATEWAY };
            (status, Json(body)).into_response()
        }
    }
}
