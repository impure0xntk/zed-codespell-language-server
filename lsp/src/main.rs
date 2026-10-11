// SPDX-License-Identifier: Apache-2.0

//! `codespell-lsp` is a thin language server that drives the real
//! `codespell` executable. The Zed extension (this repository's root
//! crate) starts it with `serve --root=<project>`.

mod codespell;
mod config;
mod server;

use std::path::PathBuf;
use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse_args(&args) {
        Command::Serve { root } => match serve(root) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("[codespell-lsp] fatal: {err}");
                ExitCode::from(1)
            }
        },
        Command::Version => {
            println!("codespell-lsp {VERSION}");
            ExitCode::SUCCESS
        }
        Command::Help => {
            print_help();
            ExitCode::SUCCESS
        }
        Command::Invalid(message) => {
            eprintln!("error: {message}");
            eprintln!("Run `codespell-lsp --help` for usage.");
            ExitCode::from(2)
        }
    }
}

enum Command {
    Serve { root: Option<PathBuf> },
    Version,
    Help,
    Invalid(String),
}

fn parse_args(args: &[String]) -> Command {
    let mut root: Option<PathBuf> = None;
    let mut serve = false;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--help" | "-h" => return Command::Help,
            "--version" | "-V" => return Command::Version,
            "serve" => serve = true,
            _ if arg.starts_with("--root=") => {
                root = Some(PathBuf::from(&arg["--root=".len()..]));
            }
            "--root" => match iter.next() {
                Some(value) => root = Some(PathBuf::from(value)),
                None => return Command::Invalid("--root requires a value".to_string()),
            },
            _ => return Command::Invalid(format!("unexpected argument \"{arg}\"")),
        }
    }

    if serve {
        Command::Serve { root }
    } else {
        Command::Invalid("missing `serve` subcommand".to_string())
    }
}

fn serve(root: Option<PathBuf>) -> anyhow::Result<()> {
    server::Server::run(root)
}

fn print_help() {
    println!(
        "codespell-lsp {VERSION}
Language server adapter that exposes the codespell spell checker over LSP.

The Zed extension launches this binary; `codespell` itself must be installed
separately (e.g. `pip install codespell`), either on PATH or configured via
the `codespell.path` LSP setting. This adapter only speaks LSP: all spelling
logic lives in the real codespell binary.

USAGE:
    codespell-lsp --root=<project-path> serve

OPTIONS:
    --root=<project-path>  Workspace root used for codespell config discovery
    -h, --help             Print help
    -V, --version          Print version
"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_serve_with_root_flag() {
        match parse_args(&["serve".into(), "--root".into(), "/proj".into()]) {
            Command::Serve { root } => assert_eq!(root, Some(PathBuf::from("/proj"))),
            _ => panic!("expected serve command"),
        }
    }

    #[test]
    fn test_parse_serve_with_root_equals() {
        match parse_args(&["--root=/proj".into(), "serve".into()]) {
            Command::Serve { root } => assert_eq!(root, Some(PathBuf::from("/proj"))),
            _ => panic!("expected serve command"),
        }
    }

    #[test]
    fn test_parse_serve_without_root() {
        match parse_args(&["serve".into()]) {
            Command::Serve { root } => assert!(root.is_none()),
            _ => panic!("expected serve command"),
        }
    }

    #[test]
    fn test_missing_serve_subcommand() {
        assert!(matches!(parse_args(&[]), Command::Invalid(_)));
        assert!(matches!(
            parse_args(&["--root=/x".into()]),
            Command::Invalid(_)
        ));
    }

    #[test]
    fn test_version_and_help() {
        assert!(matches!(
            parse_args(&["--version".into()]),
            Command::Version
        ));
        assert!(matches!(parse_args(&["--help".into()]), Command::Help));
    }
}
