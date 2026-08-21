use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use reproit_backend::config::{BackendSdk, ProjectConfig};
use reproit_cloud_api::ManagedOciGrant;
use reproit_core::{
    Error, ErrorCode,
    crypto::{decode_base64url, encode_base64url, verify_signed_value},
    identity::{Digest, Timestamp},
    model::{
        ExecutionResult, ExecutorCapabilityEvidence, ExecutorEvidenceScope, ExecutorLocality,
        ProcessingMode, Validate, replay_capabilities_present, verify_executor_capability_evidence,
    },
};
use secrecy::{ExposeSecret as _, SecretString};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

pub use reproit_worker::{
    ManagedWorkerExecutionRequest, WorkerSource, WorkerSourceFile, WorkerSubject,
};

use crate::{ManagedSource, source_package::collect_source};

const KEYRING_SERVICE: &str = "com.reproit.cli";
const KEYRING_ACCOUNT: &str = "managed-replay-directory";
const MAX_CONFIGURATION_BYTES: usize = 65_536;
const MAX_ENDPOINTS: usize = 64;
const MAX_EXECUTION_REQUEST_BYTES: usize = 384 * 1_024 * 1_024;
const MAX_RESPONSE_BYTES: usize = 64 * 1_024;
const MAX_RESPONSE_HEADER_BYTES: usize = 32 * 1_024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const EXECUTION_TIMEOUT: Duration = Duration::from_mins(15);
const DEBUGGER_READY_TIMEOUT: Duration = Duration::from_mins(5);

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReplayEndpoint {
    bearer_token: String,
    client_identity_pem: String,
    origin: String,
    requester_identity: String,
    server_ca_pem: String,
    tls_identity: Digest,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReplayDirectory {
    endpoints: Vec<ReplayEndpoint>,
    verification_keys: BTreeMap<String, String>,
}

pub struct ExecutorControl {
    configuration: ReplayDirectory,
    verification_keys: BTreeMap<String, [u8; 32]>,
}

pub struct ManagedExecutionSession {
    client: reqwest::Client,
    endpoint: ReplayEndpoint,
    grant: ManagedOciGrant,
    debugger_capability: SecretString,
}

impl ExecutorControl {
    pub fn load_for_project(project: &ProjectConfig) -> Result<Self, Error> {
        if project.processing_mode != ProcessingMode::Managed {
            return Err(configuration_invalid());
        }
        let value = if let Some(path) = std::env::var_os("REPROIT_REPLAY_CONFIGURATION_FILE") {
            read_configuration_file(&path)?
        } else {
            keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT)
                .and_then(|entry| entry.get_password())
                .map_err(|_| configuration_unavailable())?
        };
        Self::parse(&value)
    }

    fn parse(value: &str) -> Result<Self, Error> {
        if value.is_empty() || value.len() > MAX_CONFIGURATION_BYTES {
            return Err(configuration_invalid());
        }
        let configuration: ReplayDirectory =
            serde_json::from_str(value).map_err(|_| configuration_invalid())?;
        validate_configuration(&configuration)?;
        let verification_keys = configuration
            .verification_keys
            .iter()
            .map(|(key_id, key)| {
                decode_base64url::<32>(key)
                    .map(|decoded| (key_id.clone(), decoded))
                    .map_err(|_| configuration_invalid())
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            configuration,
            verification_keys,
        })
    }

    pub async fn open_managed(
        &self,
        required_capabilities: &[String],
        project: &ProjectConfig,
        grant: ManagedOciGrant,
        debugger_capability: SecretString,
    ) -> Result<ManagedExecutionSession, Error> {
        validate_grant(
            project,
            &grant,
            &debugger_capability,
            &self.verification_keys,
        )?;
        let scope = ExecutorEvidenceScope {
            organization_id: project.organization_id,
            project_id: project.project_id,
            service_id: project.service_id,
        };
        let (endpoint, _evidence) = self
            .select_endpoint(required_capabilities, &scope, &grant)
            .await?;
        Ok(ManagedExecutionSession {
            client: endpoint_client(&endpoint, EXECUTION_TIMEOUT)?,
            endpoint,
            grant,
            debugger_capability,
        })
    }

    async fn select_endpoint(
        &self,
        required_capabilities: &[String],
        scope: &ExecutorEvidenceScope,
        grant: &ManagedOciGrant,
    ) -> Result<(ReplayEndpoint, ExecutorCapabilityEvidence), Error> {
        for endpoint in &self.configuration.endpoints {
            if endpoint.requester_identity != grant.requester_identity {
                continue;
            }
            let Ok(evidence) = self.capability_evidence(endpoint, grant).await else {
                continue;
            };
            let Some(public_key) = self.verification_keys.get(&evidence.signer_key_id) else {
                continue;
            };
            let verified_at = current_timestamp()?;
            if evidence.locality == ExecutorLocality::ManagedWorker
                && evidence.executor_id == grant.worker_id
                && evidence.platform_identity_digest == grant.worker_release
                && verify_executor_capability_evidence(&evidence, scope, &verified_at, public_key)
                    .is_ok()
                && replay_capabilities_present(required_capabilities, &evidence.capabilities)
            {
                return Ok((endpoint.clone(), evidence));
            }
        }
        Err(replay_host_unavailable())
    }

    async fn capability_evidence(
        &self,
        endpoint: &ReplayEndpoint,
        grant: &ManagedOciGrant,
    ) -> Result<ExecutorCapabilityEvidence, Error> {
        let response = endpoint_client(endpoint, REQUEST_TIMEOUT)?
            .post(endpoint_url(endpoint, "v1/managed-capabilities")?)
            .bearer_auth(&endpoint.bearer_token)
            .json(grant)
            .send()
            .await
            .map_err(|_| replay_host_unavailable())?;
        decode_response(response).await
    }
}

impl ManagedExecutionSession {
    pub fn grant(&self) -> &ManagedOciGrant {
        &self.grant
    }

    pub async fn execute(
        &self,
        attach_debugger: bool,
        profile_configuration: &[u8],
        repro_id: reproit_core::identity::ReproId,
        source: &ManagedSource,
        subject: WorkerSubject,
    ) -> Result<ExecutionResult, Error> {
        let body = ManagedWorkerExecutionRequest {
            attach_debugger,
            debugger_capability: self.debugger_capability.expose_secret().to_owned(),
            grant: self.grant.clone(),
            profile_configuration: String::from_utf8(profile_configuration.to_vec())
                .map_err(|_| replay_host_unavailable())?,
            repro_id,
            source: WorkerSource {
                files: collect_source(Path::new(&source.workspace), &source.source_revision)?,
                repository_id: source.repository_id.clone(),
                source_revision: source.source_revision.clone(),
            },
            subject,
        };
        ensure_request_size(&body)?;
        if attach_debugger {
            execute_with_debugger(self, &body).await
        } else {
            send_execution(self, &body).await
        }
    }
}

fn validate_grant(
    project: &ProjectConfig,
    grant: &ManagedOciGrant,
    debugger_capability: &SecretString,
    verification_keys: &BTreeMap<String, [u8; 32]>,
) -> Result<(), Error> {
    grant.validate()?;
    let public_key = verification_keys
        .get(&grant.signer_key_id)
        .ok_or_else(configuration_invalid)?;
    verify_signed_value(
        &serde_json::to_value(grant).map_err(|_| configuration_invalid())?,
        public_key,
    )?;
    let now = current_timestamp()?;
    if project.processing_mode != ProcessingMode::Managed
        || grant.organization_id != project.organization_id
        || grant.project_id != project.project_id
        || grant.service_id != project.service_id
        || grant.requester_identity.is_empty()
        || grant.debugger_capability_digest
            != Digest::of(debugger_capability.expose_secret().as_bytes())
        || now < grant.not_before
        || now >= grant.expires_at
    {
        return Err(configuration_invalid());
    }
    Ok(())
}

async fn send_execution(
    session: &ManagedExecutionSession,
    body: &ManagedWorkerExecutionRequest,
) -> Result<ExecutionResult, Error> {
    ensure_request_size(body)?;
    let path = format!("v1/managed-executions/{}/run", session.grant.grant_id);
    let response = session
        .client
        .post(endpoint_url(&session.endpoint, &path)?)
        .bearer_auth(&session.endpoint.bearer_token)
        .json(body)
        .send()
        .await
        .map_err(|_| replay_host_unavailable())?;
    decode_response(response).await
}

async fn execute_with_debugger(
    session: &ManagedExecutionSession,
    body: &ManagedWorkerExecutionRequest,
) -> Result<ExecutionResult, Error> {
    let execution = send_execution(session, body);
    tokio::pin!(execution);
    let upgraded = {
        let tunnel = open_debugger_tunnel(session);
        tokio::pin!(tunnel);
        tokio::select! {
            result = &mut execution => {
                result?;
                return Err(replay_host_unavailable());
            }
            upgraded = &mut tunnel => upgraded?,
        }
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|_| replay_host_unavailable())?;
    let address = listener
        .local_addr()
        .map_err(|_| replay_host_unavailable())?;
    crate::render::stdout_line(format_args!("Debugger ready at {address}."))?;
    crate::render::stdout_line(format_args!(
        "Attach with {}, then press Enter.",
        debugger_client_name(body)?
    ))?;
    let (developer, peer) = {
        let accepted = tokio::time::timeout(EXECUTION_TIMEOUT, listener.accept());
        tokio::pin!(accepted);
        tokio::select! {
            result = &mut execution => {
                result?;
                return Err(replay_host_unavailable());
            }
            accepted = &mut accepted => accepted
                .map_err(|_| replay_host_unavailable())?
                .map_err(|_| replay_host_unavailable())?,
        }
    };
    drop(listener);
    if !peer.ip().is_loopback() {
        return Err(replay_host_unavailable());
    }
    developer
        .set_nodelay(true)
        .map_err(|_| replay_host_unavailable())?;
    tokio::task::spawn_blocking(read_debugger_confirmation)
        .await
        .map_err(|_| replay_host_unavailable())??;
    let relay = relay_debugger(developer, upgraded);
    let (result, ()) = tokio::try_join!(execution, relay)?;
    Ok(result)
}

async fn open_debugger_tunnel(
    session: &ManagedExecutionSession,
) -> Result<reqwest::Upgraded, Error> {
    let deadline = Instant::now()
        .checked_add(DEBUGGER_READY_TIMEOUT)
        .ok_or_else(replay_host_unavailable)?;
    let response = loop {
        let path = format!("v1/managed-executions/{}/debugger", session.grant.grant_id);
        let response = session
            .client
            .get(endpoint_url(&session.endpoint, &path)?)
            .bearer_auth(&session.endpoint.bearer_token)
            .header(reqwest::header::CONNECTION, "upgrade")
            .header(reqwest::header::UPGRADE, "reproit-debugger-v1")
            .header(
                "x-reproit-debugger-capability",
                session.debugger_capability.expose_secret(),
            )
            .send()
            .await
            .map_err(|_| replay_host_unavailable())?;
        match response.status() {
            reqwest::StatusCode::SWITCHING_PROTOCOLS => break response,
            reqwest::StatusCode::TOO_EARLY if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            _ => return Err(replay_host_unavailable()),
        }
    };
    response
        .upgrade()
        .await
        .map_err(|_| replay_host_unavailable())
}

async fn relay_debugger(
    mut developer: tokio::net::TcpStream,
    mut replay_host: reqwest::Upgraded,
) -> Result<(), Error> {
    tokio::time::timeout(
        EXECUTION_TIMEOUT,
        tokio::io::copy_bidirectional(&mut developer, &mut replay_host),
    )
    .await
    .map_err(|_| replay_host_unavailable())?
    .map_err(|_| replay_host_unavailable())?;
    Ok(())
}

fn debugger_client_name(request: &ManagedWorkerExecutionRequest) -> Result<&'static str, Error> {
    if request.grant.operation != reproit_cloud_api::ManagedOperation::Debug {
        return Err(replay_host_unavailable());
    }
    let project: ProjectConfig =
        reproit_core::canonical::parse_strict(request.profile_configuration.as_bytes())?;
    project.validate()?;
    Ok(match project.sdk {
        BackendSdk::Rust => "GDB",
        BackendSdk::Nodejs => "Chrome DevTools",
        BackendSdk::Dotnet | BackendSdk::Go | BackendSdk::Python => {
            "a Debug Adapter Protocol client"
        }
    })
}

fn read_debugger_confirmation() -> Result<(), Error> {
    let mut confirmation = String::new();
    std::io::stdin()
        .read_line(&mut confirmation)
        .map_err(|_| replay_host_unavailable())?;
    if matches!(confirmation.as_str(), "" | "\n" | "\r\n") {
        Ok(())
    } else {
        Err(replay_host_unavailable())
    }
}

fn ensure_request_size(value: &impl Serialize) -> Result<(), Error> {
    let mut counter = RequestSizeCounter { bytes: 0 };
    serde_json::to_writer(&mut counter, value).map_err(|_| {
        Error::new(
            ErrorCode::RuntimeQuota,
            "The replay-host request exceeds its configured limit.",
        )
    })
}

struct RequestSizeCounter {
    bytes: usize,
}

impl std::io::Write for RequestSizeCounter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(buffer.len())
            .filter(|bytes| *bytes <= MAX_EXECUTION_REQUEST_BYTES)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::FileTooLarge,
                    "the replay-host request exceeds its configured limit",
                )
            })?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

async fn decode_response<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, Error> {
    let status = response.status();
    let bytes = read_response_bytes(response).await?;
    if status != reqwest::StatusCode::OK {
        return Err(reproit_core::canonical::parse_strict(&bytes)
            .unwrap_or_else(|_| replay_host_unavailable()));
    }
    reproit_core::canonical::parse_strict(&bytes)
}

async fn read_response_bytes(mut response: reqwest::Response) -> Result<Vec<u8>, Error> {
    if response_header_bytes(response.headers()) > MAX_RESPONSE_HEADER_BYTES
        || response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(replay_host_unavailable());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| replay_host_unavailable())?
    {
        let next = bytes
            .len()
            .checked_add(chunk.len())
            .filter(|length| *length <= MAX_RESPONSE_BYTES)
            .ok_or_else(replay_host_unavailable)?;
        bytes.reserve(next - bytes.len());
        bytes.extend_from_slice(&chunk);
    }
    if bytes.is_empty() {
        return Err(replay_host_unavailable());
    }
    Ok(bytes)
}

fn response_header_bytes(headers: &reqwest::header::HeaderMap) -> usize {
    headers.iter().fold(0_usize, |total, (name, value)| {
        total
            .saturating_add(name.as_str().len())
            .saturating_add(value.as_bytes().len())
            .saturating_add(4)
    })
}

fn validate_configuration(configuration: &ReplayDirectory) -> Result<(), Error> {
    if configuration.endpoints.is_empty()
        || configuration.endpoints.len() > MAX_ENDPOINTS
        || configuration.verification_keys.is_empty()
        || configuration.verification_keys.len() > MAX_ENDPOINTS
    {
        return Err(configuration_invalid());
    }
    for endpoint in &configuration.endpoints {
        let origin = reqwest::Url::parse(&endpoint.origin).map_err(|_| configuration_invalid())?;
        if origin.scheme() != "https"
            || origin.cannot_be_a_base()
            || origin.host_str().is_none()
            || !origin.username().is_empty()
            || origin.password().is_some()
            || !matches!(origin.path(), "" | "/")
            || origin.query().is_some()
            || origin.fragment().is_some()
            || endpoint.bearer_token.is_empty()
            || endpoint.bearer_token.len() > 4_096
            || endpoint.requester_identity.is_empty()
            || endpoint.requester_identity.len() > 512
            || endpoint.requester_identity.chars().any(char::is_control)
            || endpoint.client_identity_pem.len() > 32 * 1_024
            || !endpoint.client_identity_pem.contains("PRIVATE KEY")
            || endpoint.server_ca_pem.len() > 32 * 1_024
            || !endpoint.server_ca_pem.contains("CERTIFICATE")
            || Digest::of(endpoint.server_ca_pem.as_bytes()) != endpoint.tls_identity
        {
            return Err(configuration_invalid());
        }
    }
    if configuration.verification_keys.iter().any(|(key_id, key)| {
        key_id.is_empty()
            || key_id.len() > 256
            || key.len() != 43
            || URL_SAFE_NO_PAD.decode(key).is_err()
    }) {
        return Err(configuration_invalid());
    }
    Ok(())
}

fn endpoint_client(endpoint: &ReplayEndpoint, timeout: Duration) -> Result<reqwest::Client, Error> {
    reqwest::Client::builder()
        .timeout(timeout)
        .tcp_nodelay(true)
        .http1_only()
        .https_only(true)
        .min_tls_version(reqwest::tls::Version::TLS_1_3)
        .max_tls_version(reqwest::tls::Version::TLS_1_3)
        .redirect(reqwest::redirect::Policy::none())
        .identity(
            reqwest::Identity::from_pem(endpoint.client_identity_pem.as_bytes())
                .map_err(|_| configuration_invalid())?,
        )
        .add_root_certificate(
            reqwest::Certificate::from_pem(endpoint.server_ca_pem.as_bytes())
                .map_err(|_| configuration_invalid())?,
        )
        .build()
        .map_err(|_| configuration_unavailable())
}

fn endpoint_url(endpoint: &ReplayEndpoint, path: &str) -> Result<reqwest::Url, Error> {
    reqwest::Url::parse(&endpoint.origin)
        .and_then(|base| base.join(path))
        .map_err(|_| configuration_invalid())
}

fn read_configuration_file(path: &std::ffi::OsStr) -> Result<String, Error> {
    let path = Path::new(path);
    if !path.is_absolute() {
        return Err(configuration_invalid());
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| configuration_unavailable())?;
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() == 0
        || metadata.len() > MAX_CONFIGURATION_BYTES as u64
    {
        return Err(configuration_invalid());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(configuration_invalid());
        }
    }
    fs::read_to_string(path).map_err(|_| configuration_unavailable())
}

fn current_timestamp() -> Result<Timestamp, Error> {
    let value = OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        value.year(),
        value.month() as u8,
        value.day(),
        value.hour(),
        value.minute(),
        value.second(),
        value.millisecond(),
    )
    .parse()
    .map_err(|_| replay_host_unavailable())
}

pub(crate) fn random_debugger_capability() -> Result<SecretString, Error> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| replay_host_unavailable())?;
    Ok(SecretString::from(encode_base64url(&bytes)))
}

fn configuration_invalid() -> Error {
    Error::new(
        ErrorCode::ConfigConflict,
        "The protected replay-host configuration is invalid.",
    )
}

fn configuration_unavailable() -> Error {
    Error::new(
        ErrorCode::AuthenticationRequired,
        "The protected replay-host configuration is unavailable.",
    )
}

fn replay_host_unavailable() -> Error {
    Error::new(
        ErrorCode::ServiceUnavailable,
        "The authenticated replay host is unavailable.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_size_has_an_exact_upper_bound() {
        let mut counter = RequestSizeCounter {
            bytes: MAX_EXECUTION_REQUEST_BYTES - 1,
        };
        std::io::Write::write(&mut counter, &[0]).expect("request at bound");
        assert_eq!(
            std::io::Write::write(&mut counter, &[0])
                .expect_err("request over bound")
                .kind(),
            std::io::ErrorKind::FileTooLarge
        );
    }

    #[test]
    fn debugger_confirmation_accepts_only_enter() {
        assert!(matches!("\n", "" | "\n" | "\r\n"));
        assert!(!matches!("continue\n", "" | "\n" | "\r\n"));
    }

    #[test]
    fn replay_directory_is_bounded() {
        let directory = ReplayDirectory {
            endpoints: Vec::new(),
            verification_keys: BTreeMap::new(),
        };
        assert_eq!(
            validate_configuration(&directory)
                .expect_err("empty directory")
                .code,
            ErrorCode::ConfigConflict
        );
    }
}
