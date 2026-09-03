use anyhow::{Context as _, Result};
use client::Client;
use db::kvp::KeyValueStore;
use futures_lite::StreamExt;
use gpui::{
    App, AppContext as _, AsyncApp, BackgroundExecutor, Context, Entity, Global, Task, TaskExt,
    Window, actions,
};
use http_client::{HttpClient, HttpClientWithUrl};
use paths::remote_servers_dir;
use release_channel::{AppCommitSha, ReleaseChannel, RpReleaseMetadata};
use semver::Version;
use serde::{Deserialize, Serialize};
use settings::{RegisterSetting, Settings, SettingsStore};
use sha2::{Digest, Sha256};
use smol::fs::File;
use smol::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt},
};
use std::mem;
use std::{
    env::{
        self,
        consts::{ARCH, OS},
    },
    ffi::OsStr,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};
use util::command::new_command;
use workspace::Workspace;

const SHOULD_SHOW_UPDATE_NOTIFICATION_KEY: &str = "auto-updater-should-show-updated-notification";

#[derive(Debug)]
struct MissingDependencyError(String);

impl std::fmt::Display for MissingDependencyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for MissingDependencyError {}
const POLL_INTERVAL: Duration = Duration::from_secs(60 * 60);
const NIGHTLY_POLL_INTERVAL: Duration = Duration::from_secs(15 * 60);
const REMOTE_SERVER_CACHE_LIMIT: usize = 5;
const RP_RELEASES_API: &str = "https://api.github.com/repos/JonathonRP/zed/releases?per_page=100";
const RP_REPOSITORY_RELEASES: &str = "https://github.com/JonathonRP/zed/releases/download";
const RP_MANIFEST_NAME: &str = "rp-update.json";
const RP_API_MAX_BYTES: u64 = 2 * 1024 * 1024;
const RP_MANIFEST_MAX_BYTES: u64 = 256 * 1024;
const RP_MAX_REDIRECTS: usize = 5;
const RP_UNSIGNED_TRUST_NOTICE: &str = "RP update artifacts are unsigned; manifest hashes are rooted only in JonathonRP/zed repository controls and HTTPS/TLS, not independent publisher authentication";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UpdateEndpointSource {
    Official,
    Rp,
}

fn update_endpoint_source(metadata: Option<RpReleaseMetadata>) -> UpdateEndpointSource {
    if metadata.is_some() {
        UpdateEndpointSource::Rp
    } else {
        UpdateEndpointSource::Official
    }
}

fn release_discovery_endpoint(source: UpdateEndpointSource) -> &'static str {
    match source {
        UpdateEndpointSource::Official => "/releases/{channel}/latest/asset",
        UpdateEndpointSource::Rp => RP_RELEASES_API,
    }
}

#[derive(Clone, Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<GithubReleaseAsset>,
}

#[derive(Clone, Debug, Deserialize)]
struct GithubReleaseAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RpUpdateManifest {
    schema_version: u32,
    channel: String,
    calendar_version: String,
    upstream_version: String,
    commit: String,
    tag: String,
    trust: RpTrust,
    notes_identity: String,
    assets: RpAssets,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RpTrust {
    signed: bool,
    label: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RpAssets {
    windows_x86_64_installer: RpManifestAsset,
    windows_x86_64_portable: RpManifestAsset,
    windows_x86_64_remote_server: RpManifestAsset,
    linux_x86_64_remote_server: RpManifestAsset,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RpManifestAsset {
    name: String,
    size: u64,
    sha256: String,
    url: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct RpCalendarVersion {
    date: u32,
    patch: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RpCandidateIdentity {
    calendar: RpCalendarVersion,
    calendar_version: String,
    tag: String,
    commit: String,
    upstream_version: Version,
    installer_sha256: String,
}

#[derive(Clone, Debug)]
struct ValidatedRpManifest {
    manifest: RpUpdateManifest,
    identity: RpCandidateIdentity,
}

#[derive(Clone, Debug)]
struct RpCompileIdentity {
    calendar: RpCalendarVersion,
    calendar_version: String,
    tag: String,
    commit: String,
    upstream_version: Version,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RpRequestKind {
    Api,
    ReleaseAsset,
}

enum UpdateDownload {
    Official(ReleaseAsset),
    Rp(RpManifestAsset),
}

#[cfg(target_os = "linux")]
fn linux_rsync_install_hint() -> &'static str {
    let os_release = match std::fs::read_to_string("/etc/os-release") {
        Ok(os_release) => os_release,
        Err(_) => return "Please install rsync using your package manager",
    };

    let mut distribution_ids = Vec::new();
    for line in os_release.lines() {
        let trimmed = line.trim();
        if let Some(value) = trimmed.strip_prefix("ID=") {
            distribution_ids.push(value.trim_matches('"').to_ascii_lowercase());
        } else if let Some(value) = trimmed.strip_prefix("ID_LIKE=") {
            for id in value.trim_matches('"').split_whitespace() {
                distribution_ids.push(id.to_ascii_lowercase());
            }
        }
    }

    let package_manager_hint = if distribution_ids
        .iter()
        .any(|distribution_id| distribution_id == "arch")
    {
        Some("Install it with: sudo pacman -S rsync")
    } else if distribution_ids
        .iter()
        .any(|distribution_id| distribution_id == "debian" || distribution_id == "ubuntu")
    {
        Some("Install it with: sudo apt install rsync")
    } else if distribution_ids.iter().any(|distribution_id| {
        distribution_id == "fedora"
            || distribution_id == "rhel"
            || distribution_id == "centos"
            || distribution_id == "rocky"
            || distribution_id == "almalinux"
    }) {
        Some("Install it with: sudo dnf install rsync")
    } else if distribution_ids
        .iter()
        .any(|distribution_id| distribution_id == "nixos")
    {
        Some("Install pkgs.rsync from nixpkgs")
    } else {
        None
    };

    package_manager_hint.unwrap_or("Please install rsync using your package manager")
}

actions!(
    auto_update,
    [
        /// Checks for available updates.
        Check,
        /// Dismisses the update error message.
        DismissMessage,
        /// Opens the release notes for the current version in a browser.
        ViewReleaseNotes,
    ]
);

#[derive(Serialize, Debug)]
pub struct AssetQuery<'a> {
    asset: &'a str,
    os: &'a str,
    arch: &'a str,
    metrics_id: Option<&'a str>,
    system_id: Option<&'a str>,
    is_staff: Option<bool>,
}

#[derive(Clone, Debug)]
pub enum AutoUpdateStatus {
    Idle,
    Checking,
    Downloading {
        version: Version,
        /// Download progress as a fraction in the range `0.0..=1.0`, or `None`
        /// when the total download size is not yet known.
        progress: Option<f32>,
    },
    Installing {
        version: Version,
    },
    Updated {
        version: Version,
    },
    Errored {
        error: Arc<anyhow::Error>,
    },
}

impl PartialEq for AutoUpdateStatus {
    // `progress` is deliberately not compared: two `Downloading` statuses for
    // the same version are equal regardless of how far the download is.
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (AutoUpdateStatus::Idle, AutoUpdateStatus::Idle) => true,
            (AutoUpdateStatus::Checking, AutoUpdateStatus::Checking) => true,
            (
                AutoUpdateStatus::Downloading { version: v1, .. },
                AutoUpdateStatus::Downloading { version: v2, .. },
            ) => v1 == v2,
            (
                AutoUpdateStatus::Installing { version: v1 },
                AutoUpdateStatus::Installing { version: v2 },
            ) => v1 == v2,
            (
                AutoUpdateStatus::Updated { version: v1 },
                AutoUpdateStatus::Updated { version: v2 },
            ) => v1 == v2,
            (AutoUpdateStatus::Errored { error: e1 }, AutoUpdateStatus::Errored { error: e2 }) => {
                e1.to_string() == e2.to_string()
            }
            _ => false,
        }
    }
}

impl AutoUpdateStatus {
    pub fn is_updated(&self) -> bool {
        matches!(self, Self::Updated { .. })
    }
}

pub struct AutoUpdater {
    status: AutoUpdateStatus,
    current_version: Version,
    staged_rp_candidate: Option<RpCandidateIdentity>,
    client: Arc<Client>,
    pending_poll: Option<Task<Option<()>>>,
    quit_subscription: Option<gpui::Subscription>,
    update_check_type: UpdateCheckType,
    _wake_subscription: gpui::Subscription,
    dismissed_status: Option<AutoUpdateStatus>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct ReleaseAsset {
    pub version: String,
    pub url: String,
}

struct MacOsUnmounter<'a> {
    mount_path: PathBuf,
    background_executor: &'a BackgroundExecutor,
}

impl MacOsUnmounter<'_> {
    /// Unmounts the disk image and waits for completion. This must happen
    /// before the `InstallerDir` is dropped: deleting the temp dir while the
    /// image is still mounted inside it fails silently and leaks the
    /// directory (and the downloaded DMG) in the system temp dir.
    async fn unmount(mut self) {
        let mount_path = mem::take(&mut self.mount_path);
        unmount_disk_image(&mount_path).await;
    }
}

impl Drop for MacOsUnmounter<'_> {
    fn drop(&mut self) {
        let mount_path = mem::take(&mut self.mount_path);
        // Safety net for early exits and cancellation; the happy path calls
        // `unmount`, which leaves the path empty.
        if mount_path.as_os_str().is_empty() {
            return;
        }
        self.background_executor
            .spawn(async move { unmount_disk_image(&mount_path).await })
            .detach();
    }
}

async fn unmount_disk_image(mount_path: &Path) {
    let unmount_output = new_command("hdiutil")
        .args(["detach", "-force"])
        .arg(mount_path)
        .output()
        .await;
    match unmount_output {
        Ok(output) if output.status.success() => {
            log::info!("Successfully unmounted the disk image");
        }
        Ok(output) => {
            log::error!(
                "Failed to unmount disk image: {:?}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Err(error) => {
            log::error!("Error while trying to unmount disk image: {:?}", error);
        }
    }
}

#[derive(Clone, Copy, Debug, RegisterSetting)]
struct AutoUpdateSetting(bool);

/// Whether or not to automatically check for updates.
///
/// Default: true
impl Settings for AutoUpdateSetting {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self(content.auto_update.unwrap())
    }
}

#[derive(Default)]
struct GlobalAutoUpdate(Option<Entity<AutoUpdater>>);

impl Global for GlobalAutoUpdate {}

pub fn init(client: Arc<Client>, cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|_, action, window, cx| check(action, window, cx));

        workspace.register_action(|_, action, _, cx| {
            view_release_notes(action, cx);
        });
    })
    .detach();

    let version = release_channel::AppVersion::global(cx);
    let auto_updater = cx.new(|cx| {
        let updater = AutoUpdater::new(version, client, cx);

        let poll_for_updates = ReleaseChannel::try_global(cx)
            .map(|channel| channel.poll_for_updates())
            .unwrap_or(false);

        if option_env!("ZED_UPDATE_EXPLANATION").is_none()
            && env::var("ZED_UPDATE_EXPLANATION").is_err()
            && poll_for_updates
        {
            let mut update_subscription = AutoUpdateSetting::get_global(cx)
                .0
                .then(|| updater.start_polling(cx));

            cx.observe_global::<SettingsStore>(move |updater: &mut AutoUpdater, cx| {
                if AutoUpdateSetting::get_global(cx).0 {
                    if update_subscription.is_none() {
                        update_subscription = Some(updater.start_polling(cx))
                    }
                } else {
                    update_subscription.take();
                }
            })
            .detach();
        }

        updater
    });
    cx.set_global(GlobalAutoUpdate(Some(auto_updater)));
}

pub fn check(_: &Check, window: &mut Window, cx: &mut App) {
    if let Some(message) = option_env!("ZED_UPDATE_EXPLANATION")
        .map(ToOwned::to_owned)
        .or_else(|| env::var("ZED_UPDATE_EXPLANATION").ok())
    {
        drop(window.prompt(
            gpui::PromptLevel::Info,
            "Zed was installed via a package manager.",
            Some(&message),
            &["OK"],
            cx,
        ));
        return;
    }

    if !ReleaseChannel::try_global(cx)
        .map(|channel| channel.poll_for_updates())
        .unwrap_or(false)
    {
        return;
    }

    if let Some(updater) = AutoUpdater::get(cx) {
        updater.update(cx, |updater, cx| updater.poll(UpdateCheckType::Manual, cx));
    } else {
        drop(window.prompt(
            gpui::PromptLevel::Info,
            "Could not check for updates",
            Some("Auto-updates disabled for non-bundled app."),
            &["OK"],
            cx,
        ));
    }
}

pub fn release_notes_url(cx: &mut App) -> Option<String> {
    let release_channel = ReleaseChannel::try_global(cx)?;
    let url = match release_channel {
        ReleaseChannel::Stable | ReleaseChannel::Preview => {
            let auto_updater = AutoUpdater::get(cx)?;
            let auto_updater = auto_updater.read(cx);
            let mut current_version = auto_updater.current_version.clone();
            current_version.pre = semver::Prerelease::EMPTY;
            current_version.build = semver::BuildMetadata::EMPTY;
            let release_channel = release_channel.dev_name();
            let path = format!("/releases/{release_channel}/{current_version}");
            auto_updater.client.http_client().build_url(&path)
        }
        ReleaseChannel::Nightly => {
            "https://github.com/zed-industries/zed/commits/nightly/".to_string()
        }
        ReleaseChannel::Dev => "https://github.com/zed-industries/zed/commits/main/".to_string(),
    };
    Some(url)
}

pub fn view_release_notes(_: &ViewReleaseNotes, cx: &mut App) -> Option<()> {
    let url = release_notes_url(cx)?;
    cx.open_url(&url);
    None
}

#[cfg(not(target_os = "windows"))]
const INSTALLER_DIR_PREFIX: &str = "zed-auto-update";

#[cfg(not(target_os = "windows"))]
struct InstallerDir(tempfile::TempDir);

#[cfg(not(target_os = "windows"))]
impl InstallerDir {
    async fn new(_rp_root: Option<&Path>) -> Result<Self> {
        Ok(Self(
            tempfile::Builder::new()
                .prefix(INSTALLER_DIR_PREFIX)
                .tempdir()?,
        ))
    }

    fn path(&self) -> &Path {
        self.0.path()
    }
}

#[cfg(target_os = "windows")]
struct InstallerDir(PathBuf);

#[cfg(target_os = "windows")]
impl InstallerDir {
    async fn new(rp_root: Option<&Path>) -> Result<Self> {
        let app_root = match rp_root {
            Some(root) => root.to_owned(),
            None => std::env::current_exe()?
                .parent()
                .context("No parent dir for Zed.exe")?
                .to_owned(),
        };
        let installer_dir = app_root.join("updates");
        if smol::fs::metadata(&installer_dir).await.is_ok() {
            smol::fs::remove_dir_all(&installer_dir).await?;
        }
        smol::fs::create_dir(&installer_dir).await?;
        Ok(Self(installer_dir))
    }

    fn path(&self) -> &Path {
        self.0.as_path()
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum UpdateCheckType {
    Automatic,
    Manual,
}

impl UpdateCheckType {
    pub fn is_manual(self) -> bool {
        self == Self::Manual
    }
}

impl RpCalendarVersion {
    fn parse(value: &str) -> Result<Self> {
        let (date, patch) = value
            .split_once('.')
            .with_context(|| format!("invalid RP calendar version {value:?}"))?;
        anyhow::ensure!(
            date.len() == 8 && date.bytes().all(|byte| byte.is_ascii_digit()),
            "invalid RP calendar date {date:?}"
        );
        anyhow::ensure!(
            !patch.is_empty()
                && !patch.starts_with('0')
                && patch.bytes().all(|byte| byte.is_ascii_digit()),
            "invalid RP calendar patch {patch:?}"
        );

        let year = date[0..4].parse::<u32>()?;
        let month = date[4..6].parse::<u32>()?;
        let day = date[6..8].parse::<u32>()?;
        let days_in_month = match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if year.is_multiple_of(400)
                || (year.is_multiple_of(4) && !year.is_multiple_of(100)) =>
            {
                29
            }
            2 => 28,
            _ => 0,
        };
        anyhow::ensure!(
            year > 0 && day > 0 && day <= days_in_month,
            "invalid RP calendar date {date:?}"
        );

        Ok(Self {
            date: date.parse()?,
            patch: patch.parse()?,
        })
    }
}

fn parse_rp_tag(tag: &str) -> Result<(RpCalendarVersion, &str)> {
    let calendar_version = tag
        .strip_prefix("rp-stable-")
        .with_context(|| format!("invalid RP release tag {tag:?}"))?;
    Ok((
        RpCalendarVersion::parse(calendar_version)?,
        calendar_version,
    ))
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn normalized_upstream_version(mut version: Version) -> Version {
    version.build = semver::BuildMetadata::EMPTY;
    version
}

impl RpCompileIdentity {
    fn new(
        metadata: RpReleaseMetadata,
        upstream_version: Version,
        commit: Option<String>,
    ) -> Result<Self> {
        let calendar = RpCalendarVersion::parse(metadata.calendar_version)?;
        let (tag_calendar, tag_calendar_text) = parse_rp_tag(metadata.release_tag)?;
        anyhow::ensure!(
            calendar == tag_calendar && metadata.calendar_version == tag_calendar_text,
            "compiled RP tag and calendar version do not match"
        );
        let commit = commit.context("compiled RP build has no AppCommitSha")?;
        anyhow::ensure!(
            is_lower_hex(&commit, 40),
            "compiled RP AppCommitSha must be a lowercase full SHA"
        );
        anyhow::ensure!(
            metadata.notes_identity.starts_with("sha256:")
                && is_lower_hex(&metadata.notes_identity["sha256:".len()..], 64),
            "compiled RP release-notes identity is invalid"
        );
        Ok(Self {
            calendar,
            calendar_version: metadata.calendar_version.to_string(),
            tag: metadata.release_tag.to_string(),
            commit,
            upstream_version: normalized_upstream_version(upstream_version),
        })
    }
}

fn expected_rp_asset_name(key: &str, calendar_version: &str) -> Result<String> {
    let prefix = format!("rp-stable-{calendar_version}");
    match key {
        "windows_x86_64_installer" => Ok(format!("Zed-{prefix}-windows-x86_64.exe")),
        "windows_x86_64_portable" => Ok(format!("zed-{prefix}-windows-x86_64-portable.zip")),
        "windows_x86_64_remote_server" => {
            Ok(format!("zed-{prefix}-remote-server-windows-x86_64.zip"))
        }
        "linux_x86_64_remote_server" => Ok(format!("zed-{prefix}-remote-server-linux-x86_64.gz")),
        _ => anyhow::bail!("unexpected RP asset key {key:?}"),
    }
}

fn expected_rp_asset_url(tag: &str, name: &str) -> String {
    format!("{RP_REPOSITORY_RELEASES}/{tag}/{name}")
}

fn validate_rp_manifest(manifest: RpUpdateManifest) -> Result<ValidatedRpManifest> {
    anyhow::ensure!(
        manifest.schema_version == 1,
        "unsupported RP manifest schema"
    );
    anyhow::ensure!(
        manifest.channel == "rp-stable",
        "RP manifest has an unexpected channel"
    );
    let calendar = RpCalendarVersion::parse(&manifest.calendar_version)?;
    let (tag_calendar, tag_calendar_text) = parse_rp_tag(&manifest.tag)?;
    anyhow::ensure!(
        calendar == tag_calendar && manifest.calendar_version == tag_calendar_text,
        "RP manifest tag and calendar version do not match"
    );
    let upstream_version = manifest
        .upstream_version
        .parse::<Version>()
        .context("RP manifest has an invalid upstream semver")?;
    anyhow::ensure!(
        is_lower_hex(&manifest.commit, 40),
        "RP manifest commit must be a lowercase full SHA"
    );
    anyhow::ensure!(
        !manifest.trust.signed && manifest.trust.label == "unsigned",
        "{RP_UNSIGNED_TRUST_NOTICE}"
    );
    anyhow::ensure!(
        manifest.notes_identity.starts_with("sha256:")
            && is_lower_hex(&manifest.notes_identity["sha256:".len()..], 64),
        "RP manifest has an invalid release-notes identity"
    );

    let assets = [
        (
            "windows_x86_64_installer",
            &manifest.assets.windows_x86_64_installer,
        ),
        (
            "windows_x86_64_portable",
            &manifest.assets.windows_x86_64_portable,
        ),
        (
            "windows_x86_64_remote_server",
            &manifest.assets.windows_x86_64_remote_server,
        ),
        (
            "linux_x86_64_remote_server",
            &manifest.assets.linux_x86_64_remote_server,
        ),
    ];
    let mut names = std::collections::HashSet::new();
    for (key, asset) in assets {
        let expected_name = expected_rp_asset_name(key, &manifest.calendar_version)?;
        anyhow::ensure!(
            asset.name == expected_name,
            "RP manifest asset {key} has an unexpected name"
        );
        anyhow::ensure!(
            names.insert(&asset.name),
            "duplicate RP manifest asset name"
        );
        anyhow::ensure!(asset.size > 0, "RP manifest asset {key} is empty");
        anyhow::ensure!(
            is_lower_hex(&asset.sha256, 64),
            "RP manifest asset {key} has an invalid SHA-256"
        );
        anyhow::ensure!(
            asset.url == expected_rp_asset_url(&manifest.tag, &asset.name),
            "RP manifest asset {key} has an unexpected owner, repository, tag, or URL"
        );
    }

    let identity = RpCandidateIdentity {
        calendar,
        calendar_version: manifest.calendar_version.clone(),
        tag: manifest.tag.clone(),
        commit: manifest.commit.clone(),
        upstream_version,
        installer_sha256: manifest.assets.windows_x86_64_installer.sha256.clone(),
    };
    Ok(ValidatedRpManifest { manifest, identity })
}

fn select_newest_rp_release(releases: &[GithubRelease]) -> Result<(&GithubRelease, String)> {
    let release = releases
        .iter()
        .filter(|release| !release.draft && !release.prerelease)
        .filter_map(|release| {
            parse_rp_tag(&release.tag_name)
                .ok()
                .map(|(version, _)| (version, release))
        })
        .max_by_key(|(version, _)| *version)
        .map(|(_, release)| release)
        .context("GitHub returned no valid RP stable calendar release")?;
    let manifest_assets = release
        .assets
        .iter()
        .filter(|asset| asset.name == RP_MANIFEST_NAME)
        .collect::<Vec<_>>();
    anyhow::ensure!(
        manifest_assets.len() == 1,
        "newest RP release must contain exactly one {RP_MANIFEST_NAME} asset"
    );
    let expected_url = expected_rp_asset_url(&release.tag_name, RP_MANIFEST_NAME);
    anyhow::ensure!(
        manifest_assets[0].browser_download_url == expected_url,
        "RP manifest release asset URL is not the pinned repository URL"
    );
    Ok((release, expected_url))
}

fn is_allowed_rp_redirect(kind: RpRequestKind, url: &http_client::Url) -> bool {
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    match kind {
        RpRequestKind::Api => host.eq_ignore_ascii_case("api.github.com"),
        RpRequestKind::ReleaseAsset => [
            "objects.githubusercontent.com",
            "release-assets.githubusercontent.com",
            "github-releases.githubusercontent.com",
        ]
        .iter()
        .any(|allowed| host.eq_ignore_ascii_case(allowed)),
    }
}

fn validate_rp_initial_url(kind: RpRequestKind, url: &str) -> Result<http_client::Url> {
    let parsed = http_client::Url::parse(url).context("invalid RP request URL")?;
    anyhow::ensure!(
        parsed.scheme() == "https"
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.port().is_none(),
        "RP requests require an uncredentialed HTTPS URL"
    );
    match kind {
        RpRequestKind::Api => {
            anyhow::ensure!(
                url == RP_RELEASES_API,
                "unexpected RP releases API endpoint"
            )
        }
        RpRequestKind::ReleaseAsset => anyhow::ensure!(
            parsed.host_str() == Some("github.com")
                && parsed
                    .path()
                    .starts_with("/JonathonRP/zed/releases/download/"),
            "unexpected RP release asset endpoint"
        ),
    }
    Ok(parsed)
}

async fn rp_get_response(
    client: &Arc<HttpClientWithUrl>,
    initial_url: &str,
    kind: RpRequestKind,
) -> Result<http_client::Response<http_client::AsyncBody>> {
    use http_client::{HttpRequestExt as _, RedirectPolicy};

    let mut url = validate_rp_initial_url(kind, initial_url)?;
    for redirect_count in 0..=RP_MAX_REDIRECTS {
        let request = http_client::Request::builder()
            .method(http_client::Method::GET)
            .uri(url.as_str())
            .header(http_client::http::header::ACCEPT_ENCODING, "identity")
            .follow_redirects(RedirectPolicy::NoFollow)
            .body(http_client::AsyncBody::default())?;
        let response = client.send(request).await?;
        if !matches!(
            response.status(),
            http_client::StatusCode::MOVED_PERMANENTLY
                | http_client::StatusCode::FOUND
                | http_client::StatusCode::SEE_OTHER
                | http_client::StatusCode::TEMPORARY_REDIRECT
                | http_client::StatusCode::PERMANENT_REDIRECT
        ) {
            anyhow::ensure!(
                response.status() == http_client::StatusCode::OK,
                "RP request failed with status {}",
                response.status()
            );
            return Ok(response);
        }
        anyhow::ensure!(
            redirect_count < RP_MAX_REDIRECTS,
            "RP request exceeded {RP_MAX_REDIRECTS} redirects"
        );
        let location = response
            .headers()
            .get(http_client::http::header::LOCATION)
            .context("RP redirect omitted Location")?
            .to_str()
            .context("RP redirect Location is not text")?;
        let next = url.join(location).context("invalid RP redirect URL")?;
        anyhow::ensure!(
            is_allowed_rp_redirect(kind, &next),
            "RP redirect target is not an allowed GitHub endpoint"
        );
        url = next;
    }
    unreachable!()
}

async fn read_rp_response_bounded(
    response: &mut http_client::Response<http_client::AsyncBody>,
    limit: u64,
) -> Result<Vec<u8>> {
    if let Some(content_length) = response
        .headers()
        .get(http_client::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
    {
        anyhow::ensure!(content_length <= limit, "RP response exceeds size limit");
    }
    let mut body = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read = response.body_mut().read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        anyhow::ensure!(
            body.len() as u64 + read as u64 <= limit,
            "RP response exceeds size limit"
        );
        body.extend_from_slice(&buffer[..read]);
    }
    Ok(body)
}

async fn fetch_rp_manifest_url(
    client: &Arc<HttpClientWithUrl>,
    tag: &str,
    url: &str,
) -> Result<ValidatedRpManifest> {
    anyhow::ensure!(
        url == expected_rp_asset_url(tag, RP_MANIFEST_NAME),
        "RP manifest URL does not match its release tag"
    );
    let mut response = rp_get_response(client, url, RpRequestKind::ReleaseAsset).await?;
    let body = read_rp_response_bounded(&mut response, RP_MANIFEST_MAX_BYTES).await?;
    let manifest: RpUpdateManifest =
        serde_json::from_slice(&body).context("invalid strict RP update manifest")?;
    let manifest = validate_rp_manifest(manifest)?;
    anyhow::ensure!(
        manifest.identity.tag == tag,
        "RP manifest tag does not match selected GitHub release"
    );
    Ok(manifest)
}

async fn discover_latest_rp_manifest(
    client: &Arc<HttpClientWithUrl>,
) -> Result<ValidatedRpManifest> {
    let endpoint = release_discovery_endpoint(UpdateEndpointSource::Rp);
    let mut response = rp_get_response(client, endpoint, RpRequestKind::Api).await?;
    let body = read_rp_response_bounded(&mut response, RP_API_MAX_BYTES).await?;
    let releases: Vec<GithubRelease> =
        serde_json::from_slice(&body).context("invalid GitHub releases API response")?;
    let (release, manifest_url) = select_newest_rp_release(&releases)?;
    fetch_rp_manifest_url(client, &release.tag_name, &manifest_url).await
}

fn newer_rp_candidate(
    installed: &RpCompileIdentity,
    staged: Option<&RpCandidateIdentity>,
    candidate: &RpCandidateIdentity,
) -> Result<bool> {
    anyhow::ensure!(
        candidate.upstream_version >= installed.upstream_version,
        "RP candidate upstream version would downgrade the installed upstream version"
    );
    let minimum_calendar = staged
        .map(|staged| staged.calendar.max(installed.calendar))
        .unwrap_or(installed.calendar);
    if candidate.calendar <= minimum_calendar {
        return Ok(false);
    }
    Ok(true)
}

fn validate_matching_remote_manifest(
    installed: &RpCompileIdentity,
    candidate: &ValidatedRpManifest,
) -> Result<()> {
    anyhow::ensure!(
        candidate.identity.calendar == installed.calendar
            && candidate.identity.calendar_version == installed.calendar_version
            && candidate.identity.tag == installed.tag
            && candidate.identity.commit == installed.commit
            && candidate.identity.upstream_version == installed.upstream_version,
        "RP remote-server manifest does not exactly match the running RP build"
    );
    Ok(())
}

fn rp_remote_asset<'a>(
    manifest: &'a ValidatedRpManifest,
    os: &str,
    arch: &str,
) -> Result<&'a RpManifestAsset> {
    anyhow::ensure!(
        os == "linux" && arch == "x86_64",
        "RP remote server is unsupported for {os}/{arch}; refusing official fallback"
    );
    Ok(&manifest.manifest.assets.linux_x86_64_remote_server)
}

impl AutoUpdater {
    pub fn get(cx: &mut App) -> Option<Entity<Self>> {
        cx.default_global::<GlobalAutoUpdate>().0.clone()
    }

    fn new(current_version: Version, client: Arc<Client>, cx: &mut Context<Self>) -> Self {
        // On windows, executable files cannot be overwritten while they are
        // running, so we must wait to overwrite the application until quitting
        // or restarting. When quitting the app, we spawn the auto update helper
        // to finish the auto update process after Zed exits. When restarting
        // the app after an update, we use `set_restart_path` to run the auto
        // update helper instead of the app, so that it can overwrite the app
        // and then spawn the new binary.
        #[cfg(target_os = "windows")]
        let quit_subscription = Some(cx.on_app_quit(|_, _| finalize_auto_update_on_quit()));
        #[cfg(not(target_os = "windows"))]
        let quit_subscription = None;

        cx.on_app_restart(|this, _| {
            this.quit_subscription.take();
        })
        .detach();

        // A download or check that was in flight when the machine went to sleep
        // is almost certainly riding a TCP connection that silently died during
        // suspend, so it would otherwise appear to stall indefinitely.
        let wake_subscription = cx.on_system_wake({
            let this = cx.entity().downgrade();
            move |cx| {
                this.update(cx, |this, cx| this.restart_after_wake(cx)).ok();
            }
        });

        Self {
            status: AutoUpdateStatus::Idle,
            current_version,
            staged_rp_candidate: None,
            client,
            pending_poll: None,
            quit_subscription,
            update_check_type: UpdateCheckType::Automatic,
            _wake_subscription: wake_subscription,
            dismissed_status: None,
        }
    }

    fn restart_after_wake(&mut self, cx: &mut Context<Self>) {
        // Only network phases can be safely restarted. `Installing` is a local
        // operation (mounting a dmg, rsync, etc.) that must not be interrupted.
        if !matches!(
            self.status,
            AutoUpdateStatus::Checking | AutoUpdateStatus::Downloading { .. }
        ) {
            return;
        }

        let check_type = self.update_check_type;
        self.pending_poll.take();
        self.status = AutoUpdateStatus::Idle;
        self.poll(check_type, cx);
    }

    pub fn start_polling(&self, cx: &mut Context<Self>) -> Task<Result<()>> {
        let poll_interval =
            ReleaseChannel::try_global(cx).map_or(POLL_INTERVAL, |channel| match channel {
                ReleaseChannel::Nightly => NIGHTLY_POLL_INTERVAL,
                _ => POLL_INTERVAL,
            });

        cx.spawn(async move |this, cx| {
            if cfg!(target_os = "windows") {
                use util::ResultExt;

                cleanup_windows()
                    .await
                    .context("failed to cleanup old directories")
                    .log_err();
            }

            #[cfg(all(not(target_os = "windows"), not(test)))]
            cx.background_spawn(cleanup_stale_installer_dirs()).detach();

            loop {
                this.update(cx, |this, cx| this.poll(UpdateCheckType::Automatic, cx))?;
                cx.background_executor().timer(poll_interval).await;
            }
        })
    }

    pub fn update_check_type(&self) -> UpdateCheckType {
        self.update_check_type
    }

    pub fn poll(&mut self, check_type: UpdateCheckType, cx: &mut Context<Self>) {
        if check_type.is_manual() {
            self.dismissed_status = None;
        }
        if self.pending_poll.is_some() {
            if self.update_check_type == UpdateCheckType::Automatic {
                self.update_check_type = check_type;
                cx.notify();
            }
            return;
        }
        self.update_check_type = check_type;

        cx.notify();

        self.pending_poll = Some(cx.spawn(async move |this, cx| {
            let result = Self::update(this.upgrade()?, cx).await;
            this.update(cx, |this, cx| {
                this.pending_poll = None;
                if let Err(error) = result {
                    let is_missing_dependency =
                        error.downcast_ref::<MissingDependencyError>().is_some();
                    this.status = match check_type {
                        UpdateCheckType::Automatic if is_missing_dependency => {
                            log::warn!("auto-update: {}", error);
                            AutoUpdateStatus::Errored {
                                error: Arc::new(error),
                            }
                        }
                        // Be quiet if the check was automated (e.g. when offline)
                        UpdateCheckType::Automatic => {
                            log::info!("auto-update check failed: error:{:?}", error);
                            AutoUpdateStatus::Idle
                        }
                        UpdateCheckType::Manual => {
                            log::error!("auto-update failed: error:{:?}", error);
                            AutoUpdateStatus::Errored {
                                error: Arc::new(error),
                            }
                        }
                    };

                    cx.notify();
                }
            })
            .ok()
        }));
    }

    pub fn current_version(&self) -> Version {
        self.current_version.clone()
    }

    pub fn status(&self) -> AutoUpdateStatus {
        self.status.clone()
    }

    pub fn dismissed_status(&self) -> Option<AutoUpdateStatus> {
        self.dismissed_status.clone()
    }

    pub fn dismiss_status(&mut self, status: AutoUpdateStatus, cx: &mut Context<Self>) {
        self.dismissed_status = Some(status);
        cx.notify();
    }

    pub fn dismiss(&mut self, cx: &mut Context<Self>) -> bool {
        if let AutoUpdateStatus::Idle = self.status {
            return false;
        }
        self.status = AutoUpdateStatus::Idle;
        cx.notify();
        true
    }

    // If you are packaging Zed and need to override the place it downloads SSH remotes from,
    // you can override this function. You should also update get_remote_server_release_url to return
    // Ok(None).
    pub async fn download_remote_server_release(
        release_channel: ReleaseChannel,
        version: Option<Version>,
        os: &str,
        arch: &str,
        set_status: impl Fn(&str, &mut AsyncApp) + Send + 'static,
        cx: &mut AsyncApp,
    ) -> Result<PathBuf> {
        let this = cx.update(|cx| {
            cx.default_global::<GlobalAutoUpdate>()
                .0
                .clone()
                .context("auto-update not initialized")
        })?;

        if let Some(metadata) = release_channel::rp_release_metadata() {
            anyhow::ensure!(
                release_channel == ReleaseChannel::Stable,
                "RP metadata may only update the stable channel"
            );
            set_status("Fetching matching RP remote server release", cx);
            let (client, installed_version, commit) = this.read_with(cx, |this, cx| {
                (
                    this.client.http_client(),
                    this.current_version.clone(),
                    AppCommitSha::try_global(cx).map(|sha| sha.full()),
                )
            });
            let installed = RpCompileIdentity::new(metadata, installed_version, commit)?;
            if let Some(requested_version) = version {
                anyhow::ensure!(
                    normalized_upstream_version(requested_version) == installed.upstream_version,
                    "requested RP remote server version does not match the running build"
                );
            }
            let manifest_url = expected_rp_asset_url(&installed.tag, RP_MANIFEST_NAME);
            let manifest = fetch_rp_manifest_url(&client, &installed.tag, &manifest_url).await?;
            validate_matching_remote_manifest(&installed, &manifest)?;
            let asset = rp_remote_asset(&manifest, os, arch)?.clone();

            let servers_dir = paths::remote_servers_dir();
            let channel_dir = servers_dir.join(release_channel.dev_name());
            let platform_dir = channel_dir.join(format!("{}-{}", os, arch));
            let version_path = platform_dir.join(format!("{}.gz", installed.upstream_version));
            smol::fs::create_dir_all(&platform_dir).await.ok();

            let cached_is_valid = verify_rp_file(&version_path, &asset).await.unwrap_or(false);
            if !cached_is_valid {
                _ = smol::fs::remove_file(&version_path).await;
                log::warn!("{RP_UNSIGNED_TRUST_NOTICE}");
                set_status("Downloading verified RP remote server", cx);
                download_rp_asset(&version_path, &asset, client, |_| {}).await?;
            }
            if let Err(error) =
                cleanup_remote_server_cache(&platform_dir, &version_path, REMOTE_SERVER_CACHE_LIMIT)
                    .await
            {
                log::warn!(
                    "Failed to clean up remote server cache in {:?}: {error:#}",
                    platform_dir
                );
            }
            return Ok(version_path);
        }

        set_status("Fetching remote server release", cx);
        let release = Self::get_release_asset(
            &this,
            release_channel,
            version,
            "zed-remote-server",
            os,
            arch,
            cx,
        )
        .await?;

        let servers_dir = paths::remote_servers_dir();
        let channel_dir = servers_dir.join(release_channel.dev_name());
        let platform_dir = channel_dir.join(format!("{}-{}", os, arch));
        let version_path = platform_dir.join(format!("{}.gz", release.version));
        smol::fs::create_dir_all(&platform_dir).await.ok();

        let client = this.read_with(cx, |this, _| this.client.http_client());

        if smol::fs::metadata(&version_path).await.is_err() {
            log::info!(
                "downloading zed-remote-server {os} {arch} version {}",
                release.version
            );
            set_status("Downloading remote server", cx);
            download_remote_server_binary(&version_path, release, client).await?;
        }

        if let Err(error) =
            cleanup_remote_server_cache(&platform_dir, &version_path, REMOTE_SERVER_CACHE_LIMIT)
                .await
        {
            log::warn!(
                "Failed to clean up remote server cache in {:?}: {error:#}",
                platform_dir
            );
        }

        Ok(version_path)
    }

    pub async fn get_remote_server_release_url(
        channel: ReleaseChannel,
        version: Option<Version>,
        os: &str,
        arch: &str,
        cx: &mut AsyncApp,
    ) -> Result<Option<String>> {
        if release_channel::rp_release_metadata().is_some() {
            anyhow::ensure!(
                channel == ReleaseChannel::Stable && os == "linux" && arch == "x86_64",
                "unsupported RP remote-server request; refusing official fallback"
            );
            return Ok(None);
        }

        let this = cx.update(|cx| {
            cx.default_global::<GlobalAutoUpdate>()
                .0
                .clone()
                .context("auto-update not initialized")
        })?;

        let release =
            Self::get_release_asset(&this, channel, version, "zed-remote-server", os, arch, cx)
                .await?;

        Ok(Some(release.url))
    }

    async fn get_release_asset(
        this: &Entity<Self>,
        release_channel: ReleaseChannel,
        version: Option<Version>,
        asset: &str,
        os: &str,
        arch: &str,
        cx: &mut AsyncApp,
    ) -> Result<ReleaseAsset> {
        let client = this.read_with(cx, |this, _| this.client.clone());

        let (system_id, metrics_id, is_staff) = if client.telemetry().metrics_enabled() {
            (
                client.telemetry().system_id(),
                client.telemetry().metrics_id(),
                client.telemetry().is_staff(),
            )
        } else {
            (None, None, None)
        };

        let version = if let Some(mut version) = version {
            version.pre = semver::Prerelease::EMPTY;
            version.build = semver::BuildMetadata::EMPTY;
            version.to_string()
        } else {
            "latest".to_string()
        };
        let http_client = client.http_client();

        let path = format!("/releases/{}/{}/asset", release_channel.dev_name(), version,);
        let url = http_client.build_zed_cloud_url_with_query(
            &path,
            AssetQuery {
                os,
                arch,
                asset,
                metrics_id: metrics_id.as_deref(),
                system_id: system_id.as_deref(),
                is_staff,
            },
        )?;

        let mut response = http_client
            .get(url.as_str(), Default::default(), true)
            .await?;
        let mut body = Vec::new();
        response.body_mut().read_to_end(&mut body).await?;

        anyhow::ensure!(
            response.status().is_success(),
            "failed to fetch release: {:?}",
            String::from_utf8_lossy(&body),
        );

        serde_json::from_slice(body.as_slice()).with_context(|| {
            format!(
                "error deserializing release {:?}",
                String::from_utf8_lossy(&body),
            )
        })
    }

    async fn update(this: Entity<Self>, cx: &mut AsyncApp) -> Result<()> {
        let (client, installed_version, previous_status, release_channel, staged_rp_candidate) =
            this.read_with(cx, |this, cx| {
                (
                    this.client.http_client(),
                    this.current_version.clone(),
                    this.status.clone(),
                    ReleaseChannel::try_global(cx).unwrap_or(ReleaseChannel::Stable),
                    this.staged_rp_candidate.clone(),
                )
            });

        Self::check_dependencies()?;

        this.update(cx, |this, cx| {
            this.status = AutoUpdateStatus::Checking;
            log::info!("Auto Update: checking for updates");
            cx.notify();
        });

        let rp_metadata = release_channel::rp_release_metadata();
        let (newer_version, download, rp_candidate_to_cache) =
            if update_endpoint_source(rp_metadata) == UpdateEndpointSource::Rp {
                anyhow::ensure!(
                    release_channel == ReleaseChannel::Stable,
                    "RP metadata may only update the stable channel"
                );
                anyhow::ensure!(
                    OS == "windows" && ARCH == "x86_64",
                    "RP app updater is unsupported for {OS}/{ARCH}; refusing official fallback"
                );
                let metadata = rp_metadata.expect("source checked above");
                let installed_commit =
                    cx.update(|cx| AppCommitSha::try_global(cx).map(|sha| sha.full()));
                let installed =
                    RpCompileIdentity::new(metadata, installed_version.clone(), installed_commit)?;
                log::warn!("{RP_UNSIGNED_TRUST_NOTICE}");
                let candidate = discover_latest_rp_manifest(&client).await?;
                if !newer_rp_candidate(
                    &installed,
                    staged_rp_candidate.as_ref(),
                    &candidate.identity,
                )? {
                    (None, None, None)
                } else {
                    (
                        Some(candidate.identity.upstream_version.clone()),
                        Some(UpdateDownload::Rp(
                            candidate.manifest.assets.windows_x86_64_installer.clone(),
                        )),
                        Some(candidate.identity),
                    )
                }
            } else {
                let fetched_release_data =
                    Self::get_release_asset(&this, release_channel, None, "zed", OS, ARCH, cx)
                        .await?;
                let fetched_version = fetched_release_data.clone().version;
                let app_commit_sha =
                    Ok(cx.update(|cx| AppCommitSha::try_global(cx).map(|sha| sha.full())));
                let newer_version = Self::check_if_fetched_version_is_newer(
                    release_channel,
                    app_commit_sha,
                    installed_version,
                    fetched_version,
                    previous_status.clone(),
                )?;
                (
                    newer_version,
                    Some(UpdateDownload::Official(fetched_release_data)),
                    None,
                )
            };

        let Some(newer_version) = newer_version else {
            this.update(cx, |this, cx| {
                let status = match previous_status {
                    AutoUpdateStatus::Updated { .. } => previous_status,
                    _ => AutoUpdateStatus::Idle,
                };
                this.status = status;
                cx.notify();
            });
            return Ok(());
        };

        this.update(cx, |this, cx| {
            this.status = AutoUpdateStatus::Downloading {
                version: newer_version.clone(),
                progress: None,
            };
            cx.notify();
        });

        let rp_installer_asset = match download.as_ref() {
            Some(UpdateDownload::Rp(asset)) => Some(asset.clone()),
            _ => None,
        };
        let rp_staging_root: Option<PathBuf> = {
            #[cfg(target_os = "windows")]
            {
                if rp_installer_asset.is_some() {
                    let running_app_path = cx.update(|cx| cx.app_path())?;
                    let metadata = rp_metadata.context("RP download has no compile metadata")?;
                    Some(verify_rp_windows_running_install(&running_app_path, metadata)?.0)
                } else {
                    None
                }
            }
            #[cfg(not(target_os = "windows"))]
            {
                None
            }
        };
        let installer_dir = InstallerDir::new(rp_staging_root.as_deref())
            .await
            .context("Failed to create installer dir")?;
        let target_path = Self::target_path(&installer_dir).await?;
        let progress_entity = this.clone();
        let mut progress_cx = cx.clone();
        let mut on_progress = move |progress| {
            progress_entity.update(&mut progress_cx, |this, cx| {
                if let AutoUpdateStatus::Downloading {
                    progress: current_progress,
                    ..
                } = &mut this.status
                {
                    *current_progress = progress;
                    cx.notify();
                }
            });
        };
        match download.context("newer update has no download")? {
            UpdateDownload::Official(release) => {
                download_release(&target_path, release, client, on_progress).await
            }
            UpdateDownload::Rp(asset) => {
                download_rp_asset(&target_path, &asset, client, &mut on_progress).await
            }
        }
        .with_context(|| format!("Failed to download update to {}", target_path.display()))?;

        this.update(cx, |this, cx| {
            this.status = AutoUpdateStatus::Installing {
                version: newer_version.clone(),
            };
            cx.notify();
        });

        #[cfg(test)]
        let install_result = match cx
            .try_read_global::<tests::InstallOverride, _>(|g, _| g.0.clone())
            .map(|test_install| test_install(&target_path, cx))
        {
            Some(result) => result,
            None => return Ok(()),
        };

        #[cfg(not(test))]
        let install_result = {
            let running_app_path = cx.update(|cx| cx.app_path())?;
            let background_executor = cx.background_executor().clone();
            let channel = cx.update(|cx| ReleaseChannel::global(cx).dev_name());
            cx.background_spawn(Self::install_release(
                installer_dir,
                target_path.clone(),
                running_app_path,
                channel,
                background_executor,
                rp_installer_asset,
            ))
            .await
        };
        let new_binary_path = install_result
            .with_context(|| format!("Failed to install update at: {}", target_path.display()))?;
        if let Some(new_binary_path) = new_binary_path {
            cx.update(|cx| cx.set_restart_path(new_binary_path));
        }

        this.update(cx, |this, cx| {
            this.set_should_show_update_notification(true, cx)
                .detach_and_log_err(cx);
            if let Some(candidate) = rp_candidate_to_cache {
                this.staged_rp_candidate = Some(candidate);
            }
            this.status = AutoUpdateStatus::Updated {
                version: newer_version,
            };
            cx.notify();
        });
        Ok(())
    }

    fn check_if_fetched_version_is_newer(
        release_channel: ReleaseChannel,
        app_commit_sha: Result<Option<String>>,
        installed_version: Version,
        fetched_version: String,
        status: AutoUpdateStatus,
    ) -> Result<Option<Version>> {
        let fetched_version = fetched_version.parse::<Version>()?;

        match release_channel {
            ReleaseChannel::Nightly => {
                let should_download = if let AutoUpdateStatus::Updated { version } = status {
                    fetched_version != version
                } else {
                    let fetched_sha = fetched_version.build.as_str().rsplit('.').next();
                    app_commit_sha
                        .ok()
                        .flatten()
                        .is_none_or(|sha| fetched_sha != Some(sha.as_str()))
                };
                Ok(should_download.then_some(fetched_version))
            }
            _ => {
                let current_version = if let AutoUpdateStatus::Updated { version } = status {
                    version
                } else {
                    installed_version
                };
                Ok(Self::check_if_fetched_version_is_newer_non_nightly(
                    current_version,
                    fetched_version,
                ))
            }
        }
    }

    fn check_dependencies() -> Result<()> {
        #[cfg(target_os = "linux")]
        if which::which("rsync").is_err() {
            let install_hint = linux_rsync_install_hint();
            return Err(MissingDependencyError(format!(
                "rsync is required for auto-updates but is not installed. {install_hint}"
            ))
            .into());
        }

        #[cfg(target_os = "macos")]
        anyhow::ensure!(
            which::which("rsync").is_ok(),
            "Could not auto-update because the required rsync utility was not found."
        );

        Ok(())
    }

    async fn target_path(installer_dir: &InstallerDir) -> Result<PathBuf> {
        let filename = match OS {
            "macos" => anyhow::Ok("Zed.dmg"),
            "linux" => Ok("zed.tar.gz"),
            "windows" => Ok("Zed.exe"),
            unsupported_os => anyhow::bail!("not supported: {unsupported_os}"),
        }?;

        Ok(installer_dir.path().join(filename))
    }

    #[cfg_attr(test, allow(dead_code))]
    async fn install_release(
        installer_dir: InstallerDir,
        target_path: PathBuf,
        running_app_path: PathBuf,
        channel: &str,
        background_executor: BackgroundExecutor,
        rp_installer_asset: Option<RpManifestAsset>,
    ) -> Result<Option<PathBuf>> {
        match OS {
            "macos" => {
                install_release_macos(
                    &installer_dir,
                    &target_path,
                    running_app_path,
                    &background_executor,
                )
                .await
            }
            "linux" => {
                install_release_linux(&installer_dir, &target_path, channel, running_app_path).await
            }
            "windows" => {
                install_release_windows(&target_path, running_app_path, rp_installer_asset).await
            }
            unsupported_os => anyhow::bail!("not supported: {unsupported_os}"),
        }
    }

    fn check_if_fetched_version_is_newer_non_nightly(
        mut installed_version: Version,
        fetched_version: Version,
    ) -> Option<Version> {
        // For non-nightly releases, ignore build and pre-release fields as they're not provided by our endpoints right now.
        installed_version.pre = semver::Prerelease::EMPTY;
        installed_version.build = semver::BuildMetadata::EMPTY;
        (fetched_version > installed_version).then_some(fetched_version)
    }

    pub fn set_should_show_update_notification(
        &self,
        should_show: bool,
        cx: &App,
    ) -> Task<Result<()>> {
        let kvp = KeyValueStore::global(cx);
        cx.background_spawn(async move {
            if should_show {
                kvp.write_kvp(
                    SHOULD_SHOW_UPDATE_NOTIFICATION_KEY.to_string(),
                    "".to_string(),
                )
                .await?;
            } else {
                kvp.delete_kvp(SHOULD_SHOW_UPDATE_NOTIFICATION_KEY.to_string())
                    .await?;
            }
            Ok(())
        })
    }

    pub fn should_show_update_notification(&self, cx: &App) -> Task<Result<bool>> {
        let kvp = KeyValueStore::global(cx);
        cx.background_spawn(async move {
            Ok(kvp.read_kvp(SHOULD_SHOW_UPDATE_NOTIFICATION_KEY)?.is_some())
        })
    }
}

async fn download_remote_server_binary(
    target_path: &PathBuf,
    release: ReleaseAsset,
    client: Arc<HttpClientWithUrl>,
) -> Result<()> {
    let temp = tempfile::Builder::new().tempfile_in(remote_servers_dir())?;
    let mut temp_file = File::create(&temp).await?;

    let mut response = client.get(&release.url, Default::default(), true).await?;
    anyhow::ensure!(
        response.status().is_success(),
        "failed to download remote server release: {:?}",
        response.status()
    );
    smol::io::copy(response.body_mut(), &mut temp_file).await?;
    smol::fs::rename(&temp, &target_path).await?;

    Ok(())
}

async fn verify_rp_file(path: &Path, asset: &RpManifestAsset) -> Result<bool> {
    let metadata = match smol::fs::metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if metadata.len() != asset.size {
        return Ok(false);
    }
    let mut file = File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()) == asset.sha256)
}

async fn download_rp_asset(
    target_path: &Path,
    asset: &RpManifestAsset,
    client: Arc<HttpClientWithUrl>,
    mut on_progress: impl FnMut(Option<f32>),
) -> Result<()> {
    let result = async {
        anyhow::ensure!(
            asset.url
                == expected_rp_asset_url(
                    asset
                        .url
                        .strip_prefix(&format!("{RP_REPOSITORY_RELEASES}/"))
                        .and_then(|rest| rest.split_once('/'))
                        .map(|(tag, _)| tag)
                        .context("RP asset URL has no release tag")?,
                    &asset.name
                ),
            "RP asset URL is not an exact repository release URL"
        );
        let mut response =
            rp_get_response(&client, &asset.url, RpRequestKind::ReleaseAsset).await?;
        if let Some(content_length) = response
            .headers()
            .get(http_client::http::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
        {
            anyhow::ensure!(
                content_length == asset.size,
                "RP asset Content-Length does not match manifest"
            );
        }

        let mut target_file = File::create(target_path).await?;
        let mut hasher = Sha256::new();
        let mut written = 0u64;
        let mut last_reported_percent = None;
        let mut buffer = [0u8; 8192];
        loop {
            let read = response.body_mut().read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            anyhow::ensure!(
                written + read as u64 <= asset.size,
                "RP asset exceeded its declared size"
            );
            target_file.write_all(&buffer[..read]).await?;
            hasher.update(&buffer[..read]);
            written += read as u64;
            let fraction = (written as f32 / asset.size as f32).clamp(0.0, 1.0);
            let percent = (fraction * 100.0) as u8;
            if last_reported_percent != Some(percent) {
                last_reported_percent = Some(percent);
                on_progress(Some(fraction));
            }
        }
        target_file.flush().await?;
        drop(target_file);
        anyhow::ensure!(
            written == asset.size,
            "RP asset size mismatch: expected {}, wrote {written}",
            asset.size
        );
        let digest = format!("{:x}", hasher.finalize());
        anyhow::ensure!(
            digest == asset.sha256,
            "RP asset SHA-256 mismatch; {RP_UNSIGNED_TRUST_NOTICE}"
        );
        if last_reported_percent != Some(100) {
            on_progress(Some(1.0));
        }
        log::info!(
            "downloaded verified RP update. path:{:?}, bytes_written:{written}",
            target_path
        );
        Ok(())
    }
    .await;
    if result.is_err() {
        _ = smol::fs::remove_file(target_path).await;
    }
    result
}

async fn cleanup_remote_server_cache(
    platform_dir: &Path,
    keep_path: &Path,
    limit: usize,
) -> Result<()> {
    if limit == 0 {
        return Ok(());
    }

    let mut entries = smol::fs::read_dir(platform_dir).await?;
    let now = SystemTime::now();
    let mut candidates = Vec::new();

    while let Some(entry) = entries.next().await {
        let entry = entry?;
        let path = entry.path();
        if path.extension() != Some(OsStr::new("gz")) {
            continue;
        }

        let mtime = if path == keep_path {
            now
        } else {
            smol::fs::metadata(&path)
                .await
                .and_then(|metadata| metadata.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH)
        };

        candidates.push((path, mtime));
    }

    if candidates.len() <= limit {
        return Ok(());
    }

    candidates.sort_by(|(path_a, time_a), (path_b, time_b)| {
        time_b.cmp(time_a).then_with(|| path_a.cmp(path_b))
    });

    for (index, (path, _)) in candidates.into_iter().enumerate() {
        if index < limit || path == keep_path {
            continue;
        }

        if let Err(error) = smol::fs::remove_file(&path).await {
            log::warn!(
                "Failed to remove old remote server archive {:?}: {}",
                path,
                error
            );
        }
    }

    Ok(())
}

async fn download_release(
    target_path: &Path,
    release: ReleaseAsset,
    client: Arc<HttpClientWithUrl>,
    mut on_progress: impl FnMut(Option<f32>),
) -> Result<()> {
    let mut target_file = File::create(&target_path).await?;

    let mut response = client.get(&release.url, Default::default(), true).await?;
    anyhow::ensure!(
        response.status().is_success(),
        "failed to download update: {:?}",
        response.status()
    );

    let total_bytes = response
        .headers()
        .get(http_client::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|total_bytes| *total_bytes > 0);

    let mut downloaded_bytes: u64 = 0;
    let mut last_reported_percent: Option<u8> = None;
    let mut buffer = [0u8; 8192];
    let body = response.body_mut();
    loop {
        let bytes_read = body.read(&mut buffer).await?;
        if bytes_read == 0 {
            break;
        }
        target_file.write_all(&buffer[..bytes_read]).await?;
        downloaded_bytes += bytes_read as u64;

        if let Some(total_bytes) = total_bytes {
            let fraction = (downloaded_bytes as f32 / total_bytes as f32).clamp(0.0, 1.0);
            // Only report when the whole-number percentage changes to avoid notifying the UI on every chunk.
            let percent = (fraction * 100.0) as u8;
            if last_reported_percent != Some(percent) {
                last_reported_percent = Some(percent);
                on_progress(Some(fraction));
            }
        }
    }
    target_file.flush().await?;
    if total_bytes.is_some() && last_reported_percent != Some(100) {
        on_progress(Some(1.0));
    }
    log::info!("downloaded update. path:{:?}", target_path);

    Ok(())
}

async fn install_release_linux(
    temp_dir: &InstallerDir,
    downloaded_tar_gz: &Path,
    channel: &str,
    running_app_path: PathBuf,
) -> Result<Option<PathBuf>> {
    let home_dir = PathBuf::from(env::var("HOME").context("no HOME env var set")?);

    let extracted = temp_dir.path().join("zed");
    fs::create_dir_all(&extracted)
        .await
        .context("failed to create directory into which to extract update")?;

    let mut cmd = new_command("tar");
    cmd.arg("-xzf")
        .arg(&downloaded_tar_gz)
        .arg("-C")
        .arg(&extracted);
    let output = cmd
        .output()
        .await
        .with_context(|| "failed to extract: {cmd}")?;

    anyhow::ensure!(
        output.status.success(),
        "failed to extract {:?} to {:?}: {:?}",
        downloaded_tar_gz,
        extracted,
        String::from_utf8_lossy(&output.stderr)
    );

    let suffix = if channel != "stable" {
        format!("-{}", channel)
    } else {
        String::default()
    };
    let app_folder_name = format!("zed{}.app", suffix);

    let from = extracted.join(&app_folder_name);
    let mut to = home_dir.join(".local");

    let expected_suffix = format!("{}/libexec/zed-editor", app_folder_name);

    if let Some(prefix) = running_app_path
        .to_str()
        .and_then(|str| str.strip_suffix(&expected_suffix))
    {
        to = PathBuf::from(prefix);
    }

    let mut cmd = new_command("rsync");
    cmd.args(["-av", "--delete"]).arg(&from).arg(&to);
    let output = cmd
        .output()
        .await
        .with_context(|| "failed to rsync: {cmd}")?;

    anyhow::ensure!(
        output.status.success(),
        "failed to copy Zed update from {:?} to {:?}: {:?}",
        from,
        to,
        String::from_utf8_lossy(&output.stderr)
    );

    Ok(Some(to.join(expected_suffix)))
}

async fn install_release_macos(
    temp_dir: &InstallerDir,
    downloaded_dmg: &Path,
    running_app_path: PathBuf,
    background_executor: &BackgroundExecutor,
) -> Result<Option<PathBuf>> {
    let running_app_filename = running_app_path
        .file_name()
        .with_context(|| format!("invalid running app path {running_app_path:?}"))?;

    let mount_path = temp_dir.path().join("Zed");
    let mut mounted_app_path: OsString = mount_path.join(running_app_filename).into();

    mounted_app_path.push("/");
    let mut cmd = new_command("hdiutil");
    cmd.args(["attach", "-nobrowse"])
        .arg(&downloaded_dmg)
        .arg("-mountroot")
        .arg(temp_dir.path());
    let output = cmd
        .output()
        .await
        .with_context(|| "failed to mount: {cmd}")?;

    anyhow::ensure!(
        output.status.success(),
        "failed to mount: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );

    let unmounter = MacOsUnmounter {
        mount_path: mount_path.clone(),
        background_executor,
    };

    let mut cmd = new_command("rsync");
    cmd.args(["-av", "--delete", "--exclude", "Icon?"])
        .arg(&mounted_app_path)
        .arg(&running_app_path);
    let rsync_output = cmd.output().await;

    // Await the unmount (even if rsync failed) so that the installer temp dir
    // can be deleted once this function returns.
    unmounter.unmount().await;

    let output = rsync_output.with_context(|| "failed to rsync: {cmd}")?;

    anyhow::ensure!(
        output.status.success(),
        "failed to copy app: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );

    Ok(None)
}

/// Removes stale installer dirs from the system temp dir. Older Zed versions
/// leaked one per update by deleting the dir while the downloaded disk image
/// was still mounted inside it, which made the deletion fail silently.
#[cfg(any(rust_analyzer, all(not(target_os = "windows"), not(test))))]
async fn cleanup_stale_installer_dirs() {
    const STALE_INSTALLER_DIR_AGE: Duration = Duration::from_secs(24 * 60 * 60);

    let temp_dir = std::env::temp_dir();
    let Ok(mut entries) = fs::read_dir(&temp_dir).await else {
        log::warn!("failed to read temp dir {temp_dir:?} while cleaning up installer dirs");
        return;
    };
    while let Some(entry) = entries.next().await {
        let Ok(entry) = entry else {
            continue;
        };
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with(INSTALLER_DIR_PREFIX)
        {
            continue;
        }
        // Leave recent dirs alone, as they may belong to an update currently
        // in progress in another Zed instance.
        let is_stale = entry.metadata().await.ok().is_some_and(|metadata| {
            metadata.is_dir()
                && metadata.modified().ok().is_some_and(|modified| {
                    SystemTime::now()
                        .duration_since(modified)
                        .is_ok_and(|age| age > STALE_INSTALLER_DIR_AGE)
                })
        });
        if is_stale {
            if let Err(error) = fs::remove_dir_all(entry.path()).await {
                log::warn!(
                    "failed to remove stale installer dir {:?}: {error}",
                    entry.path()
                );
            } else {
                log::info!("removed stale installer dir {:?}", entry.path());
            }
        }
    }
}

async fn cleanup_windows() -> Result<()> {
    let current_exe = std::env::current_exe()?;
    let parent = if release_channel::rp_release_metadata().is_some() {
        let local_app_data = env::var("LOCALAPPDATA").context("LOCALAPPDATA is not set")?;
        PathBuf::from(
            validate_rp_windows_path_model(&local_app_data, &current_exe.to_string_lossy())?.root,
        )
    } else {
        current_exe
            .parent()
            .context("No parent dir for Zed.exe")?
            .to_owned()
    };

    // keep in sync with crates/auto_update_helper/src/updater.rs
    _ = smol::fs::remove_dir(parent.join("updates")).await;
    _ = smol::fs::remove_dir(parent.join("install")).await;
    _ = smol::fs::remove_dir(parent.join("old")).await;

    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct RpWindowsPathModel {
    root: String,
    running_exe: String,
}

fn normalize_windows_path_model(path: &str) -> Result<String> {
    anyhow::ensure!(!path.is_empty(), "empty Windows path");
    let path = if path
        .as_bytes()
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(br"\\?\"))
    {
        &path[4..]
    } else {
        path
    };
    anyhow::ensure!(
        !path.starts_with(r"\\") && !path.starts_with(r"\\.\"),
        "UNC and device paths are not allowed"
    );
    anyhow::ensure!(
        path.len() >= 3
            && path.as_bytes()[0].is_ascii_alphabetic()
            && path.as_bytes()[1] == b':'
            && (path.as_bytes()[2] == b'\\' || path.as_bytes()[2] == b'/'),
        "Windows path must be absolute"
    );
    let path = path.replace('/', r"\");
    let mut components = path[3..].split('\\').collect::<Vec<_>>();
    while components.last() == Some(&"") {
        components.pop();
    }
    anyhow::ensure!(
        components
            .iter()
            .all(|component| !component.is_empty() && *component != "." && *component != ".."),
        "Windows path contains an empty, dot, or parent component"
    );
    let drive = path[..2].to_ascii_uppercase();
    let suffix = components.join(r"\");
    if suffix.is_empty() {
        Ok(format!("{drive}\\"))
    } else {
        Ok(format!("{drive}\\{suffix}"))
    }
}

fn validate_rp_windows_path_model(
    local_app_data: &str,
    running_app_path: &str,
) -> Result<RpWindowsPathModel> {
    let local_app_data = normalize_windows_path_model(local_app_data)?;
    let running_app_path = normalize_windows_path_model(running_app_path)?;
    let root = format!(r"{local_app_data}\Programs\Zed-ACP-Patched");
    let expected_exe = format!(r"{root}\Zed.exe");
    anyhow::ensure!(
        running_app_path.eq_ignore_ascii_case(&expected_exe),
        "running RP executable is not the exact side-by-side Zed-ACP-Patched layout"
    );
    Ok(RpWindowsPathModel {
        root,
        running_exe: running_app_path,
    })
}

#[cfg(target_os = "windows")]
fn windows_paths_equal(left: &Path, right: &Path) -> Result<bool> {
    Ok(normalize_windows_path_model(&left.to_string_lossy())?
        .eq_ignore_ascii_case(&normalize_windows_path_model(&right.to_string_lossy())?))
}

#[cfg(target_os = "windows")]
fn ensure_windows_path_is_not_reparse(path: &Path) -> Result<()> {
    use std::os::windows::fs::MetadataExt as _;
    use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect Windows path {}", path.display()))?;
    anyhow::ensure!(
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0,
        "RP installation path contains a reparse point: {}",
        path.display()
    );
    Ok(())
}

#[cfg(target_os = "windows")]
fn ensure_windows_path_components_are_not_reparse(path: &Path) -> Result<()> {
    use std::path::Component;

    let normalized = PathBuf::from(normalize_windows_path_model(&path.to_string_lossy())?);
    let mut current = PathBuf::new();
    for component in normalized.components() {
        current.push(component);
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        ensure_windows_path_is_not_reparse(&current)?;
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn verify_rp_windows_running_install(
    running_app_path: &Path,
    metadata: RpReleaseMetadata,
) -> Result<(PathBuf, PathBuf)> {
    let local_app_data = env::var("LOCALAPPDATA").context("LOCALAPPDATA is not set")?;
    let model =
        validate_rp_windows_path_model(&local_app_data, &running_app_path.to_string_lossy())?;
    let root = PathBuf::from(&model.root);

    ensure_windows_path_components_are_not_reparse(&root)?;

    let canonical_root =
        std::fs::canonicalize(&root).context("failed to canonicalize RP installation root")?;
    anyhow::ensure!(
        windows_paths_equal(&canonical_root, &root)?,
        "RP installation root canonicalized outside the expected location"
    );
    let canonical_running = std::fs::canonicalize(running_app_path)
        .context("failed to canonicalize running RP executable")?;
    anyhow::ensure!(
        windows_paths_equal(&canonical_running, &root.join("Zed.exe"))?,
        "running executable canonicalized outside the RP installation root"
    );
    ensure_windows_path_components_are_not_reparse(&canonical_running)?;

    let marker = root.join(".zed-rp-installer");
    ensure_windows_path_components_are_not_reparse(&marker)?;
    let marker_text =
        std::fs::read_to_string(&marker).context("failed to read RP installer marker")?;
    let mut marker_lines = marker_text.lines();
    anyhow::ensure!(
        marker_lines.next() == Some("identity=Zed-ACP-Patched-RP-Stable"),
        "RP installer marker has the wrong identity prefix"
    );
    let marker_version = marker_lines
        .next()
        .and_then(|line| line.strip_prefix("version="))
        .context("RP installer marker has no calendar version")?;
    anyhow::ensure!(
        RpCalendarVersion::parse(marker_version)?
            >= RpCalendarVersion::parse(metadata.calendar_version)?,
        "RP installer marker calendar version predates the running build"
    );

    const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\{4A5A5F9D-AE49-4238-AD6B-FD55FEF6DA84}_is1";
    let uninstall_key = windows_registry::CURRENT_USER
        .open(UNINSTALL_KEY)
        .context("RP Inno uninstall registry key is missing")?;
    let registry_root = uninstall_key
        .get_string("Inno Setup: App Path")
        .context("RP Inno App Path registry value is missing")?;
    anyhow::ensure!(
        normalize_windows_path_model(&registry_root)?.eq_ignore_ascii_case(&model.root),
        "RP Inno App Path does not exactly match the verified installation root"
    );

    let helper = root.join("tools").join("auto_update_helper.exe");
    let canonical_helper =
        std::fs::canonicalize(&helper).context("failed to canonicalize RP update helper")?;
    anyhow::ensure!(
        windows_paths_equal(&canonical_helper, &helper)?,
        "RP update helper canonicalized outside the installation root"
    );
    ensure_windows_path_components_are_not_reparse(&canonical_helper)?;

    Ok((root, canonical_helper))
}

#[cfg(target_os = "windows")]
fn verify_rp_windows_install(
    downloaded_installer: &Path,
    running_app_path: &Path,
    metadata: RpReleaseMetadata,
) -> Result<(PathBuf, PathBuf, PathBuf)> {
    let (root, canonical_helper) = verify_rp_windows_running_install(running_app_path, metadata)?;
    let canonical_installer = std::fs::canonicalize(downloaded_installer)
        .context("failed to canonicalize downloaded RP installer")?;
    anyhow::ensure!(
        windows_paths_equal(
            canonical_installer
                .parent()
                .context("downloaded RP installer has no parent")?,
            &root.join("updates")
        )?,
        "downloaded RP installer is outside the verified staging directory"
    );
    ensure_windows_path_components_are_not_reparse(&canonical_installer)?;
    Ok((root, canonical_helper, canonical_installer))
}

async fn install_release_windows(
    downloaded_installer: &Path,
    running_app_path: PathBuf,
    rp_installer_asset: Option<RpManifestAsset>,
) -> Result<Option<PathBuf>> {
    if let Some(metadata) = release_channel::rp_release_metadata() {
        #[cfg(target_os = "windows")]
        {
            let (verified_root, helper_path, canonical_installer) =
                verify_rp_windows_install(downloaded_installer, &running_app_path, metadata)?;
            let rp_installer_asset =
                rp_installer_asset.context("verified RP install has no manifest asset")?;
            anyhow::ensure!(
                verify_rp_file(&canonical_installer, &rp_installer_asset).await?,
                "RP installer changed after download verification"
            );
            let mut cmd = new_command(canonical_installer);
            cmd.arg("/verysilent")
                .arg("/update=true")
                .arg("/MERGETASKS=!desktopicon")
                .arg(format!("/DIR={}", verified_root.display()));
            let output = cmd.output().await?;
            anyhow::ensure!(
                output.status.success(),
                "failed to start verified unsigned RP installer: {:?}",
                String::from_utf8_lossy(&output.stderr)
            );
            return Ok(Some(helper_path));
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = (metadata, running_app_path, rp_installer_asset);
            anyhow::bail!("RP Windows installer validation is only available on Windows");
        }
    }

    let mut cmd = new_command(downloaded_installer);
    cmd.arg("/verysilent")
        .arg("/update=true")
        .arg("/MERGETASKS=!desktopicon");
    let output = cmd.output().await?;
    anyhow::ensure!(
        output.status.success(),
        "failed to start installer: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    // We return the path to the update helper program, because it will
    // perform the final steps of the update process, copying the new binary,
    // deleting the old one, and launching the new binary.
    let helper_path = std::env::current_exe()?
        .parent()
        .context("No parent dir for Zed.exe")?
        .join("tools")
        .join("auto_update_helper.exe");
    Ok(Some(helper_path))
}

pub async fn finalize_auto_update_on_quit() {
    let Some(installer_path) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.join("updates")))
    else {
        return;
    };

    // The installer will create a flag file after it finishes updating
    let flag_file = installer_path.join("versions.txt");
    if flag_file.exists()
        && let Some(helper) = installer_path
            .parent()
            .map(|p| p.join("tools").join("auto_update_helper.exe"))
    {
        let mut command = util::command::new_command(helper);
        command.arg("--launch");
        command.arg("false");
        if let Ok(mut cmd) = command.spawn() {
            _ = cmd.status().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use client::Client;
    use clock::FakeSystemClock;
    use futures::channel::oneshot;
    use gpui::TestAppContext;
    use http_client::{FakeHttpClient, Response};
    use settings::default_settings;
    use std::{
        rc::Rc,
        sync::{
            Arc,
            atomic::{self, AtomicBool},
        },
    };
    use tempfile::tempdir;

    #[ctor::ctor(unsafe)]
    fn init_logger() {
        zlog::init_test();
    }

    use super::*;

    pub(super) struct InstallOverride(pub Rc<dyn Fn(&Path, &AsyncApp) -> Result<Option<PathBuf>>>);
    impl Global for InstallOverride {}

    const TEST_COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    fn sample_manifest(calendar: &str, upstream: &str) -> serde_json::Value {
        let tag = format!("rp-stable-{calendar}");
        let asset = |key: &str| {
            let name = expected_rp_asset_name(key, calendar).unwrap();
            serde_json::json!({
                "name": name,
                "size": 4,
                "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "url": expected_rp_asset_url(&tag, &name),
            })
        };
        serde_json::json!({
            "schema_version": 1,
            "channel": "rp-stable",
            "calendar_version": calendar,
            "upstream_version": upstream,
            "commit": TEST_COMMIT,
            "tag": tag,
            "trust": {"signed": false, "label": "unsigned"},
            "notes_identity": format!("sha256:{}", "b".repeat(64)),
            "assets": {
                "windows_x86_64_installer": asset("windows_x86_64_installer"),
                "windows_x86_64_portable": asset("windows_x86_64_portable"),
                "windows_x86_64_remote_server": asset("windows_x86_64_remote_server"),
                "linux_x86_64_remote_server": asset("linux_x86_64_remote_server"),
            }
        })
    }

    fn validated_sample(calendar: &str, upstream: &str) -> Result<ValidatedRpManifest> {
        validate_rp_manifest(serde_json::from_value(sample_manifest(calendar, upstream))?)
    }

    fn compile_identity(calendar: &str, upstream: &str) -> RpCompileIdentity {
        RpCompileIdentity {
            calendar: RpCalendarVersion::parse(calendar).unwrap(),
            calendar_version: calendar.to_string(),
            tag: format!("rp-stable-{calendar}"),
            commit: TEST_COMMIT.to_string(),
            upstream_version: upstream.parse().unwrap(),
        }
    }

    #[test]
    fn rp_and_official_update_sources_are_isolated() {
        let metadata = RpReleaseMetadata {
            calendar_version: "20260902.1",
            release_tag: "rp-stable-20260902.1",
            release_notes: "",
            notes_identity: "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            manifest: "{}",
        };
        assert_eq!(
            update_endpoint_source(Some(metadata)),
            UpdateEndpointSource::Rp
        );
        assert_eq!(
            release_discovery_endpoint(UpdateEndpointSource::Rp),
            "https://api.github.com/repos/JonathonRP/zed/releases?per_page=100"
        );
        assert_eq!(update_endpoint_source(None), UpdateEndpointSource::Official);
        assert!(
            release_discovery_endpoint(UpdateEndpointSource::Official).starts_with("/releases")
        );
        assert!(!release_discovery_endpoint(UpdateEndpointSource::Rp).contains("zed.dev"));
        assert!(!release_discovery_endpoint(UpdateEndpointSource::Official).contains("JonathonRP"));
    }

    #[test]
    fn rp_calendar_and_candidate_comparison_are_monotonic() {
        assert!(RpCalendarVersion::parse("20240229.1").is_ok());
        for invalid in [
            "20230229.1",
            "20261301.1",
            "20260931.1",
            "20260902.0",
            "20260902.01",
            "2026092.1",
        ] {
            assert!(RpCalendarVersion::parse(invalid).is_err(), "{invalid}");
        }

        let installed = compile_identity("20260902.1", "1.2.3");
        let newer = validated_sample("20260903.1", "1.2.3").unwrap().identity;
        assert!(newer_rp_candidate(&installed, None, &newer).unwrap());
        let higher_upstream = validated_sample("20260903.1", "1.3.0").unwrap().identity;
        assert!(newer_rp_candidate(&installed, None, &higher_upstream).unwrap());
        let upstream_downgrade = validated_sample("20260903.1", "1.2.2").unwrap().identity;
        assert!(newer_rp_candidate(&installed, None, &upstream_downgrade).is_err());

        let same = validated_sample("20260902.1", "1.2.3").unwrap().identity;
        assert!(!newer_rp_candidate(&installed, None, &same).unwrap());
        let downgrade = validated_sample("20260901.9", "1.3.0").unwrap().identity;
        assert!(!newer_rp_candidate(&installed, None, &downgrade).unwrap());
        let staged = newer;
        let rerun = validated_sample("20260903.1", "1.2.3").unwrap().identity;
        assert!(!newer_rp_candidate(&installed, Some(&staged), &rerun).unwrap());
    }

    #[test]
    fn rp_manifest_is_strict_and_repository_bound() {
        assert!(validated_sample("20260902.1", "1.2.3").is_ok());
        for (pointer, replacement) in [
            ("/schema_version", serde_json::json!(2)),
            ("/channel", serde_json::json!("stable")),
            ("/tag", serde_json::json!("rp-stable-20260903.1")),
            ("/upstream_version", serde_json::json!("not-semver")),
            ("/commit", serde_json::json!("ABC")),
            ("/trust/signed", serde_json::json!(true)),
            ("/trust/label", serde_json::json!("signed")),
            ("/notes_identity", serde_json::json!("sha256:ABC")),
            (
                "/assets/windows_x86_64_installer/url",
                serde_json::json!("https://github.com/zed-industries/zed/releases/download/x/y"),
            ),
            (
                "/assets/windows_x86_64_installer/name",
                serde_json::json!("Zed.exe"),
            ),
            (
                "/assets/windows_x86_64_installer/size",
                serde_json::json!(0),
            ),
            (
                "/assets/windows_x86_64_installer/sha256",
                serde_json::json!("A".repeat(64)),
            ),
        ] {
            let mut manifest = sample_manifest("20260902.1", "1.2.3");
            *manifest.pointer_mut(pointer).unwrap() = replacement;
            let parsed: Result<RpUpdateManifest, _> = serde_json::from_value(manifest);
            assert!(
                parsed.is_err() || validate_rp_manifest(parsed.unwrap()).is_err(),
                "accepted malformed field {pointer}"
            );
        }

        let mut duplicate_name = sample_manifest("20260902.1", "1.2.3");
        let installer_name = duplicate_name["assets"]["windows_x86_64_installer"]["name"].clone();
        duplicate_name["assets"]["windows_x86_64_portable"]["name"] = installer_name;
        let parsed: RpUpdateManifest = serde_json::from_value(duplicate_name).unwrap();
        assert!(validate_rp_manifest(parsed).is_err());

        let mut missing = sample_manifest("20260902.1", "1.2.3");
        missing["assets"]
            .as_object_mut()
            .unwrap()
            .remove("linux_x86_64_remote_server");
        assert!(serde_json::from_value::<RpUpdateManifest>(missing).is_err());
        let mut missing_notes = sample_manifest("20260902.1", "1.2.3");
        missing_notes
            .as_object_mut()
            .unwrap()
            .remove("notes_identity");
        assert!(serde_json::from_value::<RpUpdateManifest>(missing_notes).is_err());

        let mut unknown = sample_manifest("20260902.1", "1.2.3");
        unknown["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<RpUpdateManifest>(unknown).is_err());

        let duplicate_field = serde_json::to_string(&sample_manifest("20260902.1", "1.2.3"))
            .unwrap()
            .replacen(
                r#""schema_version":1"#,
                r#""schema_version":1,"schema_version":1"#,
                1,
            );
        assert!(serde_json::from_str::<RpUpdateManifest>(&duplicate_field).is_err());
    }

    #[test]
    fn rp_release_discovery_selects_newest_calendar_and_requires_one_manifest() {
        let release = |tag: &str, manifest_count: usize| GithubRelease {
            tag_name: tag.to_string(),
            draft: false,
            prerelease: false,
            assets: (0..manifest_count)
                .map(|_| GithubReleaseAsset {
                    name: RP_MANIFEST_NAME.to_string(),
                    browser_download_url: expected_rp_asset_url(tag, RP_MANIFEST_NAME),
                })
                .collect(),
        };
        let releases = vec![
            release("rp-stable-20260901.9", 1),
            release("rp-stable-not-a-calendar", 1),
            release("rp-stable-20260903.1", 1),
            release("rp-stable-20260902.4", 1),
        ];
        assert_eq!(
            select_newest_rp_release(&releases).unwrap().0.tag_name,
            "rp-stable-20260903.1"
        );
        assert!(select_newest_rp_release(&[release("rp-stable-20260903.1", 0)]).is_err());
        assert!(select_newest_rp_release(&[release("rp-stable-20260903.1", 2)]).is_err());
    }

    #[test]
    fn rp_redirect_allowlist_is_https_and_host_exact() {
        for allowed in [
            "https://objects.githubusercontent.com/object",
            "https://release-assets.githubusercontent.com/object",
            "https://github-releases.githubusercontent.com/object",
        ] {
            assert!(is_allowed_rp_redirect(
                RpRequestKind::ReleaseAsset,
                &http_client::Url::parse(allowed).unwrap()
            ));
        }
        for rejected in [
            "http://release-assets.githubusercontent.com/object",
            "https://release-assets.githubusercontent.com.evil.example/object",
            "https://github.com/JonathonRP/zed/releases/download/x/y",
            "https://user@objects.githubusercontent.com/object",
            "https://objects.githubusercontent.com:444/object",
        ] {
            assert!(!is_allowed_rp_redirect(
                RpRequestKind::ReleaseAsset,
                &http_client::Url::parse(rejected).unwrap()
            ));
        }
        assert!(is_allowed_rp_redirect(
            RpRequestKind::Api,
            &http_client::Url::parse("https://api.github.com/next").unwrap()
        ));
        assert!(!is_allowed_rp_redirect(
            RpRequestKind::Api,
            &http_client::Url::parse("https://github.com/next").unwrap()
        ));
    }

    #[test]
    fn rp_windows_path_model_rejects_overlapping_and_ambiguous_paths() {
        let local = r"C:\Users\RP\AppData\Local";
        let expected = r"C:\Users\RP\AppData\Local\Programs\Zed-ACP-Patched\Zed.exe";
        let model = validate_rp_windows_path_model(local, expected).unwrap();
        assert_eq!(
            model.root,
            r"C:\Users\RP\AppData\Local\Programs\Zed-ACP-Patched"
        );
        assert!(validate_rp_windows_path_model(local, &expected.to_ascii_lowercase()).is_ok());
        assert!(
            validate_rp_windows_path_model(&format!(r"\\?\{local}"), &format!(r"\\?\{expected}"))
                .is_ok()
        );
        for rejected in [
            r"C:\Users\RP\AppData\Local\Programs\Zed\Zed.exe",
            r"C:\Users\RP\AppData\Local\Programs\Zed-ACP-Patched-Evil\Zed.exe",
            r"C:\Users\RP\AppData\Local\Programs\Zed-ACP-Patched\sub\..\Zed.exe",
            r"..\Programs\Zed-ACP-Patched\Zed.exe",
            r"\\server\share\Zed.exe",
            r"\\.\C:\Users\RP\AppData\Local\Programs\Zed-ACP-Patched\Zed.exe",
            r"\\?\UNC\server\share\Zed.exe",
        ] {
            assert!(
                validate_rp_windows_path_model(local, rejected).is_err(),
                "{rejected}"
            );
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn rp_windows_reparse_walk_accepts_canonical_verbatim_paths() {
        let current_exe = std::env::current_exe().unwrap();
        let canonical_exe = std::fs::canonicalize(current_exe).unwrap();
        ensure_windows_path_components_are_not_reparse(&canonical_exe).unwrap();
    }

    #[test]
    fn matching_rp_remote_selection_is_exact() {
        let installed = compile_identity("20260902.1", "1.2.3");
        let manifest = validated_sample("20260902.1", "1.2.3").unwrap();
        assert!(validate_matching_remote_manifest(&installed, &manifest).is_ok());
        assert_eq!(
            rp_remote_asset(&manifest, "linux", "x86_64").unwrap().name,
            "zed-rp-stable-20260902.1-remote-server-linux-x86_64.gz"
        );
        assert!(rp_remote_asset(&manifest, "windows", "x86_64").is_err());
        let mut wrong_commit = manifest;
        wrong_commit.identity.commit = "1123456789abcdef0123456789abcdef01234567".into();
        assert!(validate_matching_remote_manifest(&installed, &wrong_commit).is_err());
    }

    #[gpui::test]
    async fn rp_download_rejects_size_and_digest_and_cleans_up(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();
        let client = FakeHttpClient::create(|request| async move {
            assert_eq!(
                request
                    .headers()
                    .get(http_client::http::header::ACCEPT_ENCODING)
                    .unwrap(),
                "identity"
            );
            assert!(!request.headers().contains_key("authorization"));
            assert!(!request.headers().contains_key("cookie"));
            Ok(Response::builder()
                .status(200)
                .body(Vec::from("data").into())
                .unwrap())
        });
        let temp = tempdir().unwrap();
        let target = temp.path().join("asset");
        let mut asset = validated_sample("20260902.1", "1.2.3")
            .unwrap()
            .manifest
            .assets
            .windows_x86_64_installer;
        asset.size = 3;
        assert!(
            download_rp_asset(&target, &asset, client.clone(), |_| {})
                .await
                .is_err()
        );
        assert!(!target.exists());

        asset.size = 4;
        asset.sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into();
        assert!(
            download_rp_asset(&target, &asset, client, |_| {})
                .await
                .is_err()
        );
        assert!(!target.exists());
    }

    #[gpui::test]
    async fn rp_discovery_uses_only_pinned_github_endpoints(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();
        let manifest = serde_json::to_vec(&sample_manifest("20260902.1", "1.2.3")).unwrap();
        let calls = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let client = FakeHttpClient::create({
            let calls = calls.clone();
            move |request| {
                let manifest = manifest.clone();
                let calls = calls.clone();
                async move {
                    let uri = request.uri().to_string();
                    calls.lock().push(uri.clone());
                    assert_eq!(
                        request
                            .headers()
                            .get(http_client::http::header::ACCEPT_ENCODING)
                            .unwrap(),
                        "identity"
                    );
                    if uri == RP_RELEASES_API {
                        let releases = serde_json::json!([{
                            "tag_name": "rp-stable-20260902.1",
                            "draft": false,
                            "prerelease": false,
                            "assets": [{
                                "name": "rp-update.json",
                                "browser_download_url": expected_rp_asset_url(
                                    "rp-stable-20260902.1",
                                    RP_MANIFEST_NAME
                                )
                            }]
                        }]);
                        Ok(Response::builder()
                            .status(200)
                            .body(serde_json::to_vec(&releases).unwrap().into())
                            .unwrap())
                    } else {
                        Ok(Response::builder()
                            .status(200)
                            .body(manifest.into())
                            .unwrap())
                    }
                }
            }
        });
        let discovered = discover_latest_rp_manifest(&client).await.unwrap();
        assert_eq!(discovered.identity.calendar_version, "20260902.1");
        let calls = calls.lock();
        assert_eq!(calls.len(), 2);
        assert!(calls.iter().all(|url| {
            url.starts_with("https://api.github.com/")
                || url.starts_with("https://github.com/JonathonRP/zed/releases/download/")
        }));
    }

    #[gpui::test]
    fn test_auto_update_defaults_to_true(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let mut store = SettingsStore::new(cx, &settings::default_settings());
            store
                .set_default_settings(&default_settings(), cx)
                .expect("Unable to set default settings");
            store
                .set_user_settings("{}", cx)
                .expect("Unable to set user settings");
            cx.set_global(store);
            assert!(AutoUpdateSetting::get_global(cx).0);
        });
    }

    #[gpui::test]
    async fn test_auto_update_downloads(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();
        zlog::init_test();
        let release_available = Arc::new(AtomicBool::new(false));

        let (dmg_tx, dmg_rx) = oneshot::channel::<String>();

        cx.update(|cx| {
            settings::init(cx);

            let current_version = semver::Version::new(0, 100, 0);
            release_channel::init_test(current_version, ReleaseChannel::Stable, cx);

            let clock = Arc::new(FakeSystemClock::new());
            let release_available = Arc::clone(&release_available);
            let dmg_rx = Arc::new(parking_lot::Mutex::new(Some(dmg_rx)));
            let fake_client_http = FakeHttpClient::create(move |req| {
                let release_available = release_available.load(atomic::Ordering::Relaxed);
                let dmg_rx = dmg_rx.clone();
                async move {
                if req.uri().path() == "/releases/stable/latest/asset" {
                    if release_available {
                        return Ok(Response::builder().status(200).body(
                            r#"{"version":"0.100.1","url":"https://test.example/new-download"}"#.into()
                        ).unwrap());
                    } else {
                        return Ok(Response::builder().status(200).body(
                            r#"{"version":"0.100.0","url":"https://test.example/old-download"}"#.into()
                        ).unwrap());
                    }
                } else if req.uri().path() == "/new-download" {
                    return Ok(Response::builder().status(200).body({
                        let dmg_rx = dmg_rx.lock().take().unwrap();
                        dmg_rx.await.unwrap().into()
                    }).unwrap());
                }
                Ok(Response::builder().status(404).body("".into()).unwrap())
                }
            });
            let client = Client::new(clock, fake_client_http, cx);
            crate::init(client, cx);
        });

        let auto_updater = cx.update(|cx| AutoUpdater::get(cx).expect("auto updater should exist"));

        cx.background_executor.run_until_parked();

        auto_updater.read_with(cx, |updater, _| {
            assert_eq!(updater.status(), AutoUpdateStatus::Idle);
            assert_eq!(updater.current_version(), semver::Version::new(0, 100, 0));
        });

        release_available.store(true, atomic::Ordering::SeqCst);
        cx.background_executor.advance_clock(POLL_INTERVAL);
        cx.background_executor.run_until_parked();

        loop {
            cx.background_executor.timer(Duration::from_millis(0)).await;
            cx.run_until_parked();
            let status = auto_updater.read_with(cx, |updater, _| updater.status());
            if !matches!(status, AutoUpdateStatus::Idle) {
                break;
            }
        }
        let status = auto_updater.read_with(cx, |updater, _| updater.status());
        assert_eq!(
            status,
            AutoUpdateStatus::Downloading {
                version: semver::Version::new(0, 100, 1),
                progress: None,
            }
        );

        dmg_tx.send("<fake-zed-update>".to_owned()).unwrap();

        let tmp_dir = Arc::new(tempdir().unwrap());

        cx.update(|cx| {
            let tmp_dir = tmp_dir.clone();
            cx.set_global(InstallOverride(Rc::new(move |target_path, _cx| {
                let tmp_dir = tmp_dir.clone();
                let dest_path = tmp_dir.path().join("zed");
                std::fs::copy(&target_path, &dest_path)?;
                Ok(Some(dest_path))
            })));
        });

        loop {
            cx.background_executor.timer(Duration::from_millis(0)).await;
            cx.run_until_parked();
            let status = auto_updater.read_with(cx, |updater, _| updater.status());
            if !matches!(status, AutoUpdateStatus::Downloading { .. }) {
                break;
            }
        }
        let status = auto_updater.read_with(cx, |updater, _| updater.status());
        assert_eq!(
            status,
            AutoUpdateStatus::Updated {
                version: semver::Version::new(0, 100, 1)
            }
        );
        let will_restart = cx.expect_restart();
        cx.update(|cx| cx.restart());
        let (path, arguments) = will_restart.await.unwrap();
        assert!(arguments.is_empty());
        let path = path.unwrap();
        assert_eq!(path, tmp_dir.path().join("zed"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "<fake-zed-update>");
    }

    #[gpui::test]
    async fn test_download_release_reports_progress(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();

        let body = vec![0u8; 20_000];
        let content_length = body.len();

        let client = FakeHttpClient::create(move |_req| {
            let body = body.clone();
            async move {
                Ok(Response::builder()
                    .status(200)
                    .header(
                        http_client::http::header::CONTENT_LENGTH,
                        body.len().to_string(),
                    )
                    .body(body.into())
                    .unwrap())
            }
        });

        let temp_dir = tempdir().unwrap();
        let target_path = temp_dir.path().join("zed-download");
        let release = ReleaseAsset {
            version: "1.0.0".to_string(),
            url: "https://test.example/download".to_string(),
        };

        let reported = Rc::new(std::cell::RefCell::new(Vec::<f32>::new()));
        download_release(&target_path, release, client, {
            let reported = reported.clone();
            move |fraction| {
                if let Some(fraction) = fraction {
                    reported.borrow_mut().push(fraction);
                }
            }
        })
        .await
        .unwrap();

        let reported = reported.borrow();
        assert!(
            reported.len() >= 2,
            "expected progress to be reported across multiple reads, got {reported:?}"
        );
        assert_eq!(
            reported.last().copied(),
            Some(1.0),
            "download should finish at 100%"
        );
        for fraction in reported.iter() {
            assert!(
                (0.0..=1.0).contains(fraction),
                "progress {fraction} out of range"
            );
        }
        for pair in reported.windows(2) {
            assert!(
                pair[0] <= pair[1],
                "progress must not decrease: {reported:?}"
            );
        }

        let downloaded_len = std::fs::metadata(&target_path).unwrap().len();
        assert_eq!(downloaded_len, content_length as u64);
    }

    #[gpui::test]
    async fn test_download_release_without_content_length_reports_no_progress(
        cx: &mut TestAppContext,
    ) {
        cx.background_executor.allow_parking();

        let body = vec![0u8; 20_000];
        let content_length = body.len();

        let client = FakeHttpClient::create(move |_req| {
            let body = body.clone();
            async move { Ok(Response::builder().status(200).body(body.into()).unwrap()) }
        });

        let temp_dir = tempdir().unwrap();
        let target_path = temp_dir.path().join("zed-download");
        let release = ReleaseAsset {
            version: "1.0.0".to_string(),
            url: "https://test.example/download".to_string(),
        };

        let reported = Rc::new(std::cell::RefCell::new(Vec::<Option<f32>>::new()));
        download_release(&target_path, release, client, {
            let reported = reported.clone();
            move |fraction| {
                reported.borrow_mut().push(fraction);
            }
        })
        .await
        .unwrap();

        assert!(
            reported.borrow().is_empty(),
            "progress should not be reported when the total size is unknown, got {:?}",
            reported.borrow()
        );

        let downloaded_len = std::fs::metadata(&target_path).unwrap().len();
        assert_eq!(downloaded_len, content_length as u64);
    }

    #[test]
    fn test_stable_does_not_update_when_fetched_version_is_not_higher() {
        let release_channel = ReleaseChannel::Stable;
        let app_commit_sha = Ok(Some("a".to_string()));
        let installed_version = semver::Version::new(1, 0, 0);
        let status = AutoUpdateStatus::Idle;
        let fetched_version = semver::Version::new(1, 0, 0);

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_version.to_string(),
            status,
        );

        assert_eq!(newer_version.unwrap(), None);
    }

    #[test]
    fn test_stable_does_update_when_fetched_version_is_higher() {
        let release_channel = ReleaseChannel::Stable;
        let app_commit_sha = Ok(Some("a".to_string()));
        let installed_version = semver::Version::new(1, 0, 0);
        let status = AutoUpdateStatus::Idle;
        let fetched_version = semver::Version::new(1, 0, 1);

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_version.to_string(),
            status,
        );

        assert_eq!(newer_version.unwrap(), Some(fetched_version));
    }

    #[test]
    fn test_stable_does_not_update_when_fetched_version_is_not_higher_than_cached() {
        let release_channel = ReleaseChannel::Stable;
        let app_commit_sha = Ok(Some("a".to_string()));
        let installed_version = semver::Version::new(1, 0, 0);
        let status = AutoUpdateStatus::Updated {
            version: semver::Version::new(1, 0, 1),
        };
        let fetched_version = semver::Version::new(1, 0, 1);

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_version.to_string(),
            status,
        );

        assert_eq!(newer_version.unwrap(), None);
    }

    #[test]
    fn test_stable_does_update_when_fetched_version_is_higher_than_cached() {
        let release_channel = ReleaseChannel::Stable;
        let app_commit_sha = Ok(Some("a".to_string()));
        let installed_version = semver::Version::new(1, 0, 0);
        let status = AutoUpdateStatus::Updated {
            version: semver::Version::new(1, 0, 1),
        };
        let fetched_version = semver::Version::new(1, 0, 2);

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_version.to_string(),
            status,
        );

        assert_eq!(newer_version.unwrap(), Some(fetched_version));
    }

    #[test]
    fn test_nightly_does_not_update_when_fetched_sha_is_same() {
        let release_channel = ReleaseChannel::Nightly;
        let app_commit_sha = Ok(Some("a".to_string()));
        let mut installed_version = semver::Version::new(1, 0, 0);
        installed_version.build = semver::BuildMetadata::new("a").unwrap();
        let status = AutoUpdateStatus::Idle;
        let fetched_version = "1.0.0+a".to_string();

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_version,
            status,
        );

        assert_eq!(newer_version.unwrap(), None);
    }

    #[test]
    fn test_nightly_does_update_when_fetched_sha_is_not_same() {
        let release_channel = ReleaseChannel::Nightly;
        let app_commit_sha = Ok(Some("a".to_string()));
        let installed_version = semver::Version::new(1, 0, 0);
        let status = AutoUpdateStatus::Idle;
        let fetched_version = "1.0.0+b".to_string();

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_version.clone(),
            status,
        );

        assert_eq!(
            newer_version.unwrap(),
            Some(fetched_version.parse().unwrap())
        );
    }

    #[test]
    fn test_nightly_does_not_update_when_fetched_version_is_same_as_cached() {
        let release_channel = ReleaseChannel::Nightly;
        let app_commit_sha = Ok(Some("a".to_string()));
        let mut installed_version = semver::Version::new(1, 0, 0);
        installed_version.build = semver::BuildMetadata::new("a").unwrap();
        let status = AutoUpdateStatus::Updated {
            version: "1.0.0+b".parse().unwrap(),
        };
        let fetched_version = "1.0.0+b".to_string();

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_version,
            status,
        );

        assert_eq!(newer_version.unwrap(), None);
    }

    #[test]
    fn test_nightly_does_update_when_fetched_sha_is_not_same_as_cached() {
        let release_channel = ReleaseChannel::Nightly;
        let app_commit_sha = Ok(Some("a".to_string()));
        let mut installed_version = semver::Version::new(1, 0, 0);
        installed_version.build = semver::BuildMetadata::new("a").unwrap();
        let status = AutoUpdateStatus::Updated {
            version: "1.0.0+b".parse().unwrap(),
        };
        let fetched_version = "1.0.0+c".to_string();

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_version.clone(),
            status,
        );

        assert_eq!(
            newer_version.unwrap(),
            Some(fetched_version.parse().unwrap())
        );
    }

    #[test]
    fn test_nightly_does_not_redownload_after_updating_to_fetched_version() {
        let release_channel = ReleaseChannel::Nightly;
        let installed_version = semver::Version::new(1, 0, 0);
        let fetched_version = "1.0.0+nightly.b".to_string();

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            Ok(Some("a".to_string())),
            installed_version.clone(),
            fetched_version.clone(),
            AutoUpdateStatus::Idle,
        )
        .unwrap()
        .expect("a newer nightly version should be available");

        let next_check = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            Ok(Some("a".to_string())),
            installed_version,
            fetched_version,
            AutoUpdateStatus::Updated {
                version: newer_version,
            },
        );

        assert_eq!(next_check.unwrap(), None);
    }

    #[test]
    fn test_nightly_does_update_when_installed_versions_sha_cannot_be_retrieved() {
        let release_channel = ReleaseChannel::Nightly;
        let app_commit_sha = Ok(None);
        let installed_version = semver::Version::new(1, 0, 0);
        let status = AutoUpdateStatus::Idle;
        let fetched_version = "1.0.0+a".to_string();

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_version.clone(),
            status,
        );

        assert_eq!(
            newer_version.unwrap(),
            Some(fetched_version.parse().unwrap())
        );
    }

    #[test]
    fn test_nightly_does_not_update_when_cached_update_is_same_as_fetched_and_installed_versions_sha_cannot_be_retrieved()
     {
        let release_channel = ReleaseChannel::Nightly;
        let app_commit_sha = Ok(None);
        let installed_version = semver::Version::new(1, 0, 0);
        let status = AutoUpdateStatus::Updated {
            version: "1.0.0+b".parse().unwrap(),
        };
        let fetched_version = "1.0.0+b".to_string();

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_version,
            status,
        );

        assert_eq!(newer_version.unwrap(), None);
    }

    #[test]
    fn test_nightly_does_update_when_cached_update_is_not_same_as_fetched_and_installed_versions_sha_cannot_be_retrieved()
     {
        let release_channel = ReleaseChannel::Nightly;
        let app_commit_sha = Ok(None);
        let installed_version = semver::Version::new(1, 0, 0);
        let status = AutoUpdateStatus::Updated {
            version: "1.0.0+b".parse().unwrap(),
        };
        let fetched_version = "1.0.0+c".to_string();

        let newer_version = AutoUpdater::check_if_fetched_version_is_newer(
            release_channel,
            app_commit_sha,
            installed_version,
            fetched_version.clone(),
            status,
        );

        assert_eq!(
            newer_version.unwrap(),
            Some(fetched_version.parse().unwrap())
        );
    }
}
