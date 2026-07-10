//! A small async GitHub Releases client for setup, plus release resolution.

use semver::Version;
use serde::Deserialize;

use super::{Result, SetupError};

const API: &str = "https://api.github.com";
const USER_AGENT: &str = concat!("peko/", env!("CARGO_PKG_VERSION"));

/// The desired release of a repo: the latest versioned tag, or a specific tag.
#[derive(Debug, Clone)]
pub enum Channel {
    Latest,
    Specific(String),
}

impl Channel {
    /// Build from a `--*-version` flag value: empty or "latest" means Latest.
    pub fn from_flag(value: Option<String>) -> Self {
        match value {
            Some(tag) if !tag.is_empty() && tag != "latest" => Channel::Specific(tag),
            _ => Channel::Latest,
        }
    }
}

/// One published release with its downloadable assets.
#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
}

impl Release {
    /// The first asset whose name equals `name`.
    pub fn find_asset_named(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|asset| asset.name == name)
    }

    /// The first asset whose name contains `needle`.
    pub fn find_asset_containing(&self, needle: &str) -> Option<&Asset> {
        self.assets.iter().find(|asset| asset.name.contains(needle))
    }

    /// The version string without a leading `v`.
    pub fn version(&self) -> &str {
        self.tag_name.strip_prefix('v').unwrap_or(&self.tag_name)
    }
}

pub struct GithubClient {
    http: reqwest::Client,
}

impl GithubClient {
    pub fn new() -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .build()
            .map_err(|e| SetupError::Network(e.to_string()))?;
        Ok(Self { http })
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// Resolve a repo's release for the given channel.
    pub async fn resolve(&self, repo: &str, channel: &Channel) -> Result<Release> {
        match channel {
            Channel::Specific(tag) => self.release_by_tag(repo, tag).await,
            Channel::Latest => self.latest_versioned(repo).await,
        }
    }

    async fn release_by_tag(&self, repo: &str, tag: &str) -> Result<Release> {
        self.get_json(&format!("{API}/repos/{repo}/releases/tags/{tag}"))
            .await
    }

    async fn list_releases(&self, repo: &str) -> Result<Vec<Release>> {
        self.get_json(&format!("{API}/repos/{repo}/releases?per_page=100"))
            .await
    }

    /// The highest-versioned (non-nightly) release in the repo.
    async fn latest_versioned(&self, repo: &str) -> Result<Release> {
        let releases = self.list_releases(repo).await?;
        let mut best: Option<(Version, Release)> = None;
        for release in releases {
            let Some(version) = version_in_tag(&release.tag_name) else {
                continue;
            };
            let better = best.as_ref().is_none_or(|(current, _)| version > *current);
            if better {
                best = Some((version, release));
            }
        }
        best.map(|(_, release)| release)
            .ok_or_else(|| SetupError::ReleaseNotFound(format!("no versioned release in {repo}")))
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        let response = self
            .http
            .get(url)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .send()
            .await
            .map_err(|e| SetupError::Network(e.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(SetupError::HttpStatus {
                status: status.as_u16(),
                url: url.to_string(),
            });
        }
        response
            .json::<T>()
            .await
            .map_err(|e| SetupError::Network(e.to_string()))
    }
}

/// Parse a semver version out of a tag like `v2.0.0` or `2.0.0`.
pub fn version_in_tag(tag: &str) -> Option<Version> {
    let trimmed = tag
        .strip_prefix('v')
        .or_else(|| tag.strip_prefix('V'))
        .unwrap_or(tag);
    Version::parse(trimmed).ok()
}
