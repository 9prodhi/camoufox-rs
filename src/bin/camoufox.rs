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
            run_client(
                &sock,
                &DaemonRequest::NewPage { instance_id },
                cli.json,
            );
        }

        Command::Navigate {
            instance_id,
            page_id,
            url,
        } => {
            run_client(
                &sock,
                &DaemonRequest::Navigate {
                    instance_id,
                    page_id,
                    url,
                },
                cli.json,
            );
        }

        Command::Evaluate {
            instance_id,
            page_id,
            expression,
        } => {
            run_client(
                &sock,
                &DaemonRequest::Evaluate {
                    instance_id,
                    page_id,
                    expression,
                },
                cli.json,
            );
        }

        Command::Screenshot {
            instance_id,
            page_id,
            output,
            format,
            quality,
        } => {
            run_client(
                &sock,
                &DaemonRequest::Screenshot {
                    instance_id,
                    page_id,
                    format: Some(format),
                    quality,
                    path: output,
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
    }
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
