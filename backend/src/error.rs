use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use tracing::error;

pub type AppResult<T> = Result<T, AppError>;

#[derive(Debug)]
pub struct AppError {
    pub status: StatusCode,
    pub code: String,
    pub message: String,
    audit_handled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AppErrorAuditInfo {
    pub code: String,
    pub message: String,
    pub handled: bool,
}

impl AppError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            code: status
                .canonical_reason()
                .unwrap_or("operation_failed")
                .to_ascii_lowercase()
                .replace(' ', "_"),
            message: message.into(),
            audit_handled: false,
        }
    }

    pub fn with_code(
        status: StatusCode,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            status,
            code: code.into(),
            message: message.into(),
            audit_handled: false,
        }
    }

    pub fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "需要管理员登录")
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn audit_handled(mut self) -> Self {
        self.audit_handled = true;
        self
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let audit_info = AppErrorAuditInfo {
            code: self.code.clone(),
            message: self.message.clone(),
            handled: self.audit_handled,
        };
        let mut response = (
            self.status,
            Json(ErrorResponse {
                code: self.code,
                message: self.message,
            }),
        )
            .into_response();
        response.extensions_mut().insert(audit_info);
        response
    }
}

impl From<sqlx::Error> for AppError {
    fn from(error: sqlx::Error) -> Self {
        error!(?error, "database operation failed");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "数据库操作失败")
    }
}

impl From<anyhow::Error> for AppError {
    fn from(error: anyhow::Error) -> Self {
        error!(?error, "operation failed");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "服务内部错误")
    }
}

impl From<std::io::Error> for AppError {
    fn from(error: std::io::Error) -> Self {
        error!(?error, "filesystem operation failed");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "文件操作失败")
    }
}

#[derive(Serialize)]
struct ErrorResponse {
    code: String,
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_preserves_stable_audit_error_details() {
        let response = AppError::with_code(
            StatusCode::FORBIDDEN,
            "permission_denied",
            "当前管理员无权执行此操作",
        )
        .audit_handled()
        .into_response();

        let info = response
            .extensions()
            .get::<AppErrorAuditInfo>()
            .expect("audit error info");
        assert_eq!(info.code, "permission_denied");
        assert_eq!(info.message, "当前管理员无权执行此操作");
        assert!(info.handled);
    }
}
