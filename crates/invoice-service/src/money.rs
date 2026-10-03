// Invoice total math. Money is i64 cents only; every operation is checked, so an overflow
// becomes a 422 instead of a silently wrong number.
use serde::Deserialize;

use crate::error::AppError;

pub const MAX_LINE_ITEMS: usize = 100;
pub const MAX_QUANTITY: i64 = 1_000_000;
pub const MAX_UNIT_AMOUNT_CENTS: i64 = 10_000_000_000; // 100 million dollars (the 100_000_000_00 in the brief)

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LineItemInput {
    pub description: String,
    pub quantity: i64,
    pub unit_amount_cents: i64,
}

fn unprocessable(code: &str, message: impl Into<String>) -> AppError {
    AppError::Unprocessable {
        code: code.to_string(),
        message: message.into(),
    }
}

fn overflow() -> AppError {
    unprocessable("amount_overflow", "the invoice total is too large")
}

fn checked_mul(a: i64, b: i64) -> Result<i64, AppError> {
    a.checked_mul(b).ok_or_else(overflow)
}

fn checked_add(a: i64, b: i64) -> Result<i64, AppError> {
    a.checked_add(b).ok_or_else(overflow)
}

/// Validates the line items and returns the invoice total in cents.
/// All failures are 422: the request is well-formed JSON but its numbers are not acceptable.
pub fn compute_total(items: &[LineItemInput]) -> Result<i64, AppError> {
    if items.is_empty() || items.len() > MAX_LINE_ITEMS {
        return Err(unprocessable(
            "invalid_line_items",
            format!("an invoice needs between 1 and {MAX_LINE_ITEMS} line items"),
        ));
    }
    let mut total: i64 = 0;
    for item in items {
        if !(1..=MAX_QUANTITY).contains(&item.quantity) {
            return Err(unprocessable(
                "amount_out_of_range",
                format!("quantity must be between 1 and {MAX_QUANTITY}"),
            ));
        }
        if !(0..=MAX_UNIT_AMOUNT_CENTS).contains(&item.unit_amount_cents) {
            return Err(unprocessable(
                "amount_out_of_range",
                format!("unit_amount_cents must be between 0 and {MAX_UNIT_AMOUNT_CENTS}"),
            ));
        }
        let line = checked_mul(item.quantity, item.unit_amount_cents)?;
        total = checked_add(total, line)?;
    }
    if total <= 0 {
        return Err(unprocessable(
            "invalid_total",
            "the invoice total must be greater than zero",
        ));
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(quantity: i64, unit_amount_cents: i64) -> LineItemInput {
        LineItemInput {
            description: "x".into(),
            quantity,
            unit_amount_cents,
        }
    }

    fn code(err: AppError) -> String {
        match err {
            AppError::Unprocessable { code, .. } => code,
            other => panic!("expected 422, got {other:?}"),
        }
    }

    #[test]
    fn two_times_4900_is_9800() {
        assert_eq!(compute_total(&[item(2, 4900)]).unwrap(), 9800);
    }

    #[test]
    fn sums_several_lines() {
        assert_eq!(
            compute_total(&[item(2, 4900), item(1, 150), item(3, 0)]).unwrap(),
            9950
        );
    }

    #[test]
    fn zero_total_is_rejected() {
        assert_eq!(
            code(compute_total(&[item(5, 0)]).unwrap_err()),
            "invalid_total"
        );
        assert_eq!(
            code(compute_total(&[item(1, 0), item(2, 0)]).unwrap_err()),
            "invalid_total"
        );
    }

    #[test]
    fn empty_and_too_many_items_are_rejected() {
        assert_eq!(code(compute_total(&[]).unwrap_err()), "invalid_line_items");
        let many: Vec<LineItemInput> = (0..101).map(|_| item(1, 1)).collect();
        assert_eq!(
            code(compute_total(&many).unwrap_err()),
            "invalid_line_items"
        );
        let max: Vec<LineItemInput> = (0..100).map(|_| item(1, 1)).collect();
        assert_eq!(compute_total(&max).unwrap(), 100);
    }

    #[test]
    fn quantity_and_unit_amount_ranges() {
        assert_eq!(
            code(compute_total(&[item(0, 100)]).unwrap_err()),
            "amount_out_of_range"
        );
        assert_eq!(
            code(compute_total(&[item(-1, 100)]).unwrap_err()),
            "amount_out_of_range"
        );
        assert_eq!(
            code(compute_total(&[item(1_000_001, 1)]).unwrap_err()),
            "amount_out_of_range"
        );
        assert_eq!(
            code(compute_total(&[item(1, -1)]).unwrap_err()),
            "amount_out_of_range"
        );
        assert_eq!(
            code(compute_total(&[item(1, MAX_UNIT_AMOUNT_CENTS + 1)]).unwrap_err()),
            "amount_out_of_range"
        );
        // The largest allowed line is exactly representable.
        assert_eq!(
            compute_total(&[item(MAX_QUANTITY, MAX_UNIT_AMOUNT_CENTS)]).unwrap(),
            MAX_QUANTITY * MAX_UNIT_AMOUNT_CENTS
        );
    }

    #[test]
    fn the_biggest_legal_invoice_does_not_overflow() {
        let items: Vec<LineItemInput> = (0..100)
            .map(|_| item(MAX_QUANTITY, MAX_UNIT_AMOUNT_CENTS))
            .collect();
        assert_eq!(
            compute_total(&items).unwrap(),
            100 * MAX_QUANTITY * MAX_UNIT_AMOUNT_CENTS
        );
    }

    // The range limits keep real requests far from i64::MAX, so the overflow guards are
    // tested directly: they are the safety net if a limit is ever raised.
    #[test]
    fn checked_helpers_report_overflow() {
        assert_eq!(
            code(checked_mul(i64::MAX, 2).unwrap_err()),
            "amount_overflow"
        );
        assert_eq!(
            code(checked_add(i64::MAX, 1).unwrap_err()),
            "amount_overflow"
        );
        assert_eq!(checked_mul(3, 4).unwrap(), 12);
    }
}
