//! The wallet page. Reached through a one-time link from the desktop app.

use crate::auth::{COOKIE, SESSION_HEADER};
use crate::engine::Engine;
use crate::gateway::AccountInfo;
use crate::model::{today, Item, Kind, Limits, State as PayState};
use crate::proxy::{error, refused, Shared, ACCOUNT_AGE};
use crate::signer::Status;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use zeroize::Zeroizing;

const HTML: &str = include_str!("page/index.html");
const JS: &str = include_str!("page/app.js");
const CSS: &str = include_str!("page/app.css");
const CLOSED: &str = include_str!("page/closed.html");
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";
const RECENT: usize = 100;

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/wallet", get(page))
        .route("/wallet/app.js", get(|s: State<Shared>, h: HeaderMap| asset(s, h, "text/javascript", JS)))
        .route("/wallet/app.css", get(|s: State<Shared>, h: HeaderMap| asset(s, h, "text/css", CSS)))
        .route("/wallet/api/state", get(state))
        .route("/wallet/api/qr", get(qr))
        .route("/wallet/api/refresh", post(refresh))
        .route("/wallet/api/limits", post(limits))
        .route("/wallet/api/approve", post(approve))
        .route("/wallet/api/retry", post(retry))
        .route("/wallet/api/cancel", post(cancel))
        .route("/wallet/api/withdraw", post(withdraw))
        .route("/wallet/api/reveal", post(reveal))
        .route("/wallet/api/restore", post(restore))
        .route("/wallet/api/rotate", post(rotate))
}

fn headers(kind: &'static str) -> [(header::HeaderName, HeaderValue); 6] {
    [
        (header::CONTENT_TYPE, HeaderValue::from_static(kind)),
        (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        (header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP)),
        (header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff")),
        (header::REFERRER_POLICY, HeaderValue::from_static("no-referrer")),
        (header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY")),
    ]
}

#[derive(Deserialize)]
struct LinkQuery {
    t: Option<String>,
}

async fn page(State(app): State<Shared>, h: HeaderMap, Query(q): Query<LinkQuery>) -> Response {
    if let Err(r) = app.auth.host(&h) {
        return refused(&r);
    }
    if let Some(t) = q.t {
        return match app.auth.redeem(&t) {
            Ok((cookie, page)) => {
                let set = format!("{COOKIE}={cookie}; HttpOnly; SameSite=Strict; Path=/wallet");
                let mut res = (StatusCode::SEE_OTHER, headers("text/html; charset=utf-8")).into_response();
                // The fragment never reaches a server; the page moves it to its own storage.
                for (k, v) in [(header::SET_COOKIE, set), (header::LOCATION, format!("/wallet#{page}"))] {
                    if let Ok(v) = HeaderValue::from_str(&v) {
                        res.headers_mut().insert(k, v);
                    }
                }
                res
            }
            Err(_) => (StatusCode::UNAUTHORIZED, headers("text/html; charset=utf-8"), CLOSED).into_response(),
        };
    }
    if app.auth.has_cookie(&h) {
        (headers("text/html; charset=utf-8"), HTML).into_response()
    } else {
        (StatusCode::UNAUTHORIZED, headers("text/html; charset=utf-8"), CLOSED).into_response()
    }
}

async fn asset(State(app): State<Shared>, h: HeaderMap, kind: &'static str, body: &'static str) -> Response {
    match app.auth.host(&h) {
        Ok(()) => (headers(kind), body).into_response(),
        Err(r) => refused(&r),
    }
}

fn reply<T: Serialize>(out: anyhow::Result<T>) -> Response {
    match out {
        Ok(v) => (headers("application/json"), serde_json::to_vec(&v).unwrap_or_default()).into_response(),
        Err(e) => error(StatusCode::BAD_REQUEST, "wallet_error", "wallet_error", &format!("{e:#}")),
    }
}

macro_rules! guard {
    ($app:expr, $h:expr, $writes:expr) => {
        if let Err(r) = $app.auth.page(&$h, $writes) {
            return refused(&r);
        }
        debug_assert!($h.contains_key(SESSION_HEADER));
    };
}

fn parse<'a, T: Deserialize<'a>>(body: &'a [u8]) -> anyhow::Result<T> {
    serde_json::from_slice(body).map_err(|_| anyhow::anyhow!("that request makes no sense"))
}

#[derive(Serialize)]
struct Row {
    id: String,
    kind: Kind,
    state: PayState,
    ts: i64,
    model: String,
    sats: u64,
    fee: u64,
    usd: Option<f64>,
    to: String,
    txid: String,
    note: String,
}

impl Row {
    fn new(i: Item, usd: Option<f64>) -> Self {
        let sats = i.sats;
        Self {
            id: i.id,
            kind: i.kind,
            state: i.state,
            ts: i.created_ms,
            model: i.model,
            sats,
            fee: i.fee,
            usd: usd.map(|r| sats as f64 * r / 1e8),
            to: i.to,
            txid: i.txid,
            note: i.note,
        }
    }
}

#[derive(Serialize)]
struct Snapshot {
    status: &'static str,
    problem: String,
    address: String,
    pubkey: String,
    pay_to: &'static str,
    ready_sats: u64,
    coming_sats: u64,
    owed_sats: u64,
    bsv_usd: Option<f64>,
    gateway: Option<AccountInfo>,
    gateway_error: String,
    limits: Limits,
    spent_today_usd: f64,
    waiting: Vec<Row>,
    recent: Vec<Row>,
    can_restore: bool,
}

async fn snapshot(engine: &Engine) -> anyhow::Result<Snapshot> {
    let account = engine.signer.account();
    let (status, problem) = match account.status {
        Status::Ready => ("ready", String::new()),
        Status::Missing => ("missing", "The recovery phrase is not in this computer's keychain. Restore it from your backup.".to_string()),
        Status::Locked(why) => ("locked", format!("The keychain could not be read ({why}). Unlock it and reopen TwetchCode.")),
    };
    let (gateway, gateway_error) = if status == "ready" {
        match engine.account(ACCOUNT_AGE).await {
            Ok(a) => (Some(a), String::new()),
            Err(e) => (None, format!("{e:#}")),
        }
    } else {
        (None, String::new())
    };
    let usd = engine.bsv_usd().await.ok();
    let (ready_sats, coming_sats) = engine.balance()?;
    let recent = engine.db.recent(RECENT)?;
    let can_restore = status == "missing" || (status == "ready" && recent.is_empty() && ready_sats == 0);
    let active = engine.db.active()?;
    let owed_sats = active.iter().filter(|i| matches!(i.state, PayState::Owed | PayState::Held)).map(|i| i.sats).sum();
    let waiting = active
        .into_iter()
        .filter(|i| i.state == PayState::Held)
        .chain(recent.iter().filter(|i| i.state == PayState::Failed).cloned())
        .map(|i| Row::new(i, usd))
        .collect();
    Ok(Snapshot {
        status,
        problem,
        address: account.address,
        pubkey: account.pubkey,
        pay_to: crate::PAY_TO,
        ready_sats,
        coming_sats,
        owed_sats,
        bsv_usd: usd,
        gateway,
        gateway_error,
        limits: engine.db.limits()?,
        spent_today_usd: engine.db.spent_on(&today())? as f64 / 1e6,
        waiting,
        recent: recent.into_iter().map(|i| Row::new(i, usd)).collect(),
        can_restore,
    })
}

async fn state(State(app): State<Shared>, h: HeaderMap) -> Response {
    guard!(app, h, false);
    reply(snapshot(&app.engine).await)
}

async fn qr(State(app): State<Shared>, h: HeaderMap) -> Response {
    guard!(app, h, false);
    let address = app.engine.signer.account().address;
    let svg = qrcode::QrCode::new(address.as_bytes()).map(|c| c.render::<qrcode::render::svg::Color>().min_dimensions(220, 220).quiet_zone(true).build());
    match svg {
        Ok(s) => (headers("image/svg+xml"), s).into_response(),
        Err(e) => reply::<()>(Err(anyhow::anyhow!("{e}"))),
    }
}

async fn refresh(State(app): State<Shared>, h: HeaderMap) -> Response {
    guard!(app, h, true);
    let done = app.engine.refresh_coins(false).await;
    if done.is_ok() {
        app.engine.retry_now();
    }
    reply(done)
}

async fn limits(State(app): State<Shared>, h: HeaderMap, body: Bytes) -> Response {
    guard!(app, h, true);
    reply(parse::<Limits>(&body).and_then(|l| {
        let fine = |v: f64| v.is_finite() && v >= 0.0;
        anyhow::ensure!(fine(l.per_payment_usd) && fine(l.per_day_usd), "a limit is a dollar amount");
        app.engine.set_limits(l)
    }))
}

#[derive(Deserialize)]
struct IdBody {
    id: String,
}

async fn approve(State(app): State<Shared>, h: HeaderMap, body: Bytes) -> Response {
    guard!(app, h, true);
    reply(parse::<IdBody>(&body).and_then(|b| app.engine.approve(&b.id)))
}

async fn retry(State(app): State<Shared>, h: HeaderMap, body: Bytes) -> Response {
    guard!(app, h, true);
    reply(parse::<IdBody>(&body).and_then(|b| app.engine.retry_failed(&b.id)))
}

async fn cancel(State(app): State<Shared>, h: HeaderMap, body: Bytes) -> Response {
    guard!(app, h, true);
    reply(parse::<IdBody>(&body).and_then(|b| app.engine.cancel_failed(&b.id)))
}

#[derive(Deserialize)]
struct WithdrawBody {
    to: String,
    /// Absent sends everything.
    sats: Option<u64>,
}

async fn withdraw(State(app): State<Shared>, h: HeaderMap, body: Bytes) -> Response {
    guard!(app, h, true);
    let out = match parse::<WithdrawBody>(&body) {
        // Its own task, so a closed page cannot stop it half way.
        Ok(b) => {
            let engine = app.engine.clone();
            tokio::spawn(async move { engine.withdraw(b.to.trim(), b.sats).await }).await.unwrap_or_else(|_| Err(anyhow::anyhow!("the wallet stopped")))
        }
        Err(e) => Err(e),
    };
    let out = out.and_then(|i| {
        anyhow::ensure!(i.state != PayState::Failed, "the network refused it: {}", i.note);
        Ok(Row::new(i, None))
    });
    reply(out)
}

#[derive(Serialize)]
struct Phrase<'a> {
    phrase: &'a str,
}

async fn reveal(State(app): State<Shared>, h: HeaderMap) -> Response {
    guard!(app, h, true);
    match app.engine.signer.reveal().await {
        Ok(p) => reply(Ok(Phrase { phrase: &p })),
        Err(e) => reply::<()>(Err(e)),
    }
}

#[derive(Deserialize)]
struct RestoreBody {
    phrase: String,
}

async fn restore(State(app): State<Shared>, h: HeaderMap, body: Bytes) -> Response {
    guard!(app, h, true);
    match parse::<RestoreBody>(&body) {
        Ok(b) => reply(app.engine.restore(Zeroizing::new(b.phrase)).await),
        Err(e) => reply::<()>(Err(e)),
    }
}

async fn rotate(State(app): State<Shared>, h: HeaderMap) -> Response {
    guard!(app, h, true);
    reply(
        tokio::time::timeout(Duration::from_secs(60), app.engine.gateway.rotate()).await.unwrap_or_else(|_| Err(anyhow::anyhow!("the gateway did not answer"))),
    )
}
