//! The OpenAI-compatible side: what the coding agent talks to.

use crate::auth::{Auth, Refusal, Who};
use crate::engine::{Blocked, Engine};
use crate::gateway::{demand, Model, Models};
use crate::sse::Scanner;
use crate::ui;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

/// Offered when the gateway's list cannot be read.
pub const FALLBACK: [(&str, &str); 4] = [
    ("deepseek/deepseek-v4.1-flash-1017", "DeepSeek V4.1 Flash"),
    ("z-ai/glm-5.3", "GLM 5.3"),
    ("z-ai/glm-5.3-flash", "GLM 5.3 Flash"),
    ("deepseek/deepseek-v4-pro-0813", "DeepSeek V4 Pro"),
];
pub const FALLBACK_CONTEXT: u64 = 1_048_576;
const FALLBACK_OUTPUT: u64 = 65_536;

pub struct App {
    pub engine: Arc<Engine>,
    pub auth: Auth,
}

impl App {
    #[must_use]
    pub fn new(engine: Arc<Engine>, auth: Auth) -> Self {
        Self { engine, auth }
    }
}

pub type Shared = Arc<App>;

pub fn router(app: Shared) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat))
        .route("/local/status", get(status))
        .route("/local/opencode-config", get(opencode_config))
        .route("/local/wallet-link", post(wallet_link))
        .merge(ui::routes())
        // A long conversation with pictures is large; the gateway decides what is too much.
        .layer(DefaultBodyLimit::disable())
        .with_state(app)
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: ErrorFields<'a>,
}

#[derive(Serialize)]
struct ErrorFields<'a> {
    message: &'a str,
    r#type: &'a str,
    code: &'a str,
}

pub fn error(status: StatusCode, kind: &str, code: &str, message: &str) -> Response {
    let body = serde_json::to_vec(&ErrorBody { error: ErrorFields { message, r#type: kind, code } }).unwrap_or_default();
    (status, [(header::CONTENT_TYPE, "application/json"), (header::CACHE_CONTROL, "no-store")], body).into_response()
}

fn blocked(b: &Blocked) -> Response {
    error(StatusCode::PAYMENT_REQUIRED, "payment_required", b.code, &b.message)
}

pub fn refused(r: &Refusal) -> Response {
    match r {
        Refusal::Host => error(StatusCode::MISDIRECTED_REQUEST, "invalid_request_error", "bad_host", "unknown host"),
        Refusal::Browser => error(StatusCode::FORBIDDEN, "invalid_request_error", "cross_origin", "browser pages may not call this"),
        Refusal::Token => error(StatusCode::UNAUTHORIZED, "invalid_request_error", "bad_token", "a TwetchCode token is required"),
    }
}

fn json<T: Serialize>(value: &T) -> Response {
    let body = serde_json::to_vec(value).unwrap_or_default();
    ([(header::CONTENT_TYPE, "application/json"), (header::CACHE_CONTROL, "no-store")], body).into_response()
}

async fn model_list(engine: &Engine) -> Option<Vec<Model>> {
    let list: Models = serde_json::from_slice(&engine.gateway.models_raw().await.ok()?).ok()?;
    if let Some(usd) = list.bsv_usd {
        engine.learn_rate(usd);
    }
    (!list.data.is_empty()).then_some(list.data)
}

fn fallback() -> Vec<Model> {
    FALLBACK
        .iter()
        .enumerate()
        .map(|(i, (id, name))| Model {
            id: (*id).into(),
            name: Some((*name).into()),
            context_length: Some(FALLBACK_CONTEXT),
            max_output: None,
            reasoning: true,
            default: i == 0,
            pricing: None,
        })
        .collect()
}

#[derive(Serialize)]
struct ListRow<'a> {
    id: &'a str,
    object: &'a str,
    name: &'a str,
    context_length: u64,
}

#[derive(Serialize)]
struct List<'a> {
    object: &'a str,
    data: Vec<ListRow<'a>>,
}

async fn models(State(app): State<Shared>, h: HeaderMap) -> Response {
    if let Err(r) = app.auth.bearer(&h) {
        return refused(&r);
    }
    if let Ok(raw) = app.engine.gateway.models_raw().await {
        return ([(header::CONTENT_TYPE, "application/json")], raw).into_response();
    }
    let list = fallback();
    json(&List {
        object: "list",
        data: list
            .iter()
            .map(|m| ListRow {
                id: &m.id,
                object: "model",
                name: m.name.as_deref().unwrap_or(&m.id),
                context_length: m.context_length.unwrap_or(FALLBACK_CONTEXT),
            })
            .collect(),
    })
}

#[derive(Serialize)]
struct StatusBody {
    ready: bool,
    balance_sats: u64,
    balance_usd: Option<f64>,
}

async fn status(State(app): State<Shared>, h: HeaderMap) -> Response {
    if let Err(r) = app.auth.bearer(&h) {
        return refused(&r);
    }
    let (ready, coming) = app.engine.balance().unwrap_or_default();
    let sats = ready + coming;
    let usd = app.engine.bsv_usd().await.ok();
    json(&StatusBody {
        ready: app.engine.signer.account().status == crate::signer::Status::Ready,
        balance_sats: sats,
        balance_usd: usd.map(|r| sats as f64 * r / 1e8),
    })
}

#[derive(Serialize)]
struct Link {
    path: String,
}

async fn wallet_link(State(app): State<Shared>, h: HeaderMap) -> Response {
    match app.auth.bearer(&h) {
        Ok(Who::Admin) => match app.auth.mint_link() {
            Ok(t) => json(&Link { path: format!("/wallet?t={t}") }),
            Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, "server_error", "entropy", &e.to_string()),
        },
        Ok(Who::Proxy) => refused(&Refusal::Token),
        Err(r) => refused(&r),
    }
}

/// The OpenCode provider for this sidecar, without any token: the caller adds it.
async fn opencode_config(State(app): State<Shared>, h: HeaderMap) -> Response {
    if let Err(r) = app.auth.bearer(&h) {
        return refused(&r);
    }
    let host = h.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or_default().replace("localhost", "127.0.0.1");
    let list = model_list(&app.engine).await.unwrap_or_else(fallback);
    json(&opencode(&list, &format!("http://{host}/v1")))
}

fn per_million(price: Option<&String>) -> f64 {
    price.and_then(|p| p.parse::<f64>().ok()).filter(|p| p.is_finite() && *p >= 0.0).map_or(0.0, |p| p * 1e6)
}

fn opencode(list: &[Model], base: &str) -> serde_json::Value {
    let mut models = serde_json::Map::new();
    for m in list {
        let context = m.context_length.unwrap_or(FALLBACK_CONTEXT);
        let mut row = serde_json::json!({
            "name": m.name.as_deref().unwrap_or(&m.id),
            "reasoning": m.reasoning,
            "tool_call": true,
            "limit": { "context": context, "output": m.max_output.unwrap_or(FALLBACK_OUTPUT).min(context) },
        });
        if let Some(p) = &m.pricing {
            row["cost"] = serde_json::json!({
                "input": per_million(p.prompt.as_ref()),
                "output": per_million(p.completion.as_ref()),
                "cache_read": per_million(p.cached_prompt.as_ref()),
            });
        }
        models.insert(m.id.clone(), row);
    }
    let default = list.iter().find(|m| m.default).or(list.first()).map(|m| format!("twetch/{}", m.id));
    let small = list.iter().find(|m| m.id.contains("flash")).map(|m| format!("twetch/{}", m.id)).or_else(|| default.clone());
    serde_json::json!({
        "model": default,
        "small_model": small,
        "share": "disabled",
        "autoupdate": false,
        "provider": {
            "twetch": {
                "npm": "@ai-sdk/openai-compatible",
                "name": "Twetch",
                "options": { "baseURL": base },
                "models": models,
            }
        }
    })
}

#[derive(Deserialize)]
struct Wanted {
    #[serde(default)]
    model: String,
}

async fn chat(State(app): State<Shared>, h: HeaderMap, body: Bytes) -> Response {
    if let Err(r) = app.auth.bearer(&h) {
        return refused(&r);
    }
    let engine = &app.engine;
    if let Err(b) = engine.admit().await {
        return blocked(&b);
    }
    let model = serde_json::from_slice::<Wanted>(&body).map(|w| w.model).unwrap_or_default();
    let mut answer = ask(engine, &body, &model).await;
    if let Ok(Answer::Refused402(text)) = &answer {
        let ask_for = match demand(text) {
            Ok(d) => d,
            Err(_) => return passed_on(StatusCode::PAYMENT_REQUIRED, text.clone()),
        };
        // Its own task: a payment in progress outlives a client that left.
        let payer = engine.clone();
        let paid = tokio::spawn(async move { payer.top_up(&ask_for).await }).await;
        match paid {
            Ok(Ok(())) => answer = ask(engine, &body, &model).await,
            Ok(Err(b)) => return blocked(&b),
            Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "server_error", "wallet_error", "the wallet stopped while paying"),
        }
    }
    match answer {
        Ok(Answer::Stream { response, request_id }) => stream(engine, response, request_id),
        Ok(Answer::Refused402(text)) => passed_on(StatusCode::PAYMENT_REQUIRED, text),
        Err(e) => error(StatusCode::BAD_GATEWAY, "server_error", "gateway_unreachable", &format!("Twetch's inference gateway could not be reached: {e:#}")),
    }
}

fn passed_on(status: StatusCode, body: Bytes) -> Response {
    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
}

enum Answer {
    Stream { response: reqwest::Response, request_id: Option<String> },
    Refused402(Bytes),
}

/// Send upstream in its own task: when the client leaves before the headers
/// arrive, the request id is still recorded and its cost still paid.
async fn ask(engine: &Arc<Engine>, body: &Bytes, model: &str) -> anyhow::Result<Answer> {
    let (engine, body, model) = (engine.clone(), body.clone(), model.to_string());
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let out = ask_now(&engine, body, &model).await;
        if let Err(Ok(Answer::Stream { request_id: Some(id), response })) = tx.send(out) {
            drop(response);
            engine.request_ended(&id);
        }
    });
    rx.await.map_err(|_| anyhow::anyhow!("the request stopped"))?
}

async fn ask_now(engine: &Arc<Engine>, body: Bytes, model: &str) -> anyhow::Result<Answer> {
    let response = engine.gateway.chat(body).await?;
    if response.status() == reqwest::StatusCode::PAYMENT_REQUIRED {
        return Ok(Answer::Refused402(response.bytes().await?));
    }
    let request_id = response.headers().get("x-twetch-request-id").and_then(|v| v.to_str().ok()).map(str::to_string);
    if let Some(id) = &request_id {
        let (e, id, model) = (engine.clone(), id.clone(), model.to_string());
        tokio::task::spawn_blocking(move || e.open_request(&id, &model)).await??;
    }
    Ok(Answer::Stream { response, request_id })
}

fn stream(engine: &Arc<Engine>, response: reqwest::Response, request_id: Option<String>) -> Response {
    let status = StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut out = HeaderMap::new();
    for name in ["content-type", "cache-control", "retry-after", "x-twetch-request-id", "x-twetch-cost-sats"] {
        if let Some(v) = response.headers().get(name).and_then(|v| HeaderValue::from_bytes(v.as_bytes()).ok()) {
            out.insert(name, v);
        }
    }
    let cost = response.headers().get("x-twetch-cost-sats").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
    let mut meter = Meter { engine: engine.clone(), request_id, settled: false, scan: Scanner::default() };
    if let Some(sats) = cost {
        meter.settle(None, sats);
    }
    let body = Body::from_stream(Tee { inner: Box::pin(response.bytes_stream()), meter });
    (status, out, body).into_response()
}

/// Reads the cost as it passes by, and pays once the answer ends, however it ends.
struct Meter {
    engine: Arc<Engine>,
    request_id: Option<String>,
    settled: bool,
    scan: Scanner,
}

impl Meter {
    fn settle(&mut self, id: Option<String>, sats: u64) {
        if self.settled {
            return;
        }
        let Some(id) = self.request_id.clone().or(id) else { return };
        self.settled = true;
        let engine = self.engine.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(e) = engine.settle_request(&id, sats) {
                eprintln!("twetch-wallet: request {id}: {e:#}");
                engine.request_ended(&id);
            }
        });
    }
}

impl Drop for Meter {
    fn drop(&mut self) {
        if let Some(c) = self.scan.finish() {
            self.settle(c.request_id, c.sats);
        }
        if !self.settled {
            if let Some(id) = &self.request_id {
                self.engine.request_ended(id);
            }
        }
    }
}

struct Tee {
    inner: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>,
    meter: Meter,
}

impl Stream for Tee {
    type Item = reqwest::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let next = self.inner.poll_next_unpin(cx);
        if let Poll::Ready(Some(Ok(bytes))) = &next {
            if let Some(c) = self.meter.scan.push(bytes) {
                self.meter.settle(c.request_id, c.sats);
            }
        }
        next
    }
}

/// How fresh the page wants the gateway balance.
pub const ACCOUNT_AGE: Duration = Duration::from_secs(10);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opencode_config_names_every_model_and_no_secret() {
        let c = opencode(&fallback(), "http://127.0.0.1:1/v1");
        assert_eq!(c["model"], "twetch/deepseek/deepseek-v4.1-flash-1017");
        let p = &c["provider"]["twetch"];
        assert_eq!(p["npm"], "@ai-sdk/openai-compatible");
        assert_eq!(p["options"]["baseURL"], "http://127.0.0.1:1/v1");
        assert!(p["options"].get("apiKey").is_none());
        assert_eq!(p["models"].as_object().unwrap().len(), 4);
        assert_eq!(p["models"]["z-ai/glm-5.3"]["limit"]["context"], FALLBACK_CONTEXT);
        assert!((per_million(Some(&"0.000000125".into())) - 0.125).abs() < 1e-9);
    }
}
