//! OAuth 2.0 Authorization Code + PKCE (`S256`) loopback authenticator and token refresh.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::Utc;
use rand::Rng;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sn_core::{InstanceProfile, SnError, SnResult};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use url::Url;

use crate::keychain::{CredentialStore, StoredCredentials};

/// RFC 7636 unreserved URL-safe characters for PKCE code verifiers.
const PKCE_CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";

/// Default browser callback wait timeout used by [`OAuthPkceFlow::authorize_via_browser`].
const DEFAULT_CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

/// PKCE (`S256`) verifier/challenge pair plus random CSRF `state`.
#[derive(Debug, Clone)]
pub struct PkceChallenge {
    pub code_verifier: String,
    pub code_challenge: String,
    pub state: String,
}

impl PkceChallenge {
    /// Generates a fresh 64-character URL-safe `code_verifier`, its `S256` `code_challenge`,
    /// and a 32-character CSRF `state` parameter.
    pub fn generate() -> Self {
        let mut rng = rand::thread_rng();
        let code_verifier: String = (0..64)
            .map(|_| {
                let idx = rng.gen_range(0..PKCE_CHARSET.len());
                PKCE_CHARSET[idx] as char
            })
            .collect();
        let state: String = (0..32)
            .map(|_| {
                let idx = rng.gen_range(0..62);
                PKCE_CHARSET[idx] as char
            })
            .collect();
        let code_challenge = Self::compute_s256_challenge(&code_verifier);
        Self {
            code_verifier,
            code_challenge,
            state,
        }
    }

    /// Computes `BASE64URL_NO_PAD(SHA256(code_verifier))`.
    pub fn compute_s256_challenge(code_verifier: &str) -> String {
        let digest = Sha256::digest(code_verifier.as_bytes());
        URL_SAFE_NO_PAD.encode(digest)
    }
}

/// Active loopback OAuth 2.0 PKCE listener awaiting the browser redirect.
///
/// Splitting listener startup from callback completion allows the GUI to display
/// [`PendingOAuthSession::authorize_url`] (for manual copy/paste) and wire a Cancel button.
pub struct PendingOAuthSession {
    pub authorize_url: String,
    pub redirect_uri: String,
    pub callback_port: u16,
    pub state: String,
    listener: TcpListener,
    profile: InstanceProfile,
    pkce: PkceChallenge,
    client_secret: Option<String>,
    cred_store: Arc<dyn CredentialStore>,
    cancel_rx: Option<watch::Receiver<bool>>,
    http: reqwest::Client,
}

impl std::fmt::Debug for PendingOAuthSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingOAuthSession")
            .field("authorize_url", &self.authorize_url)
            .field("redirect_uri", &self.redirect_uri)
            .field("callback_port", &self.callback_port)
            .finish()
    }
}

impl PendingOAuthSession {
    /// Waits for the `/oauth/callback` redirect, validates `state`, exchanges the authorization
    /// `code` + `code_verifier` at `/oauth_token.do`, and persists [`StoredCredentials`].
    pub async fn wait_for_callback(mut self, timeout: Duration) -> SnResult<StoredCredentials> {
        if let Some(ref rx) = self.cancel_rx
            && *rx.borrow()
        {
            return Err(SnError::OAuth("OAuth login cancelled".to_string()));
        }

        let code = tokio::select! {
            res = accept_callback_loop(&self.listener, &self.pkce.state) => res?,
            _ = tokio::time::sleep(timeout) => {
                return Err(SnError::OAuth(format!(
                    "Timed out after {}s waiting for ServiceNow OAuth callback on {}",
                    timeout.as_secs(),
                    self.redirect_uri
                )));
            }
            _ = wait_for_cancel(&mut self.cancel_rx) => {
                return Err(SnError::OAuth("OAuth login cancelled by user".to_string()));
            }
        };

        let creds = exchange_authorization_code(
            &self.http,
            &self.profile,
            &self.redirect_uri,
            &code,
            &self.pkce.code_verifier,
            self.client_secret.clone(),
        )
        .await?;

        self.cred_store.save_credentials(&self.profile.id, &creds)?;
        Ok(creds)
    }
}

/// ServiceNow OAuth 2.0 Authorization Code + PKCE coordinator.
pub struct OAuthPkceFlow;

impl OAuthPkceFlow {
    /// Builds the ServiceNow `/oauth_auth.do` URL using `profile.redirect_uri()`.
    pub fn build_authorize_url(
        profile: &InstanceProfile,
        pkce: &PkceChallenge,
    ) -> SnResult<String> {
        Self::build_authorize_url_with_redirect(profile, &profile.redirect_uri(), pkce)
    }

    /// Builds the ServiceNow `/oauth_auth.do` URL with an explicit `redirect_uri`.
    pub fn build_authorize_url_with_redirect(
        profile: &InstanceProfile,
        redirect_uri: &str,
        pkce: &PkceChallenge,
    ) -> SnResult<String> {
        if profile.client_id.trim().is_empty() {
            return Err(SnError::InvalidConfig(
                "OAuth client_id cannot be empty".to_string(),
            ));
        }
        let mut url = Url::parse(&profile.authorize_endpoint()).map_err(|err| {
            SnError::InvalidConfig(format!("Invalid OAuth authorize endpoint: {err}"))
        })?;
        {
            let mut qp = url.query_pairs_mut();
            qp.append_pair("response_type", "code");
            qp.append_pair("client_id", profile.client_id.trim());
            qp.append_pair("redirect_uri", redirect_uri);
            qp.append_pair("state", &pkce.state);
            qp.append_pair("code_challenge", &pkce.code_challenge);
            qp.append_pair("code_challenge_method", "S256");
            let scopes = profile.scopes.trim();
            if !scopes.is_empty() {
                qp.append_pair("scope", scopes);
            }
        }
        Ok(url.to_string())
    }

    /// Binds `127.0.0.1:{profile.callback_port}`, optionally launches the default macOS browser,
    /// and returns a [`PendingOAuthSession`] ready to await the redirect.
    pub async fn start_pkce_listener(
        profile: &InstanceProfile,
        client_secret: Option<String>,
        cred_store: Arc<dyn CredentialStore>,
        open_browser: bool,
        cancel_rx: Option<watch::Receiver<bool>>,
    ) -> SnResult<PendingOAuthSession> {
        let bind_addr = ("127.0.0.1", profile.callback_port);
        let listener = TcpListener::bind(bind_addr).await.map_err(|err| {
            SnError::OAuth(format!(
                "Cannot bind OAuth loopback listener on 127.0.0.1:{} (port may be in use): {err}",
                profile.callback_port
            ))
        })?;

        let actual_port = listener
            .local_addr()
            .map_err(|err| {
                SnError::OAuth(format!(
                    "Failed reading local OAuth listener address: {err}"
                ))
            })?
            .port();

        let redirect_uri = format!("http://127.0.0.1:{actual_port}/oauth/callback");
        let pkce = PkceChallenge::generate();
        let authorize_url = Self::build_authorize_url_with_redirect(profile, &redirect_uri, &pkce)?;

        let resolved_secret = client_secret
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| cred_store.get_client_secret(&profile.id).ok().flatten());

        tracing::info!(
            instance = %profile.name,
            callback_port = actual_port,
            authorize_url = %authorize_url,
            "Started OAuth 2.0 PKCE loopback session"
        );

        if open_browser && let Err(err) = open::that(&authorize_url) {
            tracing::warn!(
                error = %err,
                authorize_url = %authorize_url,
                "Failed opening default browser automatically; user can open URL manually"
            );
        }

        let http = reqwest::Client::builder()
            .tcp_nodelay(true)
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|err| SnError::Network(format!("Failed building OAuth HTTP client: {err}")))?;

        Ok(PendingOAuthSession {
            authorize_url,
            redirect_uri,
            callback_port: actual_port,
            state: pkce.state.clone(),
            listener,
            profile: profile.clone(),
            pkce,
            client_secret: resolved_secret,
            cred_store,
            cancel_rx,
            http,
        })
    }

    /// Convenience end-to-end helper that starts the loopback PKCE listener, optionally opens
    /// the browser, waits for the callback, exchanges the code, and returns `(authorize_url, creds)`.
    pub async fn authorize_via_browser(
        profile: &InstanceProfile,
        client_secret: Option<String>,
        cred_store: Arc<dyn CredentialStore>,
        open_browser: bool,
        cancel_rx: Option<watch::Receiver<bool>>,
    ) -> SnResult<(String, StoredCredentials)> {
        let session =
            Self::start_pkce_listener(profile, client_secret, cred_store, open_browser, cancel_rx)
                .await?;
        let auth_url = session.authorize_url.clone();
        let creds = session.wait_for_callback(DEFAULT_CALLBACK_TIMEOUT).await?;
        Ok((auth_url, creds))
    }

    /// Exchanges a `refresh_token` for a fresh access token via `POST {base_url}/oauth_token.do`.
    pub async fn refresh_access_token(
        http: &reqwest::Client,
        profile: &InstanceProfile,
        current: &StoredCredentials,
    ) -> SnResult<StoredCredentials> {
        refresh_access_token(http, profile, current).await
    }
}

/// Exchanges a `refresh_token` for a fresh access token via `POST {base_url}/oauth_token.do`.
///
/// Preserves `current.refresh_token` and `current.client_secret` when ServiceNow omits
/// a rotated `refresh_token` in the refresh response.
pub async fn refresh_access_token(
    http: &reqwest::Client,
    profile: &InstanceProfile,
    current: &StoredCredentials,
) -> SnResult<StoredCredentials> {
    let refresh_tok = current
        .refresh_token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            SnError::OAuth(format!(
                "No refresh_token available for instance '{}'",
                profile.name
            ))
        })?;

    let mut form: Vec<(&str, &str)> = vec![
        ("grant_type", "refresh_token"),
        ("client_id", profile.client_id.trim()),
        ("refresh_token", refresh_tok),
    ];
    if let Some(secret) = current.client_secret.as_deref().filter(|s| !s.is_empty()) {
        form.push(("client_secret", secret));
    }

    let token_resp = post_token_form(http, &profile.token_endpoint(), &form).await?;
    let expires_at = token_resp
        .expires_in
        .map(|secs| Utc::now() + chrono::Duration::seconds(secs));

    let merged_refresh = token_resp
        .refresh_token
        .filter(|s| !s.trim().is_empty())
        .or_else(|| current.refresh_token.clone());

    Ok(StoredCredentials {
        access_token: token_resp.access_token,
        refresh_token: merged_refresh,
        expires_at,
        client_secret: current.client_secret.clone(),
    })
}

async fn exchange_authorization_code(
    http: &reqwest::Client,
    profile: &InstanceProfile,
    redirect_uri: &str,
    code: &str,
    code_verifier: &str,
    client_secret: Option<String>,
) -> SnResult<StoredCredentials> {
    let mut form: Vec<(&str, &str)> = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", profile.client_id.trim()),
        ("code_verifier", code_verifier),
    ];
    if let Some(secret) = client_secret.as_deref().filter(|s| !s.is_empty()) {
        form.push(("client_secret", secret));
    }

    let token_resp = post_token_form(http, &profile.token_endpoint(), &form).await?;
    let expires_at = token_resp
        .expires_in
        .map(|secs| Utc::now() + chrono::Duration::seconds(secs));

    Ok(StoredCredentials {
        access_token: token_resp.access_token,
        refresh_token: token_resp.refresh_token.filter(|s| !s.trim().is_empty()),
        expires_at,
        client_secret: client_secret.filter(|s| !s.trim().is_empty()),
    })
}

#[derive(Debug, Deserialize)]
struct RawTokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_secs")]
    expires_in: Option<i64>,
    error: Option<String>,
    error_description: Option<String>,
}

struct ValidatedTokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
}

fn deserialize_optional_secs<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: Option<serde_json::Value> = Option::deserialize(deserializer)?;
    let Some(val) = raw else {
        return Ok(None);
    };
    match val {
        serde_json::Value::Number(n) => Ok(n.as_i64()),
        serde_json::Value::String(s) => Ok(s.trim().parse::<i64>().ok()),
        _ => Ok(None),
    }
}

async fn post_token_form(
    http: &reqwest::Client,
    token_url: &str,
    form: &[(&str, &str)],
) -> SnResult<ValidatedTokenResponse> {
    let resp = http
        .post(token_url)
        .header(reqwest::header::ACCEPT, "application/json")
        .form(form)
        .send()
        .await
        .map_err(|err| SnError::Network(format!("OAuth token request failed: {err}")))?;

    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|err| SnError::Network(format!("Failed reading OAuth token response: {err}")))?;

    let parsed: RawTokenResponse = serde_json::from_str(&body).map_err(|err| {
        SnError::OAuth(format!(
            "Invalid JSON from OAuth token endpoint (HTTP {}): {err}",
            status.as_u16()
        ))
    })?;

    if !status.is_success() || parsed.error.is_some() {
        let err_code = parsed
            .error
            .unwrap_or_else(|| format!("HTTP {}", status.as_u16()));
        let desc = parsed
            .error_description
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "OAuth token request rejected by ServiceNow".to_string());
        return Err(SnError::OAuth(format!("{err_code}: {desc}")));
    }

    let access_token = parsed
        .access_token
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            SnError::OAuth("ServiceNow OAuth response missing access_token".to_string())
        })?;

    Ok(ValidatedTokenResponse {
        access_token,
        refresh_token: parsed.refresh_token,
        expires_in: parsed.expires_in,
    })
}

async fn wait_for_cancel(cancel_rx: &mut Option<watch::Receiver<bool>>) {
    match cancel_rx {
        Some(rx) => {
            while !*rx.borrow_and_update() {
                if rx.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
            }
        }
        None => std::future::pending::<()>().await,
    }
}

async fn accept_callback_loop(listener: &TcpListener, expected_state: &str) -> SnResult<String> {
    loop {
        let (mut stream, _) = listener.accept().await.map_err(|err| {
            SnError::OAuth(format!("Failed accepting OAuth loopback connection: {err}"))
        })?;

        let Some(target) = read_http_request_target(&mut stream).await? else {
            continue;
        };

        let parsed_url = Url::parse(&format!("http://127.0.0.1{target}")).map_err(|err| {
            SnError::OAuth(format!(
                "Malformed OAuth callback request URI '{target}': {err}"
            ))
        })?;

        if parsed_url.path() != "/oauth/callback" {
            let _ = write_html_response(&mut stream, "404 Not Found", "Not Found", false).await;
            continue;
        }

        let params: HashMap<String, String> = parsed_url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();

        if let Some(oauth_err) = params.get("error") {
            let desc = params
                .get("error_description")
                .cloned()
                .unwrap_or_else(|| "Authorization denied by ServiceNow".to_string());
            let msg = format!("ServiceNow OAuth error ({oauth_err}): {desc}");
            let _ = write_html_response(&mut stream, "400 Bad Request", &msg, false).await;
            return Err(SnError::OAuth(msg));
        }

        let returned_state = params.get("state").map(String::as_str).unwrap_or("");
        if returned_state != expected_state {
            let msg =
                "OAuth CSRF state validation failed \u{2014} please retry sign-in from snrust";
            let _ = write_html_response(&mut stream, "400 Bad Request", msg, false).await;
            return Err(SnError::OAuth("OAuth CSRF state mismatch".to_string()));
        }

        let Some(code) = params.get("code").filter(|c| !c.trim().is_empty()).cloned() else {
            let msg = "OAuth callback did not include an authorization code";
            let _ = write_html_response(&mut stream, "400 Bad Request", msg, false).await;
            return Err(SnError::OAuth(msg.to_string()));
        };

        let success_headline =
            "Authenticated with ServiceNow — you may close this tab and return to snrust";
        let _ = write_html_response(&mut stream, "200 OK", success_headline, true).await;
        return Ok(code);
    }
}

async fn read_http_request_target(stream: &mut TcpStream) -> SnResult<Option<String>> {
    let mut buf = vec![0u8; 8192];
    let mut total = 0usize;
    loop {
        let n = stream.read(&mut buf[total..]).await.map_err(|err| {
            SnError::OAuth(format!("Failed reading OAuth callback socket: {err}"))
        })?;
        if n == 0 {
            break;
        }
        total += n;
        let slice = &buf[..total];
        if slice.windows(4).any(|w| w == b"\r\n\r\n")
            || slice.windows(2).any(|w| w == b"\n\n")
            || total == buf.len()
        {
            break;
        }
    }
    if total == 0 {
        return Ok(None);
    }
    let req_text = String::from_utf8_lossy(&buf[..total]);
    let first_line = req_text.lines().next().unwrap_or("");
    let mut parts = first_line.split_whitespace();
    let _method = parts.next();
    Ok(parts.next().map(str::to_string))
}

async fn write_html_response(
    stream: &mut TcpStream,
    status: &str,
    message: &str,
    is_success: bool,
) -> std::io::Result<()> {
    let accent = if is_success { "#34D399" } else { "#F87171" };
    let badge = if is_success {
        "Connected"
    } else {
        "Sign-In Failed"
    };
    let escaped = html_escape(message);
    let html = format!(
        "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>snrust — ServiceNow OAuth</title>\
<style>\
body{{margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;\
background:#0B1120;color:#F3F4F6;font-family:-apple-system,BlinkMacSystemFont,'SF Pro Text',sans-serif;}}\
.card{{max-width:460px;padding:32px 36px;border-radius:14px;background:#111827;\
border:1px solid #1F2937;box-shadow:0 20px 50px rgba(0,0,0,.45);text-align:center;}}\
.pill{{display:inline-block;padding:4px 12px;border-radius:999px;font-size:12px;\
font-weight:600;letter-spacing:.04em;text-transform:uppercase;color:{accent};\
background:rgba(255,255,255,.05);border:1px solid {accent};margin-bottom:16px;}}\
h1{{font-size:18px;line-height:1.45;margin:0;font-weight:600;color:#F9FAFB;}}\
</style></head><body><div class=\"card\"><div class=\"pill\">{badge}</div>\
<h1>{escaped}</h1></div></body></html>"
    );
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{html}",
        html.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    let _ = stream.shutdown().await;
    Ok(())
}

fn html_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
