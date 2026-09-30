use serde::Serialize;

use crate::error::ApiError;

const DEFAULT_LIMIT: i64 = 20;
const MAX_LIMIT: i64 = 100;

pub fn limit(requested: Option<i64>) -> Result<i64, ApiError> {
    match requested {
        None => Ok(DEFAULT_LIMIT),
        Some(n) if (1..=MAX_LIMIT).contains(&n) => Ok(n),
        Some(_) => Err(ApiError::bad_request(format!(
            "limit must be between 1 and {MAX_LIMIT}"
        ))),
    }
}

#[derive(Debug, Serialize)]
pub struct Page<T> {
    pub data: Vec<T>,
    pub has_more: bool,
}

impl<T> Page<T> {
    pub fn from_overfetched(mut rows: Vec<T>, limit: i64) -> Self {
        let has_more = rows.len() as i64 > limit;
        rows.truncate(limit as usize);
        Self { data: rows, has_more }
    }
}
