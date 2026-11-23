//! Launch configuration for the Camoufox browser process.
//!
//! Builds the argument list and environment according to the Juggler protocol
//! specification (PROTOCOL.md section 2 & 12).

use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Default launch timeout (3 minutes), matching Playwright.
const DEFAULT_LAUNCH_TIMEOUT: Duration = Duration::from_secs(180);

/// Default graceful-close timeout (30 seconds), matching Playwright.
const DEFAULT_CLOSE_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum chunk size for `CAMOU_CONFIG_*` environment variable values on Unix.
const CAMOU_CONFIG_CHUNK_SIZE_UNIX: usize = 32767;

/// Maximum chunk size for `CAMOU_CONFIG_*` environment variable values on Windows.
#[allow(dead_code)]
const CAMOU_CONFIG_CHUNK_SIZE_WINDOWS: usize = 2047;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors arising from invalid launch configuration.
#[derive(Debug, Clone)]
pub enum ConfigError {
    /// The executable path is empty.
    MissingExecutable,
    /// No profile directory has been assigned before launch.
    MissingProfile,
    /// A user-supplied arg collides with internally managed flags.
    ForbiddenArg(String),
    /// A user-supplied arg is empty.
    EmptyArg,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingExecutable => write!(f, "executable path must not be empty"),
            Self::MissingProfile => write!(f, "profile_dir must be set before launch"),
            Self::ForbiddenArg(a) => write!(
                f,
                "user arg {a:?} conflicts with managed flags (-profile, -juggler)"
            ),
            Self::EmptyArg => write!(f, "user args must not be empty strings"),
        }
    }
}

impl std::error::Error for ConfigError {}

// ---------------------------------------------------------------------------
// LaunchConfig
// ---------------------------------------------------------------------------

/// Configuration for spawning a Camoufox child process.
///
/// The struct carries every knob needed to build the full argv and environment.
/// Use [`Default::default()`] and then override only the fields you care about.
#[derive(Debug, Clone)]
pub struct LaunchConfig {
    /// Path to the camoufox binary.
    pub executable: PathBuf,

    /// User-data / profile directory.  When `None` the caller is expected to
    /// create a temporary directory and fill this in before launch.
    pub profile_dir: Option<PathBuf>,

    /// Run in headless mode (`-headless`).
    /// When `false` the headed flags `-wait-for-browser -foreground` are used.
    pub headless: bool,

    /// Extra command-line arguments passed after `-juggler-pipe`.
    /// Must not contain `-profile`, `--profile`, or `-juggler` prefixes.
    pub args: Vec<String>,

    /// Extra environment variables to set on the child process.
    /// `CAMOU_CONFIG_*` chunked variables are generated separately via
    /// [`Self::camou_config_env`].
    pub env: HashMap<String, String>,

    /// Persistent browser-context mode.
    /// When `true` the trailing arg is `about:blank`; otherwise `-silent`.
    pub persistent: bool,

    /// Maximum time to wait for the `"Juggler listening to the pipe"` message.
    pub timeout: Duration,

    /// Maximum time to wait for a graceful close after sending `Browser.close`.
    pub close_timeout: Duration,
}

impl Default for LaunchConfig {
    fn default() -> Self {
        Self {
            executable: PathBuf::from("camoufox"),
            profile_dir: None,
            headless: true,
            args: Vec::new(),
            env: HashMap::new(),
            persistent: false,
            timeout: DEFAULT_LAUNCH_TIMEOUT,
            close_timeout: DEFAULT_CLOSE_TIMEOUT,
        }
    }
}

impl LaunchConfig {
    /// Validate the configuration, returning an error on the first problem.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.executable.as_os_str().is_empty() {
            return Err(ConfigError::MissingExecutable);
        }

        for arg in &self.args {
            if arg.is_empty() {
                return Err(ConfigError::EmptyArg);
            }
            let lower = arg.to_ascii_lowercase();
            if lower.starts_with("-profile") || lower.starts_with("--profile") {
                return Err(ConfigError::ForbiddenArg(arg.clone()));
            }
            if lower.trim_start_matches('-').starts_with("juggler") {
                return Err(ConfigError::ForbiddenArg(arg.clone()));
            }
        }

        if self.profile_dir.is_none() {
            return Err(ConfigError::MissingProfile);
        }

        Ok(())
    }

    /// Build the full argument vector for `Command::args(...)`.
    ///
    /// Layout (per PROTOCOL.md section 2 & 12):
    /// ```text
    /// -no-remote
    /// [-headless | -wait-for-browser -foreground]
    /// -profile <dir>
    /// -juggler-pipe
    /// [user args...]
    /// [-silent | about:blank]
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if `profile_dir` is `None`. The caller must ensure a directory
    /// has been assigned (or created as a temp dir) before calling this.
    pub fn build_args(&self) -> Vec<String> {
        let mut out = Vec::with_capacity(8 + self.args.len());

        // Mandatory first flag.
        out.push("-no-remote".into());

        // Display mode.
        if self.headless {
            out.push("-headless".into());
        } else {
            out.push("-wait-for-browser".into());
            out.push("-foreground".into());
        }

        // Profile directory (required).
        let profile = self
            .profile_dir
            .as_ref()
            .expect("profile_dir must be set before calling build_args");
        out.push("-profile".into());
        out.push(profile.to_string_lossy().into_owned());

        // Juggler pipe transport.
        out.push("-juggler-pipe".into());

        // User-supplied extra args.
        out.extend(self.args.iter().cloned());

        // Trailing sentinel.
        if self.persistent {
            out.push("about:blank".into());
        } else {
            out.push("-silent".into());
        }

        out
    }

    /// Produce the `CAMOU_CONFIG_*` environment variable map for a given
    /// JSON config string.
    ///
    /// The config payload is split into numbered chunks (`CAMOU_CONFIG_1`,
    /// `CAMOU_CONFIG_2`, ...) whose size is platform-dependent (32767 on Unix,
    /// 2047 on Windows).
    pub fn camou_config_env(config_json: &str) -> HashMap<String, String> {
        let chunk_size = if cfg!(windows) {
            CAMOU_CONFIG_CHUNK_SIZE_WINDOWS
        } else {
            CAMOU_CONFIG_CHUNK_SIZE_UNIX
        };

        let mut map = HashMap::new();
        let bytes = config_json.as_bytes();
        let total = bytes.len();

        if total == 0 {
            return map;
        }

        let mut offset = 0usize;
        let mut idx = 1usize;
        while offset < total {
            let end = std::cmp::min(offset + chunk_size, total);
            // We chunk by byte offset but the input is UTF-8.  Ensure we don't
            // split a multi-byte character.  Back up until we sit on a char
            // boundary.
            let mut safe_end = end;
            while safe_end > offset && !config_json.is_char_boundary(safe_end) {
                safe_end -= 1;
            }
            let chunk = &config_json[offset..safe_end];
            map.insert(format!("CAMOU_CONFIG_{idx}"), chunk.to_owned());
            offset = safe_end;
            idx += 1;
        }

        map
    }
}

// ---------------------------------------------------------------------------
// Executable discovery
// ---------------------------------------------------------------------------

/// Locate the Camoufox browser executable.
///
/// Resolution order, first match wins:
///
/// 1. `$CAMOUFOX_BIN`, if set and non-empty (returned as-is, not checked).
/// 2. The install marked active in `<cache>/config.json` (`active_version`),
///    as written by the Python `camoufox fetch` / `camoufox set` commands.
/// 3. The newest `<cache>/browsers/<repo>/<version>/camoufox` on disk.
/// 4. `<cache>/camoufox` — the legacy flat layout. Returned even when it does
///    not exist, so a missing browser fails at spawn with that path.
///
/// `<cache>` is `$XDG_CACHE_HOME/camoufox` when `XDG_CACHE_HOME` is an
/// absolute path, else `$HOME/.cache/camoufox` (`/root/.cache/camoufox` when
/// `HOME` is unset). This is the Linux layout; macOS installs from the Python
/// package live elsewhere and need `$CAMOUFOX_BIN`.
pub fn default_executable() -> PathBuf {
    resolve_executable(
        std::env::var_os("CAMOUFOX_BIN"),
        &cache_root(std::env::var_os("XDG_CACHE_HOME"), std::env::var_os("HOME")),
    )
}

fn cache_root(xdg_cache_home: Option<OsString>, home: Option<OsString>) -> PathBuf {
    // The XDG spec says relative values are invalid and must be ignored.
    if let Some(xdg) = xdg_cache_home
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    {
        return xdg.join("camoufox");
    }
    let home = home
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/root"));
    home.join(".cache").join("camoufox")
}

fn resolve_executable(camoufox_bin: Option<OsString>, root: &Path) -> PathBuf {
    if let Some(bin) = camoufox_bin.filter(|b| !b.is_empty()) {
        return PathBuf::from(bin);
    }
    active_install(root)
        .or_else(|| newest_install(root))
        .unwrap_or_else(|| root.join("camoufox"))
}

/// `<root>/<active_version>/camoufox` from `<root>/config.json`, if it exists.
fn active_install(root: &Path) -> Option<PathBuf> {
    let config = std::fs::read(root.join("config.json")).ok()?;
    let config: serde_json::Value = serde_json::from_slice(&config).ok()?;
    let active = config.get("active_version")?.as_str()?;
    let exe = root.join(active).join("camoufox");
    exe.is_file().then_some(exe)
}

/// The highest-versioned `<root>/browsers/<repo>/<version>/camoufox`.
fn newest_install(root: &Path) -> Option<PathBuf> {
    let mut best: Option<(String, PathBuf)> = None;
    for repo in std::fs::read_dir(root.join("browsers")).ok()?.flatten() {
        let Ok(versions) = std::fs::read_dir(repo.path()) else {
            continue;
        };
        for version in versions.flatten() {
            let exe = version.path().join("camoufox");
            if !exe.is_file() {
                continue;
            }
            let name = version.file_name().to_string_lossy().into_owned();
            if best
                .as_ref()
                .is_none_or(|(b, _)| natural_cmp(&name, b).is_gt())
            {
                best = Some((name, exe));
            }
        }
    }
    best.map(|(_, exe)| exe)
}

/// Compare strings with runs of ASCII digits ordered numerically, so
/// `158.0.1-beta.41` sorts after `158.0.1-beta.9`.
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    loop {
        match (a.first(), b.first()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let an = a.iter().take_while(|c| c.is_ascii_digit()).count();
                let bn = b.iter().take_while(|c| c.is_ascii_digit()).count();
                let (ad, bd) = (trim_zeros(&a[..an]), trim_zeros(&b[..bn]));
                let ord = ad.len().cmp(&bd.len()).then_with(|| ad.cmp(bd));
                if ord.is_ne() {
                    return ord;
                }
                (a, b) = (&a[an..], &b[bn..]);
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(y);
                }
                (a, b) = (&a[1..], &b[1..]);
            }
        }
    }
}

fn trim_zeros(digits: &[u8]) -> &[u8] {
    let zeros = digits.iter().take_while(|&&c| c == b'0').count();
    &digits[zeros..]
}

#[cfg(test)]
mod executable_tests {
    use super::*;
    use std::fs;

    /// Create `<root>/<rel>/camoufox` as an empty file and return its path.
    fn install(root: &Path, rel: &str) -> PathBuf {
        let dir = root.join(rel);
        fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("camoufox");
        fs::write(&exe, b"").unwrap();
        exe
    }

    fn set_active(root: &Path, rel: &str) {
        fs::write(
            root.join("config.json"),
            format!(r#"{{"active_version": "{rel}"}}"#),
        )
        .unwrap();
    }

    #[test]
    fn camoufox_bin_env_wins_over_installs() {
        let root = tempfile::tempdir().unwrap();
        install(root.path(), "browsers/official/158.0.1-beta.41-aaaa");
        let got = resolve_executable(Some("/opt/cf/camoufox".into()), root.path());
        assert_eq!(got, PathBuf::from("/opt/cf/camoufox"));
    }

    #[test]
    fn empty_camoufox_bin_is_ignored() {
        let root = tempfile::tempdir().unwrap();
        let legacy = install(root.path(), "");
        assert_eq!(resolve_executable(Some("".into()), root.path()), legacy);
    }

    #[test]
    fn active_version_from_config_json_is_used() {
        let root = tempfile::tempdir().unwrap();
        install(root.path(), "browsers/official/158.0.1-beta.41-aaaa");
        let active = install(root.path(), "browsers/official/150.0.2-beta.25-bbbb");
        set_active(root.path(), "browsers/official/150.0.2-beta.25-bbbb");
        assert_eq!(resolve_executable(None, root.path()), active);
    }

    #[test]
    fn active_version_missing_on_disk_falls_back_to_newest_install() {
        let root = tempfile::tempdir().unwrap();
        let newest = install(root.path(), "browsers/official/158.0.1-beta.41-aaaa");
        set_active(root.path(), "browsers/official/999.0-gone");
        assert_eq!(resolve_executable(None, root.path()), newest);
    }

    #[test]
    fn newest_versioned_install_wins_without_config() {
        let root = tempfile::tempdir().unwrap();
        install(root.path(), "browsers/official/158.0.1-beta.9-cccc");
        install(root.path(), "browsers/official/150.0.2-beta.25-bbbb");
        let newest = install(root.path(), "browsers/jwriter20/158.0.1-beta.41-aaaa");
        assert_eq!(resolve_executable(None, root.path()), newest);
    }

    #[test]
    fn versioned_install_preferred_over_legacy_flat_install() {
        let root = tempfile::tempdir().unwrap();
        install(root.path(), "");
        let versioned = install(root.path(), "browsers/official/158.0.1-beta.41-aaaa");
        assert_eq!(resolve_executable(None, root.path()), versioned);
    }

    #[test]
    fn malformed_config_json_is_ignored() {
        let root = tempfile::tempdir().unwrap();
        let newest = install(root.path(), "browsers/official/158.0.1-beta.41-aaaa");
        fs::write(root.path().join("config.json"), b"{not json").unwrap();
        assert_eq!(resolve_executable(None, root.path()), newest);
    }

    #[test]
    fn nothing_installed_falls_back_to_legacy_path() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_executable(None, root.path()),
            root.path().join("camoufox")
        );
    }

    #[test]
    fn natural_cmp_orders_digit_runs_numerically() {
        use std::cmp::Ordering::*;
        assert_eq!(natural_cmp("158.0.1-beta.41", "158.0.1-beta.9"), Greater);
        assert_eq!(natural_cmp("150.0.2-beta.25", "158.0.1-beta.9"), Less);
        assert_eq!(natural_cmp("158.0", "158.0.1"), Less);
        assert_eq!(natural_cmp("v010", "v10"), Equal);
        assert_eq!(natural_cmp("1.2-a", "1.2-b"), Less);
    }

    #[test]
    fn cache_root_honours_absolute_xdg_cache_home() {
        assert_eq!(
            cache_root(Some("/xdg/cache".into()), Some("/home/u".into())),
            PathBuf::from("/xdg/cache/camoufox")
        );
    }

    #[test]
    fn cache_root_ignores_empty_or_relative_xdg_cache_home() {
        for xdg in ["", "rel/cache"] {
            assert_eq!(
                cache_root(Some(xdg.into()), Some("/home/u".into())),
                PathBuf::from("/home/u/.cache/camoufox"),
                "XDG_CACHE_HOME={xdg:?}"
            );
        }
    }

    #[test]
    fn cache_root_without_home_uses_root() {
        assert_eq!(
            cache_root(None, None),
            PathBuf::from("/root/.cache/camoufox")
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_has_sane_values() {
        let cfg = LaunchConfig::default();
        assert_eq!(cfg.executable, PathBuf::from("camoufox"));
        assert!(cfg.headless);
        assert!(!cfg.persistent);
        assert_eq!(cfg.timeout, Duration::from_secs(180));
        assert_eq!(cfg.close_timeout, Duration::from_secs(30));
    }

    #[test]
    fn build_args_headless_non_persistent() {
        let cfg = LaunchConfig {
            profile_dir: Some(PathBuf::from("/tmp/test-profile")),
            ..Default::default()
        };

        let args = cfg.build_args();
        assert_eq!(
            args,
            vec![
                "-no-remote",
                "-headless",
                "-profile",
                "/tmp/test-profile",
                "-juggler-pipe",
                "-silent",
            ]
        );
    }

    #[test]
    fn build_args_headed_persistent_with_extra_args() {
        let cfg = LaunchConfig {
            headless: false,
            persistent: true,
            profile_dir: Some(PathBuf::from("/data/profile")),
            args: vec!["--width=800".into(), "--height=600".into()],
            ..Default::default()
        };

        let args = cfg.build_args();
        assert_eq!(
            args,
            vec![
                "-no-remote",
                "-wait-for-browser",
                "-foreground",
                "-profile",
                "/data/profile",
                "-juggler-pipe",
                "--width=800",
                "--height=600",
                "about:blank",
            ]
        );
    }

    #[test]
    fn validate_rejects_empty_executable() {
        let cfg = LaunchConfig {
            executable: PathBuf::new(),
            ..Default::default()
        };
        assert!(matches!(
            cfg.validate(),
            Err(ConfigError::MissingExecutable)
        ));
    }

    #[test]
    fn validate_rejects_forbidden_profile_arg() {
        let cfg = LaunchConfig {
            args: vec!["-profile".into(), "/some/dir".into()],
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, ConfigError::ForbiddenArg(_)));
    }

    #[test]
    fn validate_rejects_forbidden_profile_double_dash() {
        let cfg = LaunchConfig {
            args: vec!["--profile=/foo".into()],
            ..Default::default()
        };
        assert!(matches!(cfg.validate(), Err(ConfigError::ForbiddenArg(_))));
    }

    #[test]
    fn validate_rejects_juggler_arg() {
        let cfg = LaunchConfig {
            args: vec!["-juggler-pipe".into()],
            ..Default::default()
        };
        assert!(matches!(cfg.validate(), Err(ConfigError::ForbiddenArg(_))));
    }

    #[test]
    fn validate_rejects_empty_arg() {
        let cfg = LaunchConfig {
            args: vec!["".into()],
            ..Default::default()
        };
        assert!(matches!(cfg.validate(), Err(ConfigError::EmptyArg)));
    }

    #[test]
    fn validate_requires_profile_before_launch() {
        let cfg = LaunchConfig::default();
        assert!(matches!(cfg.validate(), Err(ConfigError::MissingProfile)));
    }

    #[test]
    fn validate_rejects_all_juggler_dash_prefixes() {
        for arg in ["--juggler-pipe", "--juggler=x", "---JuGgLeR-pipe"] {
            let cfg = LaunchConfig {
                profile_dir: Some(PathBuf::from("/tmp/profile")),
                args: vec![arg.into()],
                ..Default::default()
            };
            assert!(matches!(cfg.validate(), Err(ConfigError::ForbiddenArg(_))));
        }
    }

    #[cfg(unix)]
    #[test]
    fn spawn_rejects_invalid_config_without_panicking() {
        for cfg in [
            LaunchConfig::default(),
            LaunchConfig {
                profile_dir: Some(PathBuf::from("/tmp/profile")),
                args: vec!["--juggler-pipe".into()],
                ..Default::default()
            },
        ] {
            let err = match crate::process::unix::spawn(&cfg) {
                Err(err) => err,
                Ok(_) => panic!("invalid config must fail before spawning"),
            };
            assert!(matches!(err, crate::process::ProcessError::Io(ref e)
                if e.get_ref().is_some_and(|source| source.is::<ConfigError>())));
        }
    }

    #[test]
    fn validate_ok_with_harmless_args() {
        let cfg = LaunchConfig {
            profile_dir: Some(PathBuf::from("/tmp/profile")),
            args: vec!["--width=1920".into(), "--some-flag".into()],
            ..Default::default()
        };
        cfg.validate().unwrap();
    }

    #[test]
    fn camou_config_env_empty() {
        let map = LaunchConfig::camou_config_env("");
        assert!(map.is_empty());
    }

    #[test]
    fn camou_config_env_single_chunk() {
        let json = r#"{"key":"value"}"#;
        let map = LaunchConfig::camou_config_env(json);
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("CAMOU_CONFIG_1").unwrap(), json);
    }

    #[test]
    fn camou_config_env_multiple_chunks() {
        // Force chunking with a small-ish payload that exceeds one chunk on
        // a hypothetical platform.  On Unix the chunk size is 32767 so we
        // need a payload larger than that.
        let json = "x".repeat(32767 + 100);
        let map = LaunchConfig::camou_config_env(&json);
        assert_eq!(map.len(), 2);
        let reassembled = format!(
            "{}{}",
            map.get("CAMOU_CONFIG_1").unwrap(),
            map.get("CAMOU_CONFIG_2").unwrap()
        );
        assert_eq!(reassembled, json);
    }

    #[test]
    #[should_panic(expected = "profile_dir must be set")]
    fn build_args_panics_without_profile() {
        let cfg = LaunchConfig::default();
        let _ = cfg.build_args();
    }
}
