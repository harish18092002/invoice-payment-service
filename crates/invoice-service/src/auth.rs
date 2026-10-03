// API key authentication and the admin bootstrap endpoint.
//
// Key format:  dodo_sk_<prefix8>_<secret32>
//   - prefix8  : 8 random base62 characters, stored in clear in api_keys.prefix as a lookup handle.
//   - secret32 : 32 random base62 characters (about 190 bits) from the operating system's RNG.
//                Only sha256(secret32) is stored (api_keys.key_hash); the secret itself is shown
//                once, at creation, and can never be read back.
// Base62 has no underscore, so the two parts split unambiguously.
use std::sync::Arc;

use axum::{
    Json,
    extract::{FromRequestParts, State},
    http::{StatusCode, request::Parts},
};
use rand::{RngCore, TryRngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::{
    app::AppState,
    error::{ApiJson, AppError},
};

const KEY_PREFIX: &str = "dodo_sk_";
const PREFIX_LEN: usize = 8;
const SECRET_LEN: usize = 32;
const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/// A freshly generated key. `full` is shown to the caller once and never stored.
pub struct NewKey {
    pub prefix: String,
    pub full: String,
    pub hash: Vec<u8>,
}

fn random_string(len: usize) -> String {
    // OS random source; the wrapper panics if the OS cannot supply randomness (never silently weak).
    let mut rng = OsRng.unwrap_err();
    (0..len)
        .map(|_| {
            // 248 is the largest multiple of 62 below 256; rejecting higher bytes keeps the
            // choice of character perfectly uniform (no modulo bias).
            loop {
                let byte = rng.next_u32() as u8;
                if byte < 248 {
                    break ALPHABET[(byte % 62) as usize] as char;
                }
            }
        })
        .collect()
}

pub fn hash_secret(secret: &str) -> Vec<u8> {
    Sha256::digest(secret.as_bytes()).to_vec()
}

pub fn generate_key() -> NewKey {
    let prefix = random_string(PREFIX_LEN);
    let secret = random_string(SECRET_LEN);
    NewKey {
        full: format!("{KEY_PREFIX}{prefix}_{secret}"),
        hash: hash_secret(&secret),
        prefix,
    }
}

/// Splits "Bearer dodo_sk_<prefix>_<secret>" into (prefix, secret), or None if malformed.
fn parse_bearer(header: &str) -> Option<(String, String)> {
    let key = header.strip_prefix("Bearer ")?.strip_prefix(KEY_PREFIX)?;
    let (prefix, secret) = key.split_once('_')?;
    let valid =
        |s: &str, len: usize| s.len() == len && s.bytes().all(|b| b.is_ascii_alphanumeric());
    if valid(prefix, PREFIX_LEN) && valid(secret, SECRET_LEN) {
        Some((prefix.to_string(), secret.to_string()))
    } else {
        None
    }
}

/// The authenticated caller. Handlers that list this as an argument are protected:
/// Axum runs `from_request_parts` first and returns its error without calling the handler.
/// `business_id` is the only source of tenant identity; never read it from a request body.
pub struct AuthedBusiness {
    pub business_id: Uuid,
    #[allow(dead_code)] // read by later steps (audit trail)
    pub key_id: Uuid,
}

impl FromRequestParts<Arc<AppState>> for AuthedBusiness {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, AppError> {
        // Every failure below returns the identical 401, so callers cannot tell a bad
        // prefix from a bad secret or a revoked key.
        let header = parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .ok_or(AppError::Unauthorized)?;
        let (prefix, secret) = parse_bearer(header).ok_or(AppError::Unauthorized)?;

        let row: Option<(Uuid, Uuid, Vec<u8>, bool)> = sqlx::query_as(
            "SELECT id, business_id, key_hash, revoked_at IS NOT NULL FROM api_keys WHERE prefix = $1",
        )
        .bind(&prefix)
        .fetch_optional(&state.pool)
        .await?;

        // Always hash and compare, even when the prefix is unknown, so the work done does not
        // depend on whether the prefix exists.
        let presented = hash_secret(&secret);
        let (key_id, business_id, stored, revoked) =
            row.unwrap_or((Uuid::nil(), Uuid::nil(), vec![0u8; 32], true));
        let hash_ok: bool = stored.ct_eq(&presented).into();
        if !hash_ok || revoked {
            return Err(AppError::Unauthorized);
        }

        sqlx::query("UPDATE api_keys SET last_used_at = now() WHERE id = $1")
            .bind(key_id)
            .execute(&state.pool)
            .await?;
        Ok(AuthedBusiness {
            business_id,
            key_id,
        })
    }
}

/// Checks the X-Admin-Token header against ADMIN_TOKEN in constant time.
/// Both sides are hashed first so the comparison always works on equal-length values.
fn check_admin_token(parts: &Parts, expected: &str) -> Result<(), AppError> {
    let given = parts
        .headers
        .get("x-admin-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let ok: bool = hash_secret(given).ct_eq(&hash_secret(expected)).into();
    if ok {
        Ok(())
    } else {
        Err(AppError::Unauthorized)
    }
}

pub struct AdminAuth;

impl FromRequestParts<Arc<AppState>> for AdminAuth {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, AppError> {
        check_admin_token(parts, &state.config.admin_token)?;
        Ok(AdminAuth)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateBusinessRequest {
    name: String,
}

#[derive(Serialize)]
struct BusinessView {
    id: Uuid,
    name: String,
}

#[derive(Serialize)]
pub struct CreatedKey {
    pub id: Uuid,
    pub prefix: String,
    /// The full key. Returned only in this response.
    pub key: String,
}

#[derive(Serialize)]
pub struct CreateBusinessResponse {
    business: BusinessView,
    api_key: CreatedKey,
}

/// POST /admin/businesses: creates a business and its first API key in one transaction.
pub async fn create_business(
    _admin: AdminAuth,
    State(state): State<Arc<AppState>>,
    ApiJson(req): ApiJson<CreateBusinessRequest>,
) -> Result<(StatusCode, Json<CreateBusinessResponse>), AppError> {
    let name = req.name.trim().to_string();
    if name.is_empty() || name.chars().count() > 200 {
        return Err(AppError::InvalidRequest(
            "name must be 1 to 200 characters".into(),
        ));
    }

    let business_id = Uuid::now_v7();
    let key_id = Uuid::now_v7();
    let key = generate_key();

    // Both inserts commit together or not at all. No network call happens inside it.
    let mut tx = state.pool.begin().await?;
    sqlx::query("INSERT INTO businesses (id, name) VALUES ($1, $2)")
        .bind(business_id)
        .bind(&name)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO api_keys (id, business_id, prefix, key_hash, label) VALUES ($1, $2, $3, $4, 'initial')")
        .bind(key_id)
        .bind(business_id)
        .bind(&key.prefix)
        .bind(&key.hash)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    Ok((
        StatusCode::CREATED,
        Json(CreateBusinessResponse {
            business: BusinessView {
                id: business_id,
                name,
            },
            api_key: CreatedKey {
                id: key_id,
                prefix: key.prefix,
                key: key.full,
            },
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_key_parses_back_and_hash_matches() {
        let k = generate_key();
        let (prefix, secret) = parse_bearer(&format!("Bearer {}", k.full)).unwrap();
        assert_eq!(prefix, k.prefix);
        assert_eq!(hash_secret(&secret), k.hash);
    }

    #[test]
    fn keys_are_unique() {
        assert_ne!(generate_key().full, generate_key().full);
    }

    #[test]
    fn malformed_headers_are_rejected() {
        for bad in [
            "",
            "Bearer",
            "Bearer dodo_sk_",
            "Basic dodo_sk_abcdefgh_0123456789abcdef0123456789abcdef",
            "Bearer dodo_pk_abcdefgh_0123456789abcdef0123456789abcdef",
            "Bearer dodo_sk_abcdefg_0123456789abcdef0123456789abcdef",
            "Bearer dodo_sk_abcdefgh_short",
            "Bearer dodo_sk_abcdefgh_0123456789abcdef0123456789abcde!",
        ] {
            assert!(parse_bearer(bad).is_none(), "accepted: {bad}");
        }
    }
}
