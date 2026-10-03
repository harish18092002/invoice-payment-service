// Keyset pagination shared by every list endpoint, ordered by (created_at DESC, id DESC).
//
// Why keyset and not OFFSET: the cursor says "continue after this exact row", so rows added
// or removed between requests never cause skipped or repeated items, and the database uses
// the (business_id, created_at DESC, id DESC) indexes instead of counting past skipped rows.
//
// How a list handler uses it:
//   1. `let page = params.resolve()?;`
//   2. run its query with `LIMIT page.limit + 1`, and the cursor condition
//      `($n::timestamptz IS NULL OR (created_at, id) < ($n, $m))` bound to page.after
//   3. `Ok(Json(finish(rows, page.limit, |row| (row.created_at, row.id))))`
// The extra row fetched in step 2 only tells us whether another page exists.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::AppError;

const DEFAULT_LIMIT: i64 = 20;
const MAX_LIMIT: i64 = 100;

/// Query-string parameters: `?limit=&cursor=`.
#[derive(Deserialize)]
pub struct PageParams {
    limit: Option<i64>,
    cursor: Option<String>,
}

/// Validated pagination input, ready to bind into a query.
pub struct Page {
    pub limit: i64,
    /// Position of the last row of the previous page; None for the first page.
    pub after_created_at: Option<DateTime<Utc>>,
    pub after_id: Option<Uuid>,
}

impl PageParams {
    pub fn resolve(self) -> Result<Page, AppError> {
        let limit = self.limit.unwrap_or(DEFAULT_LIMIT);
        if !(1..=MAX_LIMIT).contains(&limit) {
            return Err(AppError::InvalidRequest(format!(
                "limit must be between 1 and {MAX_LIMIT}"
            )));
        }
        let (after_created_at, after_id) = match self.cursor {
            Some(text) => {
                let (created_at, id) = decode_cursor(&text)?;
                (Some(created_at), Some(id))
            }
            None => (None, None),
        };
        Ok(Page {
            limit,
            after_created_at,
            after_id,
        })
    }
}

#[derive(Serialize)]
pub struct Paginated<T> {
    pub data: Vec<T>,
    pub next_cursor: Option<String>,
}

/// Turns the fetched rows (up to limit + 1 of them) into a response.
/// `position` tells it where a row sits in the ordering.
pub fn finish<T>(
    mut rows: Vec<T>,
    limit: i64,
    position: fn(&T) -> (DateTime<Utc>, Uuid),
) -> Paginated<T> {
    let limit = limit as usize;
    let has_more = rows.len() > limit;
    rows.truncate(limit);
    let next_cursor = if has_more {
        rows.last().map(|row| {
            let (created_at, id) = position(row);
            encode_cursor(created_at, id)
        })
    } else {
        None
    };
    Paginated {
        data: rows,
        next_cursor,
    }
}

// A cursor is "<microseconds since epoch>.<uuid>", base64-encoded so clients treat it as opaque.
// Postgres keeps timestamps to the microsecond, so nothing is lost in the round trip.
fn encode_cursor(created_at: DateTime<Utc>, id: Uuid) -> String {
    URL_SAFE_NO_PAD.encode(format!("{}.{}", created_at.timestamp_micros(), id))
}

fn decode_cursor(text: &str) -> Result<(DateTime<Utc>, Uuid), AppError> {
    let bad = || AppError::InvalidRequest("invalid cursor".into());
    let bytes = URL_SAFE_NO_PAD.decode(text).map_err(|_| bad())?;
    let text = String::from_utf8(bytes).map_err(|_| bad())?;
    let (micros, id) = text.split_once('.').ok_or_else(bad)?;
    let micros: i64 = micros.parse().map_err(|_| bad())?;
    let created_at = DateTime::from_timestamp_micros(micros).ok_or_else(bad)?;
    let id = Uuid::parse_str(id).map_err(|_| bad())?;
    Ok((created_at, id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_round_trips() {
        let now = Utc::now();
        let id = Uuid::now_v7();
        let (t, i) = decode_cursor(&encode_cursor(now, id)).unwrap();
        assert_eq!(t.timestamp_micros(), now.timestamp_micros());
        assert_eq!(i, id);
    }

    #[test]
    fn bad_cursors_are_rejected() {
        for bad in [
            "",
            "!!!",
            "bm90LWEtY3Vyc29y",
            &URL_SAFE_NO_PAD.encode("12.notauuid"),
        ] {
            assert!(decode_cursor(bad).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn limit_is_validated() {
        let p = |limit| {
            PageParams {
                limit,
                cursor: None,
            }
            .resolve()
        };
        assert_eq!(p(None).unwrap().limit, 20);
        assert_eq!(p(Some(100)).unwrap().limit, 100);
        assert!(p(Some(0)).is_err());
        assert!(p(Some(101)).is_err());
    }

    #[test]
    fn finish_sets_cursor_only_when_more_rows_exist() {
        let rows: Vec<(DateTime<Utc>, Uuid)> =
            (0..3).map(|_| (Utc::now(), Uuid::now_v7())).collect();
        let more = finish(rows.clone(), 2, |r| *r);
        assert_eq!(more.data.len(), 2);
        assert!(more.next_cursor.is_some());
        let last = finish(rows[..2].to_vec(), 2, |r| *r);
        assert!(last.next_cursor.is_none());
    }
}
