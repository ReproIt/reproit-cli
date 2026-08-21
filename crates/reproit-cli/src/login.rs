use std::{
    io::{BufRead as _, BufReader, Read as _, Write as _},
    net::TcpListener,
    thread,
    time::{Duration, Instant},
};

use oauth2::{
    AuthUrl, ClientId, CsrfToken, EndpointNotSet, EndpointSet, PkceCodeChallenge, PkceCodeVerifier,
    RedirectUrl, Scope, TokenUrl, basic::BasicClient,
};
use reproit_core::{Error, ErrorCode, identity::Digest};
use secrecy::{ExposeSecret as _, SecretString};
use serde::Deserialize;

const CALLBACK: &str = "http://127.0.0.1:8765/auth/callback";
const CALLBACK_ADDRESS: &str = "127.0.0.1:8765";
const MAX_CALLBACK_LINE_BYTES: usize = 8_192;
const MAX_LOGIN_TIMEOUT: Duration = Duration::from_mins(5);
const KEYRING_SERVICE: &str = "com.reproit.cli";
const KEYRING_ACCOUNT: &str = "reproit-session";
const MAX_METADATA_BYTES: usize = 65_536;
const OAUTH_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const OAUTH_READ_TIMEOUT: Duration = Duration::from_secs(30);
const OAUTH_TOTAL_TIMEOUT: Duration = Duration::from_mins(1);

type PublicClient =
    BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>;

pub struct LoginAttempt {
    authorization_url: String,
    listener: TcpListener,
    pkce_verifier: PkceCodeVerifier,
    state: CsrfToken,
}

pub struct AuthorizationResult {
    code: String,
    pkce_verifier: PkceCodeVerifier,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
    id_token: String,
    refresh_token: Option<String>,
    token_type: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct OAuthMetadata {
    authorization_endpoint: String,
    code_challenge_methods_supported: Vec<String>,
    grant_types_supported: Vec<String>,
    issuer: String,
    response_types_supported: Vec<String>,
    token_endpoint: String,
    token_endpoint_auth_methods_supported: Vec<String>,
}

pub async fn discover_oauth_metadata(authority: &str) -> Result<OAuthMetadata, Error> {
    validate_configuration(authority, "metadata-validation")?;
    let client = oauth_http_client()?;
    let mut response = client
        .get(format!(
            "{}/.well-known/oauth-authorization-server",
            authority.trim_end_matches('/')
        ))
        .send()
        .await
        .map_err(|_| callback_unavailable())?;
    if !response.status().is_success() {
        return Err(callback_unavailable());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| callback_unavailable())? {
        if bytes.len().saturating_add(chunk.len()) > MAX_METADATA_BYTES {
            return Err(config_invalid());
        }
        bytes.extend_from_slice(&chunk);
    }
    let metadata: OAuthMetadata = serde_json::from_slice(&bytes).map_err(|_| config_invalid())?;
    metadata.validate(authority)?;
    Ok(metadata)
}

impl OAuthMetadata {
    fn validate(&self, authority: &str) -> Result<(), Error> {
        let base = authority.trim_end_matches('/');
        if self.issuer.trim_end_matches('/') != base
            || self.authorization_endpoint != format!("{base}/oauth2/authorize")
            || self.token_endpoint != format!("{base}/oauth2/token")
            || !contains_exact(&self.code_challenge_methods_supported, "S256")
            || !contains_exact(&self.grant_types_supported, "authorization_code")
            || !contains_exact(&self.response_types_supported, "code")
            || !contains_exact(&self.token_endpoint_auth_methods_supported, "none")
        {
            return Err(config_invalid());
        }
        validate_endpoint(&self.authorization_endpoint)?;
        validate_endpoint(&self.token_endpoint)
    }

    fn authorization_endpoint(&self) -> &str {
        &self.authorization_endpoint
    }

    fn token_endpoint(&self) -> &str {
        &self.token_endpoint
    }
}

impl AuthorizationResult {
    pub fn code(&self) -> &str {
        &self.code
    }

    pub fn pkce_verifier(&self) -> &str {
        self.pkce_verifier.secret()
    }
}

pub async fn exchange_authorization_code(
    metadata: &OAuthMetadata,
    client_id: &str,
    result: &AuthorizationResult,
) -> Result<SecretString, Error> {
    validate_configuration(&metadata.issuer, client_id)?;
    let client = oauth_http_client()?;
    let mut response = client
        .post(metadata.token_endpoint())
        .form(&[
            ("client_id", client_id),
            ("grant_type", "authorization_code"),
            ("code", result.code()),
            ("redirect_uri", CALLBACK),
            ("code_verifier", result.pkce_verifier()),
        ])
        .send()
        .await
        .map_err(|_| callback_unavailable())?;
    if !response.status().is_success() {
        return Err(authentication_required());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| callback_unavailable())? {
        if bytes.len().saturating_add(chunk.len()) > 65_536 {
            return Err(callback_invalid());
        }
        bytes.extend_from_slice(&chunk);
    }
    let response: TokenResponse = serde_json::from_slice(&bytes).map_err(|_| callback_invalid())?;
    if !response.token_type.eq_ignore_ascii_case("bearer")
        || response.access_token.is_empty()
        || response.access_token.len() > 16_384
        || response.id_token.is_empty()
        || response.id_token.len() > 16_384
        || response.expires_in == 0
        || response
            .refresh_token
            .as_ref()
            .is_some_and(|token| token.len() > 16_384)
    {
        return Err(callback_invalid());
    }
    Ok(SecretString::from(response.access_token))
}

fn oauth_http_client() -> Result<reqwest::Client, Error> {
    reqwest::Client::builder()
        .https_only(true)
        .min_tls_version(reqwest::tls::Version::TLS_1_3)
        .max_tls_version(reqwest::tls::Version::TLS_1_3)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(OAUTH_CONNECT_TIMEOUT)
        .read_timeout(OAUTH_READ_TIMEOUT)
        .timeout(OAUTH_TOTAL_TIMEOUT)
        .build()
        .map_err(|_| callback_unavailable())
}

impl LoginAttempt {
    pub fn new(metadata: &OAuthMetadata, client_id: &str) -> Result<Self, Error> {
        validate_configuration(&metadata.issuer, client_id)?;
        let listener = TcpListener::bind(CALLBACK_ADDRESS).map_err(|_| callback_unavailable())?;
        listener
            .set_nonblocking(true)
            .map_err(|_| callback_unavailable())?;
        let client: PublicClient = BasicClient::new(ClientId::new(client_id.to_owned()))
            .set_auth_uri(
                AuthUrl::new(metadata.authorization_endpoint().to_owned())
                    .map_err(|_| config_invalid())?,
            )
            .set_token_uri(
                TokenUrl::new(metadata.token_endpoint().to_owned())
                    .map_err(|_| config_invalid())?,
            )
            .set_redirect_uri(RedirectUrl::new(CALLBACK.to_owned()).map_err(|_| config_invalid())?);
        let (challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
        let nonce = CsrfToken::new_random();
        let (authorization_url, state) = client
            .authorize_url(CsrfToken::new_random)
            .add_scope(Scope::new("openid".to_owned()))
            .add_scope(Scope::new("profile".to_owned()))
            .add_scope(Scope::new("email".to_owned()))
            .add_scope(Scope::new("offline_access".to_owned()))
            .add_extra_param("nonce", nonce.secret())
            .set_pkce_challenge(challenge)
            .url();
        Ok(Self {
            authorization_url: authorization_url.to_string(),
            listener,
            pkce_verifier,
            state,
        })
    }

    pub fn authorization_url(&self) -> &str {
        &self.authorization_url
    }

    pub fn wait_for_callback(self, timeout: Duration) -> Result<AuthorizationResult, Error> {
        if timeout.is_zero() || timeout > MAX_LOGIN_TIMEOUT {
            return Err(config_invalid());
        }
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(callback_unavailable)?;
        let (mut stream, peer) = accept_one(&self.listener, deadline)?;
        if !peer.ip().is_loopback() {
            return Err(callback_invalid());
        }
        stream
            .set_read_timeout(Some(deadline.saturating_duration_since(Instant::now())))
            .map_err(|_| callback_unavailable())?;
        let mut request_line = String::new();
        BufReader::new(&stream)
            .take(u64::try_from(MAX_CALLBACK_LINE_BYTES + 1).expect("callback limit fits u64"))
            .read_line(&mut request_line)
            .map_err(|_| callback_invalid())?;
        let code = parse_callback(&request_line, self.state.secret())?;
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 31\r\nConnection: close\r\n\r\nReturn to the Repro It terminal.",
            )
            .map_err(|_| callback_unavailable())?;
        Ok(AuthorizationResult {
            code,
            pkce_verifier: self.pkce_verifier,
        })
    }
}

fn accept_one(
    listener: &TcpListener,
    deadline: Instant,
) -> Result<(std::net::TcpStream, std::net::SocketAddr), Error> {
    loop {
        match listener.accept() {
            Ok(connection) => return Ok(connection),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(callback_timeout());
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return Err(callback_unavailable()),
        }
    }
}

pub struct NativeCredentialStore {
    entry: keyring::Entry,
}

impl NativeCredentialStore {
    pub fn open() -> Result<Self, Error> {
        keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT)
            .map(|entry| Self { entry })
            .map_err(|_| credential_unavailable())
    }

    pub fn store(&self, session: &SecretString) -> Result<(), Error> {
        let value = session.expose_secret();
        if value.is_empty() || value.len() > 65_536 {
            return Err(Error::schema_invalid());
        }
        self.entry
            .set_password(value)
            .map_err(|_| credential_unavailable())
    }

    pub fn load(&self) -> Result<SecretString, Error> {
        let value = self
            .entry
            .get_password()
            .map_err(|_| authentication_required())?;
        if value.is_empty() || value.len() > 65_536 {
            return Err(authentication_required());
        }
        Ok(SecretString::from(value))
    }

    pub fn remove(&self) -> Result<(), Error> {
        self.entry
            .delete_credential()
            .map_err(|_| credential_unavailable())
    }
}

fn parse_callback(request_line: &str, expected_state: &str) -> Result<String, Error> {
    if request_line.len() > MAX_CALLBACK_LINE_BYTES || !request_line.ends_with("\r\n") {
        return Err(callback_invalid());
    }
    let mut fields = request_line.trim_end().split(' ');
    if fields.next() != Some("GET") {
        return Err(callback_invalid());
    }
    let target = fields.next().ok_or_else(callback_invalid)?;
    if fields.next() != Some("HTTP/1.1") || fields.next().is_some() {
        return Err(callback_invalid());
    }
    let url = reqwest::Url::parse(&format!("http://127.0.0.1{target}"))
        .map_err(|_| callback_invalid())?;
    if url.path() != "/auth/callback" || url.fragment().is_some() {
        return Err(callback_invalid());
    }
    let mut code = None;
    let mut state = None;
    for (name, value) in url.query_pairs() {
        match name.as_ref() {
            "code" if code.is_none() => code = Some(value.into_owned()),
            "state" if state.is_none() => state = Some(value.into_owned()),
            _ => return Err(callback_invalid()),
        }
    }
    let state = state.ok_or_else(callback_invalid)?;
    if Digest::of(state.as_bytes()) != Digest::of(expected_state.as_bytes()) {
        return Err(callback_invalid());
    }
    let code = code.ok_or_else(callback_invalid)?;
    if code.is_empty() || code.len() > 4_096 {
        return Err(callback_invalid());
    }
    Ok(code)
}

fn validate_configuration(issuer: &str, client_id: &str) -> Result<(), Error> {
    let issuer = reqwest::Url::parse(issuer).map_err(|_| config_invalid())?;
    if issuer.scheme() != "https"
        || issuer.host_str().is_none()
        || issuer.port().is_some()
        || !issuer.username().is_empty()
        || issuer.password().is_some()
        || issuer.query().is_some()
        || issuer.fragment().is_some()
        || !matches!(issuer.path(), "" | "/")
        || client_id.is_empty()
        || client_id.len() > 512
    {
        return Err(config_invalid());
    }
    Ok(())
}

fn validate_endpoint(value: &str) -> Result<(), Error> {
    let url = reqwest::Url::parse(value).map_err(|_| config_invalid())?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(config_invalid());
    }
    Ok(())
}

fn contains_exact(values: &[String], required: &str) -> bool {
    !values.is_empty()
        && values.len() <= 32
        && values
            .iter()
            .all(|value| !value.is_empty() && value.len() <= 128)
        && values.iter().any(|value| value == required)
}

fn config_invalid() -> Error {
    Error::new(
        ErrorCode::ConfigConflict,
        "The CLI login configuration is invalid.",
    )
}

fn callback_unavailable() -> Error {
    Error::new(
        ErrorCode::ServiceUnavailable,
        "The CLI login callback is unavailable.",
    )
}

fn callback_timeout() -> Error {
    Error::new(
        ErrorCode::AuthenticationRequired,
        "The CLI login callback expired.",
    )
}

fn callback_invalid() -> Error {
    Error::new(
        ErrorCode::AuthorizationDenied,
        "The CLI login callback is invalid.",
    )
}

fn credential_unavailable() -> Error {
    Error::new(
        ErrorCode::ServiceUnavailable,
        "The native credential store is unavailable.",
    )
}

fn authentication_required() -> Error {
    Error::new(
        ErrorCode::AuthenticationRequired,
        "A Repro It login session is required.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorization_uses_fixed_callback_and_pkce_s256() {
        let metadata = metadata();
        let attempt = LoginAttempt::new(&metadata, "client_test").expect("login attempt");
        let url = attempt.authorization_url();
        assert!(url.contains("response_type=code"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("nonce="));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A8765%2Fauth%2Fcallback"));
        assert!(url.contains("scope=openid+profile+email+offline_access"));
        let second = LoginAttempt::new(&metadata, "client_test");
        assert_eq!(
            second
                .err()
                .expect("the callback address is already owned")
                .code,
            ErrorCode::ServiceUnavailable
        );
    }

    #[test]
    fn callback_requires_exact_path_state_and_query() {
        let line = "GET /auth/callback?code=one&state=expected HTTP/1.1\r\n";
        assert_eq!(parse_callback(line, "expected").unwrap(), "one");
        assert!(parse_callback(line, "different").is_err());
        assert!(
            parse_callback(
                "GET /auth/callback?code=one&state=expected&extra=1 HTTP/1.1\r\n",
                "expected",
            )
            .is_err()
        );
    }

    #[test]
    fn configuration_rejects_non_https_issuer() {
        let mut metadata = metadata();
        metadata.issuer = "http://auth.example.test".to_owned();
        assert!(LoginAttempt::new(&metadata, "client_test").is_err());
        assert!(LoginAttempt::new(&metadata, "").is_err());
    }

    #[test]
    fn metadata_requires_pkce_public_client_contract() {
        let metadata = metadata();
        assert!(metadata.validate("https://auth.example.test").is_ok());
        let mut invalid = metadata;
        invalid.token_endpoint_auth_methods_supported = vec!["client_secret_post".to_owned()];
        assert!(invalid.validate("https://auth.example.test").is_err());
    }

    fn metadata() -> OAuthMetadata {
        OAuthMetadata {
            authorization_endpoint: "https://auth.example.test/oauth2/authorize".to_owned(),
            code_challenge_methods_supported: vec!["S256".to_owned()],
            grant_types_supported: vec!["authorization_code".to_owned()],
            issuer: "https://auth.example.test".to_owned(),
            response_types_supported: vec!["code".to_owned()],
            token_endpoint: "https://auth.example.test/oauth2/token".to_owned(),
            token_endpoint_auth_methods_supported: vec!["none".to_owned()],
        }
    }
}
