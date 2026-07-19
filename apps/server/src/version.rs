//! Informational release status for the public footer.
//!
//! This service never installs or executes remote content. It reads a bounded
//! release channel, validates every field, and keeps the last valid result in
//! memory. The separate host-side updater performs artifact verification.

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use chrono::{NaiveDate, Utc};
use reqwest::{Client, redirect::Policy};
use semver::Version;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use url::Url;

const RELEASE_SCHEMA: &str = "open-soverign-blog-release/1";
const CHANNEL_SCHEMA: &str = "open-soverign-blog-release-channel/1";
const REPOSITORY_URL: &str = "https://github.com/studyreadbook4ever/OpenSoverignBlog";
const DEVELOPER_URL: &str = "https://eff0rtchung.kr";
const DEFAULT_CHANNEL_URL: &str = "https://raw.githubusercontent.com/studyreadbook4ever/OpenSoverignBlog/main/release-channel.json";
const MAX_CHANNEL_BYTES: usize = 64 * 1024;
const MAX_RELEASES: usize = 64;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectRelease {
    schema_version: String,
    version: String,
    status: ProjectReleaseStatus,
    channel: String,
    #[serde(default)]
    release_date: Option<String>,
    repository_url: String,
    developer_url: String,
    license: String,
    license_path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProjectReleaseStatus {
    Unreleased,
    Released,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReleaseChannel {
    schema_version: String,
    channel: String,
    repository_url: String,
    latest: Option<ReleaseEntry>,
    releases: Vec<ReleaseEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReleaseEntry {
    version: String,
    tag: String,
    release_date: String,
    source_commit: String,
    release_url: String,
    manifest_sha256: String,
}

#[derive(Debug, Clone)]
struct ChannelState {
    channel: ReleaseChannel,
    checked_at: Option<String>,
    remote_failed: bool,
}

#[derive(Clone)]
pub struct VersionService {
    project: Arc<ProjectRelease>,
    state: Arc<RwLock<ChannelState>>,
    client: Option<Client>,
    channel_url: Option<Url>,
    interval: Duration,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicVersionStatus {
    pub current_version: String,
    pub current_release_date: Option<String>,
    pub latest_version: Option<String>,
    pub latest_release_date: Option<String>,
    pub channel: String,
    pub update_available: bool,
    pub checked_at: Option<String>,
    pub status: &'static str,
    pub repository_url: String,
    pub developer_url: String,
    pub license: String,
    pub license_href: String,
}

impl VersionService {
    pub fn start_from_environment(module_enabled: bool) -> Result<Self> {
        let project = parse_project_release(include_str!("../../../release.toml"))?;
        let bundled =
            parse_release_channel(include_str!("../../../release-channel.json"), &project)?;
        let enabled = module_enabled && environment_bool("OSB_UPDATE_CHECKS")?.unwrap_or(true);
        let interval_seconds = if enabled {
            let seconds = std::env::var("OSB_UPDATE_CHECK_INTERVAL_SECONDS")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .map(|value| {
                    value
                        .parse::<u64>()
                        .context("OSB_UPDATE_CHECK_INTERVAL_SECONDS must be an unsigned integer")
                })
                .transpose()?
                .unwrap_or(21_600);
            ensure!(
                (300..=86_400).contains(&seconds),
                "OSB_UPDATE_CHECK_INTERVAL_SECONDS must be between 300 and 86400"
            );
            seconds
        } else {
            21_600
        };
        let channel_url = if enabled {
            let raw = std::env::var("OSB_UPDATE_CHANNEL_URL")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_CHANNEL_URL.into());
            Some(validate_channel_url(&raw)?)
        } else {
            None
        };
        let client = channel_url
            .as_ref()
            .map(|_| {
                Client::builder()
                    .connect_timeout(Duration::from_secs(3))
                    .timeout(Duration::from_secs(5))
                    .redirect(Policy::none())
                    .user_agent(concat!("OpenSoverignBlog/", env!("CARGO_PKG_VERSION")))
                    .build()
                    .context("failed to build the release-check HTTP client")
            })
            .transpose()?;
        let service = Self {
            project: Arc::new(project),
            state: Arc::new(RwLock::new(ChannelState {
                channel: bundled,
                checked_at: None,
                remote_failed: false,
            })),
            client,
            channel_url,
            interval: Duration::from_secs(interval_seconds),
        };
        if service.channel_url.is_some() {
            let background = service.clone();
            tokio::spawn(async move {
                loop {
                    if let Err(error) = background.refresh().await {
                        tracing::warn!(error = %error, "release channel check failed; retaining the last validated metadata");
                        background.state.write().await.remote_failed = true;
                    }
                    tokio::time::sleep(background.interval).await;
                }
            });
        }
        Ok(service)
    }

    #[cfg(test)]
    pub fn bundled_for_tests() -> Self {
        let project = parse_project_release(include_str!("../../../release.toml")).unwrap();
        let bundled =
            parse_release_channel(include_str!("../../../release-channel.json"), &project).unwrap();
        Self {
            project: Arc::new(project),
            state: Arc::new(RwLock::new(ChannelState {
                channel: bundled,
                checked_at: None,
                remote_failed: false,
            })),
            client: None,
            channel_url: None,
            interval: Duration::from_secs(21_600),
        }
    }

    pub async fn public_status(&self) -> PublicVersionStatus {
        let state = self.state.read().await;
        let latest = state.channel.latest.as_ref();
        let current = Version::parse(&self.project.version)
            .expect("project release is validated during service construction");
        let update_available = latest
            .and_then(|entry| Version::parse(&entry.version).ok())
            .is_some_and(|latest| latest > current);
        let status = if self.channel_url.is_none() {
            "disabled"
        } else if state.remote_failed && state.checked_at.is_none() {
            "offline"
        } else if latest.is_none() {
            "no_release"
        } else if update_available {
            "update_available"
        } else {
            "current"
        };
        PublicVersionStatus {
            current_version: self.project.version.clone(),
            current_release_date: self.project.release_date.clone(),
            latest_version: latest.map(|entry| entry.version.clone()),
            latest_release_date: latest.map(|entry| entry.release_date.clone()),
            channel: self.project.channel.clone(),
            update_available,
            checked_at: state.checked_at.clone(),
            status,
            repository_url: self.project.repository_url.clone(),
            developer_url: self.project.developer_url.clone(),
            license: self.project.license.clone(),
            license_href: format!("/{}", self.project.license_path),
        }
    }

    async fn refresh(&self) -> Result<()> {
        let client = self
            .client
            .as_ref()
            .context("release checks are disabled")?;
        let url = self
            .channel_url
            .as_ref()
            .context("release channel URL is disabled")?;
        let mut response = client
            .get(url.clone())
            .send()
            .await
            .context("release channel request failed")?;
        ensure!(
            response.status().is_success(),
            "release channel returned HTTP {}",
            response.status()
        );
        if let Some(length) = response.content_length() {
            ensure!(
                length <= MAX_CHANNEL_BYTES as u64,
                "release channel exceeds {MAX_CHANNEL_BYTES} bytes"
            );
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .context("failed while reading the release channel")?
        {
            ensure!(
                body.len().saturating_add(chunk.len()) <= MAX_CHANNEL_BYTES,
                "release channel exceeds {MAX_CHANNEL_BYTES} bytes"
            );
            body.extend_from_slice(&chunk);
        }
        let source = std::str::from_utf8(&body).context("release channel must be UTF-8 JSON")?;
        let channel = parse_release_channel(source, &self.project)?;
        *self.state.write().await = ChannelState {
            channel,
            checked_at: Some(Utc::now().to_rfc3339()),
            remote_failed: false,
        };
        Ok(())
    }
}

fn parse_project_release(source: &str) -> Result<ProjectRelease> {
    let project: ProjectRelease = toml::from_str(source).context("release.toml is invalid")?;
    ensure!(
        project.schema_version == RELEASE_SCHEMA,
        "unsupported release.toml schema"
    );
    ensure!(
        project.version == env!("CARGO_PKG_VERSION"),
        "release.toml version does not match the server package"
    );
    stable_version(&project.version, "release.toml version")?;
    ensure!(
        project.channel == "stable",
        "only the stable update channel is supported"
    );
    ensure!(
        project.license == "Unlicense",
        "project license metadata must remain Unlicense"
    );
    ensure!(
        project.license_path == "UNLICENSE",
        "project license path must be UNLICENSE"
    );
    ensure!(
        project.repository_url == REPOSITORY_URL,
        "release.toml repository is not canonical"
    );
    ensure!(
        project.developer_url == DEVELOPER_URL,
        "release.toml developer URL is not canonical"
    );
    validate_https_url(&project.repository_url, Some("github.com"))?;
    validate_https_url(&project.developer_url, None)?;
    match (project.status, project.release_date.as_deref()) {
        (ProjectReleaseStatus::Unreleased, None) => {}
        (ProjectReleaseStatus::Released, Some(date)) => validate_date(date)?,
        (ProjectReleaseStatus::Unreleased, Some(_)) => {
            anyhow::bail!("an unreleased build cannot claim a release date")
        }
        (ProjectReleaseStatus::Released, None) => {
            anyhow::bail!("a released build requires a release date")
        }
    }
    Ok(project)
}

fn parse_release_channel(source: &str, project: &ProjectRelease) -> Result<ReleaseChannel> {
    ensure!(
        source.len() <= MAX_CHANNEL_BYTES,
        "release channel is too large"
    );
    let channel: ReleaseChannel =
        serde_json::from_str(source).context("release channel JSON is invalid")?;
    ensure!(
        channel.schema_version == CHANNEL_SCHEMA,
        "unsupported release channel schema"
    );
    ensure!(
        channel.channel == project.channel,
        "release channel name does not match this build"
    );
    ensure!(
        channel.repository_url == project.repository_url,
        "release channel repository does not match this engine"
    );
    ensure!(
        channel.releases.len() <= MAX_RELEASES,
        "release channel has too many entries"
    );
    let mut versions = std::collections::BTreeSet::new();
    let mut source_commits = std::collections::BTreeSet::new();
    let mut previous_version: Option<Version> = None;
    for release in &channel.releases {
        let version = validate_release_entry(release, project)?;
        ensure!(
            versions.insert(release.version.as_str()),
            "release channel contains duplicate versions"
        );
        ensure!(
            source_commits.insert(release.source_commit.as_str()),
            "release channel contains duplicate source commits"
        );
        if let Some(previous) = previous_version.as_ref() {
            ensure!(
                previous > &version,
                "release channel must be strictly newest-first"
            );
        }
        previous_version = Some(version);
    }
    match (channel.latest.as_ref(), channel.releases.first()) {
        (None, None) => {}
        (Some(latest), Some(first)) => ensure!(
            latest == first,
            "release channel latest must exactly equal releases[0]"
        ),
        _ => {
            anyhow::bail!("release channel latest and releases must be empty or non-empty together")
        }
    }
    Ok(channel)
}

fn validate_release_entry(entry: &ReleaseEntry, project: &ProjectRelease) -> Result<Version> {
    let version = stable_version(&entry.version, "release version")?;
    ensure!(
        entry.tag == format!("v{}", entry.version),
        "release tag does not match its version"
    );
    validate_date(&entry.release_date)?;
    ensure!(
        is_lower_hex(&entry.source_commit, 40),
        "sourceCommit must be a 40-character lowercase Git commit"
    );
    ensure!(
        is_lower_hex(&entry.manifest_sha256, 64),
        "manifestSha256 must be a lowercase SHA-256 digest"
    );
    validate_https_url(&entry.release_url, Some("github.com"))?;
    let expected_url = format!("{}/releases/tag/{}", project.repository_url, entry.tag);
    ensure!(
        entry.release_url == expected_url,
        "releaseUrl is not the canonical release tag URL"
    );
    Ok(version)
}

fn stable_version(value: &str, label: &str) -> Result<Version> {
    let version = Version::parse(value).with_context(|| format!("{label} is not SemVer"))?;
    ensure!(
        version.pre.is_empty() && version.build.is_empty(),
        "{label} must not contain prerelease or build metadata"
    );
    Ok(version)
}

fn validate_channel_url(value: &str) -> Result<Url> {
    let url = Url::parse(value).context("OSB_UPDATE_CHANNEL_URL must be an absolute URL")?;
    let local_http = url.scheme() == "http"
        && matches!(
            url.host_str(),
            Some("localhost" | "127.0.0.1" | "::1" | "[::1]")
        );
    ensure!(
        url.scheme() == "https" || local_http,
        "update channel must use HTTPS; localhost may use HTTP for tests"
    );
    ensure!(
        url.host_str().is_some() && url.username().is_empty() && url.password().is_none(),
        "update channel URL cannot contain credentials"
    );
    ensure!(
        url.fragment().is_none(),
        "update channel URL cannot contain a fragment"
    );
    Ok(url)
}

fn validate_https_url(value: &str, expected_host: Option<&str>) -> Result<Url> {
    let url = Url::parse(value).context("release metadata URL must be absolute")?;
    ensure!(
        url.scheme() == "https" && url.host_str().is_some(),
        "release metadata URL must use HTTPS"
    );
    ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "release metadata URL cannot contain credentials, query, or fragment"
    );
    if let Some(host) = expected_host {
        ensure!(
            url.host_str() == Some(host),
            "release metadata URL has an unexpected host"
        );
    }
    Ok(url)
}

fn validate_date(value: &str) -> Result<()> {
    let date =
        NaiveDate::parse_from_str(value, "%Y-%m-%d").context("release date must be YYYY-MM-DD")?;
    ensure!(
        date.format("%Y-%m-%d").to_string() == value,
        "release date must use canonical YYYY-MM-DD form"
    );
    Ok(())
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn environment_bool(name: &str) -> Result<Option<bool>> {
    let Some(value) = std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(None);
    };
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(Some(true)),
        "0" | "false" | "no" | "off" => Ok(Some(false)),
        _ => anyhow::bail!("{name} must be true/false, yes/no, on/off, or 1/0"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release_entry(version: &str, commit_byte: char) -> serde_json::Value {
        serde_json::json!({
            "version": version,
            "tag": format!("v{version}"),
            "releaseDate": "2026-07-19",
            "sourceCommit": commit_byte.to_string().repeat(40),
            "releaseUrl": format!("{REPOSITORY_URL}/releases/tag/v{version}"),
            "manifestSha256": commit_byte.to_string().repeat(64),
        })
    }

    fn channel_source(
        project: &ProjectRelease,
        latest: Option<serde_json::Value>,
        releases: Vec<serde_json::Value>,
    ) -> String {
        serde_json::json!({
            "schemaVersion": CHANNEL_SCHEMA,
            "channel": "stable",
            "repositoryUrl": project.repository_url,
            "latest": latest,
            "releases": releases,
        })
        .to_string()
    }

    #[test]
    fn bundled_release_metadata_is_consistent_and_honest() {
        let project = parse_project_release(include_str!("../../../release.toml")).unwrap();
        assert_eq!(project.status, ProjectReleaseStatus::Unreleased);
        assert!(project.release_date.is_none());
        let channel =
            parse_release_channel(include_str!("../../../release-channel.json"), &project).unwrap();
        assert!(channel.latest.is_none());
        assert!(channel.releases.is_empty());
    }

    #[test]
    fn channel_rejects_unknown_fields_and_off_namespace_releases() {
        let project = parse_project_release(include_str!("../../../release.toml")).unwrap();
        let unknown = include_str!("../../../release-channel.json")
            .replace("\"releases\": []", "\"releases\": [], \"surprise\": true");
        assert!(parse_release_channel(&unknown, &project).is_err());

        let malicious = format!(
            r#"{{
              "schemaVersion":"{CHANNEL_SCHEMA}",
              "channel":"stable",
              "repositoryUrl":"{}",
              "latest":{{"version":"9.0.0","tag":"v9.0.0","releaseDate":"2026-09-06","sourceCommit":"{}","releaseUrl":"https://github.com/attacker/project/releases/tag/v9.0.0","manifestSha256":"{}"}},
              "releases":[{{"version":"9.0.0","tag":"v9.0.0","releaseDate":"2026-09-06","sourceCommit":"{}","releaseUrl":"https://github.com/attacker/project/releases/tag/v9.0.0","manifestSha256":"{}"}}]
            }}"#,
            project.repository_url,
            "a".repeat(40),
            "b".repeat(64),
            "a".repeat(40),
            "b".repeat(64),
        );
        assert!(parse_release_channel(&malicious, &project).is_err());
    }

    #[test]
    fn stable_channel_rejects_cross_contract_drift() {
        let project = parse_project_release(include_str!("../../../release.toml")).unwrap();
        let newest = release_entry("0.3.0", 'c');
        let older = release_entry("0.2.0", 'b');
        let valid = channel_source(
            &project,
            Some(newest.clone()),
            vec![newest.clone(), older.clone()],
        );
        assert!(parse_release_channel(&valid, &project).is_ok());

        let prerelease = release_entry("0.4.0-rc.1", 'd');
        let prerelease_channel =
            channel_source(&project, Some(prerelease.clone()), vec![prerelease]);
        assert!(parse_release_channel(&prerelease_channel, &project).is_err());

        let wrong_order = channel_source(
            &project,
            Some(older.clone()),
            vec![older.clone(), newest.clone()],
        );
        assert!(parse_release_channel(&wrong_order, &project).is_err());

        let wrong_latest = channel_source(
            &project,
            Some(older.clone()),
            vec![newest.clone(), older.clone()],
        );
        assert!(parse_release_channel(&wrong_latest, &project).is_err());

        let mut duplicate_commit = older.clone();
        duplicate_commit["sourceCommit"] = newest["sourceCommit"].clone();
        let duplicate_commit_channel = channel_source(
            &project,
            Some(newest.clone()),
            vec![newest.clone(), duplicate_commit],
        );
        assert!(parse_release_channel(&duplicate_commit_channel, &project).is_err());

        let mut suffix_trick = newest.clone();
        suffix_trick["releaseUrl"] =
            serde_json::json!(format!("{REPOSITORY_URL}/releases/tag/not-v0.3.0"));
        let suffix_trick_channel =
            channel_source(&project, Some(suffix_trick.clone()), vec![suffix_trick]);
        assert!(parse_release_channel(&suffix_trick_channel, &project).is_err());

        let mut tag_drift = newest.clone();
        tag_drift["tag"] = serde_json::json!("v0.3.1");
        let tag_drift_channel = channel_source(&project, Some(tag_drift.clone()), vec![tag_drift]);
        assert!(parse_release_channel(&tag_drift_channel, &project).is_err());

        let mut invalid_date = newest.clone();
        invalid_date["releaseDate"] = serde_json::json!("2026-7-19");
        let invalid_date_channel =
            channel_source(&project, Some(invalid_date.clone()), vec![invalid_date]);
        assert!(parse_release_channel(&invalid_date_channel, &project).is_err());

        let mut invalid_manifest = newest.clone();
        invalid_manifest["manifestSha256"] = serde_json::json!("C".repeat(64));
        let invalid_manifest_channel = channel_source(
            &project,
            Some(invalid_manifest.clone()),
            vec![invalid_manifest],
        );
        assert!(parse_release_channel(&invalid_manifest_channel, &project).is_err());
    }
}
