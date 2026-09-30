use serde::Deserialize;

use crate::error::ApiError;

pub const MAX_LINE_ITEMS: usize = 100;
pub const MAX_QUANTITY: i64 = 1_000_000;
pub const MAX_UNIT_AMOUNT_CENTS: i64 = 100_000_000;
pub const MAX_TOTAL_CENTS: i64 = 1_000_000_000;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewLineItem {
    pub description: String,
    pub quantity: i64,
    pub unit_amount_cents: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PricedLineItem {
    pub description: String,
    pub quantity: i64,
    pub unit_amount_cents: i64,
    pub amount_cents: i64,
}

#[derive(Debug, PartialEq, Eq)]
pub struct PricedInvoice {
    pub line_items: Vec<PricedLineItem>,
    pub total_cents: i64,
}

pub fn price(items: Vec<NewLineItem>) -> Result<PricedInvoice, ApiError> {
    if items.is_empty() || items.len() > MAX_LINE_ITEMS {
        return Err(ApiError::validation(format!(
            "an invoice needs between 1 and {MAX_LINE_ITEMS} line items"
        )));
    }

    let mut total_cents: i64 = 0;
    let mut line_items = Vec::with_capacity(items.len());

    for (i, item) in items.into_iter().enumerate() {
        let description = item.description.trim();
        if !(1..=500).contains(&description.chars().count()) {
            return Err(ApiError::validation(format!(
                "line_items[{i}].description must be 1 to 500 characters"
            )));
        }
        if !(1..=MAX_QUANTITY).contains(&item.quantity) {
            return Err(ApiError::validation(format!(
                "line_items[{i}].quantity must be between 1 and {MAX_QUANTITY}"
            )));
        }
        if !(0..=MAX_UNIT_AMOUNT_CENTS).contains(&item.unit_amount_cents) {
            return Err(ApiError::validation(format!(
                "line_items[{i}].unit_amount_cents must be between 0 and {MAX_UNIT_AMOUNT_CENTS}"
            )));
        }

        let amount_cents = item
            .quantity
            .checked_mul(item.unit_amount_cents)
            .ok_or_else(total_too_large)?;
        total_cents = total_cents
            .checked_add(amount_cents)
            .ok_or_else(total_too_large)?;

        line_items.push(PricedLineItem {
            description: description.to_owned(),
            quantity: item.quantity,
            unit_amount_cents: item.unit_amount_cents,
            amount_cents,
        });
    }

    if total_cents > MAX_TOTAL_CENTS {
        return Err(total_too_large());
    }
    if total_cents == 0 {
        return Err(ApiError::validation("invoice total must be greater than zero"));
    }

    Ok(PricedInvoice {
        line_items,
        total_cents,
    })
}

fn total_too_large() -> ApiError {
    ApiError::validation(format!("invoice total cannot exceed {MAX_TOTAL_CENTS} cents"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(quantity: i64, unit_amount_cents: i64) -> NewLineItem {
        NewLineItem {
            description: "Widget".into(),
            quantity,
            unit_amount_cents,
        }
    }

    #[test]
    fn total_is_the_sum_of_quantity_times_unit_amount() {
        let priced = price(vec![item(3, 1_999), item(1, 1), item(2, 0)]).unwrap();

        let amounts: Vec<i64> = priced.line_items.iter().map(|li| li.amount_cents).collect();
        assert_eq!(amounts, [5_997, 1, 0]);
        assert_eq!(priced.total_cents, 5_998);
    }

    #[test]
    fn rejects_non_positive_quantities_and_negative_prices() {
        assert!(price(vec![item(0, 100)]).is_err());
        assert!(price(vec![item(-1, 100)]).is_err());
        assert!(price(vec![item(1, -100)]).is_err());
    }

    #[test]
    fn rejects_values_that_could_overflow() {
        assert!(price(vec![item(i64::MAX, 2)]).is_err());
        assert!(price(vec![item(2, i64::MAX)]).is_err());
    }

    #[test]
    fn rejects_totals_above_the_cap_even_when_each_line_is_valid() {
        let items = vec![item(MAX_QUANTITY, MAX_UNIT_AMOUNT_CENTS); 3];
        assert!(price(items).is_err());
    }

    #[test]
    fn rejects_empty_and_zero_value_invoices() {
        assert!(price(vec![]).is_err());
        assert!(price(vec![item(5, 0)]).is_err());
    }

    #[test]
    fn rejects_blank_descriptions() {
        let mut blank = item(1, 100);
        blank.description = "   ".into();
        assert!(price(vec![blank]).is_err());
    }
}
