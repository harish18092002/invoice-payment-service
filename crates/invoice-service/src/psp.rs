// Client for the payment provider (the mock PSP in this project).
//
// Every call has a timeout. Results are sorted into outcomes the payment flow can act on:
// a definite answer (succeeded or failed) or "ambiguous" (we do not know whether money moved).
use std::time::Duration;

use serde_json::{Value, json};

/// A lookup is a cheap read, so it gets a much shorter deadline than a charge.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, PartialEq)]
pub enum ChargeOutcome {
    Succeeded {
        psp_ref: String,
    },
    Failed {
        code: String,
    },
    /// Our own deadline fired. The request may still be on its way to the PSP, so even a lookup that
    /// finds nothing proves nothing yet. The caller must not judge it; the reconciler decides later.
    TimedOut,
    /// A connection error, 5xx or an unreadable answer: the charge may or may not exist, but the
    /// request is not still travelling, so one immediate lookup can settle it.
    Ambiguous,
}

#[derive(Debug, PartialEq)]
pub enum LookupOutcome {
    Processing,
    Succeeded {
        psp_ref: String,
    },
    Failed {
        code: String,
    },
    /// The PSP has no record of this reference.
    NotFound,
    Ambiguous,
}

#[derive(Clone)]
pub struct PspClient {
    http: reqwest::Client,
    base_url: String,
    charge_timeout: Duration,
}

impl PspClient {
    pub fn new(base_url: String, charge_timeout: Duration) -> PspClient {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("failed to build the PSP HTTP client");
        PspClient {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            charge_timeout,
        }
    }

    /// Asks the PSP to charge. `reference` is our attempt id; the PSP deduplicates on it.
    /// Neither the card token nor the PSP's answer body is logged.
    pub async fn charge(&self, reference: &str, amount_cents: i64, token: &str) -> ChargeOutcome {
        let sent = self
            .http
            .post(format!("{}/charges", self.base_url))
            .timeout(self.charge_timeout)
            .json(&json!({ "reference": reference, "amount_cents": amount_cents, "token": token }))
            .send()
            .await;
        match sent {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let body = resp.json::<Value>().await.ok();
                classify_charge(status, body.as_ref())
            }
            Err(e) => {
                let timed_out = e.is_timeout();
                tracing::warn!(
                    reference,
                    timed_out,
                    "PSP charge call failed; outcome unknown"
                );
                if timed_out {
                    ChargeOutcome::TimedOut
                } else {
                    ChargeOutcome::Ambiguous
                }
            }
        }
    }

    /// Asks the PSP what it knows about a reference.
    pub async fn lookup(&self, reference: &str) -> LookupOutcome {
        let sent = self
            .http
            .get(format!("{}/charges/{reference}", self.base_url))
            .timeout(LOOKUP_TIMEOUT)
            .send()
            .await;
        match sent {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let body = resp.json::<Value>().await.ok();
                classify_lookup(status, body.as_ref())
            }
            Err(e) => {
                tracing::warn!(reference, timed_out = e.is_timeout(), "PSP lookup failed");
                LookupOutcome::Ambiguous
            }
        }
    }
}

fn text_field(body: Option<&Value>, name: &str) -> Option<String> {
    body?.get(name)?.as_str().map(str::to_string)
}

fn classify_charge(http_status: u16, body: Option<&Value>) -> ChargeOutcome {
    match http_status {
        200 => match text_field(body, "status").as_deref() {
            Some("succeeded") => match text_field(body, "psp_ref") {
                Some(psp_ref) => ChargeOutcome::Succeeded { psp_ref },
                None => ChargeOutcome::Ambiguous,
            },
            Some("failed") => ChargeOutcome::Failed {
                code: text_field(body, "code").unwrap_or_else(|| "psp_declined".to_string()),
            },
            // "processing" (a replay of a charge still in flight) or anything unexpected.
            _ => ChargeOutcome::Ambiguous,
        },
        // The PSP understood us and refused the request itself (for example an unknown token).
        400..=499 => ChargeOutcome::Failed {
            code: "psp_rejected".to_string(),
        },
        _ => ChargeOutcome::Ambiguous,
    }
}

fn classify_lookup(http_status: u16, body: Option<&Value>) -> LookupOutcome {
    match http_status {
        404 => LookupOutcome::NotFound,
        200 => match text_field(body, "status").as_deref() {
            Some("processing") => LookupOutcome::Processing,
            Some("succeeded") => match text_field(body, "psp_ref") {
                Some(psp_ref) => LookupOutcome::Succeeded { psp_ref },
                None => LookupOutcome::Ambiguous,
            },
            Some("failed") => LookupOutcome::Failed {
                code: text_field(body, "code").unwrap_or_else(|| "psp_declined".to_string()),
            },
            _ => LookupOutcome::Ambiguous,
        },
        _ => LookupOutcome::Ambiguous,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charge_answers_are_classified() {
        let ok = json!({"status":"succeeded","psp_ref":"p1"});
        assert_eq!(
            classify_charge(200, Some(&ok)),
            ChargeOutcome::Succeeded {
                psp_ref: "p1".into()
            }
        );
        let no = json!({"status":"failed","code":"card_declined"});
        assert_eq!(
            classify_charge(200, Some(&no)),
            ChargeOutcome::Failed {
                code: "card_declined".into()
            }
        );
        assert_eq!(
            classify_charge(200, Some(&json!({"status":"processing"}))),
            ChargeOutcome::Ambiguous
        );
        assert_eq!(
            classify_charge(200, Some(&json!({"status":"succeeded"}))),
            ChargeOutcome::Ambiguous
        ); // no psp_ref
        assert_eq!(classify_charge(200, None), ChargeOutcome::Ambiguous);
        assert_eq!(classify_charge(500, None), ChargeOutcome::Ambiguous);
        assert_eq!(classify_charge(503, None), ChargeOutcome::Ambiguous);
        assert_eq!(
            classify_charge(400, None),
            ChargeOutcome::Failed {
                code: "psp_rejected".into()
            }
        );
    }

    // Starts a PSP stand-in whose /charges answers only after 5 s; returns its base URL.
    async fn spawn_slow_psp() -> String {
        let router = axum::Router::new().route(
            "/charges",
            axum::routing::post(|| async {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{addr}")
    }

    // After our own deadline fires the request may still be on its way to the PSP, so this must be
    // told apart from other unclear answers (DESIGN.md section 3, "Ambiguous PSP answers").
    #[tokio::test]
    async fn a_call_that_hits_the_deadline_is_a_timeout() {
        let client = PspClient::new(spawn_slow_psp().await, Duration::from_millis(100));
        assert_eq!(
            client.charge("r1", 500, "tok_x").await,
            ChargeOutcome::TimedOut
        );
    }

    // A refused connection means the request never left, so a lookup may judge it (plain Ambiguous).
    #[tokio::test]
    async fn a_refused_connection_is_ambiguous_not_a_timeout() {
        let client = PspClient::new("http://127.0.0.1:1".into(), Duration::from_secs(2));
        assert_eq!(
            client.charge("r1", 500, "tok_x").await,
            ChargeOutcome::Ambiguous
        );
    }

    #[test]
    fn lookup_answers_are_classified() {
        assert_eq!(classify_lookup(404, None), LookupOutcome::NotFound);
        assert_eq!(
            classify_lookup(200, Some(&json!({"status":"processing"}))),
            LookupOutcome::Processing
        );
        assert_eq!(
            classify_lookup(200, Some(&json!({"status":"succeeded","psp_ref":"p"}))),
            LookupOutcome::Succeeded {
                psp_ref: "p".into()
            }
        );
        assert_eq!(
            classify_lookup(
                200,
                Some(&json!({"status":"failed","code":"card_declined"}))
            ),
            LookupOutcome::Failed {
                code: "card_declined".into()
            }
        );
        assert_eq!(classify_lookup(500, None), LookupOutcome::Ambiguous);
        assert_eq!(
            classify_lookup(200, Some(&json!({"status":"weird"}))),
            LookupOutcome::Ambiguous
        );
    }
}
