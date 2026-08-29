use std::time::Duration;

use reproit_cloud_api::{
    ConfigConflict, FuzzCampaignCreate, FuzzCampaignCreated, FuzzCampaignStatus,
    ManagedKeepRequest, ManagedKeepResult, ManagedOciGrant, ManagedOciGrantRequest, OccurrenceList,
    OccurrenceListQuery, ProjectCreateRequest, ProjectCreateResult, ProjectTokenId,
    ProjectTokenIssueRequest, ProjectTokenIssueResult, ProjectTokenRevokeRequest,
    ProjectTokenRevokeResult, ProjectTokenRotateRequest, ProjectTokenRotateResult,
    ReleaseJobCreateRequest, ReleaseJobCreateResult, ReleaseJobDetailResponse, ReleaseJobId,
    ReproDetail, ReproList, ReproListQuery, ServiceCatalog, ServiceCatalogQuery,
    ServiceCreateRequest, ServiceCreateResult, Triage, TriageConflict,
};
use reproit_core::{
    Error, ErrorCode, canonical,
    identity::{ProjectId, ReproId},
    model::Validate,
};
use secrecy::{ExposeSecret as _, SecretString};
use serde::{Serialize, de::DeserializeOwned};

const MAX_CLOUD_RESPONSE_BYTES: usize = 2 * 1_024 * 1_024;
const MAX_RESPONSE_HEADER_BYTES: usize = 32 * 1_024;
const MAX_RELEASE_JOB_REQUEST_BYTES: usize = 4 * 1_024;
const CLOUD_API_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub struct HttpCloudClient {
    base_url: String,
    bearer_token: SecretString,
    client: reqwest::Client,
}

impl HttpCloudClient {
    pub fn new(base_url: &str, bearer_token: SecretString) -> Result<Self, Error> {
        let base_url = parse_cloud_origin(base_url)?;
        let client = reqwest::Client::builder()
            .https_only(true)
            .min_tls_version(reqwest::tls::Version::TLS_1_3)
            .max_tls_version(reqwest::tls::Version::TLS_1_3)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(30))
            .timeout(Duration::from_mins(5))
            .build()
            .map_err(service_unavailable)?;
        Ok(Self {
            base_url,
            bearer_token,
            client,
        })
    }

    pub async fn list_services(&self, repository_id: &str) -> Result<ServiceCatalog, Error> {
        self.list_services_page(&ServiceCatalogQuery {
            cursor: None,
            limit: Some(50),
            repository_id: repository_id.to_owned(),
        })
        .await
    }

    pub async fn list_services_page(
        &self,
        query: &ServiceCatalogQuery,
    ) -> Result<ServiceCatalog, Error> {
        query.validate()?;
        let catalog: ServiceCatalog = self.get_json("/v1/services", query).await?;
        catalog.validate()?;
        Ok(catalog)
    }

    pub async fn create_project(
        &self,
        request: &ProjectCreateRequest,
    ) -> Result<ProjectCreateResult, Error> {
        request.validate()?;
        let result: ProjectCreateResult = self
            .send_json(reqwest::Method::POST, "/v1/projects", request)
            .await?;
        result.validate()?;
        Ok(result)
    }

    pub async fn create_service(
        &self,
        project_id: ProjectId,
        request: &ServiceCreateRequest,
    ) -> Result<ServiceCreateResult, Error> {
        request.validate()?;
        let result: ServiceCreateResult = self
            .send_json(
                reqwest::Method::POST,
                &project_services_path(project_id),
                request,
            )
            .await?;
        result.validate()?;
        Ok(result)
    }

    pub async fn create_fuzz_campaign(
        &self,
        project_id: ProjectId,
        request: &FuzzCampaignCreate,
    ) -> Result<FuzzCampaignCreated, Error> {
        request.validate()?;
        if request.project_id != project_id {
            return Err(Error::schema_invalid());
        }
        let result: FuzzCampaignCreated = self
            .send_json(
                reqwest::Method::POST,
                &format!("/v1/projects/{project_id}/fuzz-campaigns"),
                request,
            )
            .await?;
        result.validate()?;
        Ok(result)
    }

    pub async fn get_fuzz_campaign(
        &self,
        campaign_id: reproit_core::identity::FuzzCampaignId,
    ) -> Result<FuzzCampaignStatus, Error> {
        let result: FuzzCampaignStatus = self
            .get_json(&format!("/v1/fuzz-campaigns/{campaign_id}"), &[(); 0])
            .await?;
        result.validate()?;
        if result.campaign_id != campaign_id {
            return Err(Error::schema_invalid());
        }
        Ok(result)
    }

    pub async fn cancel_fuzz_campaign(
        &self,
        campaign_id: reproit_core::identity::FuzzCampaignId,
    ) -> Result<FuzzCampaignStatus, Error> {
        let result: FuzzCampaignStatus = self
            .send_json(
                reqwest::Method::POST,
                &format!("/v1/fuzz-campaigns/{campaign_id}/cancel"),
                &[(); 0],
            )
            .await?;
        result.validate()?;
        if result.campaign_id != campaign_id {
            return Err(Error::schema_invalid());
        }
        Ok(result)
    }

    pub async fn create_release_job(
        &self,
        project_id: ProjectId,
        request: &ReleaseJobCreateRequest,
    ) -> Result<ReleaseJobCreateResult, Error> {
        let response = self
            .release_job_create_request(project_id, request)?
            .send()
            .await
            .map_err(service_unavailable)?;
        let result: ReleaseJobCreateResult = decode_response(response).await?;
        result.validate()?;
        Ok(result)
    }

    pub async fn get_release_job(
        &self,
        release_job_id: ReleaseJobId,
    ) -> Result<ReleaseJobDetailResponse, Error> {
        let response = self
            .release_job_get_request(release_job_id)
            .send()
            .await
            .map_err(service_unavailable)?;
        let result: ReleaseJobDetailResponse = decode_response(response).await?;
        result.validate()?;
        if result.release_job_id != release_job_id {
            return Err(Error::schema_invalid());
        }
        Ok(result)
    }

    pub async fn issue_project_token(
        &self,
        project_id: ProjectId,
        request: &ProjectTokenIssueRequest,
    ) -> Result<ProjectTokenIssueResult, Error> {
        request.validate()?;
        let result: ProjectTokenIssueResult = self
            .send_json(
                reqwest::Method::POST,
                &project_tokens_path(project_id),
                request,
            )
            .await?;
        result.validate()?;
        Ok(result)
    }

    pub async fn rotate_project_token(
        &self,
        project_id: ProjectId,
        token_id: &ProjectTokenId,
        request: &ProjectTokenRotateRequest,
    ) -> Result<ProjectTokenRotateResult, Error> {
        request.validate()?;
        let result: ProjectTokenRotateResult = self
            .send_json(
                reqwest::Method::POST,
                &project_token_path(project_id, token_id, "/rotate"),
                request,
            )
            .await?;
        result.validate()?;
        Ok(result)
    }

    pub async fn revoke_project_token(
        &self,
        project_id: ProjectId,
        token_id: &ProjectTokenId,
        request: &ProjectTokenRevokeRequest,
    ) -> Result<ProjectTokenRevokeResult, Error> {
        request.validate()?;
        let result: ProjectTokenRevokeResult = self
            .send_json(
                reqwest::Method::DELETE,
                &project_token_path(project_id, token_id, ""),
                request,
            )
            .await?;
        result.validate()?;
        Ok(result)
    }

    pub async fn create_managed_grant(
        &self,
        repro_id: ReproId,
        request: &ManagedOciGrantRequest,
    ) -> Result<ManagedOciGrant, Error> {
        request.validate()?;
        self.send_json(
            reqwest::Method::POST,
            &format!("/v1/repros/{repro_id}/managed-grants"),
            request,
        )
        .await
    }

    pub async fn cancel_managed_grant(&self, grant: &ManagedOciGrant) -> Result<(), Error> {
        grant.validate()?;
        let response = self
            .client
            .delete(format!(
                "{}/v1/managed-executions/{}",
                self.base_url, grant.grant_id
            ))
            .bearer_auth(self.bearer_token.expose_secret())
            .send()
            .await
            .map_err(service_unavailable)?;
        if response.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(());
        }
        let _: serde_json::Value = decode_response(response).await?;
        Err(service_unavailable(()))
    }

    pub async fn keep_managed(
        &self,
        repro_id: ReproId,
        request: &ManagedKeepRequest,
    ) -> Result<ManagedKeepResult, Error> {
        request.validate()?;
        let result = self
            .send_json(
                reqwest::Method::POST,
                &format!("/v1/repros/{repro_id}/managed-keep"),
                request,
            )
            .await?;
        ManagedKeepResult::validate(&result)?;
        Ok(result)
    }

    pub async fn get_repro(&self, repro_id: ReproId) -> Result<ReproDetail, Error> {
        self.get_json(&format!("/v1/repros/{repro_id}"), &[(); 0])
            .await
    }

    pub async fn list_occurrences(
        &self,
        repro_id: ReproId,
        query: &OccurrenceListQuery,
    ) -> Result<OccurrenceList, Error> {
        self.get_json(&format!("/v1/repros/{repro_id}/occurrences"), query)
            .await
    }

    pub async fn list_repros(&self, query: &ReproListQuery) -> Result<ReproList, Error> {
        self.get_json("/v1/repros", query).await
    }

    pub async fn update_triage(&self, repro_id: ReproId, triage: &Triage) -> Result<Triage, Error> {
        self.send_json(
            reqwest::Method::PATCH,
            &format!("/v1/repros/{repro_id}/triage"),
            triage,
        )
        .await
    }

    async fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &impl Serialize,
    ) -> Result<T, Error> {
        let response = self
            .cloud_request(reqwest::Method::GET, path)
            .query(query)
            .send()
            .await
            .map_err(service_unavailable)?;
        decode_response(response).await
    }

    async fn send_json<B: Serialize, T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: &B,
    ) -> Result<T, Error> {
        let response = self
            .cloud_request(method, path)
            .json(body)
            .send()
            .await
            .map_err(service_unavailable)?;
        decode_response(response).await
    }

    fn cloud_request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.base_url))
            .bearer_auth(self.bearer_token.expose_secret())
            .timeout(CLOUD_API_REQUEST_TIMEOUT)
    }

    fn release_job_create_request(
        &self,
        project_id: ProjectId,
        request: &ReleaseJobCreateRequest,
    ) -> Result<reqwest::RequestBuilder, Error> {
        request.validate()?;
        if request.project_id != project_id {
            return Err(Error::schema_invalid());
        }
        let body = canonical::canonical_bytes(request)?;
        if body.len() > MAX_RELEASE_JOB_REQUEST_BYTES {
            return Err(Error::new(
                ErrorCode::RuntimeQuota,
                "The release-job request exceeds its byte limit.",
            ));
        }
        Ok(self
            .cloud_request(
                reqwest::Method::POST,
                &project_release_jobs_path(project_id),
            )
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body))
    }

    fn release_job_get_request(&self, release_job_id: ReleaseJobId) -> reqwest::RequestBuilder {
        self.cloud_request(reqwest::Method::GET, &release_job_path(release_job_id))
    }
}

async fn decode_response<T: DeserializeOwned>(mut response: reqwest::Response) -> Result<T, Error> {
    let successful = response.status().is_success();
    let bytes = read_bounded(&mut response, MAX_CLOUD_RESPONSE_BYTES).await?;
    if successful {
        return canonical::parse_strict(&bytes);
    }
    if let Ok(conflict) = canonical::parse_strict::<TriageConflict>(&bytes) {
        return Err(conflict.error);
    }
    if let Ok(conflict) = canonical::parse_strict::<ConfigConflict>(&bytes) {
        return Err(conflict.error);
    }
    match canonical::parse_strict::<Error>(&bytes) {
        Ok(error) => Err(error),
        Err(_) => Err(Error::new(
            ErrorCode::ServiceUnavailable,
            "Cloud returned an invalid error response.",
        )),
    }
}

async fn read_bounded(
    response: &mut reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, Error> {
    if response_header_bytes(response.headers()) > MAX_RESPONSE_HEADER_BYTES
        || response
            .content_length()
            .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(response_too_large());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(service_unavailable)? {
        let next = bytes
            .len()
            .checked_add(chunk.len())
            .filter(|length| *length <= max_bytes)
            .ok_or_else(response_too_large)?;
        bytes.reserve(next - bytes.len());
        bytes.extend_from_slice(&chunk);
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

fn response_too_large() -> Error {
    Error::new(
        ErrorCode::UploadLimitExceeded,
        "The Cloud response exceeds its configured limit.",
    )
}

fn project_token_path(project_id: ProjectId, token_id: &ProjectTokenId, suffix: &str) -> String {
    format!("/v1/projects/{project_id}/tokens/{token_id}{suffix}")
}

fn project_services_path(project_id: ProjectId) -> String {
    format!("/v1/projects/{project_id}/services")
}

fn project_tokens_path(project_id: ProjectId) -> String {
    format!("/v1/projects/{project_id}/tokens")
}

fn project_release_jobs_path(project_id: ProjectId) -> String {
    format!("/v1/projects/{project_id}/release-jobs")
}

fn release_job_path(release_job_id: ReleaseJobId) -> String {
    format!("/v1/release-jobs/{release_job_id}")
}

/// Parse the canonical Cloud origin: one exact HTTPS origin with a root path
/// and no credentials, port, query, or fragment. Return the origin without a
/// trailing slash so callers can append absolute API paths. Both origin
/// spellings, with and without the trailing slash, name the same origin. This
/// rule is the CLI side of the shared Cloud origin contract.
fn parse_cloud_origin(base_url: &str) -> Result<String, Error> {
    if base_url.len() > 2_048 {
        return Err(Error::schema_invalid());
    }
    let origin = reqwest::Url::parse(base_url).map_err(|_| Error::schema_invalid())?;
    if origin.scheme() != "https"
        || origin.host_str().is_none()
        || origin.port().is_some()
        || !origin.username().is_empty()
        || origin.password().is_some()
        || !matches!(origin.path(), "" | "/")
        || origin.query().is_some()
        || origin.fragment().is_some()
    {
        return Err(Error::schema_invalid());
    }
    Ok(origin.as_str().trim_end_matches('/').to_owned())
}

fn service_unavailable<T>(_error: T) -> Error {
    Error::new(
        ErrorCode::ServiceUnavailable,
        "Repro It Cloud is unavailable.",
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use reproit_cloud_api::{
        ProjectCreateRequest, ProjectTokenId, ProjectTokenIssueRequest, ProjectTokenRevokeRequest,
        ProjectTokenRotateRequest, ReleaseJobCreateRequest, ReleaseJobId, ServiceCatalogQuery,
        ServiceCreateRequest,
    };
    use reproit_core::{ErrorCode, canonical, identity::ProjectId};
    use secrecy::SecretString;

    use super::{
        CLOUD_API_REQUEST_TIMEOUT, HttpCloudClient, parse_cloud_origin, project_services_path,
        project_token_path, project_tokens_path, release_job_path,
    };

    #[test]
    fn cloud_origin_accepts_both_root_spellings_of_one_origin() {
        assert_eq!(
            parse_cloud_origin("https://cloud.reproit.com").unwrap(),
            "https://cloud.reproit.com"
        );
        assert_eq!(
            parse_cloud_origin("https://managed.test/").unwrap(),
            "https://managed.test"
        );
    }

    #[test]
    fn cloud_origin_rejects_every_non_origin_form() {
        for invalid in [
            "http://cloud.reproit.com",
            "https://cloud.reproit.com:8443",
            "https://user@cloud.example",
            "https://user:secret@cloud.example",
            "https://cloud.reproit.com/v1",
            "https://cloud.reproit.com/?next=evil",
            "https://cloud.reproit.com/#fragment",
            "https://",
            "cloud.reproit.com",
            "",
        ] {
            assert!(parse_cloud_origin(invalid).is_err(), "{invalid}");
        }
        assert!(parse_cloud_origin(&format!("https://{}.test", "a".repeat(2_048))).is_err());
    }

    #[test]
    fn onboarding_routes_use_exact_scoped_paths() {
        let project_id: ProjectId = "prj_01890f3e-7b1c-7cc0-8a1b-123456789abe".parse().unwrap();
        let token_id =
            ProjectTokenId::new("ptk_01890f3e-7b1c-7cc0-8a1b-123456789ac4".to_owned()).unwrap();

        assert_eq!(
            project_services_path(project_id),
            "/v1/projects/prj_01890f3e-7b1c-7cc0-8a1b-123456789abe/services"
        );
        assert_eq!(
            project_tokens_path(project_id),
            "/v1/projects/prj_01890f3e-7b1c-7cc0-8a1b-123456789abe/tokens"
        );
        assert_eq!(
            project_token_path(project_id, &token_id, "/rotate"),
            concat!(
                "/v1/projects/prj_01890f3e-7b1c-7cc0-8a1b-123456789abe/tokens/",
                "ptk_01890f3e-7b1c-7cc0-8a1b-123456789ac4/rotate"
            )
        );
        assert_eq!(
            project_token_path(project_id, &token_id, ""),
            concat!(
                "/v1/projects/prj_01890f3e-7b1c-7cc0-8a1b-123456789abe/tokens/",
                "ptk_01890f3e-7b1c-7cc0-8a1b-123456789ac4"
            )
        );
    }

    #[test]
    fn service_catalog_request_is_authenticated_bounded_and_paged() {
        let client = cloud_client();
        let query = ServiceCatalogQuery {
            cursor: Some("next-page".to_owned()),
            limit: Some(100),
            repository_id: "source.example/acme/commerce".to_owned(),
        };
        let request = client
            .cloud_request(reqwest::Method::GET, "/v1/services")
            .query(&query)
            .build()
            .unwrap();

        assert_eq!(request.method(), reqwest::Method::GET);
        assert_eq!(request.url().path(), "/v1/services");
        let query = request
            .url()
            .query_pairs()
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(
            query.get("repository_id").unwrap(),
            "source.example/acme/commerce"
        );
        assert_eq!(query.get("limit").unwrap(), "100");
        assert_eq!(query.get("cursor").unwrap(), "next-page");
        let expected_authorization = ["Bearer", "developer-session-token"].join(" ");
        assert_eq!(
            request.headers()[reqwest::header::AUTHORIZATION],
            expected_authorization
        );
        assert_eq!(
            request.timeout(),
            Some(&Duration::from_secs(CLOUD_API_REQUEST_TIMEOUT.as_secs()))
        );
    }

    #[test]
    fn release_job_http_requests_use_exact_authenticated_routes_and_bounded_bodies() {
        let client = cloud_client();
        let request = release_job_create_request();
        let create = client
            .release_job_create_request(request.project_id, &request)
            .unwrap()
            .build()
            .unwrap();

        assert_eq!(create.method(), reqwest::Method::POST);
        assert_eq!(
            create.url().path(),
            "/v1/projects/prj_01890f3e-7b1c-7cc0-8a1b-123456789ac0/release-jobs"
        );
        assert_eq!(
            create.headers()[reqwest::header::CONTENT_TYPE],
            "application/json"
        );
        let expected_authorization = ["Bearer", "developer-session-token"].join(" ");
        assert_eq!(
            create.headers()[reqwest::header::AUTHORIZATION],
            expected_authorization
        );
        assert_eq!(
            create.timeout(),
            Some(&Duration::from_secs(CLOUD_API_REQUEST_TIMEOUT.as_secs()))
        );
        let body: ReleaseJobCreateRequest = canonical::parse_strict(
            create
                .body()
                .and_then(reqwest::Body::as_bytes)
                .expect("release-job request body"),
        )
        .unwrap();
        assert_eq!(body, request);

        let release_job_id: ReleaseJobId =
            "rev_01890f3e-7b1c-7cc0-8a1b-123456789abd".parse().unwrap();
        let get = client
            .release_job_get_request(release_job_id)
            .build()
            .unwrap();
        assert_eq!(get.method(), reqwest::Method::GET);
        assert_eq!(
            get.url().path(),
            "/v1/release-jobs/rev_01890f3e-7b1c-7cc0-8a1b-123456789abd"
        );
        assert_eq!(
            get.headers()[reqwest::header::AUTHORIZATION],
            expected_authorization
        );
        assert!(get.body().is_none());
        assert_eq!(
            release_job_path(release_job_id),
            "/v1/release-jobs/rev_01890f3e-7b1c-7cc0-8a1b-123456789abd"
        );
    }

    #[tokio::test]
    async fn release_job_create_rejects_invalid_scope_and_body_before_network_access() {
        let client = cloud_client();
        let mut request = release_job_create_request();
        request.idempotency_key = "too-short".to_owned();
        assert_schema_invalid(
            &client
                .create_release_job(request.project_id, &request)
                .await
                .unwrap_err(),
        );

        let request = release_job_create_request();
        let other_project: ProjectId = "prj_01890f3e-7b1c-7cc0-8a1b-123456789abe".parse().unwrap();
        assert_schema_invalid(
            &client
                .create_release_job(other_project, &request)
                .await
                .unwrap_err(),
        );
    }

    #[tokio::test]
    async fn onboarding_rejects_invalid_requests_before_network_access() {
        let client = cloud_client();
        let project_id: ProjectId = "prj_01890f3e-7b1c-7cc0-8a1b-123456789abe".parse().unwrap();
        let token_id =
            ProjectTokenId::new("ptk_01890f3e-7b1c-7cc0-8a1b-123456789ac4".to_owned()).unwrap();

        let project = ProjectCreateRequest {
            organization_name: "Acme".to_owned(),
            project_name: "commerce".to_owned(),
        };
        assert_schema_invalid(&client.create_project(&project).await.unwrap_err());

        let service = ServiceCreateRequest {
            repository_id: "source.example/acme/commerce repo".to_owned(),
            service_name: "payments".to_owned(),
        };
        assert_schema_invalid(
            &client
                .create_service(project_id, &service)
                .await
                .unwrap_err(),
        );

        let issue = ProjectTokenIssueRequest {
            expires_in_seconds: 2_592_001,
            name: "production-capture".to_owned(),
            service_id: "svc_01890f3e-7b1c-7cc0-8a1b-123456789abf".parse().unwrap(),
        };
        assert_schema_invalid(
            &client
                .issue_project_token(project_id, &issue)
                .await
                .err()
                .unwrap(),
        );

        let rotate = ProjectTokenRotateRequest {
            expires_in_seconds: 86_400,
            token_revision: 0,
        };
        assert_schema_invalid(
            &client
                .rotate_project_token(project_id, &token_id, &rotate)
                .await
                .err()
                .unwrap(),
        );

        let revoke = ProjectTokenRevokeRequest { token_revision: 0 };
        assert_schema_invalid(
            &client
                .revoke_project_token(project_id, &token_id, &revoke)
                .await
                .unwrap_err(),
        );

        let query = ServiceCatalogQuery {
            cursor: None,
            limit: Some(0),
            repository_id: "source.example/acme/commerce".to_owned(),
        };
        assert_schema_invalid(&client.list_services_page(&query).await.unwrap_err());
    }

    fn cloud_client() -> HttpCloudClient {
        HttpCloudClient::new(
            "https://cloud.example",
            SecretString::from("developer-session-token".to_owned()),
        )
        .unwrap()
    }

    fn release_job_create_request() -> ReleaseJobCreateRequest {
        canonical::parse_strict(
            &serde_json::to_vec(&serde_json::json!({
                "baseline_artifact_digest": concat!(
                    "sha256:",
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                ),
                "candidate_artifact_digest": concat!(
                    "sha256:",
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                ),
                "dataset_digest": concat!(
                    "sha256:",
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                ),
                "evaluator_digest": concat!(
                    "sha256:",
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                ),
                "idempotency_key": "release-test-key",
                "organization_id": "org_01890f3e-7b1c-7cc0-8a1b-123456789abc",
                "primary_evidence": {
                    "environment_digest": concat!(
                        "sha256:",
                        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    ),
                    "evidence_digest": concat!(
                        "sha256:",
                        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                    ),
                    "runner_digest": concat!(
                        "sha256:",
                        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    )
                },
                "project_id": "prj_01890f3e-7b1c-7cc0-8a1b-123456789ac0",
                "service_id": "svc_01890f3e-7b1c-7cc0-8a1b-123456789ac1"
            }))
            .unwrap(),
        )
        .unwrap()
    }

    fn assert_schema_invalid(error: &reproit_core::Error) {
        assert_eq!(error.code, ErrorCode::SchemaInvalid);
    }
}
