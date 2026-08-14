//! Camoufox CLI binary entrypoint.

use clap::Parser;

use camoufox::cli::client::send_request;
use camoufox::cli::commands::{Cli, Command};
use camoufox::cli::ipc::DaemonRequest;
use camoufox::cli::output::print_response;
use camoufox::cli::socket::socket_path;

fn main() {
    let cli = Cli::parse();
    let sock = socket_path(cli.socket.as_deref());

    match cli.command {
        Command::Serve { foreground } => {
            env_logger::init();
            if let Err(e) = camoufox::cli::daemon::run(&sock, foreground) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }

        Command::Launch { headed, executable } => {
            let request = DaemonRequest::Launch {
                headless: Some(!headed),
                executable,
            };
            run_client(&sock, &request, cli.json);
        }

        Command::List => {
            run_client(&sock, &DaemonRequest::List, cli.json);
        }

        Command::Stop { instance_id } => {
            run_client(&sock, &DaemonRequest::Stop { instance_id }, cli.json);
        }

        Command::NewPage { instance_id } => {
            run_client(&sock, &DaemonRequest::NewPage { instance_id }, cli.json);
        }

        Command::Navigate {
            instance_id,
            page_id,
            url,
            timeout,
            wait_until,
        } => {
            run_client(
                &sock,
                &DaemonRequest::Navigate {
                    instance_id,
                    page_id,
                    url,
                    timeout_secs: timeout,
                    wait_until,
                },
                cli.json,
            );
        }

        Command::Evaluate {
            instance_id,
            page_id,
            expression,
            timeout,
        } => {
            run_client(
                &sock,
                &DaemonRequest::Evaluate {
                    instance_id,
                    page_id,
                    expression,
                    timeout_secs: timeout,
                },
                cli.json,
            );
        }

        // `click <inst> <page> <x> <y>` (two numbers) keeps the original
        // coordinate behavior; `click <inst> <page> <selector>` is the new
        // selector form.
        Command::Click {
            instance_id,
            page_id,
            target,
            y,
            timeout,
        } => {
            let request = match y {
                Some(y) => match target.parse::<i32>() {
                    Ok(x) => DaemonRequest::Click {
                        instance_id,
                        page_id,
                        x,
                        y,
                    },
                    Err(_) => fail(
                        cli.json,
                        &format!(
                            "X coordinate must be an integer when a Y coordinate is given \
                             (got {target:?}); for a selector click, omit the Y argument"
                        ),
                    ),
                },
                None => DaemonRequest::ClickSelector {
                    instance_id,
                    page_id,
                    selector: target,
                    timeout_secs: timeout,
                },
            };
            run_client(&sock, &request, cli.json);
        }

        Command::Screenshot {
            instance_id,
            page_id,
            output,
            format,
            quality,
            selector,
            clip,
            timeout,
        } => {
            let clip = match clip.as_deref().map(parse_clip) {
                Some(Ok(rect)) => Some(rect),
                Some(Err(e)) => fail(cli.json, &e),
                None => None,
            };
            run_client(
                &sock,
                &DaemonRequest::Screenshot {
                    instance_id,
                    page_id,
                    format: Some(format),
                    quality,
                    path: output,
                    selector,
                    clip,
                    timeout_secs: timeout,
                },
                cli.json,
            );
        }

        Command::Shutdown => {
            run_client(&sock, &DaemonRequest::Shutdown, cli.json);
        }

        Command::Ping => {
            run_client(&sock, &DaemonRequest::Ping, cli.json);
        }

        Command::Cookies { instance_id } => {
            run_client(&sock, &DaemonRequest::Cookies { instance_id }, cli.json);
        }

        // -------------------------------------------------------------------
        // Reading
        // -------------------------------------------------------------------
        Command::Text {
            instance_id,
            page_id,
            selector,
            timeout,
        } => {
            run_client(
                &sock,
                &DaemonRequest::Text {
                    instance_id,
                    page_id,
                    selector,
                    timeout_secs: timeout,
                },
                cli.json,
            );
        }

        Command::Html {
            instance_id,
            page_id,
            selector,
            timeout,
        } => {
            run_client(
                &sock,
                &DaemonRequest::Html {
                    instance_id,
                    page_id,
                    selector,
                    timeout_secs: timeout,
                },
                cli.json,
            );
        }

        Command::Links {
            instance_id,
            page_id,
            selector,
            timeout,
        } => {
            run_client(
                &sock,
                &DaemonRequest::Links {
                    instance_id,
                    page_id,
                    selector,
                    timeout_secs: timeout,
                },
                cli.json,
            );
        }

        Command::Data {
            instance_id,
            page_id,
            og,
            jsonld,
            meta,
            timeout,
        } => {
            run_client(
                &sock,
                &DaemonRequest::Data {
                    instance_id,
                    page_id,
                    og,
                    jsonld,
                    meta,
                    timeout_secs: timeout,
                },
                cli.json,
            );
        }

        // -------------------------------------------------------------------
        // Navigation / waiting
        // -------------------------------------------------------------------
        Command::Url {
            instance_id,
            page_id,
            timeout,
        } => {
            run_client(
                &sock,
                &DaemonRequest::Url {
                    instance_id,
                    page_id,
                    timeout_secs: timeout,
                },
                cli.json,
            );
        }

        Command::Back {
            instance_id,
            page_id,
        } => {
            run_client(
                &sock,
                &DaemonRequest::Back {
                    instance_id,
                    page_id,
                },
                cli.json,
            );
        }

        Command::Forward {
            instance_id,
            page_id,
        } => {
            run_client(
                &sock,
                &DaemonRequest::Forward {
                    instance_id,
                    page_id,
                },
                cli.json,
            );
        }

        Command::Reload {
            instance_id,
            page_id,
        } => {
            run_client(
                &sock,
                &DaemonRequest::Reload {
                    instance_id,
                    page_id,
                },
                cli.json,
            );
        }

        Command::Wait {
            instance_id,
            page_id,
            selector,
            timeout,
        } => {
            run_client(
                &sock,
                &DaemonRequest::Wait {
                    instance_id,
                    page_id,
                    selector,
                    timeout_secs: timeout,
                },
                cli.json,
            );
        }

        // -------------------------------------------------------------------
        // Cookies / headers
        // -------------------------------------------------------------------
        Command::Cookie {
            instance_id,
            page_id,
            pair,
            url,
            domain,
            path,
            secure,
            http_only,
            timeout,
        } => {
            let (name, value) = match split_once_trimmed(&pair, '=') {
                Some(parts) => parts,
                None => fail(
                    cli.json,
                    &format!("cookie must be `name=value` (got {pair:?})"),
                ),
            };
            run_client(
                &sock,
                &DaemonRequest::SetCookie {
                    instance_id,
                    page_id,
                    name,
                    value,
                    url,
                    domain,
                    path,
                    secure,
                    http_only,
                    timeout_secs: timeout,
                },
                cli.json,
            );
        }

        Command::Header {
            instance_id,
            page_id,
            pair,
        } => {
            let (name, value) = match split_once_trimmed(&pair, ':') {
                Some(parts) => parts,
                None => fail(
                    cli.json,
                    &format!("header must be `Name: value` (got {pair:?})"),
                ),
            };
            run_client(
                &sock,
                &DaemonRequest::SetHeader {
                    instance_id,
                    page_id,
                    name,
                    value,
                },
                cli.json,
            );
        }

        // -------------------------------------------------------------------
        // Interaction
        // -------------------------------------------------------------------
        Command::Fill {
            instance_id,
            page_id,
            selector,
            value,
            timeout,
        } => {
            run_client(
                &sock,
                &DaemonRequest::Fill {
                    instance_id,
                    page_id,
                    selector,
                    value,
                    timeout_secs: timeout,
                },
                cli.json,
            );
        }

        Command::Type {
            instance_id,
            page_id,
            text,
        } => {
            run_client(
                &sock,
                &DaemonRequest::Type {
                    instance_id,
                    page_id,
                    text,
                },
                cli.json,
            );
        }

        Command::Press {
            instance_id,
            page_id,
            key,
        } => {
            run_client(
                &sock,
                &DaemonRequest::Press {
                    instance_id,
                    page_id,
                    key,
                },
                cli.json,
            );
        }

        Command::Hover {
            instance_id,
            page_id,
            selector,
            timeout,
        } => {
            run_client(
                &sock,
                &DaemonRequest::Hover {
                    instance_id,
                    page_id,
                    selector,
                    timeout_secs: timeout,
                },
                cli.json,
            );
        }

        Command::Select {
            instance_id,
            page_id,
            selector,
            value,
            timeout,
        } => {
            run_client(
                &sock,
                &DaemonRequest::Select {
                    instance_id,
                    page_id,
                    selector,
                    value,
                    timeout_secs: timeout,
                },
                cli.json,
            );
        }

        Command::Scroll {
            instance_id,
            page_id,
            selector,
            timeout,
        } => {
            run_client(
                &sock,
                &DaemonRequest::Scroll {
                    instance_id,
                    page_id,
                    selector,
                    timeout_secs: timeout,
                },
                cli.json,
            );
        }

        // -------------------------------------------------------------------
        // Tabs
        // -------------------------------------------------------------------
        Command::Tabs {
            instance_id,
            timeout,
        } => {
            run_client(
                &sock,
                &DaemonRequest::Tabs {
                    instance_id,
                    timeout_secs: timeout,
                },
                cli.json,
            );
        }

        Command::CloseTab {
            instance_id,
            page_id,
        } => {
            run_client(
                &sock,
                &DaemonRequest::CloseTab {
                    instance_id,
                    page_id,
                },
                cli.json,
            );
        }
    }
}

/// Split `s` on the first `sep`, trimming whitespace around both halves.
///
/// Returns `None` when `sep` is absent or the key half is empty.
fn split_once_trimmed(s: &str, sep: char) -> Option<(String, String)> {
    let (name, value) = s.split_once(sep)?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    Some((name.to_string(), value.trim().to_string()))
}

/// Parse a `x,y,width,height` clip string into four floats.
fn parse_clip(s: &str) -> Result<[f64; 4], String> {
    let parts: Vec<&str> = s.split(',').map(str::trim).collect();
    if parts.len() != 4 {
        return Err(format!(
            "--clip needs 4 comma-separated numbers `x,y,width,height` (got {s:?})"
        ));
    }
    let mut out = [0.0f64; 4];
    for (i, p) in parts.iter().enumerate() {
        out[i] = p
            .parse::<f64>()
            .map_err(|_| format!("--clip component {p:?} is not a number"))?;
    }
    if out[2] <= 0.0 || out[3] <= 0.0 {
        return Err("--clip width and height must be positive".to_string());
    }
    Ok(out)
}

/// Report a client-side argument error and exit, honouring `--json`.
fn fail(json_mode: bool, message: &str) -> ! {
    if json_mode {
        let resp = camoufox::cli::ipc::DaemonResponse::err(message);
        println!(
            "{}",
            serde_json::to_string_pretty(&resp).unwrap_or_else(|_| "{}".into())
        );
    } else {
        eprintln!("error: {message}");
    }
    std::process::exit(2);
}

fn run_client(sock: &std::path::Path, request: &DaemonRequest, json_mode: bool) {
    match send_request(sock, request) {
        Ok(response) => {
            let ok = response.ok;
            print_response(&response, json_mode);
            if !ok {
                std::process::exit(1);
            }
        }
        Err(e) => {
            if json_mode {
                let resp = camoufox::cli::ipc::DaemonResponse::err(&e);
                println!(
                    "{}",
                    serde_json::to_string_pretty(&resp).unwrap_or_else(|_| "{}".into())
                );
            } else {
                eprintln!("error: {e}");
            }
            std::process::exit(1);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// `name=value` / `Name: value` pairs split on the FIRST separator, so a
    /// value may itself contain one (e.g. a URL in a header value).
    #[test]
    fn split_once_trimmed_splits_on_first_separator() {
        assert_eq!(
            split_once_trimmed("session=abc123", '='),
            Some(("session".into(), "abc123".into()))
        );
        assert_eq!(
            split_once_trimmed("Referer: https://a.example/x?y=1", ':'),
            Some(("Referer".into(), "https://a.example/x?y=1".into()))
        );
        // Surrounding whitespace is trimmed from both halves.
        assert_eq!(
            split_once_trimmed(" X-Test :  hello ", ':'),
            Some(("X-Test".into(), "hello".into()))
        );
        // An empty value is legal (clears a header / sets an empty cookie).
        assert_eq!(
            split_once_trimmed("empty=", '='),
            Some(("empty".into(), String::new()))
        );
    }

    /// A pair with no separator, or with an empty name, is rejected so the
    /// user gets an argument error instead of a confusing daemon-side failure.
    #[test]
    fn split_once_trimmed_rejects_malformed_pairs() {
        assert_eq!(split_once_trimmed("nopair", '='), None);
        assert_eq!(split_once_trimmed("=value", '='), None);
        assert_eq!(split_once_trimmed("  : value", ':'), None);
    }

    /// `--clip` accepts four numbers, with or without spaces.
    #[test]
    fn parse_clip_accepts_four_numbers() {
        assert_eq!(parse_clip("0,0,300,120").unwrap(), [0.0, 0.0, 300.0, 120.0]);
        assert_eq!(
            parse_clip(" 1.5 , 2 , 30.25 , 40 ").unwrap(),
            [1.5, 2.0, 30.25, 40.0]
        );
    }

    /// Malformed clips are rejected client-side with an actionable message.
    #[test]
    fn parse_clip_rejects_malformed_input() {
        assert!(parse_clip("bogus").unwrap_err().contains("4 comma"));
        assert!(parse_clip("1,2,3").unwrap_err().contains("4 comma"));
        assert!(parse_clip("1,2,x,4").unwrap_err().contains("not a number"));
        assert!(parse_clip("0,0,0,100").unwrap_err().contains("positive"));
        assert!(parse_clip("0,0,100,-1").unwrap_err().contains("positive"));
    }
}
