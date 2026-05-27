//! Launch configuration for the Camoufox browser process.
//!
//! Builds the argument list and environment according to the Juggler protocol
//! specification (PROTOCOL.md section 2 & 12).

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
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
    /// A user-supplied arg collides with internally managed flags.
    ForbiddenArg(String),
    /// A user-supplied arg is empty.
    EmptyArg,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingExecutable => write!(f, "executable path must not be empty"),
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
            if lower.starts_with("-juggler") {
                return Err(ConfigError::ForbiddenArg(arg.clone()));
            }
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
    fn validate_ok_for_default() {
        let cfg = LaunchConfig::default();
        cfg.validate().unwrap();
    }

    #[test]
    fn validate_ok_with_harmless_args() {
        let cfg = LaunchConfig {
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
