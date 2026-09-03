//! Provides constructs for the Zed app version and release channel.

#![deny(missing_docs)]

use std::{env, str::FromStr, sync::LazyLock};

use gpui::{App, Global};
use semver::Version;

const ZED_DOCS_URL: &str = "https://zed.dev/docs";

/// stable | dev | nightly | preview
pub static RELEASE_CHANNEL_NAME: LazyLock<String> = LazyLock::new(|| {
    if cfg!(debug_assertions) {
        env::var("ZED_RELEASE_CHANNEL").unwrap_or_else(|_| compile_time_release_channel_name())
    } else {
        compile_time_release_channel_name()
    }
});

/// When a crate in zed is used as a dependency that uses the `crane` nix
/// library, it vendors each crate separately and builds it in isolation, which
/// makes the `include_str!` fail.
///
/// The build script checks for `$ZED_RELEASE_CHANNEL` and emits the `cfg`
#[cfg(__do_not_set_zed_release_channel)]
fn compile_time_release_channel_name() -> String {
    env!("ZED_RELEASE_CHANNEL").trim().to_string()
}

#[cfg(not(__do_not_set_zed_release_channel))]
fn compile_time_release_channel_name() -> String {
    include_str!("../../zed/RELEASE_CHANNEL").trim().to_string()
}

#[doc(hidden)]
pub static RELEASE_CHANNEL: LazyLock<ReleaseChannel> =
    LazyLock::new(|| match ReleaseChannel::from_str(&RELEASE_CHANNEL_NAME) {
        Ok(channel) => channel,
        _ => panic!("invalid release channel {}", *RELEASE_CHANNEL_NAME),
    });

/// The app identifier for the current release channel, Windows only.
#[cfg(target_os = "windows")]
pub fn app_identifier() -> &'static str {
    app_identifier_for(*RELEASE_CHANNEL, rp_release_metadata().is_some())
}

#[cfg(any(target_os = "windows", test))]
const RP_APP_IDENTIFIER: &str = "Zed-ACP-Patched-RP-Stable";

#[cfg(any(target_os = "windows", test))]
fn app_identifier_for(release_channel: ReleaseChannel, has_rp_metadata: bool) -> &'static str {
    if has_rp_metadata {
        return RP_APP_IDENTIFIER;
    }

    match release_channel {
        ReleaseChannel::Dev => "Zed-Editor-Dev",
        ReleaseChannel::Nightly => "Zed-Editor-Nightly",
        ReleaseChannel::Preview => "Zed-Editor-Preview",
        ReleaseChannel::Stable => "Zed-Editor-Stable",
    }
}

/// The Git commit SHA that Zed was built at.
#[derive(Clone, Eq, Debug, PartialEq)]
pub struct AppCommitSha(String);

struct GlobalAppCommitSha(AppCommitSha);

impl Global for GlobalAppCommitSha {}

impl AppCommitSha {
    /// Creates a new [`AppCommitSha`].
    pub fn new(sha: String) -> Self {
        AppCommitSha(sha)
    }

    /// Returns the global [`AppCommitSha`], if one is set.
    pub fn try_global(cx: &App) -> Option<AppCommitSha> {
        cx.try_global::<GlobalAppCommitSha>()
            .map(|sha| sha.0.clone())
    }

    /// Sets the global [`AppCommitSha`].
    pub fn set_global(sha: AppCommitSha, cx: &mut App) {
        cx.set_global(GlobalAppCommitSha(sha))
    }

    /// Returns the full commit SHA.
    pub fn full(&self) -> String {
        self.0.to_string()
    }

    /// Returns the short (7 character) commit SHA.
    pub fn short(&self) -> String {
        self.0.chars().take(7).collect()
    }
}

struct GlobalAppVersion(Version);

impl Global for GlobalAppVersion {}

/// The version of Zed.
pub struct AppVersion;

impl AppVersion {
    /// Load the app version from env.
    pub fn load(
        pkg_version: &str,
        build_id: Option<&str>,
        commit_sha: Option<AppCommitSha>,
    ) -> Version {
        let mut version: Version = if let Ok(from_env) = env::var("ZED_APP_VERSION") {
            from_env.parse().expect("invalid ZED_APP_VERSION")
        } else {
            pkg_version.parse().expect("invalid version in Cargo.toml")
        };
        let mut pre = String::from(RELEASE_CHANNEL.dev_name());

        if let Some(build_id) = build_id {
            pre.push('.');
            pre.push_str(&build_id);
        }

        if let Some(sha) = commit_sha {
            pre.push('.');
            pre.push_str(&sha.0);
        }
        if let Ok(build) = semver::BuildMetadata::new(&pre) {
            version.build = build;
        }

        version
    }

    /// Returns the global version number.
    pub fn global(cx: &App) -> Version {
        if cx.has_global::<GlobalAppVersion>() {
            cx.global::<GlobalAppVersion>().0.clone()
        } else {
            Version::new(0, 0, 0)
        }
    }
}

/// Additive release identity for RP fork builds.
///
/// These values are absent from ordinary Zed builds, whose version and update
/// behavior remain unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RpReleaseMetadata {
    /// Independent UTC calendar version in `YYYYMMDD.patch` form.
    pub calendar_version: &'static str,
    /// Git tag that identifies this release.
    pub release_tag: &'static str,
    /// Curated Markdown embedded in the packaged application.
    pub release_notes: &'static str,
    /// SHA-256 identity of `release_notes`.
    pub notes_identity: &'static str,
    /// Build-time release manifest as JSON.
    pub manifest: &'static str,
}

/// Returns RP fork metadata when every compile-time value is present.
pub fn rp_release_metadata() -> Option<RpReleaseMetadata> {
    match (
        option_env!("ZED_RP_RELEASE_VERSION"),
        option_env!("ZED_RP_RELEASE_TAG"),
        option_env!("ZED_RP_RELEASE_NOTES"),
        option_env!("ZED_RP_RELEASE_NOTES_IDENTITY"),
        option_env!("ZED_RP_RELEASE_MANIFEST"),
    ) {
        (
            Some(calendar_version),
            Some(release_tag),
            Some(release_notes),
            Some(notes_identity),
            Some(manifest),
        ) => Some(RpReleaseMetadata {
            calendar_version,
            release_tag,
            release_notes,
            notes_identity,
            manifest,
        }),
        _ => None,
    }
}

/// Formats the public release identity without prerelease or build metadata.
pub fn release_display_identity(
    rp_release: Option<RpReleaseMetadata>,
    release_channel: ReleaseChannel,
    version: &Version,
) -> String {
    let mut version = version.clone();
    version.pre = semver::Prerelease::EMPTY;
    version.build = semver::BuildMetadata::EMPTY;
    rp_release
        .map(|release| {
            format!(
                "Unsigned RP Stable {} (Zed {})",
                release.calendar_version, version
            )
        })
        .unwrap_or_else(|| format!("{} {}", release_channel.display_name(), version))
}

/// Formats the embedded RP release-notes tab title with both release identities.
pub fn rp_release_notes_title(release: RpReleaseMetadata, version: &Version) -> String {
    let mut version = version.clone();
    version.pre = semver::Prerelease::EMPTY;
    version.build = semver::BuildMetadata::EMPTY;
    format!(
        "RP Fork Release Notes {} (Zed {})",
        release.calendar_version, version
    )
}

/// A Zed release channel.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub enum ReleaseChannel {
    /// The development release channel.
    ///
    /// Used for local debug builds of Zed.
    #[default]
    Dev,

    /// The Nightly release channel.
    Nightly,

    /// The Preview release channel.
    Preview,

    /// The Stable release channel.
    Stable,
}

struct GlobalReleaseChannel(ReleaseChannel);

impl Global for GlobalReleaseChannel {}

/// Initializes the release channel.
pub fn init(app_version: Version, cx: &mut App) {
    cx.set_global(GlobalAppVersion(app_version));
    cx.set_global(GlobalReleaseChannel(*RELEASE_CHANNEL))
}

/// Initializes the release channel for tests that rely on fake release channel.
pub fn init_test(app_version: Version, release_channel: ReleaseChannel, cx: &mut App) {
    cx.set_global(GlobalAppVersion(app_version));
    cx.set_global(GlobalReleaseChannel(release_channel))
}

/// Returns the Zed docs URL for the current release channel for the given
/// `slug`.
pub fn docs_url(slug: &str, cx: &App) -> String {
    ReleaseChannel::try_global(cx)
        .unwrap_or(*RELEASE_CHANNEL)
        .docs_url(slug)
}

impl ReleaseChannel {
    /// All release channels.
    pub const ALL: [ReleaseChannel; 4] = [
        ReleaseChannel::Dev,
        ReleaseChannel::Nightly,
        ReleaseChannel::Preview,
        ReleaseChannel::Stable,
    ];

    /// Returns the global [`ReleaseChannel`].
    pub fn global(cx: &App) -> Self {
        cx.global::<GlobalReleaseChannel>().0
    }

    /// Returns the global [`ReleaseChannel`], if one is set.
    pub fn try_global(cx: &App) -> Option<Self> {
        cx.try_global::<GlobalReleaseChannel>()
            .map(|channel| channel.0)
    }

    /// Returns whether we want to poll for updates for this [`ReleaseChannel`]
    pub fn poll_for_updates(&self) -> bool {
        !matches!(self, ReleaseChannel::Dev)
    }

    /// Returns the display name for this [`ReleaseChannel`].
    pub fn display_name(&self) -> &'static str {
        match self {
            ReleaseChannel::Dev => "Zed Dev",
            ReleaseChannel::Nightly => "Zed Nightly",
            ReleaseChannel::Preview => "Zed Preview",
            ReleaseChannel::Stable => "Zed",
        }
    }

    /// Returns the programmatic name for this [`ReleaseChannel`].
    pub fn dev_name(&self) -> &'static str {
        match self {
            ReleaseChannel::Dev => "dev",
            ReleaseChannel::Nightly => "nightly",
            ReleaseChannel::Preview => "preview",
            ReleaseChannel::Stable => "stable",
        }
    }

    /// Returns the application ID that's used by Wayland as application ID
    /// and WM_CLASS on X11.
    /// This also has to match the bundle identifier for Zed on macOS.
    pub fn app_id(&self) -> &'static str {
        match self {
            ReleaseChannel::Dev => "dev.zed.Zed-Dev",
            ReleaseChannel::Nightly => "dev.zed.Zed-Nightly",
            ReleaseChannel::Preview => "dev.zed.Zed-Preview",
            ReleaseChannel::Stable => "dev.zed.Zed",
        }
    }

    /// Returns the query parameter for this [`ReleaseChannel`].
    pub fn release_query_param(&self) -> Option<&'static str> {
        match self {
            Self::Dev => None,
            Self::Nightly => Some("nightly=1"),
            Self::Preview => Some("preview=1"),
            Self::Stable => None,
        }
    }

    /// Returns the Zed docs URL for this [`ReleaseChannel`] for the given
    /// `slug`.
    pub fn docs_url(&self, slug: &str) -> String {
        let channel_path_segment = match self {
            Self::Dev | Self::Nightly => Some("nightly"),
            Self::Preview => Some("preview"),
            Self::Stable => None,
        };

        match channel_path_segment {
            Some(channel) if slug.is_empty() => format!("{ZED_DOCS_URL}/{channel}"),
            Some(channel) => format!("{ZED_DOCS_URL}/{channel}/{slug}"),
            None if slug.is_empty() => ZED_DOCS_URL.to_string(),
            None => format!("{ZED_DOCS_URL}/{slug}"),
        }
    }
}

/// Error indicating that release channel string does not match any known release channel names.
#[derive(Copy, Clone, Debug, Hash, PartialEq)]
pub struct InvalidReleaseChannel;

impl FromStr for ReleaseChannel {
    type Err = InvalidReleaseChannel;

    fn from_str(channel: &str) -> Result<Self, Self::Err> {
        Ok(match channel {
            "dev" => ReleaseChannel::Dev,
            "nightly" => ReleaseChannel::Nightly,
            "preview" => ReleaseChannel::Preview,
            "stable" => ReleaseChannel::Stable,
            _ => return Err(InvalidReleaseChannel),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        RP_APP_IDENTIFIER, ReleaseChannel, RpReleaseMetadata, app_identifier_for,
        release_display_identity, rp_release_notes_title,
    };
    use semver::Version;

    const RP_RELEASE: RpReleaseMetadata = RpReleaseMetadata {
        calendar_version: "20260902.1",
        release_tag: "rp-stable-20260902.1",
        release_notes: "# Notes",
        notes_identity: "sha256:notes",
        manifest: "{}",
    };

    #[test]
    fn rp_windows_identity_does_not_overlap_official_channels() {
        for channel in ReleaseChannel::ALL {
            let official_identifier = app_identifier_for(channel, false);
            assert_ne!(RP_APP_IDENTIFIER, official_identifier);
            assert_eq!(
                app_identifier_for(channel, true),
                RP_APP_IDENTIFIER,
                "complete RP metadata must select the fork identity"
            );
        }
    }

    #[test]
    fn release_titles_show_both_rp_and_upstream_identity() {
        let version = Version::parse("1.17.2-preview.3+stable.sha").unwrap();
        assert_eq!(
            release_display_identity(Some(RP_RELEASE), ReleaseChannel::Stable, &version),
            "Unsigned RP Stable 20260902.1 (Zed 1.17.2)"
        );
        assert_eq!(
            rp_release_notes_title(RP_RELEASE, &version),
            "RP Fork Release Notes 20260902.1 (Zed 1.17.2)"
        );
    }

    #[test]
    fn official_release_identity_keeps_upstream_fallback() {
        let version = Version::parse("1.17.2-preview.3+stable.sha").unwrap();
        assert_eq!(
            release_display_identity(None, ReleaseChannel::Preview, &version),
            "Zed Preview 1.17.2"
        );
    }

    #[test]
    fn test_docs_url_for_release_channel() {
        assert_eq!(
            ReleaseChannel::Dev.docs_url("settings"),
            "https://zed.dev/docs/nightly/settings"
        );
        assert_eq!(
            ReleaseChannel::Nightly.docs_url("settings"),
            "https://zed.dev/docs/nightly/settings"
        );
        assert_eq!(
            ReleaseChannel::Preview.docs_url("settings"),
            "https://zed.dev/docs/preview/settings"
        );
        assert_eq!(
            ReleaseChannel::Stable.docs_url("settings"),
            "https://zed.dev/docs/settings"
        );
    }
}
