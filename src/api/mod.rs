//! High-level API for interacting with the Camoufox browser.
//!
//! This module provides ergonomic wrappers around the raw Juggler protocol
//! commands. The three main types form a hierarchy:
//!
//! - [`Browser`] — top-level handle wrapping the root session.
//! - [`BrowserContext`] — an isolated browser context (profile).
//! - [`MainFrame`] — a single page/tab within a context, pinned to its top frame.
//!
//! # Usage
//!
//! ```rust,no_run
//! use camoufox::api::{Browser, BrowserOptions};
//!
//! // After establishing a Connection via transport + protocol layers:
//! // let browser = Browser::connect(connection, BrowserOptions::default())?;
//! // let context = browser.new_context(Default::default())?;
//! // let main_frame = context.new_main_frame()?;
//! // main_frame.navigate("https://example.com", Default::default(), std::time::Duration::from_secs(30))?;
//! ```

pub mod browser;
pub mod context;
pub mod main_frame;

pub use browser::{Browser, BrowserOptions, ProxyConfig};
pub use context::{BrowserContext, ContextOptions, Cookie, CookieOptions, Geolocation, Viewport};
pub use main_frame::{
    KeyEventParams, MainFrame, MouseEventParams, NavigateOptions, Rect, ScreenshotOptions,
    WheelEventParams,
};
