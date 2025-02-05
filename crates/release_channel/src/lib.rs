//! Provides constructs for the Zed app version and release channel.

#![deny(missing_docs)]

use std::{env, fmt::Display, str::FromStr, sync::LazyLock};

use gpui::{App, Global, SemanticVersion};

/// stable | dev | nightly | preview
pub static RELEASE_CHANNEL_NAME: LazyLock<String> = LazyLock::new(|| {
    if cfg!(debug_assertions) {
        env::var("ZED_RELEASE_CHANNEL")
            .unwrap_or_else(|_| include_str!("../../zed/RELEASE_CHANNEL").trim().to_string())
    } else {
        include_str!("../../zed/RELEASE_CHANNEL").trim().to_string()
    }
});

#[doc(hidden)]
pub static RELEASE_CHANNEL: LazyLock<ReleaseChannel> =
    LazyLock::new(|| match ReleaseChannel::from_str(&RELEASE_CHANNEL_NAME) {
        Ok(channel) => channel,
        _ => panic!("invalid release channel {}", *RELEASE_CHANNEL_NAME),
    });

/// Git repo information for the build.
#[derive(Clone)]
pub struct AppBuildInfo {
    /// Git commit SHA the app was built at.
    pub commit_sha: Option<String>,
    /// Whether some tracked files had modifications.
    pub had_modified_files: bool,
}

const DEFAULT_APP_BUILD_INFO: AppBuildInfo = AppBuildInfo {
    commit_sha: None,
    had_modified_files: false,
};

struct GlobalAppBuildInfo(AppBuildInfo);

impl Global for GlobalAppBuildInfo {}

impl AppBuildInfo {
    /// Returns the global [`AppBuildInfo`] if set, otherwise returns default.
    pub fn global_or_default(cx: &App) -> &AppBuildInfo {
        cx.try_global::<GlobalAppBuildInfo>()
            .map_or(&DEFAULT_APP_BUILD_INFO, |global| &global.0)
    }

    /// Sets the global [`AppBuildInfo`].
    pub fn set_global(info: AppBuildInfo, cx: &mut App) {
        cx.set_global(GlobalAppBuildInfo(info))
    }
}

impl Display for AppBuildInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.commit_sha {
            Some(commit_sha) => {
                if self.had_modified_files {
                    write!(f, "{} with modified files", commit_sha)
                } else {
                    write!(f, "{}", commit_sha)
                }
            }
            None => write!(f, "No build info"),
        }
    }
}

struct GlobalAppVersion(SemanticVersion);

impl Global for GlobalAppVersion {}

/// The version of Zed.
pub struct AppVersion;

impl AppVersion {
    /// Initializes the global [`AppVersion`].
    pub fn init(pkg_version: &str) -> SemanticVersion {
        if let Ok(from_env) = env::var("ZED_APP_VERSION") {
            from_env.parse().expect("invalid ZED_APP_VERSION")
        } else {
            pkg_version.parse().expect("invalid version in Cargo.toml")
        }
    }

    /// Returns the global version number.
    pub fn global(cx: &App) -> SemanticVersion {
        if cx.has_global::<GlobalAppVersion>() {
            cx.global::<GlobalAppVersion>().0
        } else {
            SemanticVersion::default()
        }
    }
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
pub fn init(app_version: SemanticVersion, cx: &mut App) {
    cx.set_global(GlobalAppVersion(app_version));
    cx.set_global(GlobalReleaseChannel(*RELEASE_CHANNEL))
}

impl ReleaseChannel {
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
