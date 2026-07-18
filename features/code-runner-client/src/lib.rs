//! Client-side policy boundary for a separately operated code runner.
//!
//! This crate never evaluates code, starts a process, talks to a container
//! runtime, or claims that the remote service is safely sandboxed. It only
//! prepares bounded requests for operator-approved immutable profiles and
//! exchanges them with an authenticated remote broker. A deployment still has
//! to satisfy `docs/security/CODE-RUNNER.md` before advertising the capability.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    net::{Ipv4Addr, Ipv6Addr},
    time::Duration,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::{
    Client, RequestBuilder, Response, StatusCode,
    header::{ACCEPT, AUTHORIZATION, HeaderValue},
    redirect::Policy,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use url::{Host, Url};
use uuid::Uuid;

pub const RUNNER_PROTOCOL_VERSION: &str = "1";
pub const READINESS_PATH: &str = "v1/health/ready";
pub const JOBS_PATH: &str = "v1/jobs";

const MIN_WALL_TIME_MS: u64 = 100;
const MAX_WALL_TIME_MS: u64 = 60_000;
const MIN_CPU_TIME_MS: u64 = 10;
const MAX_CPU_TIME_MS: u64 = 60_000;
const MIN_MEMORY_BYTES: u64 = 16 * 1024 * 1024;
const MAX_MEMORY_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MIN_OUTPUT_BYTES: u64 = 1024;
const MAX_OUTPUT_BYTES: u64 = 4 * 1024 * 1024;
const MIN_PROCESS_LIMIT: u32 = 1;
const MAX_PROCESS_LIMIT: u32 = 256;
const MAX_SOURCE_BYTES: usize = 1024 * 1024;
const MIN_REQUEST_TIMEOUT: Duration = Duration::from_millis(100);
const MAX_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MIN_RESPONSE_BYTES: usize = 1024;
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MIN_JOB_TTL: Duration = Duration::from_secs(5);
const MAX_JOB_TTL: Duration = Duration::from_secs(5 * 60);

/// V1 intentionally has no network-enabled variant.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPolicy {
    #[default]
    Denied,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunLimits {
    wall_time_ms: u64,
    cpu_time_ms: u64,
    memory_bytes: u64,
    output_bytes: u64,
    process_limit: u32,
    #[serde(default)]
    network: NetworkPolicy,
}

impl RunLimits {
    pub fn new(
        wall_time_ms: u64,
        cpu_time_ms: u64,
        memory_bytes: u64,
        output_bytes: u64,
        process_limit: u32,
    ) -> Result<Self, RunnerError> {
        let limits = Self {
            wall_time_ms,
            cpu_time_ms,
            memory_bytes,
            output_bytes,
            process_limit,
            network: NetworkPolicy::Denied,
        };
        limits.validate_global()?;
        Ok(limits)
    }

    pub const fn wall_time_ms(&self) -> u64 {
        self.wall_time_ms
    }

    pub const fn cpu_time_ms(&self) -> u64 {
        self.cpu_time_ms
    }

    pub const fn memory_bytes(&self) -> u64 {
        self.memory_bytes
    }

    pub const fn output_bytes(&self) -> u64 {
        self.output_bytes
    }

    pub const fn process_limit(&self) -> u32 {
        self.process_limit
    }

    pub const fn network(&self) -> NetworkPolicy {
        self.network
    }

    fn validate_global(&self) -> Result<(), RunnerError> {
        if !(MIN_WALL_TIME_MS..=MAX_WALL_TIME_MS).contains(&self.wall_time_ms)
            || !(MIN_CPU_TIME_MS..=MAX_CPU_TIME_MS).contains(&self.cpu_time_ms)
            || self.cpu_time_ms > self.wall_time_ms
            || !(MIN_MEMORY_BYTES..=MAX_MEMORY_BYTES).contains(&self.memory_bytes)
            || !(MIN_OUTPUT_BYTES..=MAX_OUTPUT_BYTES).contains(&self.output_bytes)
            || !(MIN_PROCESS_LIMIT..=MAX_PROCESS_LIMIT).contains(&self.process_limit)
            || self.network != NetworkPolicy::Denied
        {
            return Err(RunnerError::LimitsExceeded);
        }
        Ok(())
    }

    fn validate_against(&self, maximum: &Self) -> Result<(), RunnerError> {
        self.validate_global()?;
        maximum.validate_global()?;
        if self.wall_time_ms > maximum.wall_time_ms
            || self.cpu_time_ms > maximum.cpu_time_ms
            || self.memory_bytes > maximum.memory_bytes
            || self.output_bytes > maximum.output_bytes
            || self.process_limit > maximum.process_limit
        {
            return Err(RunnerError::LimitsExceeded);
        }
        Ok(())
    }
}

impl Default for RunLimits {
    fn default() -> Self {
        Self {
            wall_time_ms: 10_000,
            cpu_time_ms: 5_000,
            memory_bytes: 256 * 1024 * 1024,
            output_bytes: 256 * 1024,
            process_limit: 32,
            network: NetworkPolicy::Denied,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputMode {
    Console,
    WebPreview,
}

/// An operator-approved immutable execution profile.
///
/// `digest` identifies the complete remote profile policy and execution image,
/// not merely a mutable container tag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunnerProfile {
    id: String,
    digest: String,
    fence_aliases: BTreeSet<String>,
    output_mode: OutputMode,
    maximum_limits: RunLimits,
    maximum_source_bytes: usize,
}

impl RunnerProfile {
    pub fn new(
        id: impl Into<String>,
        digest: impl Into<String>,
        fence_aliases: impl IntoIterator<Item = impl Into<String>>,
        output_mode: OutputMode,
        maximum_limits: RunLimits,
        maximum_source_bytes: usize,
    ) -> Result<Self, RunnerError> {
        let profile = Self {
            id: id.into(),
            digest: digest.into(),
            fence_aliases: fence_aliases.into_iter().map(Into::into).collect(),
            output_mode,
            maximum_limits,
            maximum_source_bytes,
        };
        profile.validate()?;
        Ok(profile)
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }

    pub fn fence_aliases(&self) -> &BTreeSet<String> {
        &self.fence_aliases
    }

    pub const fn output_mode(&self) -> OutputMode {
        self.output_mode
    }

    pub fn maximum_limits(&self) -> &RunLimits {
        &self.maximum_limits
    }

    pub const fn maximum_source_bytes(&self) -> usize {
        self.maximum_source_bytes
    }

    fn validate(&self) -> Result<(), RunnerError> {
        if !safe_identifier(&self.id)
            || !valid_sha256_digest(&self.digest)
            || self.fence_aliases.is_empty()
            || self
                .fence_aliases
                .iter()
                .any(|alias| !safe_identifier(alias) || alias.starts_with("language-"))
            || !(1..=MAX_SOURCE_BYTES).contains(&self.maximum_source_bytes)
        {
            return Err(RunnerError::InvalidProfile);
        }
        self.maximum_limits.validate_global()
    }
}

#[derive(Debug, Clone)]
pub struct ProfileRegistry {
    profiles: BTreeMap<String, RunnerProfile>,
    aliases: BTreeMap<String, String>,
}

impl ProfileRegistry {
    pub fn new(profiles: impl IntoIterator<Item = RunnerProfile>) -> Result<Self, RunnerError> {
        let mut by_id = BTreeMap::new();
        let mut aliases = BTreeMap::new();
        for profile in profiles {
            profile.validate()?;
            if by_id.contains_key(&profile.id) {
                return Err(RunnerError::DuplicateProfile);
            }
            for alias in &profile.fence_aliases {
                if aliases.insert(alias.clone(), profile.id.clone()).is_some() {
                    return Err(RunnerError::DuplicateProfileAlias);
                }
            }
            by_id.insert(profile.id.clone(), profile);
        }
        if by_id.is_empty() {
            return Err(RunnerError::InvalidProfile);
        }
        Ok(Self {
            profiles: by_id,
            aliases,
        })
    }

    pub fn profile(&self, id: &str) -> Result<&RunnerProfile, RunnerError> {
        self.profiles.get(id).ok_or(RunnerError::UnapprovedProfile)
    }

    /// Resolves `rust`, `language-rust`, or a code element class list such as
    /// `language-rust hljs`. Only aliases approved by the operator are returned.
    pub fn resolve_fence_alias(&self, value: &str) -> Option<&RunnerProfile> {
        let alias = value
            .split_ascii_whitespace()
            .find_map(|part| part.strip_prefix("language-"))
            .or_else(|| (!value.chars().any(char::is_whitespace)).then_some(value))?;
        let profile_id = self.aliases.get(alias)?;
        self.profiles.get(profile_id)
    }

    pub fn profiles(&self) -> impl Iterator<Item = &RunnerProfile> {
        self.profiles.values()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct BearerToken(String);

impl BearerToken {
    pub fn new(value: impl Into<String>) -> Result<Self, RunnerError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 4096
            || !value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric()
                    || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'+' | b'/' | b'=')
            })
        {
            return Err(RunnerError::InvalidConfiguration);
        }
        Ok(Self(value))
    }

    fn header_value(&self) -> Result<HeaderValue, RunnerError> {
        let mut value = HeaderValue::from_str(&format!("Bearer {}", self.0))
            .map_err(|_| RunnerError::InvalidConfiguration)?;
        value.set_sensitive(true);
        Ok(value)
    }
}

impl fmt::Debug for BearerToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BearerToken([REDACTED])")
    }
}

#[derive(Clone)]
pub struct RemoteRunnerConfig {
    endpoint: Url,
    bearer_token: Option<BearerToken>,
    request_timeout: Duration,
    maximum_response_bytes: usize,
    job_ttl: Duration,
}

impl RemoteRunnerConfig {
    pub fn new(mut endpoint: Url) -> Result<Self, RunnerError> {
        validate_endpoint(&endpoint)?;
        if !endpoint.path().ends_with('/') {
            let path = format!("{}/", endpoint.path());
            endpoint.set_path(&path);
        }
        Ok(Self {
            endpoint,
            bearer_token: None,
            request_timeout: Duration::from_secs(10),
            maximum_response_bytes: 1024 * 1024,
            job_ttl: Duration::from_secs(60),
        })
    }

    pub fn with_bearer_token(mut self, token: BearerToken) -> Self {
        self.bearer_token = Some(token);
        self
    }

    pub fn with_request_timeout(mut self, timeout: Duration) -> Result<Self, RunnerError> {
        self.request_timeout = timeout;
        self.validate()?;
        Ok(self)
    }

    pub fn with_maximum_response_bytes(mut self, maximum: usize) -> Result<Self, RunnerError> {
        self.maximum_response_bytes = maximum;
        self.validate()?;
        Ok(self)
    }

    pub fn with_job_ttl(mut self, ttl: Duration) -> Result<Self, RunnerError> {
        self.job_ttl = ttl;
        self.validate()?;
        Ok(self)
    }

    pub fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    fn validate(&self) -> Result<(), RunnerError> {
        validate_endpoint(&self.endpoint)?;
        if !(MIN_REQUEST_TIMEOUT..=MAX_REQUEST_TIMEOUT).contains(&self.request_timeout)
            || !(MIN_RESPONSE_BYTES..=MAX_RESPONSE_BYTES).contains(&self.maximum_response_bytes)
            || !(MIN_JOB_TTL..=MAX_JOB_TTL).contains(&self.job_ttl)
        {
            return Err(RunnerError::InvalidConfiguration);
        }
        Ok(())
    }
}

impl fmt::Debug for RemoteRunnerConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteRunnerConfig")
            .field("endpoint", &self.endpoint)
            .field(
                "bearer_token",
                &self.bearer_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("request_timeout", &self.request_timeout)
            .field("maximum_response_bytes", &self.maximum_response_bytes)
            .field("job_ttl", &self.job_ttl)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmissionContext {
    site_id: Uuid,
    principal_id: String,
}

impl SubmissionContext {
    pub fn new(site_id: Uuid, principal_id: impl Into<String>) -> Result<Self, RunnerError> {
        let context = Self {
            site_id,
            principal_id: principal_id.into(),
        };
        context.validate()?;
        Ok(context)
    }

    pub const fn site_id(&self) -> Uuid {
        self.site_id
    }

    pub fn principal_id(&self) -> &str {
        &self.principal_id
    }

    fn validate(&self) -> Result<(), RunnerError> {
        if self.site_id.is_nil()
            || self.principal_id.is_empty()
            || self.principal_id.len() > 200
            || self.principal_id.contains('\0')
            || self.principal_id.chars().any(char::is_control)
        {
            return Err(RunnerError::InvalidRequest);
        }
        Ok(())
    }
}

/// Complete request sent to the broker. IDs, timestamps, expiry and
/// idempotency are generated inside `RemoteRunnerClient`, never accepted from
/// article content or a browser request.
#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRequest {
    request_id: Uuid,
    attempt_id: Uuid,
    idempotency_key: Uuid,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    site_id: Uuid,
    principal_id: String,
    profile_id: String,
    profile_digest: String,
    output_mode: OutputMode,
    source: String,
    source_digest: String,
    limits: RunLimits,
}

impl fmt::Debug for RunRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RunRequest")
            .field("request_id", &self.request_id)
            .field("attempt_id", &self.attempt_id)
            .field("idempotency_key", &self.idempotency_key)
            .field("created_at", &self.created_at)
            .field("expires_at", &self.expires_at)
            .field("site_id", &self.site_id)
            .field("principal_id", &self.principal_id)
            .field("profile_id", &self.profile_id)
            .field("profile_digest", &self.profile_digest)
            .field("output_mode", &self.output_mode)
            .field("source", &"[REDACTED]")
            .field("source_digest", &self.source_digest)
            .field("limits", &self.limits)
            .finish()
    }
}

impl RunRequest {
    pub const fn request_id(&self) -> Uuid {
        self.request_id
    }

    pub const fn attempt_id(&self) -> Uuid {
        self.attempt_id
    }

    pub const fn idempotency_key(&self) -> Uuid {
        self.idempotency_key
    }

    pub fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProfileIdentity {
    pub id: String,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerReadiness {
    pub ready: bool,
    pub protocol_version: String,
    pub approved_profiles: Vec<ProfileIdentity>,
}

impl RunnerReadiness {
    fn supports(&self, profile: &RunnerProfile) -> bool {
        self.ready
            && self
                .approved_profiles
                .iter()
                .any(|candidate| candidate.id == profile.id && candidate.digest == profile.digest)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalOutcome {
    Succeeded,
    Failed,
    TimedOut,
    ResourceLimitExceeded,
    Cancelled,
    PolicyRejected,
    RunnerLost,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QueuedRun {
    job_id: Uuid,
    request_id: Uuid,
    attempt_id: Uuid,
    profile_id: String,
    profile_digest: String,
    accepted_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    poll_after_ms: u64,
    output_limit_bytes: u64,
}

impl QueuedRun {
    pub const fn job_id(&self) -> Uuid {
        self.job_id
    }

    pub const fn request_id(&self) -> Uuid {
        self.request_id
    }

    pub const fn poll_after_ms(&self) -> u64 {
        self.poll_after_ms
    }

    pub fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TerminalRun {
    pub job_id: Uuid,
    pub request_id: Uuid,
    pub attempt_id: Uuid,
    pub profile_id: String,
    pub profile_digest: String,
    pub completed_at: DateTime<Utc>,
    pub outcome: TerminalOutcome,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunSubmissionResult {
    Queued(QueuedRun),
    Terminal(TerminalRun),
}

#[async_trait]
pub trait CodeRunnerClient: Send + Sync {
    async fn readiness(&self) -> Result<RunnerReadiness, RunnerError>;

    async fn submit(
        &self,
        context: &SubmissionContext,
        profile_id: &str,
        source: &str,
        limits: RunLimits,
    ) -> Result<RunSubmissionResult, RunnerError>;

    async fn poll(&self, queued: &QueuedRun) -> Result<RunSubmissionResult, RunnerError>;
}

pub struct DisabledRunner;

#[async_trait]
impl CodeRunnerClient for DisabledRunner {
    async fn readiness(&self) -> Result<RunnerReadiness, RunnerError> {
        Err(RunnerError::Unavailable)
    }

    async fn submit(
        &self,
        _context: &SubmissionContext,
        _profile_id: &str,
        _source: &str,
        _limits: RunLimits,
    ) -> Result<RunSubmissionResult, RunnerError> {
        Err(RunnerError::Unavailable)
    }

    async fn poll(&self, _queued: &QueuedRun) -> Result<RunSubmissionResult, RunnerError> {
        Err(RunnerError::Unavailable)
    }
}

pub struct RemoteRunnerClient {
    client: Client,
    config: RemoteRunnerConfig,
    registry: ProfileRegistry,
}

impl RemoteRunnerClient {
    pub fn new(config: RemoteRunnerConfig, registry: ProfileRegistry) -> Result<Self, RunnerError> {
        config.validate()?;
        let client = Client::builder()
            .redirect(Policy::none())
            .connect_timeout(config.request_timeout)
            .timeout(config.request_timeout)
            .user_agent(concat!(
                "OpenSoverignBlog-runner-client/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()
            .map_err(|_| RunnerError::InvalidConfiguration)?;
        Ok(Self {
            client,
            config,
            registry,
        })
    }

    pub fn profiles(&self) -> &ProfileRegistry {
        &self.registry
    }

    fn prepare_request(
        &self,
        context: &SubmissionContext,
        profile: &RunnerProfile,
        source: &str,
        limits: RunLimits,
    ) -> Result<RunRequest, RunnerError> {
        context.validate()?;
        if source.is_empty() || source.len() > profile.maximum_source_bytes || source.contains('\0')
        {
            return Err(RunnerError::InvalidRequest);
        }
        limits.validate_against(&profile.maximum_limits)?;
        let created_at = Utc::now();
        let ttl = chrono::Duration::from_std(self.config.job_ttl)
            .map_err(|_| RunnerError::InvalidConfiguration)?;
        Ok(RunRequest {
            request_id: Uuid::now_v7(),
            attempt_id: Uuid::now_v7(),
            idempotency_key: Uuid::now_v7(),
            created_at,
            expires_at: created_at + ttl,
            site_id: context.site_id,
            principal_id: context.principal_id.clone(),
            profile_id: profile.id.clone(),
            profile_digest: profile.digest.clone(),
            output_mode: profile.output_mode,
            source: source.to_owned(),
            source_digest: format!("sha256:{:x}", Sha256::digest(source.as_bytes())),
            limits,
        })
    }

    fn endpoint(&self, path: &str) -> Result<Url, RunnerError> {
        self.config
            .endpoint
            .join(path)
            .map_err(|_| RunnerError::InvalidConfiguration)
    }

    fn authorize(&self, request: RequestBuilder) -> Result<RequestBuilder, RunnerError> {
        match self.config.bearer_token.as_ref() {
            Some(token) => Ok(request.header(AUTHORIZATION, token.header_value()?)),
            None => Ok(request),
        }
    }

    async fn send<T: DeserializeOwned>(&self, request: RequestBuilder) -> Result<T, RunnerError> {
        let response = request.send().await.map_err(map_transport_error)?;
        map_status(response.status())?;
        let bytes = read_bounded(response, self.config.maximum_response_bytes).await?;
        serde_json::from_slice(&bytes).map_err(|_| RunnerError::InvalidBrokerResponse)
    }

    async fn broker_readiness(&self) -> Result<BrokerReadiness, RunnerError> {
        let request = self
            .client
            .get(self.endpoint(READINESS_PATH)?)
            .header(ACCEPT, HeaderValue::from_static("application/json"));
        self.send(self.authorize(request)?).await
    }

    fn normalize_readiness(
        &self,
        response: BrokerReadiness,
    ) -> Result<RunnerReadiness, RunnerError> {
        if response.protocol_version != RUNNER_PROTOCOL_VERSION {
            return Err(RunnerError::ProtocolMismatch);
        }
        let mut seen = BTreeSet::new();
        for profile in &response.profiles {
            if !safe_identifier(&profile.id)
                || !valid_sha256_digest(&profile.digest)
                || !seen.insert(profile.id.clone())
            {
                return Err(RunnerError::InvalidBrokerResponse);
            }
        }
        let approved_profiles: Vec<_> = response
            .profiles
            .into_iter()
            .filter(|candidate| {
                self.registry
                    .profile(&candidate.id)
                    .is_ok_and(|approved| approved.digest == candidate.digest)
            })
            .collect();
        Ok(RunnerReadiness {
            ready: response.ready && !approved_profiles.is_empty(),
            protocol_version: response.protocol_version,
            approved_profiles,
        })
    }

    fn validate_broker_result(
        &self,
        response: BrokerRunResponse,
        expected: ExpectedRun<'_>,
    ) -> Result<RunSubmissionResult, RunnerError> {
        match response {
            BrokerRunResponse::Queued {
                job_id,
                request_id,
                attempt_id,
                profile_id,
                profile_digest,
                accepted_at,
                expires_at,
                poll_after_ms,
            } => {
                expected.validate(request_id, attempt_id, &profile_id, &profile_digest)?;
                if job_id.is_nil()
                    || !(100..=60_000).contains(&poll_after_ms)
                    || expires_at <= accepted_at
                    || expected
                        .maximum_expiry
                        .is_some_and(|maximum| expires_at > maximum)
                {
                    return Err(RunnerError::InvalidBrokerResponse);
                }
                Ok(RunSubmissionResult::Queued(QueuedRun {
                    job_id,
                    request_id,
                    attempt_id,
                    profile_id,
                    profile_digest,
                    accepted_at,
                    expires_at,
                    poll_after_ms,
                    output_limit_bytes: expected.output_limit_bytes,
                }))
            }
            BrokerRunResponse::Terminal {
                job_id,
                request_id,
                attempt_id,
                profile_id,
                profile_digest,
                completed_at,
                outcome,
                exit_code,
                stdout,
                stderr,
                truncated,
            } => {
                expected.validate(request_id, attempt_id, &profile_id, &profile_digest)?;
                let output_size = stdout.len().saturating_add(stderr.len()) as u64;
                if job_id.is_nil() || output_size > expected.output_limit_bytes {
                    return Err(RunnerError::InvalidBrokerResponse);
                }
                Ok(RunSubmissionResult::Terminal(TerminalRun {
                    job_id,
                    request_id,
                    attempt_id,
                    profile_id,
                    profile_digest,
                    completed_at,
                    outcome,
                    exit_code,
                    stdout: strip_terminal_controls(&stdout),
                    stderr: strip_terminal_controls(&stderr),
                    truncated,
                }))
            }
        }
    }
}

#[async_trait]
impl CodeRunnerClient for RemoteRunnerClient {
    async fn readiness(&self) -> Result<RunnerReadiness, RunnerError> {
        let response = self.broker_readiness().await?;
        self.normalize_readiness(response)
    }

    async fn submit(
        &self,
        context: &SubmissionContext,
        profile_id: &str,
        source: &str,
        limits: RunLimits,
    ) -> Result<RunSubmissionResult, RunnerError> {
        let profile = self.registry.profile(profile_id)?;
        let request = self.prepare_request(context, profile, source, limits)?;
        let readiness = self.readiness().await?;
        if !readiness.supports(profile) {
            return Err(RunnerError::ProfileNotReady);
        }
        let expected = ExpectedRun {
            request_id: request.request_id,
            attempt_id: request.attempt_id,
            profile_id: &request.profile_id,
            profile_digest: &request.profile_digest,
            output_limit_bytes: request.limits.output_bytes,
            maximum_expiry: Some(request.expires_at),
        };
        let builder = self.client.post(self.endpoint(JOBS_PATH)?).json(&request);
        let response = self.send(self.authorize(builder)?).await?;
        self.validate_broker_result(response, expected)
    }

    async fn poll(&self, queued: &QueuedRun) -> Result<RunSubmissionResult, RunnerError> {
        if queued.expires_at <= Utc::now() {
            return Err(RunnerError::RequestExpired);
        }
        let profile = self.registry.profile(&queued.profile_id)?;
        if profile.digest != queued.profile_digest
            || queued.job_id.is_nil()
            || queued.request_id.is_nil()
            || queued.attempt_id.is_nil()
            || !(MIN_OUTPUT_BYTES..=profile.maximum_limits.output_bytes)
                .contains(&queued.output_limit_bytes)
        {
            return Err(RunnerError::InvalidRequest);
        }
        let path = format!("{JOBS_PATH}/{}", queued.job_id);
        let builder = self.client.get(self.endpoint(&path)?);
        let response = self.send(self.authorize(builder)?).await?;
        self.validate_broker_result(
            response,
            ExpectedRun {
                request_id: queued.request_id,
                attempt_id: queued.attempt_id,
                profile_id: &queued.profile_id,
                profile_digest: &queued.profile_digest,
                output_limit_bytes: queued.output_limit_bytes,
                maximum_expiry: Some(queued.expires_at),
            },
        )
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BrokerReadiness {
    protocol_version: String,
    ready: bool,
    profiles: Vec<ProfileIdentity>,
}

#[derive(Debug, Deserialize)]
#[serde(
    tag = "state",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum BrokerRunResponse {
    Queued {
        job_id: Uuid,
        request_id: Uuid,
        attempt_id: Uuid,
        profile_id: String,
        profile_digest: String,
        accepted_at: DateTime<Utc>,
        expires_at: DateTime<Utc>,
        poll_after_ms: u64,
    },
    Terminal {
        job_id: Uuid,
        request_id: Uuid,
        attempt_id: Uuid,
        profile_id: String,
        profile_digest: String,
        completed_at: DateTime<Utc>,
        outcome: TerminalOutcome,
        exit_code: Option<i32>,
        stdout: String,
        stderr: String,
        truncated: bool,
    },
}

struct ExpectedRun<'a> {
    request_id: Uuid,
    attempt_id: Uuid,
    profile_id: &'a str,
    profile_digest: &'a str,
    output_limit_bytes: u64,
    maximum_expiry: Option<DateTime<Utc>>,
}

impl ExpectedRun<'_> {
    fn validate(
        &self,
        request_id: Uuid,
        attempt_id: Uuid,
        profile_id: &str,
        profile_digest: &str,
    ) -> Result<(), RunnerError> {
        if request_id != self.request_id
            || attempt_id != self.attempt_id
            || profile_id != self.profile_id
            || profile_digest != self.profile_digest
        {
            return Err(RunnerError::InvalidBrokerResponse);
        }
        Ok(())
    }
}

async fn read_bounded(mut response: Response, maximum: usize) -> Result<Vec<u8>, RunnerError> {
    if response
        .content_length()
        .is_some_and(|length| length > maximum as u64)
    {
        return Err(RunnerError::ResponseTooLarge);
    }
    let mut body =
        Vec::with_capacity(response.content_length().unwrap_or(0).min(maximum as u64) as usize);
    while let Some(chunk) = response.chunk().await.map_err(map_transport_error)? {
        if body.len().saturating_add(chunk.len()) > maximum {
            return Err(RunnerError::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn map_status(status: StatusCode) -> Result<(), RunnerError> {
    if status.is_success() {
        Ok(())
    } else if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        Err(RunnerError::AuthenticationFailed)
    } else if status == StatusCode::REQUEST_TIMEOUT || status == StatusCode::GATEWAY_TIMEOUT {
        Err(RunnerError::Timeout)
    } else {
        Err(RunnerError::RemoteFailure)
    }
}

fn map_transport_error(error: reqwest::Error) -> RunnerError {
    if error.is_timeout() {
        RunnerError::Timeout
    } else {
        RunnerError::RemoteFailure
    }
}

fn validate_endpoint(endpoint: &Url) -> Result<(), RunnerError> {
    if endpoint.cannot_be_a_base()
        || endpoint.host().is_none()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
        || endpoint.port() == Some(0)
    {
        return Err(RunnerError::UnsafeEndpoint);
    }
    match endpoint.scheme() {
        "https" => {
            if endpoint.host().is_some_and(unsafe_literal_destination) {
                return Err(RunnerError::UnsafeEndpoint);
            }
        }
        "http" if endpoint.host().is_some_and(loopback_host) => {}
        _ => return Err(RunnerError::UnsafeEndpoint),
    }
    Ok(())
}

fn loopback_host(host: Host<&str>) -> bool {
    match host {
        Host::Domain(domain) => domain.eq_ignore_ascii_case("localhost"),
        Host::Ipv4(address) => address.is_loopback(),
        Host::Ipv6(address) => address.is_loopback(),
    }
}

fn unsafe_literal_destination(host: Host<&str>) -> bool {
    match host {
        Host::Domain(_) => false,
        Host::Ipv4(address) => unsafe_ipv4(address),
        Host::Ipv6(address) => unsafe_ipv6(address),
    }
}

fn unsafe_ipv4(address: Ipv4Addr) -> bool {
    address.is_unspecified()
        || address.is_link_local()
        || address.is_multicast()
        || address == Ipv4Addr::BROADCAST
}

fn unsafe_ipv6(address: Ipv6Addr) -> bool {
    address.is_unspecified()
        || address.is_multicast()
        || is_ipv6_link_local(address)
        || address.to_ipv4_mapped().is_some_and(unsafe_ipv4)
}

fn is_ipv6_link_local(address: Ipv6Addr) -> bool {
    let first = address.segments()[0];
    first & 0xffc0 == 0xfe80
}

fn safe_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || (index > 0 && matches!(byte, b'-' | b'_' | b'.'))
        })
}

fn valid_sha256_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    })
}

fn strip_terminal_controls(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut characters = value.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' {
            if characters.next_if_eq(&'[').is_some() {
                for next in characters.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            continue;
        }
        if character == '\n' || character == '\r' || character == '\t' || !character.is_control() {
            output.push(character);
        }
    }
    output
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RunnerError {
    #[error("code runner capability is unavailable")]
    Unavailable,
    #[error("runner client configuration is invalid")]
    InvalidConfiguration,
    #[error("runner endpoint is not an approved HTTPS or loopback HTTP endpoint")]
    UnsafeEndpoint,
    #[error("execution profile is invalid")]
    InvalidProfile,
    #[error("execution profile id is duplicated")]
    DuplicateProfile,
    #[error("code fence alias is assigned to multiple profiles")]
    DuplicateProfileAlias,
    #[error("execution profile is not approved by this installation")]
    UnapprovedProfile,
    #[error("approved execution profile is not ready at the broker")]
    ProfileNotReady,
    #[error("code run request is invalid")]
    InvalidRequest,
    #[error("code run exceeds the installation profile limits")]
    LimitsExceeded,
    #[error("queued code run has expired")]
    RequestExpired,
    #[error("runner authentication failed")]
    AuthenticationFailed,
    #[error("runner request timed out")]
    Timeout,
    #[error("runner response exceeded the configured byte limit")]
    ResponseTooLarge,
    #[error("runner protocol version does not match")]
    ProtocolMismatch,
    #[error("runner returned an invalid or mismatched response")]
    InvalidBrokerResponse,
    #[error("remote runner request failed")]
    RemoteFailure,
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::{
        Json, Router,
        extract::State,
        http::{HeaderMap, StatusCode},
        routing::{get, post},
    };
    use serde_json::{Value, json};

    use super::*;

    const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn profile() -> RunnerProfile {
        RunnerProfile::new(
            "rust-stable",
            DIGEST,
            ["rust", "rs"],
            OutputMode::Console,
            RunLimits::default(),
            64 * 1024,
        )
        .unwrap()
    }

    fn registry() -> ProfileRegistry {
        ProfileRegistry::new([profile()]).unwrap()
    }

    #[test]
    fn endpoint_rejects_non_loopback_http_and_credential_or_metadata_tricks() {
        for unsafe_url in [
            "http://example.com/",
            "http://10.0.0.8/",
            "http://169.254.169.254/latest/meta-data/",
            "https://169.254.169.254/",
            "https://user:secret@runner.example/",
            "https://runner.example/?next=http://127.0.0.1",
            "ftp://runner.example/",
        ] {
            assert_eq!(
                RemoteRunnerConfig::new(Url::parse(unsafe_url).unwrap()).unwrap_err(),
                RunnerError::UnsafeEndpoint,
                "{unsafe_url}"
            );
        }
        assert!(RemoteRunnerConfig::new(Url::parse("http://127.0.0.1:8788/").unwrap()).is_ok());
        assert!(RemoteRunnerConfig::new(Url::parse("http://[::1]:8788/").unwrap()).is_ok());
        assert!(
            RemoteRunnerConfig::new(Url::parse("https://runner.example/api/").unwrap()).is_ok()
        );
    }

    #[test]
    fn aliases_resolve_only_to_approved_immutable_profiles() {
        let profiles = registry();
        assert_eq!(
            profiles.resolve_fence_alias("rust").unwrap().id(),
            "rust-stable"
        );
        assert_eq!(
            profiles
                .resolve_fence_alias("language-rust hljs")
                .unwrap()
                .digest(),
            DIGEST
        );
        assert!(profiles.resolve_fence_alias("language-kotlin").is_none());

        let conflicting = RunnerProfile::new(
            "rust-nightly",
            "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            ["rust"],
            OutputMode::Console,
            RunLimits::default(),
            1024,
        )
        .unwrap();
        assert_eq!(
            ProfileRegistry::new([profile(), conflicting]).unwrap_err(),
            RunnerError::DuplicateProfileAlias
        );
    }

    #[test]
    fn zero_and_excessive_limits_are_rejected() {
        assert_eq!(
            RunLimits::new(0, 0, 0, 0, 0),
            Err(RunnerError::LimitsExceeded)
        );
        assert_eq!(
            RunLimits::new(
                MAX_WALL_TIME_MS + 1,
                1_000,
                MIN_MEMORY_BYTES,
                MIN_OUTPUT_BYTES,
                1,
            ),
            Err(RunnerError::LimitsExceeded)
        );
        let above_profile =
            RunLimits::new(11_000, 5_000, 256 * 1024 * 1024, 256 * 1024, 32).unwrap();
        assert_eq!(
            above_profile.validate_against(profile().maximum_limits()),
            Err(RunnerError::LimitsExceeded)
        );
    }

    #[tokio::test]
    async fn unapproved_profile_is_rejected_before_any_remote_call() {
        let config = RemoteRunnerConfig::new(Url::parse("http://127.0.0.1:9/").unwrap()).unwrap();
        let client = RemoteRunnerClient::new(config, registry()).unwrap();
        let context = SubmissionContext::new(Uuid::now_v7(), "owner").unwrap();
        assert_eq!(
            client
                .submit(
                    &context,
                    "kotlin-jvm",
                    "fun main() {}",
                    RunLimits::default(),
                )
                .await,
            Err(RunnerError::UnapprovedProfile)
        );
    }

    #[derive(Clone)]
    struct TestBrokerState {
        token: String,
        request: Arc<Mutex<Option<Value>>>,
    }

    async fn ready(
        headers: HeaderMap,
        State(state): State<TestBrokerState>,
    ) -> impl axum::response::IntoResponse {
        if headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            != Some(format!("Bearer {}", state.token).as_str())
        {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "unauthorized"})),
            );
        }
        (
            StatusCode::OK,
            Json(json!({
                "protocolVersion": RUNNER_PROTOCOL_VERSION,
                "ready": true,
                "profiles": [{"id": "rust-stable", "digest": DIGEST}]
            })),
        )
    }

    async fn submit_job(
        headers: HeaderMap,
        State(state): State<TestBrokerState>,
        Json(request): Json<Value>,
    ) -> impl axum::response::IntoResponse {
        if headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            != Some(format!("Bearer {}", state.token).as_str())
        {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "unauthorized"})),
            );
        }
        *state.request.lock().unwrap() = Some(request.clone());
        (
            StatusCode::ACCEPTED,
            Json(json!({
                "state": "queued",
                "jobId": Uuid::now_v7(),
                "requestId": request["requestId"],
                "attemptId": request["attemptId"],
                "profileId": request["profileId"],
                "profileDigest": request["profileDigest"],
                "acceptedAt": Utc::now(),
                "expiresAt": request["expiresAt"],
                "pollAfterMs": 250
            })),
        )
    }

    #[tokio::test]
    async fn authenticated_client_checks_readiness_and_generates_request_identity() {
        let recorded = Arc::new(Mutex::new(None));
        let state = TestBrokerState {
            token: "test-token".into(),
            request: Arc::clone(&recorded),
        };
        let app = Router::new()
            .route(&format!("/{READINESS_PATH}"), get(ready))
            .route(&format!("/{JOBS_PATH}"), post(submit_job))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let token = BearerToken::new("test-token").unwrap();
        let config = RemoteRunnerConfig::new(
            Url::parse(&format!("http://127.0.0.1:{}/", address.port())).unwrap(),
        )
        .unwrap()
        .with_bearer_token(token.clone());
        assert!(!format!("{config:?}").contains("test-token"));
        assert!(!format!("{token:?}").contains("test-token"));

        let client = RemoteRunnerClient::new(config, registry()).unwrap();
        assert!(client.readiness().await.unwrap().ready);
        let context = SubmissionContext::new(Uuid::now_v7(), "owner").unwrap();
        let result = client
            .submit(
                &context,
                "rust-stable",
                "fn main() {}",
                RunLimits::default(),
            )
            .await
            .unwrap();
        assert!(matches!(result, RunSubmissionResult::Queued(_)));
        let request = recorded.lock().unwrap().clone().unwrap();
        assert!(Uuid::parse_str(request["requestId"].as_str().unwrap()).is_ok());
        assert!(Uuid::parse_str(request["attemptId"].as_str().unwrap()).is_ok());
        assert!(Uuid::parse_str(request["idempotencyKey"].as_str().unwrap()).is_ok());
        assert_eq!(request["limits"]["network"], "denied");
        assert_eq!(request["profileDigest"], DIGEST);
    }

    #[test]
    fn terminal_controls_are_removed_from_output() {
        assert_eq!(
            strip_terminal_controls("ok\u{1b}[31m red\u{1b}[0m\u{7}"),
            "ok red"
        );
    }
}
