use std::path::{Path, PathBuf};

use async_trait::async_trait;
use rmcp::transport::{AuthError, CredentialStore, StoredCredentials, auth::OAuthState};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::{
    AuthContext, AuthProvider, AuthStatus, AuthType, Error, McpConfig, McpServerConfig, Result,
};

#[derive(Debug, Clone)]
pub struct FileOAuthProvider {
    root: PathBuf,
}

impl FileOAuthProvider {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn store(&self, server: &str) -> FileCredentialStore {
        FileCredentialStore::new(self.root.join(format!("{server}.json")))
    }
}

impl AuthProvider for FileOAuthProvider {
    fn authorization(&self, context: &AuthContext<'_>) -> Result<Option<String>> {
        let path = self.store(context.server).path;
        let value = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes).map_err(|error| {
                Error::OAuth {
                    server: context.server.to_string(),
                    message: format!("parse {}: {error}", path.display()),
                }
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(Error::OAuth {
                    server: context.server.to_string(),
                    message: format!("read {}: {error}", path.display()),
                });
            }
        };
        Ok(value
            .pointer("/token_response/access_token")
            .and_then(serde_json::Value::as_str)
            .map(|token| format!("Bearer {token}")))
    }
}

#[derive(Debug, Clone)]
struct FileCredentialStore {
    path: PathBuf,
}

impl FileCredentialStore {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

#[async_trait]
impl CredentialStore for FileCredentialStore {
    async fn load(&self) -> std::result::Result<Option<StoredCredentials>, AuthError> {
        match tokio::fs::read(&self.path).await {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|error| AuthError::InternalError(error.to_string())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(AuthError::InternalError(error.to_string())),
        }
    }

    async fn save(&self, credentials: StoredCredentials) -> std::result::Result<(), AuthError> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| AuthError::InternalError(error.to_string()))?;
        }
        let bytes = serde_json::to_vec(&credentials)
            .map_err(|error| AuthError::InternalError(error.to_string()))?;
        tokio::fs::write(&self.path, bytes)
            .await
            .map_err(|error| AuthError::InternalError(error.to_string()))?;
        set_private_permissions(&self.path)
            .await
            .map_err(|error| AuthError::InternalError(error.to_string()))
    }

    async fn clear(&self) -> std::result::Result<(), AuthError> {
        match tokio::fs::remove_file(&self.path).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(AuthError::InternalError(error.to_string())),
        }
    }
}

pub fn oauth_status(config: &McpConfig, server: &str, root: &Path) -> Result<AuthStatus> {
    let Some((url, auth)) = oauth_server(config, server)? else {
        return Ok(AuthStatus::NotRequired);
    };
    let provider = FileOAuthProvider::new(root);
    Ok(
        if provider
            .authorization(&AuthContext { server, url, auth })?
            .is_some()
        {
            AuthStatus::Ready
        } else {
            AuthStatus::Required
        },
    )
}

pub async fn oauth_login(config: &McpConfig, server: &str, root: &Path) -> Result<()> {
    let Some((url, auth)) = oauth_server(config, server)? else {
        return Err(Error::OAuth {
            server: server.to_string(),
            message: "server does not use interactive OAuth".to_string(),
        });
    };
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| oauth_error(server, error))?;
    let address = listener
        .local_addr()
        .map_err(|error| oauth_error(server, error))?;
    let redirect_uri = format!("http://{address}/callback");
    let mut state = OAuthState::new(url, None)
        .await
        .map_err(|error| oauth_error(server, error))?;
    if let OAuthState::Unauthorized(manager) = &mut state {
        manager.set_credential_store(FileCredentialStore::new(
            root.join(format!("{server}.json")),
        ));
    }
    let scopes = auth.scopes.iter().map(String::as_str).collect::<Vec<_>>();
    state
        .start_authorization(&scopes, &redirect_uri, Some("dwoagent"))
        .await
        .map_err(|error| oauth_error(server, error))?;
    let authorization_url = state
        .get_authorization_url()
        .await
        .map_err(|error| oauth_error(server, error))?;
    open_browser(&authorization_url).map_err(|error| oauth_error(server, error))?;
    let callback_url = wait_for_callback(listener, &redirect_uri)
        .await
        .map_err(|error| oauth_error(server, error))?;
    state
        .handle_callback_url(&callback_url)
        .await
        .map_err(|error| oauth_error(server, error))
}

pub async fn oauth_logout(config: &McpConfig, server: &str, root: &Path) -> Result<()> {
    if oauth_server(config, server)?.is_none() {
        return Err(Error::OAuth {
            server: server.to_string(),
            message: "server does not use interactive OAuth".to_string(),
        });
    }
    FileCredentialStore::new(root.join(format!("{server}.json")))
        .clear()
        .await
        .map_err(|error| oauth_error(server, error))
}

pub(crate) async fn authorized_http_client(
    server: &str,
    url: &str,
    root: &Path,
) -> Result<rmcp::transport::AuthClient<reqwest::Client>> {
    let mut manager = rmcp::transport::AuthorizationManager::new(url)
        .await
        .map_err(|error| oauth_error(server, error))?;
    manager.set_credential_store(FileCredentialStore::new(
        root.join(format!("{server}.json")),
    ));
    if !manager
        .initialize_from_store()
        .await
        .map_err(|error| oauth_error(server, error))?
    {
        return Err(Error::AuthRequired {
            server: server.to_string(),
        });
    }
    Ok(rmcp::transport::AuthClient::new(
        reqwest::Client::new(),
        manager,
    ))
}

fn oauth_server<'a>(
    config: &'a McpConfig,
    server: &str,
) -> Result<Option<(&'a str, &'a crate::AuthConfig)>> {
    let server_config = config
        .servers
        .get(server)
        .ok_or_else(|| Error::UnknownServer(server.to_string()))?;
    let McpServerConfig::StreamableHttp(http) = server_config else {
        return Ok(None);
    };
    Ok(http
        .auth
        .as_ref()
        .filter(|auth| auth.auth_type == AuthType::OAuth)
        .map(|auth| (http.url.as_str(), auth)))
}

async fn wait_for_callback(listener: TcpListener, redirect_uri: &str) -> std::io::Result<String> {
    let (mut stream, _) =
        tokio::time::timeout(std::time::Duration::from_secs(300), listener.accept())
            .await
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "OAuth callback timed out")
            })??;
    let mut buffer = vec![0_u8; 16 * 1024];
    let length = stream.read(&mut buffer).await?;
    let request = String::from_utf8_lossy(&buffer[..length]);
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid OAuth callback")
        })?;
    let body = "Authorization completed. You may close this tab.";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    let base = redirect_uri
        .split("/callback")
        .next()
        .unwrap_or(redirect_uri);
    Ok(format!("{base}{target}"))
}

fn open_browser(url: &str) -> std::io::Result<()> {
    #[cfg(windows)]
    let mut command = {
        let mut command = std::process::Command::new("rundll32.exe");
        command.args(["url.dll,FileProtocolHandler", url]);
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = std::process::Command::new("open");
        command.arg(url);
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(url);
        command
    };
    command.spawn().map(|_| ())
}

fn oauth_error(server: &str, error: impl std::fmt::Display) -> Error {
    Error::OAuth {
        server: server.to_string(),
        message: error.to_string(),
    }
}

#[cfg(unix)]
async fn set_private_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await
}

#[cfg(not(unix))]
async fn set_private_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}
