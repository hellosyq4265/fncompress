//! The one error shape of the server (ADR 0002):
//! `{ "error": { "code": "snake_case", "message": "中文", "details": {...} } }`,
//! with the HTTP status giving the category.

use serde::Serialize;
use serde_json::Value;
use utoipa::ToSchema;

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AppError {
    #[serde(skip)]
    pub status: u16,
    pub code: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Value::is_null")]
    #[schema(value_type = Object)]
    pub details: Value,
}

pub type AppResult<T> = Result<T, AppError>;

impl AppError {
    pub fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            details: Value::Null,
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = details;
        self
    }

    pub fn bad_request(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(400, code, message)
    }

    pub fn not_found(what: &str) -> Self {
        Self::new(404, "not_found", format!("{what}不存在"))
    }

    pub fn conflict(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(409, code, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(500, "internal", message)
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for AppError {}

impl From<rusqlite::Error> for AppError {
    fn from(e: rusqlite::Error) -> Self {
        tracing::error!("database error: {e}");
        Self::new(500, "database", format!("数据库错误：{e}"))
    }
}
