// SPDX-License-Identifier: Apache-2.0

//! Invocation of the `codespell` executable and parsing of its output.
//!
//! `codespell` is a plain CLI: it has no LSP mode. This module spawns it
//! once per check with the document content on stdin and parses the
//! `line: word ==> suggestion` lines it prints with `--stdin-single-line`.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use regex::Regex;

use crate::config::Settings;

/// A single misspelling reported by `codespell`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpellMatch {
    /// 0-based line index.
    pub line: usize,
    /// The misspelled word exactly as it appears in the document.
    pub word: String,
    /// Suggested corrections, comma-separated, or `None` when `codespell`
    /// could not suggest an automatic fix.
    pub suggestions: Option<String>,
    /// Optional editorial reason for non-automatic fixes.
    pub reason: Option<String>,
}

#[derive(Debug)]
pub enum CheckError {
    /// The `codespell` executable could not be located.
    MissingExecutable { path: String, err: String },
    /// `codespell` exited with a usage/configuration error.
    Failed { exit_code: i32, stderr: String },
}

/// Result of one check.
#[derive(Debug)]
pub enum CheckResult {
    Matches(Vec<SpellMatch>),
    Error(CheckError),
}

const CODESPELL_BIN: &str = "codespell";

/// Whether the current error is a "definitely not a misspelling" signal.
pub fn is_usage_error(exit_code: i32) -> bool {
    // codespell: 0 = no matches, 65 = matches found,
    // 64 = usage error (bad glob / bad regex / bad options),
    // 78 = config error (EX_CONFIG on BSD-like systems).
    !matches!(exit_code, 0 | 65)
}

/// Build the `codespell` argument list. The document is passed on stdin so
/// unsaved buffers are checked with their current contents.
fn build_args(settings: &Settings) -> Vec<String> {
    let mut args: Vec<String> = vec!["--stdin".into(), "--stdin-single-line".into()];

    if settings.ignore_sic {
        args.push("--ignore-sic".into());
    }
    if let Some(regex) = &settings.ignore_regex {
        args.push("--ignore-regex".into());
        args.push(regex.clone());
    }
    if !settings.ignore_words.is_empty() {
        args.push("--ignore-words-list".into());
        args.push(settings.ignore_words.join(","));
    }
    if let Some(file) = &settings.ignore_words_file {
        args.push("--ignore-words".into());
        args.push(file.clone());
    }
    if !settings.uri_ignore_words.is_empty() {
        args.push("--uri-ignore-words-list".into());
        args.push(settings.uri_ignore_words.join(","));
    }
    if !settings.skip.is_empty() {
        args.push("--skip".into());
        args.push(settings.skip.join(","));
    }
    if !settings.dictionary.is_empty() {
        args.push("--dictionary".into());
        args.push(settings.dictionary.join(","));
    }
    if let Some(builtin) = &settings.builtin {
        args.push("--builtin".into());
        args.push(builtin.clone());
    }
    if let Some(config) = &settings.config {
        args.push("--config".into());
        args.push(config.clone());
    }

    args.extend(settings.args.iter().cloned());
    args.push("-".to_string());

    args
}

/// Resolve the `codespell` executable: explicit path from settings, then
/// `PATH`.
fn resolve_executable(settings: &Settings) -> Result<String, CheckError> {
    if let Some(path) = &settings.path {
        if Path::new(path).is_file() {
            return Ok(path.clone());
        }
        return Err(CheckError::MissingExecutable {
            path: path.clone(),
            err: "configured codespell.path does not exist or is not a file".to_string(),
        });
    }

    let path = find_in_path(CODESPELL_BIN).ok_or_else(|| CheckError::MissingExecutable {
        path: CODESPELL_BIN.to_string(),
        err: "codespell executable not found on PATH".to_string(),
    })?;
    Ok(path)
}

fn find_in_path(bin: &str) -> Option<String> {
    let path_var = std::env::var_os("PATH")?;

    #[cfg(windows)]
    const EXTENSIONS: &[&str] = &["", ".exe", ".cmd", ".bat"];
    #[cfg(not(windows))]
    const EXTENSIONS: &[&str] = &[""];

    for dir in std::env::split_paths(&path_var) {
        for extension in EXTENSIONS {
            let candidate = dir.join(format!("{bin}{extension}"));
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
    }

    None
}

/// Run `codespell` over `text` with the given settings.
///
/// `cwd` is the project root: `codespell` discovers its configuration
/// (`.codespellrc`, `setup.cfg`, `pyproject.toml`) relative to it, matching
/// command-line behavior.
pub fn check_text(text: &str, settings: &Settings, cwd: &Path) -> CheckResult {
    let executable = match resolve_executable(settings) {
        Ok(path) => path,
        Err(err) => return CheckResult::Error(err),
    };

    let mut child = match Command::new(&executable)
        .args(build_args(settings))
        .current_dir(cwd)
        .envs(settings.env.iter().cloned())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            return CheckResult::Error(CheckError::MissingExecutable {
                path: executable,
                err: err.to_string(),
            });
        }
    };

    if let Some(mut stdin) = child.stdin.take()
        && let Err(err) = stdin.write_all(text.as_bytes())
    {
        return CheckResult::Error(CheckError::MissingExecutable {
            path: executable,
            err: format!("failed to write document to codespell stdin: {err}"),
        });
    }

    let output = match child.wait_with_output() {
        Ok(output) => output,
        Err(err) => {
            return CheckResult::Error(CheckError::MissingExecutable {
                path: executable,
                err: format!("failed to wait for codespell: {err}"),
            });
        }
    };

    let exit_code = output.status.code().unwrap_or(-1);
    if is_usage_error(exit_code) {
        return CheckResult::Error(CheckError::Failed {
            exit_code,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    CheckResult::Matches(parse_output(&stdout))
}

/// Parse `codespell --stdin-single-line` output:
///
/// ```text
/// 1: teh ==> the
/// 2: fiel ==> feel, field, file, phial
/// 3: foo ==>   | this word is uncorrectable
/// ```
///
/// Lines that do not match are ignored (they come from stderr messages that
/// leaked into stdout or `codespell` warnings).
pub fn parse_output(stdout: &str) -> Vec<SpellMatch> {
    // The word cannot contain whitespace by codespell's word regex
    // (`[\w\-'’]+`), which keeps the parse unambiguous.
    let line_re = Regex::new(r"^(\d+): (\S+) ==>\s*(.*)$").expect("static regex is valid");

    let mut matches = Vec::new();
    for line in stdout.lines() {
        let Some(captures) = line_re.captures(line) else {
            continue;
        };

        let line_number: usize = captures[1].parse().unwrap_or(0);
        if line_number == 0 {
            continue;
        }

        // codespell prints `fixword` optionally followed by `  | reason`.
        // The misspelled word itself can never contain `|`, so the first
        // `|` reliably separates the suggestion from the editorial reason.
        let raw = captures[3].trim();
        let (suggestion_part, reason) = match raw.split_once('|') {
            Some((left, right)) => (left.trim(), Some(right.trim().to_string())),
            None => (raw, None),
        };
        let suggestions = (!suggestion_part.is_empty()).then(|| suggestion_part.to_string());

        matches.push(SpellMatch {
            line: line_number - 1,
            word: captures[2].to_string(),
            suggestions: suggestions.filter(|s| !s.is_empty()),
            reason,
        });
    }

    matches
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple() {
        let matches = parse_output("1: teh ==> the\n2: quikc ==> quick\n");
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].line, 0);
        assert_eq!(matches[0].word, "teh");
        assert_eq!(matches[0].suggestions.as_deref(), Some("the"));
        assert!(matches[0].reason.is_none());
    }

    #[test]
    fn test_parse_multiple_suggestions() {
        let matches = parse_output("10: fiel ==> feel, field, file, phial\n");
        assert_eq!(matches[0].line, 9);
        assert_eq!(
            matches[0].suggestions.as_deref(),
            Some("feel, field, file, phial")
        );
    }

    #[test]
    fn test_parse_reason() {
        let matches = parse_output("1: foo ==>   | this word is uncorrectable\n");
        assert_eq!(matches[0].word, "foo");
        assert_eq!(matches[0].suggestions, None);
        assert_eq!(
            matches[0].reason.as_deref(),
            Some("this word is uncorrectable")
        );
    }

    #[test]
    fn test_parse_reason_with_fallback_suggestion() {
        let matches = parse_output("1: baz ==> zog  | zag\n");
        assert_eq!(matches[0].suggestions.as_deref(), Some("zog"));
        assert_eq!(matches[0].reason.as_deref(), Some("zag"));
    }

    #[test]
    fn test_parse_ignores_garbage() {
        // A header line codespell prints in non-stdin-single-line mode, or
        // a warning, must not be misparsed as a match.
        let matches = parse_output("1: full text of the line\n\tteh ==> the\n");
        assert!(matches.is_empty());
    }

    #[test]
    fn test_parse_duplicates_kept() {
        let matches = parse_output("1: teh ==> the\n1: teh ==> the\n1: teh ==> the\n");
        assert_eq!(matches.len(), 3);
        assert!(matches.iter().all(|m| m.line == 0));
    }

    #[test]
    fn test_usage_error_codes() {
        assert!(!is_usage_error(0));
        assert!(!is_usage_error(65));
        assert!(is_usage_error(64));
        assert!(is_usage_error(78));
    }
}
