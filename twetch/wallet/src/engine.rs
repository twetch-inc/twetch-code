//! Pays for requests: one transaction each, signed one at a time, never twice.
//!
//! Every request is one row keyed by its id. A payment's bytes are stored
//! before they leave, so a lost answer or a restart sends the same bytes again.
//! A second transaction for a row is only ever signed once the first can no
//! longer confirm (the chain names another spender of one of its coins).

use crate::chain::{Chain, Sent};
use crate::db::Db;
use crate::gateway::{AccountInfo, Demand, Gateway, Submit};
use crate::model::{now_ms, today, usd_micros, Coin, CoinState, Item, Kind, Limits, Outpoint, State};
use crate::signer::{Amount, Signed, Signer, Status};
use crate::PAY_TO;
use anyhow::{anyhow, bail, ensure, Result};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{watch, Notify};

const RATE_TTL: Duration = Duration::from_secs(600);
const REFRESH_EVERY: Duration = Duration::from_secs(20);
/// A request the gateway still does not know after this long never ran there.
const GONE_AFTER_MS: i64 = 24 * 3600 * 1000;
/// How long a chat waits for its top-up to be credited.
const TOPUP_WAIT: Duration = Duration::from_secs(90);
/// Unclear conflicts are retried with the same bytes this often, then left to the user.
const UNCLEAR_TRIES: u32 = 12;

/// Why a request cannot be paid for, in words for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocked {
    pub code: &'static str,
    pub message: String,
}

impl Blocked {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

struct Rate {
    usd: f64,
    at: Instant,
}

pub struct Engine {
    pub db: Arc<Db>,
    pub gateway: Gateway,
    pub chain: Chain,
    pub signer: Signer,
    rate: tokio::sync::Mutex<Option<Rate>>,
    gateway_rate: Mutex<Option<f64>>,
    /// Held while coins are chosen, signed and submitted.
    paying: tokio::sync::Mutex<()>,
    topping: tokio::sync::Mutex<()>,
    kick: Notify,
    changed: watch::Sender<u64>,
    /// Requests whose answer is still streaming through this process.
    live: Mutex<HashSet<String>>,
    retry: Mutex<HashMap<String, (u32, Instant)>>,
    refreshed: Mutex<Option<Instant>>,
    account: Mutex<Option<(Instant, AccountInfo)>>,
}

impl Engine {
    #[must_use]
    pub fn new(db: Db, gateway: Gateway, chain: Chain, signer: Signer) -> Self {
        Self {
            db: Arc::new(db),
            gateway,
            chain,
            signer,
            rate: tokio::sync::Mutex::new(None),
            gateway_rate: Mutex::new(None),
            paying: tokio::sync::Mutex::new(()),
            topping: tokio::sync::Mutex::new(()),
            kick: Notify::new(),
            changed: watch::Sender::new(0),
            live: Mutex::default(),
            retry: Mutex::default(),
            refreshed: Mutex::default(),
            account: Mutex::default(),
        }
    }

    /// Works until it is dropped.
    pub async fn run(self: Arc<Self>) {
        loop {
            let wait = match self.step().await {
                Ok(w) => w,
                Err(e) => {
                    eprintln!("twetch-wallet: {e:#}");
                    Duration::from_secs(5)
                }
            };
            tokio::select! {
                () = self.kick.notified() => {}
                () = tokio::time::sleep(wait) => {}
            }
        }
    }

    pub fn kick(&self) {
        self.kick.notify_one();
    }

    fn note_change(&self) {
        self.changed.send_modify(|v| *v += 1);
    }

    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    fn put(&self, item: &Item) -> Result<()> {
        self.db.write(|t| t.put(item))?;
        self.note_change();
        Ok(())
    }

    // ---- requests ----

    /// A request has started upstream: remember it before its cost is known.
    ///
    /// # Errors
    /// On a storage failure.
    pub fn open_request(&self, request_id: &str, model: &str) -> Result<()> {
        let id = format!("req:{request_id}");
        self.live.lock().expect("live").insert(id.clone());
        self.db.write(|t| {
            if t.item(&id)?.is_none() {
                let mut item = t.new_item(id.clone(), Kind::Request, State::Open, 0, PAY_TO)?;
                item.model = model.to_string();
                t.put(&item)?;
            }
            Ok(())
        })?;
        self.note_change();
        Ok(())
    }

    /// The cost of a request. Saying it again changes nothing.
    ///
    /// # Errors
    /// On a storage failure.
    pub fn settle_request(&self, request_id: &str, sats: u64) -> Result<()> {
        let id = format!("req:{request_id}");
        self.live.lock().expect("live").remove(&id);
        self.db.write(|t| {
            let mut item = match t.item(&id)? {
                Some(i) => i,
                None => t.new_item(id.clone(), Kind::Request, State::Open, 0, PAY_TO)?,
            };
            if item.state == State::Open {
                item.sats = sats;
                item.state = if sats == 0 { State::Free } else { State::Owed };
                t.put(&item)?;
            }
            Ok(())
        })?;
        self.note_change();
        self.kick();
        Ok(())
    }

    /// The answer ended without saying its cost: ask for it.
    pub fn request_ended(&self, request_id: &str) {
        self.live.lock().expect("live").remove(&format!("req:{request_id}"));
        self.kick();
    }

    // ---- money and limits ----

    pub fn learn_rate(&self, usd: f64) {
        if usd.is_finite() && usd > 0.0 {
            *self.gateway_rate.lock().expect("rate") = Some(usd);
        }
    }

    /// USD per BSV for the limits: the higher of the chain provider's price and
    /// the gateway's, so neither can shrink what a payment counts for.
    ///
    /// # Errors
    /// When no price is known: a limit that cannot be checked refuses.
    pub async fn bsv_usd(&self) -> Result<f64> {
        let mut held = self.rate.lock().await;
        if held.as_ref().is_none_or(|r| r.at.elapsed() > RATE_TTL) {
            if let Ok(usd) = self.chain.bsv_usd().await {
                *held = Some(Rate { usd, at: Instant::now() });
            }
        }
        let chain = held.as_ref().map(|r| r.usd);
        drop(held);
        let gateway = *self.gateway_rate.lock().expect("rate");
        match (chain, gateway) {
            (Some(a), Some(b)) => Ok(a.max(b)),
            (Some(a), None) | (None, Some(a)) => Ok(a),
            (None, None) => {
                let info = self.account(Duration::ZERO).await.map_err(|_| anyhow!("the BSV price is not known yet"))?;
                Ok(info.bsv_usd)
            }
        }
    }

    /// The gateway's view of this account, at most `age` old.
    ///
    /// # Errors
    /// When the gateway cannot be asked.
    pub async fn account(&self, age: Duration) -> Result<AccountInfo> {
        if let Some((at, info)) = self.account.lock().expect("account").as_ref() {
            if at.elapsed() < age {
                return Ok(info.clone());
            }
        }
        let info = self.gateway.account().await?;
        self.learn_rate(info.bsv_usd);
        *self.account.lock().expect("account") = Some((Instant::now(), info.clone()));
        Ok(info)
    }

    /// Why a payment of this size may not be made without the user.
    async fn over_limit(&self, sats: u64) -> Result<Option<Blocked>> {
        let usd = self.bsv_usd().await?;
        let limits = self.db.limits()?;
        let cost = usd_micros(sats, usd);
        let spent = self.db.spent_on(&today())?;
        Ok(check_limits(limits, cost, spent))
    }

    /// May another request start? Counts what is owed but not yet signed.
    ///
    /// # Errors
    /// The reason, for the user.
    pub async fn admit(&self) -> Result<(), Blocked> {
        match &self.signer.account().status {
            Status::Ready => {}
            Status::Missing => {
                return Err(Blocked::new("wallet_missing", "TwetchCode's recovery phrase is not in this computer's keychain. Open Wallet to restore it."))
            }
            Status::Locked(_) => {
                return Err(Blocked::new(
                    "wallet_locked",
                    "TwetchCode could not read its wallet from this computer's keychain. Unlock the keychain and reopen TwetchCode.",
                ))
            }
        }
        let internal = |e: anyhow::Error| Blocked::new("wallet_error", format!("The wallet could not check your spending limits: {e:#}"));
        let active = self.db.active().map_err(internal)?;
        if let Some(h) = active.iter().find(|i| i.state == State::Held) {
            return Err(Blocked::new(
                "payment_needs_approval",
                format!("A payment needs your approval. {} Open Wallet to approve it or raise the limit.", h.note),
            ));
        }
        let usd = self.bsv_usd().await.map_err(internal)?;
        let limits = self.db.limits().map_err(internal)?;
        let spent = self.db.spent_on(&today()).map_err(internal)?;
        let owed: u64 = active.iter().filter(|i| i.state == State::Owed).map(|i| usd_micros(i.sats, usd)).sum();
        if micros_of(limits.per_day_usd) <= spent + owed {
            return Err(Blocked::new(
                "daily_limit",
                format!("You reached your daily spending limit of {}. Open Wallet to raise it.", dollars(limits.per_day_usd)),
            ));
        }
        Ok(())
    }

    /// New limits: look at what was waiting again.
    ///
    /// # Errors
    /// On a storage failure.
    pub fn set_limits(&self, limits: Limits) -> Result<()> {
        self.db.set_limits(limits)?;
        self.retry_now();
        Ok(())
    }

    /// Try everything again now (new limits, a deposit).
    pub fn retry_now(&self) {
        self.retry.lock().expect("retry").clear();
        self.note_change();
        self.kick();
    }

    /// The user lets one held payment through.
    ///
    /// # Errors
    /// On a storage failure or when nothing is held.
    pub fn approve(&self, id: &str) -> Result<()> {
        let mut item = self.db.item(id)?.ok_or_else(|| anyhow!("no such payment"))?;
        ensure!(item.state == State::Held, "that payment is not waiting");
        item.state = State::Owed;
        item.approved = true;
        item.note.clear();
        self.put(&item)?;
        self.settled(id);
        self.kick();
        Ok(())
    }

    /// Send a failed payment's bytes again.
    ///
    /// # Errors
    /// On a storage failure or when it did not fail.
    pub fn retry_failed(&self, id: &str) -> Result<()> {
        let mut item = self.db.item(id)?.ok_or_else(|| anyhow!("no such payment"))?;
        ensure!(item.state == State::Failed && !item.tx_hex.is_empty(), "that payment cannot be sent again");
        item.state = State::Signed;
        item.note.clear();
        self.put(&item)?;
        self.retry.lock().expect("retry").remove(id);
        self.kick();
        Ok(())
    }

    /// Give a failed payment's coins back to the wallet. It is never signed again.
    ///
    /// # Errors
    /// On a storage failure or when it did not fail.
    pub fn cancel_failed(&self, id: &str) -> Result<()> {
        self.db.write(|t| {
            let mut item = t.item(id)?.ok_or_else(|| anyhow!("no such payment"))?;
            ensure!(item.state == State::Failed, "that payment did not fail");
            for op in &item.inputs {
                if t.coin(op)?.is_some_and(|c| c.state == CoinState::Spent) {
                    t.set_coin_state(op, CoinState::Available)?;
                }
            }
            if item.change > 0 {
                t.set_coin_state(&(item.txid.clone(), item.change_vout), CoinState::Dropped)?;
            }
            if !item.day.is_empty() {
                t.sub_day(&item.day, item.usd_micros)?;
            }
            item.state = State::Canceled;
            t.put(&item)?;
            Ok(())
        })?;
        self.note_change();
        Ok(())
    }

    // ---- coins ----

    /// Ask the chain for our coins, unless it was asked a moment ago. `force` is for a coin
    /// that turned out to be gone, which is rare.
    ///
    /// # Errors
    /// When the provider cannot be asked.
    pub async fn refresh_coins(&self, force: bool) -> Result<()> {
        {
            let mut last = self.refreshed.lock().expect("refreshed");
            if !force && last.is_some_and(|at| at.elapsed() < REFRESH_EVERY) {
                return Ok(());
            }
            *last = Some(Instant::now());
        }
        let address = self.signer.account().address;
        let listed = self.chain.unspent(&address).await?;
        self.db.write(|t| {
            for (op, sats) in &listed {
                match t.coin(op)? {
                    None => t.put_coin(op, &Coin { sats: *sats, state: CoinState::Available, seen: true })?,
                    // Spent and dropped coins stay so: the provider lags us.
                    Some(c) if c.state == CoinState::Available && !c.seen => t.put_coin(op, &Coin { seen: true, ..c })?,
                    Some(_) => {}
                }
            }
            Ok(())
        })?;
        // A coin the chain showed us and no longer does was spent elsewhere.
        let gone: Vec<Outpoint> =
            self.db.coins()?.into_iter().filter(|(op, c)| c.state == CoinState::Available && c.seen && !listed.contains_key(op)).map(|(op, _)| op).collect();
        if !gone.is_empty() {
            self.db.write(|t| gone.iter().try_for_each(|op| t.set_coin_state(op, CoinState::Dropped)))?;
        }
        self.note_change();
        Ok(())
    }

    /// `(spendable now, change still on its way)`.
    ///
    /// # Errors
    /// On a storage failure.
    pub fn balance(&self) -> Result<(u64, u64)> {
        let ready = self.db.available()?.iter().map(|c| c.1).sum();
        let coming = self.db.active()?.iter().filter(|i| i.state == State::Signed).map(|i| i.change).sum();
        Ok((ready, coming))
    }

    // ---- the payment loop ----

    /// Do everything that is due. Returns how long until the next thing is.
    ///
    /// # Errors
    /// On a storage failure.
    pub async fn step(&self) -> Result<Duration> {
        let _paying = self.paying.lock().await;
        for item in self.db.active()? {
            if !self.due(&item.id) {
                continue;
            }
            match item.state {
                State::Open => self.ask_cost(item).await?,
                // A top-up is signed as it is made; one that was not is for a request that is gone.
                State::Owed | State::Held if item.kind == Kind::Topup && item.old_txids.is_empty() => {
                    self.db.write(|t| t.forget(&item))?;
                }
                State::Owed | State::Held => {
                    if let Some(signed) = self.sign(item).await? {
                        self.submit(signed).await?;
                    }
                }
                State::Signed | State::Pending => self.submit(item).await?,
                _ => {}
            }
        }
        let now = Instant::now();
        let next = self.retry.lock().expect("retry").values().map(|r| r.1.saturating_duration_since(now)).min();
        Ok(next.unwrap_or(Duration::from_secs(60)).clamp(Duration::from_millis(50), Duration::from_secs(60)))
    }

    fn due(&self, id: &str) -> bool {
        !self.live.lock().expect("live").contains(id) && self.retry.lock().expect("retry").get(id).is_none_or(|r| r.1 <= Instant::now())
    }

    /// Wait before the next try of this row, longer each time. Returns the tries so far.
    fn back_off(&self, id: &str) -> u32 {
        let mut map = self.retry.lock().expect("retry");
        let e = map.entry(id.to_string()).or_insert((0, Instant::now()));
        e.0 += 1;
        e.1 = Instant::now() + Duration::from_millis(500u64 << e.0.min(7));
        e.0
    }

    fn settled(&self, id: &str) {
        self.retry.lock().expect("retry").remove(id);
    }

    async fn ask_cost(&self, mut item: Item) -> Result<()> {
        let request_id = item.id.trim_start_matches("req:").to_string();
        match self.gateway.request_cost(&request_id).await {
            Ok(Some(sats)) => {
                self.settled(&item.id);
                self.settle_request(&request_id, sats)
            }
            Ok(None) | Err(_) => {
                self.back_off(&item.id);
                if now_ms() - item.created_ms > GONE_AFTER_MS {
                    item.state = State::Gone;
                    item.note = "The gateway never reported this request.".into();
                    self.settled(&item.id);
                    self.put(&item)?;
                }
                Ok(())
            }
        }
    }

    /// Sign an owed payment and store its bytes with the coins they spend, in
    /// one transaction. `None` when it has to wait.
    async fn sign(&self, mut item: Item) -> Result<Option<Item>> {
        if self.signer.account().status != Status::Ready {
            self.back_off(&item.id);
            return Ok(None);
        }
        if !item.approved {
            match self.over_limit(item.sats).await {
                Ok(None) => {}
                Ok(Some(why)) => {
                    if item.state != State::Held || item.note != why.message {
                        item.state = State::Held;
                        item.note = why.message;
                        self.put(&item)?;
                    }
                    self.back_off(&item.id);
                    return Ok(None);
                }
                Err(e) => {
                    self.wait(item, &format!("{e:#}"))?;
                    return Ok(None);
                }
            }
        }
        match self.sign_now(item.clone()).await {
            Ok(signed) => Ok(Some(signed)),
            Err(e) => {
                self.wait(item, &format!("{e:#}"))?;
                Ok(None)
            }
        }
    }

    fn wait(&self, mut item: Item, why: &str) -> Result<()> {
        self.back_off(&item.id);
        if item.note != why || item.state != State::Owed {
            item.state = State::Owed;
            item.note = why.to_string();
            self.put(&item)?;
        }
        Ok(())
    }

    async fn sign_now(&self, item: Item) -> Result<Item> {
        let signed = match self.build(&item).await {
            Ok(s) => s,
            Err(first) => {
                // Our own change may be on its way; otherwise look for a deposit.
                if self.db.active()?.iter().any(|i| i.state == State::Signed) {
                    return Err(first);
                }
                self.refresh_coins(false).await.map_err(|e| anyhow!("{first:#}; coins could not be refreshed: {e:#}"))?;
                self.build(&item).await?
            }
        };
        self.store_signed(item, signed).await
    }

    async fn build(&self, item: &Item) -> Result<Signed> {
        let coins = enough(self.db.available()?, item.sats);
        self.signer.pay(coins, item.sats, &item.to).await
    }

    async fn store_signed(&self, mut item: Item, s: Signed) -> Result<Item> {
        let micros = usd_micros(item.sats + s.fee, self.bsv_usd().await.unwrap_or(0.0));
        let old_day = std::mem::replace(&mut item.day, today());
        let old_micros = std::mem::replace(&mut item.usd_micros, micros);
        item.state = State::Signed;
        item.txid = s.txid;
        item.tx_hex = s.tx_hex;
        item.fee = s.fee;
        item.inputs = s.inputs;
        item.change = s.change;
        item.change_vout = s.change_vout;
        item.note.clear();
        let counts = item.kind != Kind::Withdraw;
        self.db.write(|t| {
            // The row as stored decides: a second signature needs a dead first one.
            if let Some(now) = t.item(&item.id)? {
                ensure!(matches!(now.state, State::Owed | State::Held), "{} was already signed", item.id);
            }
            t.spend(&item.inputs)?;
            if counts {
                if !old_day.is_empty() {
                    t.sub_day(&old_day, old_micros)?;
                }
                t.add_day(&item.day, micros)?;
            }
            t.put(&item)
        })?;
        self.settled(&item.id);
        self.note_change();
        Ok(item)
    }

    /// Send the stored bytes and act on the answer.
    async fn submit(&self, item: Item) -> Result<()> {
        if item.kind == Kind::Withdraw {
            return self.broadcast(item).await;
        }
        let answer = match self.gateway.submit(&item.tx_hex).await {
            Ok(a) => a,
            Err(e) => Submit::Later(format!("no answer: {e:#}")),
        };
        match answer {
            Submit::Credited { txid } if txid == item.txid => self.accepted(item, State::Paid),
            Submit::Credited { txid } => {
                self.back_off(&item.id);
                self.remember(item, &format!("the gateway answered for another transaction ({txid})"))
            }
            Submit::Pending => {
                self.back_off(&item.id);
                self.accepted(item, State::Pending)
            }
            Submit::Conflict { named, retry, body } => self.conflict(item, &named, retry, &body).await,
            Submit::Later(why) => {
                self.back_off(&item.id);
                self.remember(item, &why)
            }
            Submit::Invalid(why) => self.fail(item, &why),
        }
    }

    fn remember(&self, mut item: Item, note: &str) -> Result<()> {
        if item.note != note {
            item.note = note.to_string();
            self.put(&item)?;
        }
        Ok(())
    }

    fn fail(&self, mut item: Item, note: &str) -> Result<()> {
        self.settled(&item.id);
        item.state = State::Failed;
        item.note = note.to_string();
        self.put(&item)
    }

    /// The network has it: its change can be spent.
    fn accepted(&self, mut item: Item, state: State) -> Result<()> {
        if item.state == state {
            return Ok(());
        }
        let first = !item.state.is_accepted();
        item.state = state;
        item.note.clear();
        self.db.write(|t| {
            if first && item.change > 0 {
                let op = (item.txid.clone(), item.change_vout);
                if t.coin(&op)?.is_none() {
                    t.put_coin(&op, &Coin { sats: item.change, state: CoinState::Available, seen: false })?;
                }
            }
            t.put(&item)
        })?;
        if state == State::Paid {
            self.settled(&item.id);
            *self.account.lock().expect("account") = None;
        }
        self.note_change();
        Ok(())
    }

    /// A coin is said to be gone. Only a transaction that provably cannot
    /// confirm is replaced, and only once.
    async fn conflict(&self, item: Item, named: &[Outpoint], retry: bool, body: &str) -> Result<()> {
        let ours: Vec<Outpoint> = item.inputs.iter().filter(|op| named.contains(op)).cloned().collect();
        let mut dead: Vec<Outpoint> = Vec::new();
        let mut already = false;
        // Nothing named is ours: ask about every input.
        let ask = if ours.is_empty() { &item.inputs } else { &ours };
        for op in ask {
            if self.db.is_dead(&op.0)? {
                dead.push(op.clone());
                continue;
            }
            match self.chain.spender(op).await {
                Ok(Some(by)) if by == item.txid => already = true,
                Ok(Some(_)) => dead.push(op.clone()),
                Ok(None) | Err(_) => {}
            }
        }
        if dead.is_empty() {
            // It is on chain already, or nobody can say: the same bytes again.
            let tries = self.back_off(&item.id);
            if !already && (tries >= UNCLEAR_TRIES || !retry) {
                return self.fail(item, &format!("A coin is said to be spent, but the chain does not show by what: {body}"));
            }
            return self.remember(item, body);
        }
        if item.rebuilt {
            return self.fail(item, &format!("Refused twice: {body}"));
        }
        let next = self.db.write(|t| {
            let mut next = t.item(&item.id)?.ok_or_else(|| anyhow!("payment vanished"))?;
            t.mark_dead(&item.txid)?;
            for op in &item.inputs {
                let state = if dead.contains(op) { CoinState::Dropped } else { CoinState::Available };
                t.set_coin_state(op, state)?;
            }
            if item.change > 0 {
                t.set_coin_state(&(item.txid.clone(), item.change_vout), CoinState::Dropped)?;
            }
            next.old_txids.push(std::mem::take(&mut next.txid));
            next.tx_hex.clear();
            next.inputs.clear();
            next.change = 0;
            next.fee = 0;
            next.rebuilt = true;
            // It passed the limits once; its amount is still counted today.
            next.approved = true;
            next.state = State::Owed;
            next.note = "A coin was spent elsewhere; paying again with another.".into();
            t.put(&next)?;
            Ok(next)
        })?;
        self.settled(&next.id);
        self.note_change();
        let _ = self.refresh_coins(true).await;
        if next.kind == Kind::Withdraw {
            return self.fail(next, "A coin was spent elsewhere. Nothing was sent; withdraw again.");
        }
        if let Some(signed) = self.sign(next).await? {
            Box::pin(self.submit(signed)).await?;
        }
        Ok(())
    }

    // ---- top-up ----

    /// Pay what a refused request needs, and wait until it is credited.
    ///
    /// # Errors
    /// The reason, for the user.
    pub async fn top_up(&self, ask: &Demand) -> Result<(), Blocked> {
        let internal = |e: anyhow::Error| Blocked::new("wallet_error", format!("The wallet could not pay: {e:#}"));
        if ask.pay_to != PAY_TO {
            return Err(Blocked::new("wrong_payee", "The gateway asked for a payment to an address that is not Twetch's. Nothing was paid."));
        }
        let _one = self.topping.lock().await;
        // Finish what is owed first: that is money the gateway is waiting for too.
        self.drain(Duration::from_secs(20)).await;
        let info = self.account(Duration::ZERO).await.map_err(internal)?;
        if info.pay_to != PAY_TO {
            return Err(Blocked::new("wrong_payee", "The gateway names a payment address that is not Twetch's. Nothing was paid."));
        }
        // An earlier top-up still on its way comes first.
        let on_the_way = self.db.active().map_err(internal)?.into_iter().find(|i| i.kind == Kind::Topup);
        let id = if let Some(t) = on_the_way {
            t.id
        } else {
            let gained = u64::try_from((info.available_sats - ask.available_sats).max(0)).unwrap_or(0);
            let need = ask.sats_required.saturating_sub(gained);
            if need == 0 {
                return Ok(());
            }
            let sats = need.max(info.min_topup_sats);
            if let Some(why) = self.over_limit(sats).await.map_err(internal)? {
                return Err(why);
            }
            let paying = self.paying.lock().await;
            let item = self.db.write(|t| t.new_item("top:".into(), Kind::Topup, State::Owed, sats, PAY_TO)).map_err(internal)?;
            let id = item.id.clone();
            match self.sign_now(item.clone()).await {
                Ok(signed) => self.submit(signed).await.map_err(internal)?,
                Err(e) => {
                    // Not signed, so nothing is owed: the next request asks again.
                    self.db.write(|t| t.forget(&item)).map_err(internal)?;
                    self.note_change();
                    return Err(self.short_of(sats, &e));
                }
            }
            drop(paying);
            id
        };
        self.kick();
        match self.wait_done(&id, TOPUP_WAIT).await.map_err(internal)? {
            State::Paid => Ok(()),
            State::Failed => Err(Blocked::new("payment_failed", "The payment was refused. Open Wallet to see why.")),
            _ => Err(Blocked::new("payment_pending", "The payment was sent and is not credited yet. Try again in a moment; it is not paid twice.")),
        }
    }

    fn short_of(&self, sats: u64, e: &anyhow::Error) -> Blocked {
        let text = format!("{e:#}");
        if text.contains("insufficient funds") || text.contains("no coins") {
            let a = self.signer.account().address;
            Blocked::new(
                "insufficient_funds",
                format!("Your TwetchCode wallet needs {sats} more satoshis (plus a small network fee) for this request. Open Wallet and send BSV to {a}."),
            )
        } else {
            Blocked::new("wallet_error", format!("The wallet could not pay: {text}"))
        }
    }

    /// Wait until nothing owed can make progress right now.
    async fn drain(&self, limit: Duration) {
        let end = Instant::now() + limit;
        loop {
            let busy = self
                .db
                .active()
                .is_ok_and(|all| all.iter().any(|i| i.kind == Kind::Request && matches!(i.state, State::Owed | State::Signed) && i.note.is_empty()));
            if !busy || Instant::now() >= end {
                return;
            }
            self.kick();
            let mut rx = self.subscribe();
            let _ = tokio::time::timeout(Duration::from_millis(500), rx.changed()).await;
        }
    }

    /// The state of a payment once it is finished, or where it got to in time.
    ///
    /// # Errors
    /// On a storage failure.
    pub async fn wait_done(&self, id: &str, limit: Duration) -> Result<State> {
        let end = Instant::now() + limit;
        let mut rx = self.subscribe();
        loop {
            let state = self.db.item(id)?.map_or(State::Gone, |i| i.state);
            let left = end.saturating_duration_since(Instant::now());
            if !state.is_active() || left.is_zero() {
                return Ok(state);
            }
            let _ = tokio::time::timeout(left, rx.changed()).await;
        }
    }

    // ---- withdraw ----

    /// Send BSV out, because the user asked on the wallet page. `None` sends everything.
    ///
    /// # Errors
    /// When the address or amount is bad, or the coins do not cover it.
    pub async fn withdraw(&self, to: &str, sats: Option<u64>) -> Result<Item> {
        let _paying = self.paying.lock().await;
        let mut coins = self.db.available()?;
        if coins.is_empty() {
            self.refresh_coins(false).await?;
            coins = self.db.available()?;
        }
        ensure!(!coins.is_empty(), "there is nothing to withdraw");
        let amount = sats.map_or(Amount::Max, Amount::Sats);
        let total: u64 = coins.iter().map(|c| c.1).sum();
        let picked = sats.map_or_else(|| coins.clone(), |s| enough(coins.clone(), s));
        let signed = self.signer.withdraw(picked, amount, to).await?;
        let sent = sats.unwrap_or(total - signed.fee);
        let item = self.db.write(|t| t.new_item("out:".into(), Kind::Withdraw, State::Owed, sent, to))?;
        let id = item.id.clone();
        let item = self.store_signed(item, signed).await?;
        self.submit(item).await?;
        self.kick();
        self.db.item(&id)?.ok_or_else(|| anyhow!("payment vanished"))
    }

    async fn broadcast(&self, item: Item) -> Result<()> {
        match self.chain.send(&item.tx_hex).await {
            Ok(Sent::Accepted) => self.accepted(item, State::Paid),
            Ok(Sent::Conflict(why)) => {
                let inputs = item.inputs.clone();
                self.conflict(item, &inputs, true, &why).await
            }
            Ok(Sent::Refused(why)) => self.fail(item, &why),
            Err(e) => {
                self.back_off(&item.id);
                self.remember(item, &format!("no answer: {e:#}"))
            }
        }
    }

    /// Replace the wallet with a backed-up phrase, only while this one is unused.
    ///
    /// # Errors
    /// When this wallet holds anything, or the phrase is bad.
    pub async fn restore(&self, phrase: zeroize::Zeroizing<String>) -> Result<()> {
        let _paying = self.paying.lock().await;
        if self.signer.account().status == Status::Ready {
            self.refresh_coins(true).await?;
            let (ready, coming) = self.balance()?;
            if ready + coming > 0 || !self.db.recent(1)?.is_empty() {
                bail!("This wallet has been used. Withdraw everything and use a fresh TwetchCode home to restore another.");
            }
        }
        self.signer.restore(phrase, self.db.clone()).await?;
        self.gateway.forget_key().await;
        *self.account.lock().expect("account") = None;
        *self.refreshed.lock().expect("refreshed") = None;
        self.note_change();
        Ok(())
    }
}

fn micros_of(usd: f64) -> u64 {
    (usd.max(0.0) * 1e6) as u64
}

#[must_use]
pub fn dollars(usd: f64) -> String {
    format!("${usd:.2}")
}

fn check_limits(limits: Limits, cost: u64, spent: u64) -> Option<Blocked> {
    let usd = cost as f64 / 1e6;
    if cost > micros_of(limits.per_payment_usd) {
        return Some(Blocked::new(
            "payment_limit",
            format!("This payment is {}, over your limit of {} per payment. Open Wallet to raise it.", dollars(usd), dollars(limits.per_payment_usd)),
        ));
    }
    if spent + cost > micros_of(limits.per_day_usd) {
        return Some(Blocked::new(
            "daily_limit",
            format!("This payment of {} would pass your daily limit of {}. Open Wallet to raise it.", dollars(usd), dollars(limits.per_day_usd)),
        ));
    }
    None
}

/// The largest coins that surely cover `sats` and the network fee.
fn enough(coins: Vec<(Outpoint, u64)>, sats: u64) -> Vec<(Outpoint, u64)> {
    let mut out = Vec::new();
    let mut have = 0u64;
    for c in coins {
        // 400 bytes of envelope and outputs, 148 per input, at twice the rate.
        let need = sats + (400 + 148 * (out.len() as u64 + 1)) * crate::FEE_PER_KB * 2 / 1000;
        if have >= need {
            break;
        }
        have += c.1;
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits() {
        let l = Limits { per_payment_usd: 1.0, per_day_usd: 2.0 };
        assert!(check_limits(l, 1_000_000, 0).is_none());
        assert_eq!(check_limits(l, 1_000_001, 0).unwrap().code, "payment_limit");
        assert!(check_limits(l, 900_000, 1_100_000).is_none());
        assert_eq!(check_limits(l, 900_001, 1_100_000).unwrap().code, "daily_limit");
        assert_eq!(check_limits(Limits { per_payment_usd: 0.0, per_day_usd: 0.0 }, 1, 0).unwrap().code, "payment_limit");
        // $14.10 a BSV: a dollar is about 7.09M sats, rounded against us.
        assert_eq!(usd_micros(7_092_199, 14.1), 1_000_001);
        assert_eq!(usd_micros(1, 14.1), 1);
    }

    #[test]
    fn picks_the_fewest_largest_coins() {
        let coins = |v: &[u64]| v.iter().map(|s| ((String::new(), 0), *s)).collect::<Vec<_>>();
        assert_eq!(enough(coins(&[10_000, 500, 400]), 1000).len(), 1);
        assert_eq!(enough(coins(&[1000, 500, 400]), 1200).len(), 2);
        assert_eq!(enough(coins(&[10, 10]), 1200).len(), 2);
    }
}
