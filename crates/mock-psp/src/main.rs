// mock-psp: a fake payment provider for development and tests. State lives in memory only.
//
// Endpoints
//   POST /charges            {reference, amount_cents, token}  idempotent on `reference`
//        tok_success            ~100 ms, then {status:"succeeded", psp_ref}
//        tok_insufficient_funds ~100 ms, then {status:"failed", code:"insufficient_funds"}
//        tok_card_declined      ~100 ms, then {status:"failed", code:"card_declined"}
//        tok_timeout            recorded as "processing" at once, succeeds after 30 s
//        tok_network_error      records nothing, answers HTTP 500
//        tok_late               records NOTHING for 40 s (a request still on its way), then the
//                               charge lands and succeeds. A lookup meanwhile says 404.
//        any other token        HTTP 400
//   GET  /charges/{reference}  200 {status: processing|succeeded|failed, ...} or 404
//   GET  /_test/stats          {charges_created, charges_succeeded, post_calls}
//   POST /sink/{name}          records headers and raw body (name "flaky": 500 for the first 2 POSTs)
//   GET  /sink/{name}          lists what was received
//   GET  /health
//
// Env: PORT (default 9000); MOCK_PSP_FAST_MS (default 100), MOCK_PSP_SLOW_SECS (default 30) and
// MOCK_PSP_LATE_SECS (default 40) shorten the delays for tests.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Serialize)]
struct Charge {
    reference: String,
    amount_cents: i64,
    status: String, // processing | succeeded | failed
    #[serde(skip_serializing_if = "Option::is_none")]
    psp_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
}

#[derive(Clone, Serialize)]
struct SinkEntry {
    received_at_unix: u64,
    responded_with: u16,
    headers: Value,
    body: String,
}

#[derive(Default)]
struct Inner {
    charges: HashMap<String, Charge>,
    charges_created: u64,
    charges_succeeded: u64,
    post_calls: u64,
    sink: HashMap<String, Vec<SinkEntry>>,
}

struct Delays {
    fast: Duration,
    slow: Duration,
    late: Duration,
}

// Shared by all handlers behind an Arc. The Mutex is only ever locked for a few lines and
// never held across an .await, so a sleeping request cannot block the others.
struct Shared {
    inner: Mutex<Inner>,
    delays: Delays,
}

type App = Arc<Shared>;

fn build_app(delays: Delays) -> Router {
    let shared: App = Arc::new(Shared {
        inner: Mutex::new(Inner::default()),
        delays,
    });
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/charges", post(create_charge))
        .route("/charges/{reference}", get(get_charge))
        .route("/_test/stats", get(stats))
        .route("/sink/{name}", post(sink_post).get(sink_list))
        .with_state(shared)
}

fn lock(app: &Shared) -> std::sync::MutexGuard<'_, Inner> {
    app.inner.lock().expect("mock state poisoned")
}

#[derive(Deserialize)]
struct ChargeRequest {
    reference: String,
    amount_cents: i64,
    token: String,
}

enum Behaviour {
    Succeed,
    Fail(&'static str),
    Slow,
    Late,
    NetworkError,
}

fn behaviour_for(token: &str) -> Option<Behaviour> {
    match token {
        "tok_success" => Some(Behaviour::Succeed),
        "tok_insufficient_funds" => Some(Behaviour::Fail("insufficient_funds")),
        "tok_card_declined" => Some(Behaviour::Fail("card_declined")),
        "tok_timeout" => Some(Behaviour::Slow),
        "tok_late" => Some(Behaviour::Late),
        "tok_network_error" => Some(Behaviour::NetworkError),
        _ => None,
    }
}

fn bad_request(message: &str) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": "bad_request", "message": message })),
    )
}

async fn create_charge(State(app): State<App>, body: Bytes) -> (StatusCode, Json<Value>) {
    // Counts every POST, including replays and invalid requests.
    lock(&app).post_calls += 1;

    let req: ChargeRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(_) => return bad_request("body must be JSON with reference, amount_cents, token"),
    };
    if req.reference.trim().is_empty() || req.amount_cents <= 0 {
        return bad_request("reference must be set and amount_cents must be positive");
    }
    let Some(behaviour) = behaviour_for(&req.token) else {
        return bad_request("unknown token");
    };
    if matches!(behaviour, Behaviour::NetworkError) {
        // Simulates a PSP that fails before doing anything: nothing is recorded.
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "network_error" })),
        );
    }

    // Idempotency: look up and claim the reference in ONE locked step, so two simultaneous
    // requests with the same reference cannot both create a charge.
    let existing = {
        let mut inner = lock(&app);
        match inner.charges.get(&req.reference) {
            Some(charge) => Some(charge.clone()),
            None => {
                // A late charge has not reached the PSP yet, so nothing is recorded here;
                // the task below records it when it lands.
                if !matches!(behaviour, Behaviour::Late) {
                    inner.charges.insert(
                        req.reference.clone(),
                        Charge {
                            reference: req.reference.clone(),
                            amount_cents: req.amount_cents,
                            status: "processing".into(),
                            psp_ref: None,
                            code: None,
                        },
                    );
                    inner.charges_created += 1;
                }
                None
            }
        }
    };
    if let Some(charge) = existing {
        return (StatusCode::OK, Json(json!(charge)));
    }

    // The work runs in its own task. If the client hangs up while we sleep (for example it
    // gave up waiting), the task still finishes and the charge still completes, as it would
    // at a real PSP. The handler just waits for the task's result.
    let task_app = app.clone();
    let reference = req.reference.clone();
    let amount_cents = req.amount_cents;
    let task = tokio::spawn(async move {
        let (delay, outcome) = match behaviour {
            Behaviour::Succeed => (task_app.delays.fast, None),
            Behaviour::Fail(code) => (task_app.delays.fast, Some(code)),
            Behaviour::Slow => (task_app.delays.slow, None),
            Behaviour::Late => (task_app.delays.late, None),
            Behaviour::NetworkError => unreachable!("handled above"),
        };
        tokio::time::sleep(delay).await;
        let mut inner = lock(&task_app);
        if matches!(behaviour, Behaviour::Late) && !inner.charges.contains_key(&reference) {
            // The delayed request reaches the PSP only now.
            inner.charges.insert(
                reference.clone(),
                Charge {
                    reference: reference.clone(),
                    amount_cents,
                    status: "processing".into(),
                    psp_ref: None,
                    code: None,
                },
            );
            inner.charges_created += 1;
        }
        let succeeded = outcome.is_none();
        if succeeded {
            inner.charges_succeeded += 1;
        }
        let charge = inner
            .charges
            .get_mut(&reference)
            .expect("charge was recorded above");
        if let Some(code) = outcome {
            charge.status = "failed".into();
            charge.code = Some(code.into());
        } else {
            charge.status = "succeeded".into();
            charge.psp_ref = Some(uuid::Uuid::now_v7().to_string());
        }
        charge.clone()
    });
    match task.await {
        Ok(charge) => (StatusCode::OK, Json(json!(charge))),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "mock_failure" })),
        ),
    }
}

async fn get_charge(
    State(app): State<App>,
    Path(reference): Path<String>,
) -> (StatusCode, Json<Value>) {
    match lock(&app).charges.get(&reference) {
        Some(charge) => (StatusCode::OK, Json(json!(charge))),
        None => (StatusCode::NOT_FOUND, Json(json!({ "error": "not_found" }))),
    }
}

async fn stats(State(app): State<App>) -> Json<Value> {
    let inner = lock(&app);
    Json(json!({
        "charges_created": inner.charges_created,
        "charges_succeeded": inner.charges_succeeded,
        "post_calls": inner.post_calls,
    }))
}

async fn sink_post(
    State(app): State<App>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let mut header_map = serde_json::Map::new();
    for (key, value) in &headers {
        header_map.insert(
            key.as_str().to_string(),
            Value::String(String::from_utf8_lossy(value.as_bytes()).into()),
        );
    }
    let mut inner = lock(&app);
    let entries = inner.sink.entry(name.clone()).or_default();
    // The sink named "flaky" fails its first two deliveries so retries can be tested.
    let status = if name == "flaky" && entries.len() < 2 {
        StatusCode::INTERNAL_SERVER_ERROR
    } else {
        StatusCode::OK
    };
    entries.push(SinkEntry {
        received_at_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        responded_with: status.as_u16(),
        headers: Value::Object(header_map),
        body: String::from_utf8_lossy(&body).into_owned(),
    });
    status
}

async fn sink_list(State(app): State<App>, Path(name): Path<String>) -> Json<Value> {
    let entries = lock(&app).sink.get(&name).cloned().unwrap_or_default();
    Json(json!(entries))
}

#[tokio::main]
async fn main() {
    let env_number = |name: &str, default: u64| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let delays = Delays {
        fast: Duration::from_millis(env_number("MOCK_PSP_FAST_MS", 100)),
        slow: Duration::from_secs(env_number("MOCK_PSP_SLOW_SECS", 30)),
        late: Duration::from_secs(env_number("MOCK_PSP_LATE_SECS", 40)),
    };
    let port = std::env::var("PORT").unwrap_or_else(|_| "9000".to_string());
    let addr = format!("0.0.0.0:{port}");

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("failed to bind mock-psp port");
    println!("mock-psp listening on {addr}");
    axum::serve(listener, build_app(delays))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("mock-psp server error");
}

// Resolves on Ctrl-C or (on unix) SIGTERM, which is what `docker stop` sends.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Starts the mock on a random port with short delays and returns its base URL.
    async fn spawn(slow_ms: u64) -> String {
        let app = build_app(Delays {
            fast: Duration::from_millis(10),
            slow: Duration::from_millis(slow_ms),
            late: Duration::from_millis(slow_ms),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    async fn charge(base: &str, reference: &str, token: &str) -> (u16, Value) {
        let resp = reqwest::Client::new()
            .post(format!("{base}/charges"))
            .json(&json!({ "reference": reference, "amount_cents": 500, "token": token }))
            .send()
            .await
            .unwrap();
        (resp.status().as_u16(), resp.json().await.unwrap())
    }

    async fn stats_of(base: &str) -> Value {
        reqwest::get(format!("{base}/_test/stats"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn tokens_behave_as_specified() {
        let base = spawn(50).await;
        let (s, b) = charge(&base, "r1", "tok_success").await;
        assert_eq!((s, b["status"].as_str()), (200, Some("succeeded")));
        assert!(b["psp_ref"].is_string());
        let (_, b) = charge(&base, "r2", "tok_insufficient_funds").await;
        assert_eq!(
            (b["status"].as_str(), b["code"].as_str()),
            (Some("failed"), Some("insufficient_funds"))
        );
        let (_, b) = charge(&base, "r3", "tok_card_declined").await;
        assert_eq!(b["code"], "card_declined");
        let (s, _) = charge(&base, "r4", "tok_bogus").await;
        assert_eq!(s, 400);
        // Network error: 500 and nothing recorded.
        let (s, _) = charge(&base, "r5", "tok_network_error").await;
        assert_eq!(s, 500);
        let missing = reqwest::get(format!("{base}/charges/r5")).await.unwrap();
        assert_eq!(missing.status().as_u16(), 404);
    }

    #[tokio::test]
    async fn same_reference_never_creates_a_second_charge() {
        let base = spawn(50).await;
        let (_, first) = charge(&base, "dup", "tok_success").await;
        let (_, second) = charge(&base, "dup", "tok_success").await;
        assert_eq!(first["psp_ref"], second["psp_ref"]);
        let stats = stats_of(&base).await;
        assert_eq!(stats["charges_created"], 1);
        assert_eq!(stats["charges_succeeded"], 1);
        assert_eq!(stats["post_calls"], 2);
    }

    #[tokio::test]
    async fn timeout_token_is_visible_as_processing_and_finishes_even_if_client_leaves() {
        let base = spawn(300).await;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(50))
            .build()
            .unwrap();
        // The client gives up after 50 ms, long before the 300 ms the charge takes.
        let gave_up = client
            .post(format!("{base}/charges"))
            .json(&json!({ "reference": "slow", "amount_cents": 500, "token": "tok_timeout" }))
            .send()
            .await;
        assert!(gave_up.is_err());
        let during: Value = reqwest::get(format!("{base}/charges/slow"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(during["status"], "processing");
        tokio::time::sleep(Duration::from_millis(400)).await;
        let after: Value = reqwest::get(format!("{base}/charges/slow"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(after["status"], "succeeded");
    }

    // tok_late models a request that is still travelling to the PSP: for a while the PSP knows
    // nothing about the reference (a lookup says 404), then the charge lands and succeeds.
    #[tokio::test]
    async fn late_token_is_unknown_until_it_lands_and_then_succeeds() {
        let base = spawn(300).await;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(50))
            .build()
            .unwrap();
        let gave_up = client
            .post(format!("{base}/charges"))
            .json(&json!({ "reference": "late", "amount_cents": 500, "token": "tok_late" }))
            .send()
            .await;
        assert!(
            gave_up.is_err(),
            "the client should time out, got {gave_up:?}"
        );

        let during = reqwest::get(format!("{base}/charges/late")).await.unwrap();
        assert_eq!(during.status().as_u16(), 404);
        assert_eq!(stats_of(&base).await["charges_created"], 0);

        tokio::time::sleep(Duration::from_millis(500)).await;
        let after: Value = reqwest::get(format!("{base}/charges/late"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(after["status"], "succeeded");
        let stats = stats_of(&base).await;
        assert_eq!(stats["charges_created"], 1);
        assert_eq!(stats["charges_succeeded"], 1);
    }

    #[tokio::test]
    async fn sink_records_and_flaky_fails_twice() {
        let base = spawn(50).await;
        let client = reqwest::Client::new();
        let post = |name: &'static str| {
            let client = client.clone();
            let url = format!("{base}/sink/{name}");
            async move {
                client
                    .post(url)
                    .header("x-test", "1")
                    .body("hello")
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .as_u16()
            }
        };
        assert_eq!(post("good").await, 200);
        let list: Value = reqwest::get(format!("{base}/sink/good"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(list[0]["body"], "hello");
        assert_eq!(list[0]["headers"]["x-test"], "1");
        let flaky = [
            post("flaky").await,
            post("flaky").await,
            post("flaky").await,
        ];
        assert_eq!(flaky, [500, 500, 200]);
    }
}
