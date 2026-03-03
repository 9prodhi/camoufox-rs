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
    },

    /// Evaluate JavaScript on a page.
    Evaluate {
        /// Instance ID.
        instance_id: String,
        /// Page ID.
        page_id: String,
        /// JavaScript expression to evaluate.
        expression: String,
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
    },

    /// Shut down the daemon and all browser instances.
    Shutdown,

    /// Ping the daemon.
    Ping,
}
