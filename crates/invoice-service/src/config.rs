use std::env;

// Settings read once at startup. Secrets (database URL, admin token) must never be logged,
// so this struct deliberately does not derive Debug.
#[allow(dead_code)] // fields are read by later steps
pub struct Config {
    pub database_url: String,
    pub psp_url: String,
    pub admin_token: String,
    /// How long the pay handler waits for the PSP before answering 202.
    pub psp_wait_secs: u64,
    /// Hard deadline for one PSP call.
    pub psp_timeout_secs: u64,
    /// Multiplies every webhook retry delay; tests set it small (for example 0.01).
    pub webhook_delay_scale: f64,
    /// How often the dispatcher looks for due deliveries.
    pub webhook_poll_ms: u64,
    /// How often the reconciler looks for stuck payment attempts.
    pub reconcile_interval_secs: u64,
    /// Only attempts pending for at least this long are looked at (the normal path gets time to finish).
    pub reconcile_min_age_secs: u64,
    /// "PSP has no record" only proves there was no charge once the attempt is this old.
    pub reconcile_not_found_after_secs: u64,
    /// TEST ONLY. When true the process exits right after the PSP answers a charge, before the
    /// result is saved, to simulate a crash for the recovery demo. Must stay false everywhere else.
    pub crash_after_psp_call: bool,
}

impl Config {
    pub fn from_env() -> Result<Config, String> {
        Ok(Config {
            database_url: required("DATABASE_URL")?,
            psp_url: required("PSP_URL")?,
            admin_token: required("ADMIN_TOKEN")?,
            psp_wait_secs: number_or("PSP_WAIT_SECS", 5)?,
            psp_timeout_secs: number_or("PSP_TIMEOUT_SECS", 35)?,
            webhook_delay_scale: scale_from_env()?,
            webhook_poll_ms: number_or("WEBHOOK_POLL_MS", 1000)?,
            reconcile_interval_secs: number_or("RECONCILE_INTERVAL_SECS", 15)?,
            reconcile_min_age_secs: number_or("RECONCILE_MIN_AGE_SECS", 45)?,
            reconcile_not_found_after_secs: number_or("RECONCILE_NOT_FOUND_AFTER_SECS", 120)?,
            // Off unless the variable is exactly "true".
            crash_after_psp_call: env::var("CRASH_AFTER_PSP_CALL").is_ok_and(|v| v == "true"),
        })
    }
}

fn required(name: &str) -> Result<String, String> {
    match env::var(name) {
        Ok(v) if !v.is_empty() => Ok(v),
        _ => Err(format!("missing required env var {name}")),
    }
}

fn number_or(name: &str, default: u64) -> Result<u64, String> {
    match env::var(name) {
        Ok(v) => v
            .parse()
            .map_err(|_| format!("{name} must be a whole number")),
        Err(_) => Ok(default),
    }
}

fn scale_from_env() -> Result<f64, String> {
    match env::var("WEBHOOK_DELAY_SCALE") {
        Ok(v) => match v.parse::<f64>() {
            Ok(x) if x > 0.0 && x.is_finite() => Ok(x),
            _ => Err("WEBHOOK_DELAY_SCALE must be a positive number".to_string()),
        },
        Err(_) => Ok(1.0),
    }
}
