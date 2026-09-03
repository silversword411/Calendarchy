use std::collections::HashMap;

use chrono::Utc;
use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, PkceCodeChallenge,
    RedirectUrl, RefreshToken, RevocationUrl, Scope, StandardRevocableToken, TokenResponse,
    TokenUrl,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::keyring::OAuthTokens;

const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const GOOGLE_REVOKE_URL: &str = "https://oauth2.googleapis.com/revoke";

/// Google OAuth "Desktop app" client credentials, created in Google Cloud Console
/// (with the Calendar API enabled) before this module can be exercised end-to-end.
/// Per RFC 8252 the client secret for an installed app isn't treated as confidential
/// — PKCE is what actually secures the exchange (DESIGN_SPEC.md §7) — but Google's
/// console issues one for this client type regardless, and expects it on requests.
#[derive(Debug, Clone)]
pub struct GoogleOAuthConfig {
    pub client_id: String,
    pub client_secret: String,
}

impl GoogleOAuthConfig {
    /// Reads `CALENDARCHY_GOOGLE_CLIENT_ID` / `CALENDARCHY_GOOGLE_CLIENT_SECRET` from
    /// the environment. There's no built-in default — a real OAuth client has to be
    /// created first.
    pub fn from_env() -> anyhow::Result<Self> {
        let client_id = std::env::var("CALENDARCHY_GOOGLE_CLIENT_ID")
            .map_err(|_| anyhow::anyhow!("CALENDARCHY_GOOGLE_CLIENT_ID is not set"))?;
        let client_secret = std::env::var("CALENDARCHY_GOOGLE_CLIENT_SECRET")
            .map_err(|_| anyhow::anyhow!("CALENDARCHY_GOOGLE_CLIENT_SECRET is not set"))?;
        Ok(Self {
            client_id,
            client_secret,
        })
    }
}

fn http_client() -> anyhow::Result<oauth2::reqwest::Client> {
    // Not following redirects is required to avoid SSRF via a malicious token
    // endpoint response — see the oauth2 crate's own security notes.
    Ok(oauth2::reqwest::ClientBuilder::new()
        .redirect(oauth2::reqwest::redirect::Policy::none())
        .build()?)
}

/// Runs the full "Add Google Account" flow described in DESIGN_SPEC.md §7: binds an
/// ephemeral loopback port, sends the user to Google's consent screen in their system
/// browser, captures the redirect locally, and exchanges the resulting code (with
/// PKCE) for tokens. `scopes` should be exactly the OAuth scopes of the service(s)
/// being enabled at add-account time (see `Service::oauth_scopes`).
pub async fn run_loopback_oauth_flow(
    config: &GoogleOAuthConfig,
    scopes: &[&str],
) -> anyhow::Result<OAuthTokens> {
    // Bind before building the authorization URL, since the exact redirect_uri
    // (including the ephemeral port) must be known up front and matches what Google
    // sends the browser back to.
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}");

    let client = BasicClient::new(ClientId::new(config.client_id.clone()))
        .set_client_secret(ClientSecret::new(config.client_secret.clone()))
        .set_auth_uri(AuthUrl::new(GOOGLE_AUTH_URL.to_string())?)
        .set_token_uri(TokenUrl::new(GOOGLE_TOKEN_URL.to_string())?)
        .set_redirect_uri(RedirectUrl::new(redirect_uri)?)
        .set_revocation_url(RevocationUrl::new(GOOGLE_REVOKE_URL.to_string())?);

    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

    let mut auth_request = client
        .authorize_url(CsrfToken::new_random)
        .set_pkce_challenge(pkce_challenge)
        // Google only returns a refresh token on consent when both of these are set;
        // without them, re-connecting an already-authorized account silently omits it.
        .add_extra_param("access_type", "offline")
        .add_extra_param("prompt", "consent");
    for scope in scopes {
        auth_request = auth_request.add_scope(Scope::new((*scope).to_string()));
    }
    let (auth_url, csrf_token) = auth_request.url();

    tracing::info!(%auth_url, "opening system browser for Google OAuth consent");
    if let Err(err) = tokio::process::Command::new("xdg-open")
        .arg(auth_url.as_str())
        .spawn()
    {
        tracing::warn!(%err, %auth_url, "failed to launch browser automatically; open this URL manually");
    }

    let (code, returned_state) = accept_authorization_redirect(&listener).await?;
    if returned_state != *csrf_token.secret() {
        anyhow::bail!("OAuth state parameter mismatch (possible CSRF) — aborting");
    }

    let http_client = http_client()?;
    let token_result = client
        .exchange_code(AuthorizationCode::new(code))
        .set_pkce_verifier(pkce_verifier)
        .request_async(&http_client)
        .await
        .map_err(|e| anyhow::anyhow!("token exchange failed: {e}"))?;

    let refresh_token = token_result
        .refresh_token()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Google did not return a refresh token; revoke Calendarchy's access at \
                 https://myaccount.google.com/permissions and try adding the account again"
            )
        })?
        .secret()
        .clone();

    let expires_at = Utc::now()
        + token_result
            .expires_in()
            .and_then(|d| chrono::Duration::from_std(d).ok())
            .unwrap_or_else(|| chrono::Duration::hours(1));

    let granted_scopes = token_result
        .scopes()
        .map(|scopes| scopes.iter().map(|s| s.to_string()).collect())
        .unwrap_or_else(|| scopes.iter().map(|s| s.to_string()).collect());

    Ok(OAuthTokens {
        access_token: token_result.access_token().secret().clone(),
        refresh_token,
        expires_at,
        granted_scopes,
    })
}

/// Exchanges a refresh token for a new access token, keeping the same refresh token
/// (Google does not rotate it on a plain refresh). Called by the per-service sync
/// engine (DESIGN_SPEC.md §7/§9) whenever the cached access token is expired or close
/// to it.
pub async fn refresh_access_token(
    config: &GoogleOAuthConfig,
    current: &OAuthTokens,
) -> anyhow::Result<OAuthTokens> {
    let client = BasicClient::new(ClientId::new(config.client_id.clone()))
        .set_client_secret(ClientSecret::new(config.client_secret.clone()))
        .set_token_uri(TokenUrl::new(GOOGLE_TOKEN_URL.to_string())?);

    let http_client = http_client()?;
    let refresh_token = RefreshToken::new(current.refresh_token.clone());
    let token_result = client
        .exchange_refresh_token(&refresh_token)
        .request_async(&http_client)
        .await
        .map_err(|e| anyhow::anyhow!("token refresh failed: {e}"))?;

    let expires_at = Utc::now()
        + token_result
            .expires_in()
            .and_then(|d| chrono::Duration::from_std(d).ok())
            .unwrap_or_else(|| chrono::Duration::hours(1));

    Ok(OAuthTokens {
        access_token: token_result.access_token().secret().clone(),
        // Google typically omits refresh_token on a refresh response — keep the one
        // we already have rather than treating its absence as revocation.
        refresh_token: token_result
            .refresh_token()
            .map(|t| t.secret().clone())
            .unwrap_or_else(|| current.refresh_token.clone()),
        expires_at,
        granted_scopes: current.granted_scopes.clone(),
    })
}

/// Revokes a refresh token with Google (which also invalidates any access tokens
/// derived from it). Called when removing an account (DESIGN_SPEC.md §7).
pub async fn revoke_tokens(config: &GoogleOAuthConfig, tokens: &OAuthTokens) -> anyhow::Result<()> {
    let client = BasicClient::new(ClientId::new(config.client_id.clone()))
        .set_client_secret(ClientSecret::new(config.client_secret.clone()))
        .set_token_uri(TokenUrl::new(GOOGLE_TOKEN_URL.to_string())?)
        .set_revocation_url(RevocationUrl::new(GOOGLE_REVOKE_URL.to_string())?);

    let http_client = http_client()?;
    let revocable = StandardRevocableToken::RefreshToken(RefreshToken::new(tokens.refresh_token.clone()));
    client
        .revoke_token(revocable)?
        .request_async(&http_client)
        .await
        .map_err(|e| anyhow::anyhow!("token revocation failed: {e}"))?;
    Ok(())
}

/// Blocks until a browser redirect carrying `code`/`state` (or `error`) arrives on
/// the loopback listener, replies with a minimal "you can close this tab" page, and
/// returns the parsed `(code, state)`. Stray requests that don't match (e.g. a
/// browser's automatic favicon fetch) are answered and ignored rather than aborting
/// the flow.
async fn accept_authorization_redirect(listener: &TcpListener) -> anyhow::Result<(String, String)> {
    loop {
        let (mut stream, _) = listener.accept().await?;

        let mut buf = [0u8; 8192];
        let n = stream.read(&mut buf).await?;
        let request = String::from_utf8_lossy(&buf[..n]).into_owned();

        let Some(request_line) = request.lines().next() else {
            continue;
        };
        let Some(path_and_query) = request_line.split_whitespace().nth(1) else {
            continue;
        };

        respond_close_tab(&mut stream).await;

        let query = path_and_query.split_once('?').map(|(_, q)| q).unwrap_or("");
        let params = parse_query_string(query);

        if let Some(error) = params.get("error") {
            anyhow::bail!("Google OAuth authorization failed: {error}");
        }
        if let (Some(code), Some(state)) = (params.get("code"), params.get("state")) {
            return Ok((code.clone(), state.clone()));
        }
        // Not the redirect we're waiting for — keep listening for the real one.
    }
}

async fn respond_close_tab(stream: &mut tokio::net::TcpStream) {
    const BODY: &str =
        "<html><body><p>Calendarchy is connected. You can close this tab.</p></body></html>";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        BODY.len(),
        BODY
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn parse_query_string(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            Some((percent_decode(k), percent_decode(v)))
        })
        .collect()
}

/// Minimal `application/x-www-form-urlencoded` decoder — enough for the handful of
/// query parameters Google's redirect carries, without pulling in a URL crate just
/// for this one call site.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_authorization_redirect_query() {
        let params = parse_query_string("state=abc123&code=4%2F0AVG7fiA%3D&scope=email");
        assert_eq!(params.get("state").map(String::as_str), Some("abc123"));
        assert_eq!(params.get("code").map(String::as_str), Some("4/0AVG7fiA="));
        assert_eq!(params.get("scope").map(String::as_str), Some("email"));
    }

    #[test]
    fn parses_error_redirect_query() {
        let params = parse_query_string("error=access_denied&state=abc123");
        assert_eq!(params.get("error").map(String::as_str), Some("access_denied"));
    }

    /// Exercises the real loopback listener end-to-end (bind, accept, parse the GET
    /// request line, respond) by acting as the browser: connect and send exactly the
    /// kind of redirect request Google would send, without needing live credentials.
    #[tokio::test]
    async fn accepts_a_real_loopback_redirect_connection() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpStream;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();

        let client = tokio::spawn(async move {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
            stream
                .write_all(b"GET /?state=xyz&code=4%2Ftest HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
                .await
                .expect("write");
        });

        let (code, state) = accept_authorization_redirect(&listener).await.expect("accept");
        client.await.expect("client task");

        assert_eq!(code, "4/test");
        assert_eq!(state, "xyz");
    }
}
