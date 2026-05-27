//! Camoufox-specific version detection and compatibility checks.
//!
//! [`CamoufoxInfo`] is populated from the `Browser.getInfo` response and
//! provides helpers for detecting Camoufox-specific capabilities.

/// Information about the connected browser, including Camoufox detection.
///
/// Created from the `userAgent` and `version` strings returned by
/// `Browser.getInfo`.
///
/// # Camoufox detection
///
/// Camoufox builds are detected by looking for the `"Camoufox"` substring
/// in either the user-agent or version string. This is a heuristic; the
/// exact detection logic may need to be updated for future Camoufox releases.
#[derive(Debug, Clone, Default)]
pub struct CamoufoxInfo {
    /// The full user-agent string from `Browser.getInfo`.
    pub user_agent: String,

    /// The browser version string from `Browser.getInfo` (e.g., `"Firefox/128.0"`).
    pub version: String,

    /// Whether this is a Camoufox build (detected from UA or version string).
    pub is_camoufox: bool,

    /// Parsed major version number, if extractable.
    major_version: Option<u32>,
}

impl CamoufoxInfo {
    /// Create a `CamoufoxInfo` from the `Browser.getInfo` response fields.
    ///
    /// # Detection logic
    ///
    /// - Checks for `"Camoufox"` (case-insensitive) in the user-agent string.
    /// - Checks for `"Camoufox"` (case-insensitive) in the version string.
    /// - Extracts the major version number from `"Firefox/<major>.<minor>"`.
    pub fn from_browser_info(user_agent: &str, version: &str) -> Self {
        let ua_lower = user_agent.to_ascii_lowercase();
        let ver_lower = version.to_ascii_lowercase();

        let is_camoufox = ua_lower.contains("camoufox") || ver_lower.contains("camoufox");

        // Parse major version from "Firefox/128.0" or similar.
        let major_version = version
            .strip_prefix("Firefox/")
            .or_else(|| version.strip_prefix("firefox/"))
            .and_then(|rest| {
                rest.split('.')
                    .next()
                    .and_then(|major| major.parse::<u32>().ok())
            });

        CamoufoxInfo {
            user_agent: user_agent.to_owned(),
            version: version.to_owned(),
            is_camoufox,
            major_version,
        }
    }

    /// Returns the parsed major version number, if available.
    ///
    /// For example, version `"Firefox/128.0"` returns `Some(128)`.
    pub fn major_version(&self) -> Option<u32> {
        self.major_version
    }

    /// Check if this browser version supports a specific protocol method.
    ///
    /// This provides a compatibility layer for methods that may not be
    /// available in all Camoufox/Firefox versions. Methods that are part of
    /// the standard Juggler protocol are always supported.
    ///
    /// # Camoufox-specific methods
    ///
    /// - `Browser.setPlatformOverride` -- requires Camoufox
    /// - `Browser.setContrast` -- requires Firefox 128+ or Camoufox
    ///
    /// # Standard methods
    ///
    /// All 66 standard Juggler protocol methods are supported regardless
    /// of version.
    pub fn supports_method(&self, method: &str) -> bool {
        match method {
            // Camoufox-specific methods.
            "Browser.setPlatformOverride" => self.is_camoufox,

            // Contrast support was added in Firefox 128.
            "Browser.setContrast" => {
                self.major_version.is_some_and(|v| v >= 128) || self.is_camoufox
            }

            // All standard Juggler protocol methods are supported.
            _ => is_standard_method(method),
        }
    }
}

/// Check whether a method is part of the standard Juggler protocol.
///
/// Returns `true` for all 66 documented methods across the 5 domains.
fn is_standard_method(method: &str) -> bool {
    matches!(
        method,
        // Browser domain (33 methods)
        "Browser.enable"
            | "Browser.getInfo"
            | "Browser.close"
            | "Browser.createBrowserContext"
            | "Browser.removeBrowserContext"
            | "Browser.newPage"
            | "Browser.setExtraHTTPHeaders"
            | "Browser.setHTTPCredentials"
            | "Browser.setBrowserProxy"
            | "Browser.setContextProxy"
            | "Browser.setRequestInterception"
            | "Browser.setCacheDisabled"
            | "Browser.setIgnoreHTTPSErrors"
            | "Browser.setDownloadOptions"
            | "Browser.setGeolocationOverride"
            | "Browser.setUserAgentOverride"
            | "Browser.setPlatformOverride"
            | "Browser.setBypassCSP"
            | "Browser.setJavaScriptDisabled"
            | "Browser.setLocaleOverride"
            | "Browser.setTimezoneOverride"
            | "Browser.setTouchOverride"
            | "Browser.setDefaultViewport"
            | "Browser.setOnlineOverride"
            | "Browser.setColorScheme"
            | "Browser.setReducedMotion"
            | "Browser.setForcedColors"
            | "Browser.setContrast"
            | "Browser.setScreencastOptions"
            | "Browser.setInitScripts"
            | "Browser.addBinding"
            | "Browser.setCookies"
            | "Browser.getCookies"
            | "Browser.clearCookies"
            | "Browser.grantPermissions"
            | "Browser.resetPermissions"
            | "Browser.clearCache"
            | "Browser.cancelDownload"
            // Page domain (22 methods)
            | "Page.navigate"
            | "Page.reload"
            | "Page.goBack"
            | "Page.goForward"
            | "Page.close"
            | "Page.bringToFront"
            | "Page.setViewportSize"
            | "Page.setEmulatedMedia"
            | "Page.setCacheDisabled"
            | "Page.setInitScripts"
            | "Page.setInterceptFileChooserDialog"
            | "Page.handleDialog"
            | "Page.screenshot"
            | "Page.describeNode"
            | "Page.scrollIntoViewIfNeeded"
            | "Page.getContentQuads"
            | "Page.setFileInputFiles"
            | "Page.adoptNode"
            | "Page.dispatchKeyEvent"
            | "Page.insertText"
            | "Page.dispatchMouseEvent"
            | "Page.dispatchWheelEvent"
            | "Page.dispatchTapEvent"
            | "Page.sendMessageToWorker"
            | "Page.startScreencast"
            | "Page.stopScreencast"
            | "Page.screencastFrameAck"
            // Network domain (6 methods)
            | "Network.setRequestInterception"
            | "Network.setExtraHTTPHeaders"
            | "Network.getResponseBody"
            | "Network.resumeInterceptedRequest"
            | "Network.fulfillInterceptedRequest"
            | "Network.abortInterceptedRequest"
            // Runtime domain (4 methods)
            | "Runtime.evaluate"
            | "Runtime.callFunction"
            | "Runtime.getObjectProperties"
            | "Runtime.disposeObject"
            // Heap domain (1 method)
            | "Heap.collectGarbage"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_camoufox_from_user_agent() {
        let info = CamoufoxInfo::from_browser_info(
            "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Camoufox/128.0",
            "Firefox/128.0",
        );
        assert!(info.is_camoufox);
        assert_eq!(info.major_version(), Some(128));
    }

    #[test]
    fn detect_camoufox_from_version() {
        let info = CamoufoxInfo::from_browser_info(
            "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0",
            "Camoufox/128.0",
        );
        assert!(info.is_camoufox);
    }

    #[test]
    fn detect_standard_firefox() {
        let info = CamoufoxInfo::from_browser_info(
            "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0",
            "Firefox/128.0",
        );
        assert!(!info.is_camoufox);
        assert_eq!(info.major_version(), Some(128));
    }

    #[test]
    fn parse_version_number() {
        let info = CamoufoxInfo::from_browser_info("", "Firefox/115.3");
        assert_eq!(info.major_version(), Some(115));
    }

    #[test]
    fn parse_version_missing() {
        let info = CamoufoxInfo::from_browser_info("", "");
        assert_eq!(info.major_version(), None);
    }

    #[test]
    fn supports_standard_methods() {
        let info = CamoufoxInfo::from_browser_info("", "Firefox/128.0");
        assert!(info.supports_method("Browser.enable"));
        assert!(info.supports_method("Page.navigate"));
        assert!(info.supports_method("Network.getResponseBody"));
        assert!(info.supports_method("Runtime.evaluate"));
        assert!(info.supports_method("Heap.collectGarbage"));
    }

    #[test]
    fn camoufox_supports_platform_override() {
        let camoufox = CamoufoxInfo::from_browser_info("Camoufox UA", "Firefox/128.0");
        assert!(camoufox.supports_method("Browser.setPlatformOverride"));

        let firefox = CamoufoxInfo::from_browser_info("Firefox UA", "Firefox/128.0");
        assert!(!firefox.supports_method("Browser.setPlatformOverride"));
    }

    #[test]
    fn contrast_requires_version_128() {
        let old = CamoufoxInfo::from_browser_info("", "Firefox/115.0");
        assert!(!old.supports_method("Browser.setContrast"));

        let new = CamoufoxInfo::from_browser_info("", "Firefox/128.0");
        assert!(new.supports_method("Browser.setContrast"));

        let camoufox = CamoufoxInfo::from_browser_info("Camoufox", "Firefox/100.0");
        assert!(camoufox.supports_method("Browser.setContrast"));
    }

    #[test]
    fn unknown_method_returns_false() {
        let info = CamoufoxInfo::from_browser_info("", "Firefox/128.0");
        assert!(!info.supports_method("Browser.unknownMethod"));
        assert!(!info.supports_method("Foo.bar"));
    }

    #[test]
    fn default_impl() {
        let info = CamoufoxInfo::default();
        assert!(!info.is_camoufox);
        assert_eq!(info.major_version(), None);
        assert!(info.user_agent.is_empty());
        assert!(info.version.is_empty());
    }
}
