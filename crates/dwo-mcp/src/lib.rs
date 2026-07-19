//! Host-managed MCP discovery and tool execution.

mod auth;
mod catalog;
mod client;
mod config;
mod render;
mod runtime;

pub use auth::{FileOAuthProvider, oauth_login, oauth_logout};
pub use catalog::{
    Catalog, CatalogCache, CatalogServer, CatalogTool, SearchGroup, SearchTool, ServerStatus,
    ToolRef, read_catalog, read_catalog_cache, write_catalog, write_catalog_cache,
};
pub use client::{AuthContext, AuthProvider, AuthStatus, CallResult, McpClient, NoAuthProvider};
pub use config::{
    AuthConfig, AuthType, McpConfig, McpServerConfig, StdioConfig, StreamableHttpConfig,
};
pub use render::{render_list, render_search};
pub use runtime::McpRuntime;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("failed to read {path}: {source}")]
    ReadConfig {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error("invalid MCP config: {0}")]
    InvalidConfig(String),
    #[error("environment variable {0} is not set")]
    MissingEnvironment(String),
    #[error("unknown MCP server: {0}")]
    UnknownServer(String),
    #[error("invalid selector {0:?}; expected server.tool")]
    InvalidSelector(String),
    #[error("OAuth authorization is required for server {server}")]
    AuthRequired { server: String },
    #[error("OAuth operation failed for server {server}: {message}")]
    OAuth { server: String, message: String },
    #[error("MCP operation failed for server {server}: {message}")]
    Operation { server: String, message: String },
    #[error("catalog I/O failed: {0}")]
    CatalogIo(#[from] std::io::Error),
    #[error("catalog JSON failed: {0}")]
    CatalogJson(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
