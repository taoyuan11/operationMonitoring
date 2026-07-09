use chrono::Utc;

pub fn non_empty_or(value: Option<String>, fallback: String) -> String {
    value
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .unwrap_or(fallback)
}

pub fn now_ts() -> i64 {
    Utc::now().timestamp()
}
