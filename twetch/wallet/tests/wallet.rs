//! The whole sidecar over HTTP, against the stub gateway and chain.

use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use twetch_stub::{ChatFault, PayFault, Shared, PAY_TO};
use twetch_wallet::model::{CoinState, Kind, Limits, State};
use twetch_wallet::secrets::{MemorySecrets, SecretStore, GATEWAY_KEY};
use twetch_wallet::{start, Config, Running};

struct Rig {
    home: tempfile::TempDir,
    base: String,
    world: Shared,
    secrets: Arc<MemorySecrets>,
    http: reqwest::Client,
    run: Option<Running>,
    /// Each sidecar has its own runtime, so a crash takes every task of it.
    runtime: Option<tokio::runtime::Runtime>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.run = None;
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_background();
        }
    }
}

impl Rig {
    async fn new() -> Self {
        let (base, world) = twetch_stub::spawn().await;
        let mut rig = Self { home: tempfile::tempdir().unwrap(), base, world, secrets: Arc::default(), http: reqwest::Client::new(), run: None, runtime: None };
        rig.start().await;
        rig
    }

    async fn funded(sats: u64) -> Self {
        let rig = Self::new().await;
        rig.deposit(sats).await;
        rig
    }

    fn config(&self) -> Config {
        Config { home: PathBuf::from(self.home.path()), port: 0, gateway: self.base.clone(), woc: format!("{}/woc", self.base) }
    }

    async fn start(&mut self) {
        // The old process's store lock goes when its last task does.
        for _ in 0..100 {
            let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
            let started = rt.spawn(start(self.config(), self.secrets.clone())).await.unwrap();
            match started {
                Ok(r) => {
                    self.runtime = Some(rt);
                    self.run = Some(r);
                    return;
                }
                Err(e) if format!("{e:#}").contains("already open") => {
                    rt.shutdown_background();
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => panic!("{e:#}"),
            }
        }
        panic!("the store stayed locked");
    }

    /// Kill it where it stands and start again on the same folder.
    async fn crash(&mut self) {
        self.run = None;
        self.runtime.take().unwrap().shutdown_background();
        self.start().await;
    }

    fn run(&self) -> &Running {
        self.run.as_ref().unwrap()
    }

    fn address(&self) -> String {
        self.run().engine.signer.account().address
    }

    async fn deposit(&self, sats: u64) {
        self.world.lock().unwrap().faucet(&self.address(), sats);
        self.run().engine.refresh_coins(true).await.unwrap();
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.run().addr)
    }

    async fn chat(&self, stream: bool) -> reqwest::Response {
        self.http
            .post(self.url("/v1/chat/completions"))
            .bearer_auth(&self.run().proxy_token)
            .json(&json!({"model": "deepseek/deepseek-v4.1-flash-1017", "stream": stream, "messages": [{"role": "user", "content": "hi"}]}))
            .send()
            .await
            .unwrap()
    }

    async fn until(&self, what: &str, f: impl Fn(&Self) -> bool) {
        let end = Instant::now() + Duration::from_secs(15);
        while !f(self) {
            assert!(Instant::now() < end, "timed out: {what}; {:#?}", self.run().engine.db.recent(10).unwrap());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn all_paid(&self, requests: usize) {
        self.until("every request paid", |r| {
            let rows = r.run().engine.db.recent(100).unwrap();
            rows.iter().filter(|i| i.kind == Kind::Request && i.state == State::Paid).count() == requests
        })
        .await;
    }

    /// Payments the stub credited, without the top-ups.
    fn credited(&self, sats: u64) -> usize {
        self.world.lock().unwrap().payments.iter().filter(|p| p.sats == sats).count()
    }

    /// Our coins and the chain's agree to the satoshi.
    fn books_balance(&self) {
        let (ready, _) = self.run().engine.balance().unwrap();
        assert_eq!(ready, self.world.lock().unwrap().unspent_sats(&self.address()));
    }

    /// What the gateway lets an unpaid account use.
    fn credit(&self, sats: i64) {
        self.world.lock().unwrap().credit = sats;
    }

    async fn page(&self) -> Page {
        let link: Value = self.http.post(self.url("/local/wallet-link")).bearer_auth(&self.run().admin_token).send().await.unwrap().json().await.unwrap();
        let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
        let res = http.get(self.url(link["path"].as_str().unwrap())).send().await.unwrap();
        assert_eq!(res.status(), 303);
        let cookie = res.headers()["set-cookie"].to_str().unwrap().split(';').next().unwrap().to_string();
        let secret = res.headers()["location"].to_str().unwrap().split('#').nth(1).unwrap().to_string();
        Page { http, base: format!("http://{}", self.run().addr), cookie, secret }
    }
}

struct Page {
    http: reqwest::Client,
    base: String,
    cookie: String,
    secret: String,
}

impl Page {
    async fn get(&self, path: &str) -> reqwest::Response {
        self.http.get(format!("{}/wallet/api/{path}", self.base)).header("cookie", &self.cookie).header("x-twetch-session", &self.secret).send().await.unwrap()
    }

    async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        self.http
            .post(format!("{}/wallet/api/{path}", self.base))
            .header("cookie", &self.cookie)
            .header("x-twetch-session", &self.secret)
            .header("origin", &self.base)
            .json(&body)
            .send()
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn first_request_tops_up_then_pays_exactly_its_cost() {
    let rig = Rig::funded(2_000_000).await;
    let res = rig.chat(true).await;
    assert_eq!(res.status(), 200);
    let text = res.text().await.unwrap();
    assert!(text.contains("\"cost_sats\":4200") && text.ends_with("data: [DONE]\n\n"));
    rig.all_paid(1).await;
    let w = rig.world.lock().unwrap();
    assert_eq!(w.payments.iter().map(|p| p.sats).collect::<Vec<_>>(), [100_000, 4200], "the top-up, then the cost");
    let a = w.accounts.values().next().unwrap();
    assert_eq!((a.paid, a.spent, a.keys), (104_200, 4200, 1));
    drop(w);
    rig.books_balance();
}

#[tokio::test]
async fn streams_without_holding_chunks_back() {
    use futures_util::StreamExt;
    let rig = Rig::funded(2_000_000).await;
    rig.credit(1_000_000);
    rig.world.lock().unwrap().chunk_delay = Duration::from_millis(60);
    let began = Instant::now();
    let mut body = rig.chat(true).await.bytes_stream();
    body.next().await.unwrap().unwrap();
    let first = began.elapsed();
    let mut chunks = 1;
    while body.next().await.is_some() {
        chunks += 1;
    }
    assert!(chunks >= 10, "{chunks}");
    assert!(first * 4 < began.elapsed(), "first {first:?} of {:?}", began.elapsed());
    rig.all_paid(1).await;
}

#[tokio::test]
async fn many_requests_one_transaction_each_on_one_coin() {
    let rig = Rig::funded(3_000_000).await;
    rig.credit(10_000_000);
    let calls = (0..8).map(|i| rig.chat(i % 2 == 0));
    for r in futures_util::future::join_all(calls).await {
        assert_eq!(r.status(), 200);
        r.text().await.unwrap();
    }
    rig.all_paid(8).await;
    assert_eq!(rig.credited(4200), 8);
    rig.books_balance();
    // Chained on our own change: the chain was asked for coins once, at the deposit.
    assert_eq!(rig.run().engine.db.available().unwrap().len(), 1);
}

#[tokio::test]
async fn crash_after_signing_sends_the_same_bytes() {
    let mut rig = Rig::funded(2_000_000).await;
    rig.credit(1_000_000);
    rig.world.lock().unwrap().pay_faults.extend([PayFault::Down]);
    rig.chat(false).await.text().await.unwrap();
    rig.until("signed", |r| r.run().engine.db.active().unwrap().iter().any(|i| i.state == State::Signed)).await;
    let before = rig.run().engine.db.active().unwrap().remove(0);
    rig.crash().await;
    rig.all_paid(1).await;
    let after = rig.run().engine.db.recent(1).unwrap().remove(0);
    assert_eq!((after.txid, after.tx_hex), (before.txid, before.tx_hex));
    assert_eq!(rig.credited(4200), 1);
    rig.books_balance();
}

#[tokio::test]
async fn lost_answer_then_crash_is_credited_once() {
    let mut rig = Rig::funded(2_000_000).await;
    rig.credit(1_000_000);
    rig.world.lock().unwrap().pay_faults.extend([PayFault::LostAnswer]);
    rig.chat(false).await.text().await.unwrap();
    rig.until("credited upstream", |r| r.credited(4200) == 1).await;
    rig.crash().await;
    rig.all_paid(1).await;
    let w = rig.world.lock().unwrap();
    assert_eq!(w.payments.len(), 1);
    assert!(w.payments[0].posts >= 2, "the same transaction was handed in again");
    drop(w);
    rig.books_balance();
}

#[tokio::test]
async fn crash_before_signing_still_pays() {
    let mut rig = Rig::new().await;
    rig.credit(1_000_000);
    rig.chat(false).await.text().await.unwrap();
    rig.until("owed, no coins", |r| r.run().engine.db.active().unwrap().iter().any(|i| i.state == State::Owed && !i.note.is_empty())).await;
    rig.crash().await;
    rig.deposit(500_000).await;
    rig.run().engine.retry_now();
    rig.all_paid(1).await;
    assert_eq!(rig.credited(4200), 1);
}

#[tokio::test]
async fn pending_is_posted_again_never_signed_again() {
    let rig = Rig::funded(2_000_000).await;
    rig.credit(1_000_000);
    rig.world.lock().unwrap().pay_faults.extend([PayFault::Pending, PayFault::Pending]);
    rig.chat(false).await.text().await.unwrap();
    rig.all_paid(1).await;
    let row = rig.run().engine.db.recent(1).unwrap().remove(0);
    assert!(row.old_txids.is_empty());
    assert_eq!(rig.credited(4200), 1);
    rig.books_balance();
}

#[tokio::test]
async fn a_stolen_coin_is_dropped_and_the_payment_rebuilt_once() {
    let rig = Rig::funded(1_000_000).await;
    rig.deposit(600_000).await;
    rig.credit(1_000_000);
    rig.world.lock().unwrap().pay_faults.extend([PayFault::StolenCoin]);
    rig.chat(false).await.text().await.unwrap();
    rig.all_paid(1).await;
    let row = rig.run().engine.db.recent(1).unwrap().remove(0);
    assert!(row.rebuilt);
    assert_eq!(row.old_txids.len(), 1);
    assert_ne!(row.old_txids[0], row.txid);
    assert_eq!(rig.credited(4200), 1);
    let dropped = rig.run().engine.db.coins().unwrap().into_iter().filter(|(_, c)| c.state == CoinState::Dropped).count();
    assert!(dropped >= 1);
    rig.books_balance();
    // The day counts the payment once.
    let spent = rig.run().engine.db.spent_on(&twetch_wallet::model::today()).unwrap();
    assert_eq!(spent, row.usd_micros);
}

#[tokio::test]
async fn refused_twice_stops_and_asks_the_user() {
    let rig = Rig::funded(1_000_000).await;
    rig.deposit(600_000).await;
    rig.credit(1_000_000);
    rig.world.lock().unwrap().pay_faults.extend([PayFault::StolenCoin, PayFault::StolenCoin]);
    rig.chat(false).await.text().await.unwrap();
    rig.until("failed", |r| r.run().engine.db.recent(1).unwrap()[0].state == State::Failed).await;
    assert_eq!(rig.credited(4200), 0);
    let id = rig.run().engine.db.recent(1).unwrap()[0].id.clone();
    rig.run().engine.cancel_failed(&id).unwrap();
    assert!(rig.run().engine.approve(&id).is_err());
    assert!(rig.run().engine.retry_failed(&id).is_err());
}

#[tokio::test]
async fn a_conflict_the_chain_does_not_confirm_never_signs_a_second_payment() {
    let rig = Rig::funded(2_000_000).await;
    rig.credit(1_000_000);
    rig.world.lock().unwrap().pay_faults.extend([PayFault::FalseConflict, PayFault::FalseConflict]);
    rig.chat(false).await.text().await.unwrap();
    rig.all_paid(1).await;
    let row = rig.run().engine.db.recent(1).unwrap().remove(0);
    assert!(!row.rebuilt && row.old_txids.is_empty());
    assert_eq!(rig.world.lock().unwrap().payments.len(), 1);
}

#[tokio::test]
async fn a_payment_the_gateway_calls_invalid_waits_for_the_user() {
    let rig = Rig::funded(2_000_000).await;
    rig.credit(1_000_000);
    rig.world.lock().unwrap().pay_faults.extend([PayFault::NotYours]);
    rig.chat(false).await.text().await.unwrap();
    rig.until("failed", |r| r.run().engine.db.recent(1).unwrap()[0].state == State::Failed).await;
    let id = rig.run().engine.db.recent(1).unwrap()[0].id.clone();
    rig.run().engine.retry_failed(&id).unwrap();
    rig.all_paid(1).await;
    assert_eq!(rig.credited(4200), 1);
}

#[tokio::test]
async fn the_same_request_is_paid_once_however_often_it_is_reported() {
    let rig = Rig::funded(2_000_000).await;
    rig.credit(1_000_000);
    rig.chat(false).await.text().await.unwrap();
    rig.all_paid(1).await;
    let e = &rig.run().engine;
    let id = e.db.recent(1).unwrap()[0].id.trim_start_matches("req:").to_string();
    for sats in [4200, 999_999] {
        e.open_request(&id, "x").unwrap();
        e.settle_request(&id, sats).unwrap();
    }
    e.step().await.unwrap();
    assert_eq!(rig.world.lock().unwrap().payments.len(), 1);
    assert_eq!(e.db.recent(10).unwrap().len(), 1);
}

#[tokio::test]
async fn a_client_that_leaves_aborts_upstream_and_the_cost_is_still_paid() {
    use futures_util::StreamExt;
    let rig = Rig::funded(2_000_000).await;
    rig.credit(1_000_000);
    rig.world.lock().unwrap().chunk_delay = Duration::from_millis(50);
    let mut body = rig.chat(true).await.bytes_stream();
    body.next().await.unwrap().unwrap();
    drop(body);
    rig.until("upstream saw the abort", |r| r.world.lock().unwrap().aborted == 1).await;
    rig.all_paid(1).await;
    assert_eq!(rig.credited(4200), 1);
}

#[tokio::test]
async fn a_client_that_leaves_before_the_headers_is_still_paid_for() {
    let rig = Rig::funded(2_000_000).await;
    rig.credit(1_000_000);
    rig.world.lock().unwrap().head_delay = Duration::from_millis(400);
    let left = tokio::time::timeout(Duration::from_millis(120), rig.chat(true)).await;
    assert!(left.is_err());
    rig.all_paid(1).await;
    assert_eq!(rig.credited(4200), 1);
}

#[tokio::test]
async fn a_stream_cut_before_its_cost_is_asked_for() {
    let rig = Rig::funded(2_000_000).await;
    rig.credit(1_000_000);
    rig.world.lock().unwrap().chat_faults.push_back(ChatFault::CutStream);
    assert!(!rig.chat(true).await.text().await.unwrap().contains("cost_sats"));
    rig.all_paid(1).await;
    assert_eq!(rig.credited(4200), 1);
}

#[tokio::test]
async fn crash_while_streaming_asks_for_the_cost_after_restart() {
    use futures_util::StreamExt;
    let mut rig = Rig::funded(2_000_000).await;
    rig.credit(1_000_000);
    rig.world.lock().unwrap().chunk_delay = Duration::from_millis(50);
    let mut body = rig.chat(true).await.bytes_stream();
    body.next().await.unwrap().unwrap();
    assert_eq!(rig.run().engine.db.active().unwrap()[0].state, State::Open);
    drop(body);
    rig.crash().await;
    rig.all_paid(1).await;
    assert_eq!(rig.credited(4200), 1);
}

#[tokio::test]
async fn another_payee_is_never_paid() {
    let rig = Rig::funded(2_000_000).await;
    rig.world.lock().unwrap().pay_to = "1BoatSLRHtKNngkdXEeobR76b53LETtpyT".into();
    let res = rig.chat(true).await;
    assert_eq!(res.status(), 402);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["error"]["code"], "wrong_payee");
    assert!(rig.world.lock().unwrap().payments.is_empty());
    assert!(rig.run().engine.db.recent(5).unwrap().is_empty());
    assert_eq!(PAY_TO, twetch_wallet::PAY_TO);
}

#[tokio::test]
async fn limits_stop_payments_until_the_user_says_otherwise() {
    let rig = Rig::funded(50_000_000).await;
    rig.credit(1_000_000);
    let e = &rig.run().engine;
    assert_eq!(e.db.limits().unwrap(), Limits { per_payment_usd: 1.0, per_day_usd: 10.0 });
    // $14 a BSV: 10,000,000 sats is $1.40, over $1.
    rig.world.lock().unwrap().cost = 10_000_000;
    rig.chat(false).await.text().await.unwrap();
    rig.until("held", |r| r.run().engine.db.active().unwrap().iter().any(|i| i.state == State::Held)).await;
    let res = rig.chat(false).await;
    assert_eq!(res.status(), 402);
    let body: Value = res.json().await.unwrap();
    assert_eq!((body["error"]["type"].as_str(), body["error"]["code"].as_str()), (Some("payment_required"), Some("payment_needs_approval")));
    assert!(body["error"]["message"].as_str().unwrap().contains("$1.40"));
    let id = e.db.active().unwrap()[0].id.clone();
    e.approve(&id).unwrap();
    rig.all_paid(1).await;
    // The day: a limit below what is spent blocks before anything is sent upstream.
    rig.world.lock().unwrap().cost = 4200;
    e.set_limits(Limits { per_payment_usd: 1.0, per_day_usd: 1.0 }).unwrap();
    let chats = rig.world.lock().unwrap().chats;
    let body: Value = rig.chat(false).await.json().await.unwrap();
    assert_eq!(body["error"]["code"], "daily_limit");
    assert_eq!(rig.world.lock().unwrap().chats, chats);
    e.set_limits(Limits { per_payment_usd: 1.0, per_day_usd: 10.0 }).unwrap();
    rig.chat(false).await.text().await.unwrap();
    rig.all_paid(2).await;
}

#[tokio::test]
async fn a_top_up_over_the_limit_or_the_balance_is_refused_in_words() {
    let rig = Rig::funded(2_000_000).await;
    rig.world.lock().unwrap().estimate = 20_000_000;
    let body: Value = rig.chat(true).await.json().await.unwrap();
    assert_eq!(body["error"]["code"], "payment_limit");
    rig.run().engine.set_limits(Limits { per_payment_usd: 50.0, per_day_usd: 50.0 }).unwrap();
    let body: Value = rig.chat(true).await.json().await.unwrap();
    assert_eq!(body["error"]["code"], "insufficient_funds");
    assert!(body["error"]["message"].as_str().unwrap().contains(&rig.address()));
    assert!(rig.world.lock().unwrap().payments.is_empty());
    // Neither try left a row, a debt or a taken coin.
    assert!(rig.run().engine.db.recent(5).unwrap().is_empty());
    assert!(rig.run().engine.db.active().unwrap().is_empty());
    rig.books_balance();
}

#[tokio::test]
async fn concurrent_402s_share_one_top_up() {
    let rig = Rig::funded(5_000_000).await;
    rig.credit(0);
    let answers = futures_util::future::join_all((0..4).map(|_| rig.chat(true))).await;
    for r in answers {
        assert_eq!(r.status(), 200);
        r.text().await.unwrap();
    }
    rig.all_paid(4).await;
    let topups = rig.run().engine.db.recent(50).unwrap().into_iter().filter(|i| i.kind == Kind::Topup && i.state == State::Paid).count();
    assert!((1..=2).contains(&topups), "{topups}");
    rig.books_balance();
}

#[tokio::test]
async fn a_revoked_key_is_replaced_by_itself_and_by_hand() {
    let rig = Rig::funded(2_000_000).await;
    rig.credit(1_000_000);
    rig.chat(false).await.text().await.unwrap();
    let first = rig.secrets.get(GATEWAY_KEY).unwrap().unwrap();
    rig.world.lock().unwrap().keys.clear();
    assert_eq!(rig.chat(false).await.status(), 200);
    let second = rig.secrets.get(GATEWAY_KEY).unwrap().unwrap();
    assert_ne!(*first, *second);
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(rig.page().await.post("rotate", json!({})).await.status(), 200);
    let third = rig.secrets.get(GATEWAY_KEY).unwrap().unwrap();
    assert_ne!(*second, *third);
    let w = rig.world.lock().unwrap();
    assert_eq!(w.keys.len(), 1, "the old key is revoked");
    assert!(w.keys.contains_key(third.as_str()));
}

#[tokio::test]
async fn wallet_page_withdraws_reveals_and_shows_everything() {
    let rig = Rig::funded(2_000_000).await;
    let page = rig.page().await;
    let s: Value = page.get("state").await.json().await.unwrap();
    assert_eq!((s["ready_sats"].as_u64(), s["status"].as_str()), (Some(2_000_000), Some("ready")));
    assert_eq!(s["limits"]["per_day_usd"], 10.0);
    assert!(page.get("qr").await.text().await.unwrap().contains("<svg"));
    let bad = page.post("withdraw", json!({"to": "nonsense", "sats": 1000})).await;
    assert_eq!(bad.status(), 400);
    let to = "1BoatSLRHtKNngkdXEeobR76b53LETtpyT";
    let sent: Value = page.post("withdraw", json!({"to": to, "sats": 500_000})).await.json().await.unwrap();
    assert_eq!(sent["state"], "paid");
    rig.books_balance();
    let all: Value = page.post("withdraw", json!({"to": to})).await.json().await.unwrap();
    assert_eq!(all["state"], "paid");
    assert_eq!(rig.run().engine.balance().unwrap().0, 0);
    assert_eq!(rig.world.lock().unwrap().unspent_sats(to), 500_000 + all["sats"].as_u64().unwrap());
    // A withdraw does not use up the spending limit.
    assert_eq!(rig.run().engine.db.spent_on(&twetch_wallet::model::today()).unwrap(), 0);
    let phrase: Value = page.post("reveal", json!({})).await.json().await.unwrap();
    assert_eq!(phrase["phrase"].as_str().unwrap().split(' ').count(), 12);
}

#[tokio::test]
async fn an_unused_wallet_can_be_replaced_by_a_backup_and_a_used_one_cannot() {
    let mut rig = Rig::new().await;
    let words = "book fit fly ketchup also elevator scout mind edit fatal where rookie";
    let page = rig.page().await;
    assert_eq!(page.post("restore", json!({"phrase": "not a phrase"})).await.status(), 400);
    let before = rig.address();
    assert_eq!(page.post("restore", json!({"phrase": format!("  {}  ", words.to_uppercase())})).await.status(), 200);
    let after = rig.address();
    assert_ne!(before, after);
    rig.crash().await;
    assert_eq!(rig.address(), after);
    rig.credit(1_000_000);
    rig.deposit(900_000).await;
    rig.chat(false).await.text().await.unwrap();
    rig.all_paid(1).await;
    let res = rig.page().await.post("restore", json!({"phrase": words})).await;
    assert_eq!(res.status(), 400);
}

#[tokio::test]
async fn nobody_else_gets_in() {
    let rig = Rig::funded(1_000_000).await;
    let r = rig.run();
    let h = &rig.http;
    let chat = |b: reqwest::RequestBuilder| b.json(&json!({"model": "m", "messages": []}));
    let url = rig.url("/v1/chat/completions");
    assert_eq!(chat(h.post(&url)).send().await.unwrap().status(), 401);
    assert_eq!(chat(h.post(&url)).bearer_auth("x").send().await.unwrap().status(), 401);
    assert_eq!(chat(h.post(&url)).bearer_auth(&r.proxy_token).header("origin", "https://evil.example").send().await.unwrap().status(), 403);
    assert_eq!(chat(h.post(&url)).bearer_auth(&r.proxy_token).header("host", "evil.example").send().await.unwrap().status(), 421);
    // A preflight gets no permission.
    let pre = h.request(reqwest::Method::OPTIONS, &url).header("origin", "https://evil.example").send().await.unwrap();
    assert!(pre.headers().get("access-control-allow-origin").is_none());
    assert_eq!(h.get(rig.url("/v1/models")).send().await.unwrap().status(), 401);
    assert_eq!(h.get(rig.url("/local/status")).send().await.unwrap().status(), 401);
    // The agent's token cannot open the wallet page, read the phrase or withdraw.
    assert_eq!(h.post(rig.url("/local/wallet-link")).bearer_auth(&r.proxy_token).send().await.unwrap().status(), 401);
    for path in ["reveal", "withdraw", "limits", "rotate", "restore", "approve", "refresh"] {
        let res =
            h.post(rig.url(&format!("/wallet/api/{path}"))).bearer_auth(&r.proxy_token).header("origin", rig.url("")).json(&json!({})).send().await.unwrap();
        assert_eq!(res.status(), 401, "{path}");
    }
    assert_eq!(h.get(rig.url("/wallet/api/state")).bearer_auth(&r.admin_token).send().await.unwrap().status(), 401);
    let shell = h.get(rig.url("/wallet")).send().await.unwrap();
    assert_eq!(shell.status(), 401);
    assert!(shell.headers()["content-security-policy"].to_str().unwrap().contains("frame-ancestors 'none'"));
    assert_eq!(h.get(rig.url("/wallet?t=made-up")).send().await.unwrap().status(), 401);
    // The config for the agent holds no secret.
    let config = h.get(rig.url("/local/opencode-config")).bearer_auth(&r.proxy_token).send().await.unwrap().text().await.unwrap();
    assert!(config.contains("deepseek/deepseek-v4.1-flash-1017"));
    for secret in [r.proxy_token.as_str(), r.admin_token.as_str(), "tw-ai-"] {
        assert!(!config.contains(secret));
    }
}

#[tokio::test]
async fn no_secret_is_written_to_the_home_folder() {
    let rig = Rig::funded(2_000_000).await;
    rig.credit(1_000_000);
    rig.chat(false).await.text().await.unwrap();
    rig.all_paid(1).await;
    let phrase = rig.secrets.get(twetch_wallet::secrets::MNEMONIC).unwrap().unwrap();
    let key = rig.secrets.get(GATEWAY_KEY).unwrap().unwrap();
    for entry in std::fs::read_dir(rig.home.path().join("wallet")).unwrap() {
        let bytes = std::fs::read(entry.unwrap().path()).unwrap();
        for needle in [phrase.as_str(), key.as_str(), rig.run().admin_token.as_str()] {
            assert!(!bytes.windows(needle.len()).any(|w| w == needle.as_bytes()));
        }
    }
}

#[tokio::test]
async fn unreachable_gateway_is_a_plain_error_and_models_fall_back() {
    let home = tempfile::tempdir().unwrap();
    let dead = "http://127.0.0.1:9".to_string();
    let cfg = Config { home: home.path().into(), port: 0, gateway: dead.clone(), woc: format!("{dead}/woc") };
    let r = start(cfg, Arc::new(MemorySecrets::default())).await.unwrap();
    let http = reqwest::Client::new();
    let list: Value = http.get(format!("http://{}/v1/models", r.addr)).bearer_auth(&r.proxy_token).send().await.unwrap().json().await.unwrap();
    assert_eq!(list["data"][0]["id"], "deepseek/deepseek-v4.1-flash-1017");
    assert_eq!(list["data"].as_array().unwrap().len(), 4);
    // No price known, so no limit can be checked: refuse.
    let res = http.post(format!("http://{}/v1/chat/completions", r.addr)).bearer_auth(&r.proxy_token).json(&json!({"model": "m"})).send().await.unwrap();
    assert_eq!(res.status(), 402);
    r.abort();
}

/// Stops the process it started when the test ends.
struct Gateway(std::process::Child);

impl Drop for Gateway {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The real gateway binary, its upstreams stubbed. Runs when `TWETCH_GATEWAY_BIN`
/// and `TWETCH_GATEWAY_CONFIG` name one.
#[tokio::test]
async fn the_real_gateway_takes_our_payments() {
    let (Ok(bin), Ok(config)) = (std::env::var("TWETCH_GATEWAY_BIN"), std::env::var("TWETCH_GATEWAY_CONFIG")) else { return };
    let (up, world) = twetch_stub::spawn().await;
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("key");
    std::fs::write(&key, "sk-or-test").unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let child = std::process::Command::new(bin)
        .env("INFERENCE_DATA_DIR", dir.path().join("db"))
        .env("INFERENCE_CONFIG", config)
        .env("INFERENCE_ADDR", format!("127.0.0.1:{port}"))
        .env("TWETCH_API", format!("{up}/up"))
        .env("OPENROUTER_URL", format!("{up}/up"))
        .env("OPENROUTER_API_KEY_FILE", &key)
        .spawn()
        .unwrap();
    let _gateway = Gateway(child);
    let base = format!("http://127.0.0.1:{port}");
    let http = reqwest::Client::new();
    let end = Instant::now() + Duration::from_secs(20);
    while http.get(format!("{base}/healthz")).send().await.is_err() {
        assert!(Instant::now() < end, "the gateway did not start");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut rig = Rig { home: tempfile::tempdir().unwrap(), base: base.clone(), world, secrets: Arc::default(), http, run: None, runtime: None };
    // The chain is still the stub's.
    let cfg = Config { home: rig.home.path().into(), port: 0, gateway: base, woc: format!("{up}/woc") };
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    rig.run = Some(rt.spawn(start(cfg, rig.secrets.clone())).await.unwrap().unwrap());
    rig.runtime = Some(rt);
    rig.deposit(30_000_000).await;
    for stream in [true, false, true] {
        let res = rig.chat(stream).await;
        let (status, text) = (res.status(), res.text().await.unwrap());
        assert_eq!(status, 200, "{text}");
        assert!(text.contains("\"cost_sats\""), "{text}");
    }
    rig.all_paid(3).await;
    let e = &rig.run().engine;
    let account = e.account(Duration::ZERO).await.unwrap();
    let rows = e.db.recent(50).unwrap();
    let paid = |kind| rows.iter().filter(|i| i.kind == kind).map(|i| i.sats).sum::<u64>();
    assert!(rows.iter().all(|i| i.state == State::Paid), "{rows:#?}");
    assert_eq!(account.spent_sats, i64::try_from(paid(Kind::Request)).unwrap());
    assert_eq!(account.paid_sats, i64::try_from(paid(Kind::Request) + paid(Kind::Topup)).unwrap());
    assert_eq!(account.pay_to, PAY_TO);
    rig.books_balance();
    // A revoked key heals against the real one too.
    assert_eq!(rig.page().await.post("rotate", json!({})).await.status(), 200);
    assert_eq!(rig.chat(false).await.status(), 200);
    rig.all_paid(4).await;
}
