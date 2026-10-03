// Shared helpers for the integration tests.
//
// These tests talk over HTTP to a running stack (app + mock PSP + real Postgres), started with
// docker-compose.yml plus docker-compose.test.yml (see README.md). Each test creates its own
// business, API key and customer, so tests never see each other's data.
//
// Environment (all optional):
//   TEST_APP_URL         default http://localhost:8080
//   TEST_PSP_URL         default http://localhost:9000
//   TEST_ADMIN_TOKEN     default dev-admin-token
//   TEST_PSP_WAIT_SECS   default 1 (must match PSP_WAIT_SECS in docker-compose.test.yml)
#![allow(dead_code)] // every test file uses a different subset of these helpers

use std::{
    future::Future,
    ops::Deref,
    sync::OnceLock,
    time::{Duration, Instant},
};

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, MutexGuard};

// The mock PSP's counters are global, so tests that compare them must not overlap. Every test
// holds this lock for its whole run, which makes the tests inside one file run one at a time.
// (Cargo already runs separate test files one after another.)
static SERIAL: OnceLock<Mutex<()>> = OnceLock::new();

pub struct Response {
    pub status: u16,
    pub raw: String,
    pub body: Value,
    pub elapsed: Duration,
}

#[derive(Debug, Deserialize, Clone, Copy)]
pub struct PspStats {
    pub charges_created: u64,
    pub charges_succeeded: u64,
    pub post_calls: u64,
}

/// A cloneable handle for talking to the app as one business.
#[derive(Clone)]
pub struct Api {
    pub client: reqwest::Client,
    pub app_url: String,
    pub psp_url: String,
    pub api_key: String,
    pub customer_id: String,
    pub psp_wait_secs: u64,
}

/// What a test holds: the Api plus the lock that keeps tests from overlapping.
pub struct Ctx {
    pub api: Api,
    _serial: MutexGuard<'static, ()>,
}

impl Deref for Ctx {
    type Target = Api;
    fn deref(&self) -> &Api {
        &self.api
    }
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

/// Waits for the stack, then creates a fresh business, API key and customer.
pub async fn setup() -> Ctx {
    let serial = SERIAL.get_or_init(|| Mutex::new(())).lock().await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .expect("http client");
    let app_url = env_or("TEST_APP_URL", "http://localhost:8080");
    let psp_url = env_or("TEST_PSP_URL", "http://localhost:9000");
    let admin_token = env_or("TEST_ADMIN_TOKEN", "dev-admin-token");
    let psp_wait_secs: u64 = env_or("TEST_PSP_WAIT_SECS", "1")
        .parse()
        .expect("TEST_PSP_WAIT_SECS must be a number");

    // The stack may still be starting: poll /health with a deadline instead of sleeping.
    let health = format!("{app_url}/health");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if client
            .get(&health)
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the app at {app_url} is not up; start it with: docker compose -f docker-compose.yml -f docker-compose.test.yml up --build -d"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    let created: Value = client
        .post(format!("{app_url}/admin/businesses"))
        .header("X-Admin-Token", admin_token)
        .json(&json!({ "name": "test business" }))
        .send()
        .await
        .expect("create business")
        .json()
        .await
        .expect("business json");
    let api_key = created["api_key"]["key"]
        .as_str()
        .expect("api key in response")
        .to_string();

    let mut api = Api {
        client,
        app_url,
        psp_url,
        api_key,
        customer_id: String::new(),
        psp_wait_secs,
    };
    let customer = api
        .request(
            "POST",
            "/v1/customers",
            Some(json!({ "name": "Test Customer", "email": "test@example.com" })),
            &[],
        )
        .await;
    assert_eq!(customer.status, 201, "create customer: {}", customer.raw);
    api.customer_id = customer.body["id"]
        .as_str()
        .expect("customer id")
        .to_string();
    Ctx {
        api,
        _serial: serial,
    }
}

impl Api {
    /// Sends one request as this business. Returns status, raw text, parsed body and duration.
    pub async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> Response {
        let method = reqwest::Method::from_bytes(method.as_bytes()).expect("method");
        let mut req = self
            .client
            .request(method, format!("{}{path}", self.app_url))
            .bearer_auth(&self.api_key);
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
        if let Some(body) = body {
            req = req.json(&body);
        }
        let started = Instant::now();
        let resp = req.send().await.expect("request to the app failed");
        let status = resp.status().as_u16();
        let raw = resp.text().await.expect("response body");
        let elapsed = started.elapsed();
        let body = serde_json::from_str(&raw).unwrap_or(Value::Null);
        Response {
            status,
            raw,
            body,
            elapsed,
        }
    }

    /// Creates and finalizes an invoice (2 x 4900 = 9800 cents). Returns its id.
    pub async fn create_open_invoice(&self) -> String {
        let created = self
            .request(
                "POST",
                "/v1/invoices",
                Some(json!({
                    "customer_id": self.customer_id,
                    "due_date": "2030-01-01",
                    "line_items": [{ "description": "Widget", "quantity": 2, "unit_amount_cents": 4900 }]
                })),
                &[],
            )
            .await;
        assert_eq!(created.status, 201, "create invoice: {}", created.raw);
        let id = created.body["id"].as_str().expect("invoice id").to_string();
        let finalized = self
            .request("POST", &format!("/v1/invoices/{id}/finalize"), None, &[])
            .await;
        assert_eq!(finalized.status, 200, "finalize: {}", finalized.raw);
        id
    }

    /// POST /v1/invoices/{id}/pay with a card token.
    pub async fn pay(&self, invoice_id: &str, idempotency_key: &str, card_token: &str) -> Response {
        self.pay_body(
            invoice_id,
            idempotency_key,
            json!({ "card_token": card_token }),
        )
        .await
    }

    /// Same, with an arbitrary JSON body.
    pub async fn pay_body(&self, invoice_id: &str, idempotency_key: &str, body: Value) -> Response {
        self.request(
            "POST",
            &format!("/v1/invoices/{invoice_id}/pay"),
            Some(body),
            &[("Idempotency-Key", idempotency_key)],
        )
        .await
    }

    pub async fn psp_stats(&self) -> PspStats {
        self.client
            .get(format!("{}/_test/stats", self.psp_url))
            .send()
            .await
            .expect("mock psp stats")
            .json()
            .await
            .expect("stats json")
    }

    pub async fn invoice_state(&self, invoice_id: &str) -> String {
        let r = self
            .request("GET", &format!("/v1/invoices/{invoice_id}"), None, &[])
            .await;
        assert_eq!(r.status, 200, "get invoice: {}", r.raw);
        r.body["state"].as_str().expect("state").to_string()
    }

    pub async fn attempts(&self, invoice_id: &str) -> Vec<Value> {
        let r = self
            .request(
                "GET",
                &format!("/v1/invoices/{invoice_id}/payment_attempts"),
                None,
                &[],
            )
            .await;
        assert_eq!(r.status, 200, "list attempts: {}", r.raw);
        r.body["data"].as_array().expect("data array").clone()
    }

    /// Events of one type that concern one invoice.
    pub async fn events_for(&self, invoice_id: &str, event_type: &str) -> Vec<Value> {
        let r = self.request("GET", "/v1/events?limit=100", None, &[]).await;
        assert_eq!(r.status, 200, "list events: {}", r.raw);
        r.body["data"]
            .as_array()
            .expect("data array")
            .iter()
            .filter(|e| e["type"] == event_type && e["data"]["object"]["id"] == invoice_id)
            .cloned()
            .collect()
    }
}

/// Polls `check` every 100 ms until it returns true, or panics after `timeout`.
/// Used instead of fixed sleeps: it returns as soon as the condition holds.
pub async fn eventually<F, Fut>(what: &str, timeout: Duration, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if check().await {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out after {timeout:?} waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
