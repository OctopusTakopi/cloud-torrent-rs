use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum AppError {
    #[error("Internal Server Error: {0}")]
    Internal(#[from] anyhow::Error),
    #[error("Not Found: {0}")]
    NotFound(String),
    #[error("Insufficient Storage: {0}")]
    InsufficientStorage(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, error_message) = match self {
            AppError::Internal(ref e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            AppError::NotFound(ref e) => (StatusCode::NOT_FOUND, e.clone()),
            AppError::InsufficientStorage(ref e) => (StatusCode::INSUFFICIENT_STORAGE, e.clone()),
        };

        let body = Json(serde_json::json!({
            "error": error_message,
        }));

        (status, body).into_response()
    }
}

pub type AppResult<T> = Result<T, AppError>;

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct FileNode {
    pub name: String,
    pub size: u64,
    pub modified: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<FileNode>>,
}

#[derive(Deserialize)]
pub struct SearchQuery {
    pub query: String,
    pub provider: Option<String>,
}

#[derive(Deserialize)]
pub struct MagnetQuery {
    pub m: String,
}

#[derive(Deserialize, Default)]
pub struct RssQuery {
    #[serde(default)]
    pub refresh: bool,
}

#[derive(Deserialize)]
pub struct RssLoadRequest {
    pub item_id: String,
    pub load_url: String,
}
