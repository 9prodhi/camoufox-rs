//! Clap CLI subcommand definitions.

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "camoufox", about = "CLI for Camoufox browser automation")]
pub struct Cli {
    /// Output as JSON instead of human-readable text.
    #[arg(long, global = true)]
    pub json: bool,

    /// Path to the daemon Unix socket.
    #[arg(long, global = true)]
    pub socket: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Start the daemon process.
    Serve {
        /// Run in the foreground (don't daemonize).
        #[arg(long)]
        foreground: bool,
    },

    /// Launch a new browser instance.
    Launch {
        /// Run in headed mode (show browser window).
        #[arg(long)]
        headed: bool,

        /// Path to the Camoufox executable.
        #[arg(long)]
        executable: Option<String>,
    },

    /// List all running browser instances.
    List,

    /// Stop a browser instance.
    Stop {
        /// Instance ID (e.g., 00000001).
        instance_id: String,
    },

    /// Create a new page in a browser instance.
    NewPage {
        /// Instance ID.
        instance_id: String,
    },

    /// Navigate a page to a URL.
    Navigate {
        /// Instance ID.
        instance_id: String,
        /// Page ID (e.g., p1).
        page_id: String,
        /// URL to navigate to.
        url: String,
        /// Timeout in seconds for waiting for execution context.
        #[arg(long, default_value = "30")]
        timeout: u64,
        /// If set, block until the named lifecycle event fires after navigation.
        /// Supported values: load, domcontentloaded.
        /// Absent: return after the Page.navigate ack (existing behavior).
        #[arg(long)]
        wait_until: Option<String>,
    },

    /// Evaluate JavaScript on a page.
    Evaluate {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
        /// JavaScript expression to evaluate.
        expression: String,
        /// Timeout in seconds for waiting for execution context.
        #[arg(long, default_value = "30")]
        timeout: u64,
    },

    /// Click an element by CSS selector, or at viewport coordinates (x, y).
    ///
    /// `click <instance> <page> <selector>` resolves the selector, scrolls it
    /// into view and clicks its centre. `click <instance> <page> <x> <y>`
    /// (two numeric args) dispatches a click at raw viewport coordinates.
    Click {
        /// Instance ID.
        instance_id: String,
        /// Page ID (e.g., p1).
        page_id: String,
        /// CSS selector, or the X coordinate when a Y coordinate follows.
        target: String,
        /// Y coordinate (viewport pixels). When present, `target` is the X coordinate.
        y: Option<i32>,
        /// Timeout in seconds for waiting for execution context.
        #[arg(long, default_value = "30")]
        timeout: u64,
    },

    /// Take a screenshot of a page.
    Screenshot {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
        /// Output file path.
        #[arg(short = 'o', long = "output")]
        output: Option<String>,
        /// Image format: png or jpeg.
        #[arg(long, default_value = "png")]
        format: String,
        /// JPEG quality (0-100).
        #[arg(long)]
        quality: Option<u32>,
        /// Crop to the element matching this CSS selector.
        #[arg(long, conflicts_with = "clip")]
        selector: Option<String>,
        /// Clip region as `x,y,width,height` in CSS pixels.
        #[arg(long, conflicts_with = "selector")]
        clip: Option<String>,
        /// Timeout in seconds for waiting for execution context.
        #[arg(long, default_value = "30")]
        timeout: u64,
    },

    /// Shut down the daemon and all browser instances.
    Shutdown,

    /// Ping the daemon.
    Ping,

    /// Export all cookies for a browser instance (including HttpOnly).
    Cookies {
        /// Instance ID (e.g., 00000001).
        instance_id: String,
    },

    // -----------------------------------------------------------------------
    // Reading
    // -----------------------------------------------------------------------
    /// Print the page's visible text (`innerText`), optionally scoped to a selector.
    Text {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
        /// Restrict extraction to the first element matching this CSS selector.
        #[arg(long)]
        selector: Option<String>,
        /// Timeout in seconds for waiting for execution context.
        #[arg(long, default_value = "30")]
        timeout: u64,
    },

    /// Print page HTML: `outerHTML` of a selector, or the whole document.
    Html {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
        /// Return the `outerHTML` of the first element matching this CSS selector.
        #[arg(long)]
        selector: Option<String>,
        /// Timeout in seconds for waiting for execution context.
        #[arg(long, default_value = "30")]
        timeout: u64,
    },

    /// List every `<a href>` on the page as `text → href`.
    Links {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
        /// Only collect links inside the element matching this CSS selector.
        #[arg(long)]
        selector: Option<String>,
        /// Timeout in seconds for waiting for execution context.
        #[arg(long, default_value = "30")]
        timeout: u64,
    },

    /// Extract structured page metadata (Open Graph / JSON-LD / meta tags).
    ///
    /// With no flags, all three groups are returned.
    Data {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
        /// Include Open Graph (`og:` / `twitter:`) tags.
        #[arg(long)]
        og: bool,
        /// Include `application/ld+json` blocks.
        #[arg(long)]
        jsonld: bool,
        /// Include named `<meta>` tags.
        #[arg(long)]
        meta: bool,
        /// Timeout in seconds for waiting for execution context.
        #[arg(long, default_value = "30")]
        timeout: u64,
    },

    // -----------------------------------------------------------------------
    // Navigation / waiting
    // -----------------------------------------------------------------------
    /// Print the page's current URL and title.
    Url {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
        /// Timeout in seconds for waiting for execution context.
        #[arg(long, default_value = "30")]
        timeout: u64,
    },

    /// Go back one entry in the page's session history.
    Back {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
    },

    /// Go forward one entry in the page's session history.
    Forward {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
    },

    /// Reload the page.
    Reload {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
    },

    /// Poll until an element matching a CSS selector exists.
    Wait {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
        /// CSS selector to wait for.
        #[arg(long)]
        selector: String,
        /// Give up after this many seconds.
        #[arg(long, default_value = "10")]
        timeout: u64,
    },

    // -----------------------------------------------------------------------
    // Cookies / headers
    // -----------------------------------------------------------------------
    /// Set a cookie for the instance's browser context: `name=value`.
    Cookie {
        /// Instance ID.
        instance_id: String,
        /// Page ID (used to derive the cookie URL when neither --url nor --domain is given).
        page_id: String,
        /// Cookie as `name=value`.
        pair: String,
        /// Associate the cookie with this URL instead of the page's current URL.
        #[arg(long)]
        url: Option<String>,
        /// Cookie domain (mutually exclusive with --url).
        #[arg(long, conflicts_with = "url")]
        domain: Option<String>,
        /// Cookie path.
        #[arg(long)]
        path: Option<String>,
        /// Mark the cookie Secure.
        #[arg(long)]
        secure: bool,
        /// Mark the cookie HttpOnly.
        #[arg(long)]
        http_only: bool,
        /// Timeout in seconds for waiting for execution context.
        #[arg(long, default_value = "30")]
        timeout: u64,
    },

    /// Set an extra HTTP request header for a page: `Name: value`.
    ///
    /// Headers accumulate across calls for the lifetime of the page.
    Header {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
        /// Header as `Name: value`.
        pair: String,
    },

    // -----------------------------------------------------------------------
    // Interaction
    // -----------------------------------------------------------------------
    /// Focus the element matching a selector, clear it, and type a value into it.
    Fill {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
        /// CSS selector of the input/textarea/contenteditable.
        selector: String,
        /// Value to type.
        value: String,
        /// Timeout in seconds for waiting for execution context.
        #[arg(long, default_value = "30")]
        timeout: u64,
    },

    /// Type text into whatever element currently has focus.
    Type {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
        /// Text to insert.
        text: String,
    },

    /// Press a named key (Enter, Tab, Escape, ArrowDown, a, 1, …).
    Press {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
        /// Key name.
        key: String,
    },

    /// Move the mouse over the element matching a selector (no click).
    Hover {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
        /// CSS selector.
        selector: String,
        /// Timeout in seconds for waiting for execution context.
        #[arg(long, default_value = "30")]
        timeout: u64,
    },

    /// Choose an option in a `<select>` by value, label, or visible text.
    Select {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
        /// CSS selector of the `<select>`.
        selector: String,
        /// Option value / label / text to select.
        value: String,
        /// Timeout in seconds for waiting for execution context.
        #[arg(long, default_value = "30")]
        timeout: u64,
    },

    /// Scroll an element into view, or scroll to the bottom of the page.
    Scroll {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
        /// CSS selector to scroll into view. Omit to scroll to the page bottom.
        selector: Option<String>,
        /// Timeout in seconds for waiting for execution context.
        #[arg(long, default_value = "30")]
        timeout: u64,
    },

    // -----------------------------------------------------------------------
    // Tabs
    // -----------------------------------------------------------------------
    /// List the open pages (tabs) of an instance with their URLs and titles.
    Tabs {
        /// Instance ID.
        instance_id: String,
        /// Per-page timeout in seconds for reading each tab's URL/title.
        /// A tab that can't be read within this budget reports null.
        #[arg(long, default_value = "30")]
        timeout: u64,
    },

    /// Close a page (tab).
    CloseTab {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
    },
}
