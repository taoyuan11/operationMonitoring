use axum::http::{HeaderMap, header};

use crate::{
    error::{AppError, AppResult},
    state::AppState,
    utils::now_ts,
};

pub const SESSION_COOKIE: &str = "om_session";

pub async fn require_admin(state: &AppState, headers: &HeaderMap) -> AppResult<()> {
    let Some(token) = session_token(headers) else {
        return Err(AppError::unauthorized());
    };
    let now = now_ts();
    let mut sessions = state.sessions.write().await;
    sessions.retain(|_, expires_at| *expires_at > now);
    if sessions.contains_key(&token) {
        Ok(())
    } else {
        Err(AppError::unauthorized())
    }
}

pub fn session_token(headers: &HeaderMap) -> Option<String> {
    let cookie = headers.get(header::COOKIE)?.to_str().ok()?;
    cookie.split(';').find_map(|part| {
        let mut pair = part.trim().splitn(2, '=');
        let name = pair.next()?;
        let value = pair.next()?;
        (name == SESSION_COOKIE).then(|| value.to_string())
    })
}
