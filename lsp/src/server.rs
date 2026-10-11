// SPDX-License-Identifier: Apache-2.0

//! LSP server loop: translates LSP document events into `codespell` runs
//! and publishes the resulting diagnostics, code actions, and hovers.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use globset::{Glob, GlobSet, GlobSetBuilder};
use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, CodeActionParams,
    CodeActionProviderCapability, Diagnostic, DiagnosticSeverity, ExecuteCommandOptions,
    ExecuteCommandParams, Hover, HoverContents, HoverParams, HoverProviderCapability,
    InitializeParams, InitializeResult, MarkupContent, MarkupKind, Position,
    PublishDiagnosticsParams, Range, ServerCapabilities, TextDocumentSyncCapability,
    TextDocumentSyncKind, TextDocumentSyncOptions, TextDocumentSyncSaveOptions, TextEdit, Url,
    WorkspaceEdit,
};
use serde::Deserialize;
use serde_json::Value;

use crate::codespell::{self, CheckResult, SpellMatch};
use crate::config::{self, RunMode, Settings};

const DEBOUNCE: Duration = Duration::from_millis(300);
const COMMAND_ADD_TO_IGNORE_LIST: &str = "codespell.addToIgnoreList";

/// One reported misspelling with the LSP range resolved against the
/// document text.
#[derive(Debug, Clone)]
struct ResolvedMatch {
    range: Range,
    word: String,
    suggestions: Vec<String>,
    reason: Option<String>,
}

#[derive(Clone)]
struct Document {
    version: i32,
    text: String,
    path: PathBuf,
}

pub struct Server {
    connection: Connection,
    docs: HashMap<Url, Document>,
    results: HashMap<Url, Vec<ResolvedMatch>>,
    roots: Vec<PathBuf>,
    settings: Settings,
    /// `skip` globs from the project config files (`.codespellrc`,
    /// `setup.cfg`, `pyproject.toml`).
    config_skip: Vec<String>,
    /// Compiled effective skip globs (settings + config file).
    skip_set: Option<GlobSet>,
    /// Pending debounced re-checks, coalesced by deadline.
    pending: HashSet<Url>,
    deadline: Option<Instant>,
}

#[derive(Debug, Deserialize, Clone)]
struct CodeActionData {
    word: Option<String>,
    #[serde(default)]
    suggestions: Vec<String>,
}

impl Server {
    pub fn run(cli_root: Option<PathBuf>) -> anyhow::Result<()> {
        let (connection, io_threads) = Connection::stdio();

        let (id, initialize_params) = wait_for_initialize(&connection)?;
        let initialize_params: InitializeParams = serde_json::from_value(initialize_params)
            .map_err(|err| anyhow::anyhow!("invalid initialize params: {err}"))?;

        let mut roots = Vec::new();
        if let Some(root) = cli_root {
            roots.push(root);
        }
        for folder in initialize_params.workspace_folders.iter().flatten() {
            if let Ok(path) = folder.uri.to_file_path()
                && !roots.contains(&path)
            {
                roots.push(path);
            }
        }

        let mut settings = Settings::default();
        if let Some(options) = initialize_params.initialization_options.as_ref() {
            config::merge_settings(&mut settings, options);
        }

        let capabilities = ServerCapabilities {
            text_document_sync: Some(TextDocumentSyncCapability::Options(
                TextDocumentSyncOptions {
                    open_close: Some(true),
                    change: Some(TextDocumentSyncKind::FULL),
                    save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                    ..Default::default()
                },
            )),
            code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
            execute_command_provider: Some(ExecuteCommandOptions {
                commands: vec![COMMAND_ADD_TO_IGNORE_LIST.to_string()],
                ..Default::default()
            }),
            hover_provider: Some(HoverProviderCapability::Simple(true)),
            ..Default::default()
        };

        let result = InitializeResult {
            capabilities,
            ..Default::default()
        };
        // Send the initialize response ourselves instead of using
        // `initialize_finish`: that helper fails unless the *very first*
        // message after the response is the `initialized` notification, but
        // real clients (Zed included) commonly send `$/setTrace` and other
        // messages in between. The main loop below tolerates any order.
        let response = Response::new_ok(id, serde_json::to_value(result)?);
        connection.sender.send(Message::Response(response))?;

        // Ask for the `lsp.codespell-lsp.settings` provided to the extension
        // host through `language_server_workspace_configuration`. The
        // response is applied in `handle_response`.
        let config_request = Request::new(
            RequestId::from(1),
            "workspace/configuration".into(),
            serde_json::json!({ "items": [
                { "section": "codespell" },
                { "section": "codespell-lsp" },
            ] }),
        );
        connection
            .sender
            .send(Message::Request(config_request))
            .ok();

        eprintln!(
            "[codespell-lsp] started (pid={}), serving roots={roots:?}",
            std::process::id()
        );

        let mut server = Server {
            connection,
            docs: HashMap::new(),
            results: HashMap::new(),
            roots,
            settings,
            config_skip: Vec::new(),
            skip_set: None,
            pending: HashSet::new(),
            deadline: None,
        };
        server.refresh_skip();

        server.run_loop()?;

        // Drop the connection (which holds the writer-sender) before joining
        // the I/O threads: the writer thread only terminates once its
        // channel is closed, and it is joined inside `io_threads.join()`.
        drop(server);
        io_threads.join()?;
        Ok(())
    }

    fn run_loop(&mut self) -> anyhow::Result<()> {
        loop {
            let timeout = self
                .deadline
                .map(|d| d.saturating_duration_since(Instant::now()));

            // A `false` error means "timeout waiting for the debounce
            // window"; a `true` error means the client disconnected.
            let recv = match timeout {
                Some(duration) => self
                    .connection
                    .receiver
                    .recv_timeout(duration)
                    .map_err(|err| {
                        matches!(err, crossbeam_channel::RecvTimeoutError::Disconnected)
                    }),
                None => self.connection.receiver.recv().map_err(|_| true),
            };

            let message = match recv {
                Ok(message) => message,
                Err(true) => break,
                Err(false) => {
                    self.flush_pending();
                    continue;
                }
            };

            match message {
                Message::Request(request) => {
                    if !self.handle_request(request)? {
                        break;
                    }
                }
                Message::Notification(notification) => {
                    if !self.handle_notification(notification)? {
                        break;
                    }
                }
                Message::Response(response) => {
                    self.handle_response(response);
                }
            }
        }

        Ok(())
    }

    fn handle_request(&mut self, request: Request) -> anyhow::Result<bool> {
        let method = request.method.as_str();
        match method {
            "shutdown" => {
                let response = Response::new_ok(request.id, Value::Null);
                self.connection.sender.send(Message::Response(response))?;
                Ok(true)
            }
            "textDocument/codeAction" => {
                let params: CodeActionParams = serde_json::from_value(request.params)?;
                let actions = self.code_actions(&params);
                let response = Response::new_ok(request.id, serde_json::to_value(actions)?);
                self.connection.sender.send(Message::Response(response))?;
                Ok(true)
            }
            "textDocument/hover" => {
                let params: HoverParams = serde_json::from_value(request.params)?;
                let hover = self.hover(&params);
                let response = Response::new_ok(request.id, serde_json::to_value(hover)?);
                self.connection.sender.send(Message::Response(response))?;
                Ok(true)
            }
            "workspace/executeCommand" => {
                let params: ExecuteCommandParams = serde_json::from_value(request.params)?;
                // Respond before re-checking: the re-check publishes
                // diagnostics, and those must not race ahead of the command
                // response (a client waiting on the response would otherwise
                // consume them as noise).
                let response = Response::new_ok(request.id, Value::Null);
                self.connection.sender.send(Message::Response(response))?;
                self.execute_command(&params);
                Ok(true)
            }
            _ => {
                let response = Response::new_err(
                    request.id,
                    lsp_server::ErrorCode::MethodNotFound as i32,
                    format!("unhandled method {method}"),
                );
                self.connection.sender.send(Message::Response(response))?;
                Ok(true)
            }
        }
    }

    fn handle_notification(&mut self, notification: Notification) -> anyhow::Result<bool> {
        let method = notification.method.as_str();
        match method {
            "exit" => Ok(false),
            "initialized" => Ok(true),
            "textDocument/didOpen" => {
                let params: lsp_types::DidOpenTextDocumentParams =
                    serde_json::from_value(notification.params)?;
                self.open_document(&params);
                Ok(true)
            }
            "textDocument/didChange" => {
                let params: lsp_types::DidChangeTextDocumentParams =
                    serde_json::from_value(notification.params)?;
                self.change_document(&params);
                Ok(true)
            }
            "textDocument/didSave" => {
                let params: lsp_types::DidSaveTextDocumentParams =
                    serde_json::from_value(notification.params)?;
                self.save_document(&params);
                Ok(true)
            }
            "textDocument/didClose" => {
                let params: lsp_types::DidCloseTextDocumentParams =
                    serde_json::from_value(notification.params)?;
                self.close_document(&params);
                Ok(true)
            }
            "workspace/didChangeConfiguration" => {
                // Project config files and settings may have changed on
                // disk; re-check every open document with fresh state.
                eprintln!("[codespell-lsp] configuration changed, re-checking documents");
                self.refresh_skip();
                let uris: Vec<Url> = self.docs.keys().cloned().collect();
                for uri in uris {
                    self.check_document(&uri);
                }
                Ok(true)
            }
            _ => Ok(true),
        }
    }

    fn handle_response(&mut self, response: Response) {
        if response.id != RequestId::from(1) {
            return;
        }
        let Some(result) = response.result else {
            return;
        };
        let values = match result.as_array() {
            Some(values) => values,
            None => std::slice::from_ref(&result),
        };
        let before = self.settings.clone();
        for value in values {
            config::merge_settings(&mut self.settings, value);
        }
        if self.settings != before {
            eprintln!("[codespell-lsp] applied workspace settings");
            self.refresh_skip();
            let uris: Vec<Url> = self.docs.keys().cloned().collect();
            for uri in uris {
                self.check_document(&uri);
            }
        }
    }

    fn open_document(&mut self, params: &lsp_types::DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let Ok(path) = uri.to_file_path() else {
            return;
        };
        self.docs.insert(
            uri.clone(),
            Document {
                version: params.text_document.version,
                text: params.text_document.text.clone(),
                path,
            },
        );
        self.check_document(&uri);
    }

    fn change_document(&mut self, params: &lsp_types::DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let Some(document) = self.docs.get_mut(&uri) else {
            return;
        };
        document.version = params.text_document.version;
        for change in &params.content_changes {
            // Full sync: every change carries the complete document text.
            document.text = change.text.clone();
        }

        if self.settings.run == RunMode::OnType {
            self.pending.insert(uri);
            self.deadline = Some(Instant::now() + DEBOUNCE);
        }
    }

    fn save_document(&mut self, params: &lsp_types::DidSaveTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        if let (Some(document), Some(text)) = (self.docs.get_mut(&uri), params.text.as_ref()) {
            document.text = text.clone();
        }
        // Saving re-checks in both run modes.
        self.check_document(&uri);
    }

    fn close_document(&mut self, params: &lsp_types::DidCloseTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        self.docs.remove(&uri);
        self.results.remove(&uri);
        self.pending.remove(&uri);
        self.publish_diagnostics(&uri, Vec::new(), None);
    }

    /// Run checks for documents with pending debounced changes.
    fn flush_pending(&mut self) {
        self.deadline = None;
        let pending = std::mem::take(&mut self.pending);
        for uri in pending {
            self.check_document(&uri);
        }
    }

    /// (Re)read the project `skip` configuration and compile the effective
    /// glob set. Called at startup and whenever settings or config files
    /// may have changed.
    fn refresh_skip(&mut self) {
        let root = self.roots.first().cloned().unwrap_or_default();
        self.config_skip = config::config_file_skip(&root, self.settings.config.as_deref());

        let mut builder = GlobSetBuilder::new();
        for pattern in self.settings.effective_skip(&self.config_skip) {
            if let Ok(glob) = Glob::new(&pattern) {
                builder.add(glob);
            }
        }
        self.skip_set = builder.build().ok().filter(|set| !set.is_empty());
    }

    fn is_skipped(&self, document: &Document) -> bool {
        let Some(globs) = &self.skip_set else {
            return false;
        };

        let path = &document.path;
        if globs.is_match(path) {
            return true;
        }
        if let Some(file_name) = path.file_name()
            && globs.is_match(file_name)
        {
            return true;
        }
        if let Some(root) = self.roots.iter().find(|root| path.starts_with(root))
            && let Ok(relative) = path.strip_prefix(root)
            && globs.is_match(relative)
        {
            return true;
        }
        false
    }

    fn check_document(&mut self, uri: &Url) {
        let Some(document) = self.docs.get(uri).cloned() else {
            return;
        };

        if self.is_skipped(&document) {
            self.results.remove(uri);
            self.publish_diagnostics(uri, Vec::new(), Some(document.version));
            return;
        }

        let root = config::root_for_document(&self.roots, &document.path).unwrap_or_else(|| {
            document
                .path
                .parent()
                .unwrap_or(Path::new("."))
                .to_path_buf()
        });

        match codespell::check_text(&document.text, &self.settings, &root) {
            CheckResult::Matches(matches) => {
                let resolved = self.resolve_matches(&document, &matches);
                self.results.insert(uri.clone(), resolved.clone());
                let diagnostics: Vec<Diagnostic> = resolved.iter().map(diagnostic_for).collect();
                self.publish_diagnostics(uri, diagnostics, Some(document.version));
            }
            CheckResult::Error(err) => {
                self.results.remove(uri);
                let message = error_message(&err);
                eprintln!("[codespell-lsp] {message}");
                let diagnostic = Diagnostic {
                    range: Range::new(Position::new(0, 0), Position::new(0, 0)),
                    severity: Some(DiagnosticSeverity::ERROR),
                    source: Some("codespell".to_string()),
                    message,
                    ..Default::default()
                };
                self.publish_diagnostics(uri, vec![diagnostic], Some(document.version));
            }
        }
    }

    /// Map `codespell` matches to LSP ranges. Each reported occurrence is
    /// walked in order so a word repeated on one line gets distinct ranges.
    fn resolve_matches(&self, document: &Document, matches: &[SpellMatch]) -> Vec<ResolvedMatch> {
        let mut occurrence_index: HashMap<(usize, String), usize> = HashMap::new();
        let mut resolved = Vec::new();
        let lines: Vec<&str> = document.text.lines().collect();

        for m in matches {
            // Guard against codespell reporting a line past the end of the
            // fed text (e.g. trailing newline handling differences).
            let Some(line_text) = lines.get(m.line) else {
                continue;
            };

            let key = (m.line, m.word.to_lowercase());
            let index = occurrence_index.entry(key).or_insert(0);
            let Some((start, end)) = nth_occurrence(line_text, &m.word, *index) else {
                continue;
            };
            *index += 1;

            let suggestions: Vec<String> = m
                .suggestions
                .as_deref()
                .map(|text| {
                    text.split(", ")
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();

            resolved.push(ResolvedMatch {
                range: Range::new(
                    Position::new(m.line as u32, byte_to_utf16(line_text, start) as u32),
                    Position::new(m.line as u32, byte_to_utf16(line_text, end) as u32),
                ),
                word: m.word.clone(),
                suggestions,
                reason: m.reason.clone(),
            });
        }

        resolved
    }

    fn publish_diagnostics(
        &mut self,
        uri: &Url,
        diagnostics: Vec<Diagnostic>,
        version: Option<i32>,
    ) {
        let params = PublishDiagnosticsParams {
            uri: uri.clone(),
            diagnostics,
            version,
        };
        let notification = Notification::new("textDocument/publishDiagnostics".to_string(), params);
        let _ = self
            .connection
            .sender
            .send(Message::Notification(notification));
    }

    fn code_actions(&self, params: &CodeActionParams) -> Vec<CodeActionOrCommand> {
        let Some(document) = self.docs.get(&params.text_document.uri) else {
            return Vec::new();
        };

        let mut actions = Vec::new();
        for diagnostic in &params.context.diagnostics {
            if diagnostic.source.as_deref() != Some("codespell") {
                continue;
            }
            let Some(data) = diagnostic
                .data
                .as_ref()
                .and_then(|d| serde_json::from_value::<CodeActionData>(d.clone()).ok())
            else {
                continue;
            };
            let word = data.word.clone().unwrap_or_default();
            if word.is_empty() {
                continue;
            }

            // 1. Replace with each individual suggestion. Codespell prints
            // comma-separated alternatives (e.g. "feel, field, file").
            for suggestion in &data.suggestions {
                let action = CodeAction {
                    title: format!("Fix spelling: \"{word}\" -> \"{suggestion}\""),
                    kind: Some(CodeActionKind::QUICKFIX),
                    diagnostics: Some(vec![diagnostic.clone()]),
                    edit: Some(WorkspaceEdit {
                        changes: Some(HashMap::from([(
                            params.text_document.uri.clone(),
                            vec![TextEdit {
                                range: diagnostic.range,
                                new_text: suggestion.clone(),
                            }],
                        )])),
                        ..Default::default()
                    }),
                    ..Default::default()
                };
                actions.push(CodeActionOrCommand::CodeAction(action));
            }

            // 2. Ignore for the remainder of the session (mirrors the
            // VS Code extension's "Add to ignore list").
            let ignore = CodeAction {
                title: format!("Ignore \"{word}\" (this session)"),
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: Some(vec![diagnostic.clone()]),
                command: Some(lsp_types::Command {
                    title: "Add to ignore list".to_string(),
                    command: COMMAND_ADD_TO_IGNORE_LIST.to_string(),
                    arguments: Some(vec![serde_json::json!(word)]),
                }),
                ..Default::default()
            };
            actions.push(CodeActionOrCommand::CodeAction(ignore));

            // 3. Add an inline ignore directive at the end of the line.
            let Some(line_text) = document
                .text
                .lines()
                .nth(diagnostic.range.start.line as usize)
            else {
                continue;
            };
            let insert_at = Position::new(
                diagnostic.range.start.line,
                byte_to_utf16(line_text, line_text.len()) as u32,
            );
            for (title_suffix, directive) in [
                ("on this line", format!("  # codespell:ignore {word}")),
                (
                    "on the next line",
                    format!("  # codespell:ignore-next-line {word}"),
                ),
            ] {
                let action = CodeAction {
                    title: format!("Ignore \"{word}\" {title_suffix}"),
                    kind: Some(CodeActionKind::QUICKFIX),
                    diagnostics: Some(vec![diagnostic.clone()]),
                    edit: Some(WorkspaceEdit {
                        changes: Some(HashMap::from([(
                            params.text_document.uri.clone(),
                            vec![TextEdit {
                                range: Range::new(insert_at, insert_at),
                                new_text: directive,
                            }],
                        )])),
                        ..Default::default()
                    }),
                    ..Default::default()
                };
                actions.push(CodeActionOrCommand::CodeAction(action));
            }
        }

        actions
    }

    fn hover(&self, params: &HoverParams) -> Option<Hover> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let m = self
            .results
            .get(uri)?
            .iter()
            .find(|m| range_contains(&m.range, &position))?;

        let suggestions = if m.suggestions.is_empty() {
            "no automatic fix".to_string()
        } else {
            m.suggestions.join(", ")
        };
        let mut body = format!("**codespell**: \"{}\" -> {suggestions}", m.word);
        if let Some(reason) = &m.reason {
            body.push_str(&format!(" ({reason})"));
        }

        Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: body,
            }),
            range: Some(m.range),
        })
    }

    fn execute_command(&mut self, params: &ExecuteCommandParams) {
        if params.command != COMMAND_ADD_TO_IGNORE_LIST {
            return;
        }
        for argument in &params.arguments {
            if let Some(word) = argument.as_str()
                && !self.settings.ignore_words.iter().any(|w| w == word)
            {
                self.settings.ignore_words.push(word.to_string());
                eprintln!("[codespell-lsp] ignoring \"{word}\" for this session");
            }
        }
        let uris: Vec<Url> = self.docs.keys().cloned().collect();
        for uri in uris {
            self.check_document(&uri);
        }
    }
}

/// Read messages until the client's `initialize` request arrives, ignoring
/// (and, for requests, erroring) anything a real client may send during
/// startup: `$/setTrace`, notifications, or responses. The strict
/// `lsp-server` helpers couple initialization to the `initialized`
/// notification, which real clients do not guarantee to send immediately.
fn wait_for_initialize(connection: &Connection) -> anyhow::Result<(RequestId, serde_json::Value)> {
    loop {
        match connection.receiver.recv() {
            Ok(Message::Request(request)) if request.method.as_str() == "initialize" => {
                return Ok((request.id, request.params));
            }
            Ok(Message::Request(request)) => {
                // Spec: respond with ServerNotInitialized before init.
                let response = Response::new_err(
                    request.id,
                    lsp_server::ErrorCode::ServerNotInitialized as i32,
                    "Server not initialized".to_string(),
                );
                connection.sender.send(Message::Response(response)).ok();
            }
            Ok(message) => {
                let _ = message;
            }
            Err(_) => {
                return Err(anyhow::anyhow!("disconnected before initialize"));
            }
        }
    }
}

fn diagnostic_for(m: &ResolvedMatch) -> Diagnostic {
    let suggestion_text = if m.suggestions.is_empty() {
        m.reason
            .clone()
            .unwrap_or_else(|| "no suggestion".to_string())
    } else {
        m.suggestions.join(", ")
    };
    let mut message = format!(
        "Possible misspelling: \"{}\" -> \"{}\"",
        m.word, suggestion_text
    );
    if let Some(reason) = &m.reason {
        message.push_str(&format!(" ({reason})"));
    }

    Diagnostic {
        range: m.range,
        severity: Some(DiagnosticSeverity::WARNING),
        source: Some("codespell".to_string()),
        message,
        data: Some(serde_json::json!({
            "word": m.word,
            "suggestions": m.suggestions,
            "reason": m.reason,
        })),
        ..Default::default()
    }
}

fn error_message(err: &codespell::CheckError) -> String {
    match err {
        codespell::CheckError::MissingExecutable { path, err } => format!(
            "codespell: could not run \"{path}\": {err}. \
             Install it (e.g. `pip install codespell` or `pipx install codespell`) \
             or set `codespell.path` under lsp.codespell-lsp.settings."
        ),
        codespell::CheckError::Failed { exit_code, stderr } => {
            format!("codespell exited with code {exit_code}: {stderr}")
        }
    }
}

fn range_contains(range: &Range, position: &Position) -> bool {
    position.line >= range.start.line
        && position.line <= range.end.line
        && (position.line > range.start.line || position.character >= range.start.character)
        && (position.line < range.end.line || position.character <= range.end.character)
}

/// Whether `c` participates in a codespell word (`[\w\-'’]+`).
fn word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-' || c == '\'' || c == '\u{2019}'
}

/// Case-insensitive scan for the `n`-th occurrence of `word` in `line`
/// (0-based), bounded by word characters so a misspelling inside a larger
/// identifier is not matched twice. Returns the byte range.
fn nth_occurrence(line: &str, word: &str, n: usize) -> Option<(usize, usize)> {
    if word.is_empty() {
        return None;
    }
    let line_lower = line.to_lowercase();
    let word_lower = word.to_lowercase();

    let mut start = 0;
    let mut seen = 0;
    while let Some(relative) = line_lower[start..].find(&word_lower) {
        let at = start + relative;
        let end = at + word_lower.len();

        let boundary_before = at == 0 || !word_char(line[..at].chars().next_back()?);
        let boundary_after = end >= line.len() || !word_char(line[end..].chars().next()?);

        if boundary_before && boundary_after {
            if seen == n {
                return Some((at, end));
            }
            seen += 1;
        }
        start = at + 1;
    }

    None
}

/// Byte offset within `line` converted to UTF-16 code units, the unit Zed
/// uses for LSP positions.
fn byte_to_utf16(line: &str, byte: usize) -> usize {
    line[..byte].chars().map(|c| c.len_utf16()).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nth_occurrence() {
        assert_eq!(nth_occurrence("teh teh teh", "teh", 0), Some((0, 3)));
        assert_eq!(nth_occurrence("teh teh teh", "teh", 2), Some((8, 11)));
        assert_eq!(nth_occurrence("teh", "teh", 3), None);
        assert_eq!(nth_occurrence("Teh THE teh", "teh", 1), Some((8, 11)));
        // CRLF line endings: the trailing \r never shifts word offsets.
        assert_eq!(nth_occurrence("teh\r", "teh", 0), Some((0, 3)));
        assert_eq!(byte_to_utf16("teh\r", 3), 3);
    }

    #[test]
    fn test_nth_occurrence_respects_word_boundaries() {
        // "fiel" inside "warfiel" is not a standalone word.
        assert_eq!(nth_occurrence("warfiel", "fiel", 0), None);
        // Hyphens join words (`[\w\-'’]+`), so "fiel-fiel" is one token.
        assert_eq!(nth_occurrence("fiel-fiel", "fiel", 0), None);
        // Comma-separated words are distinct tokens.
        assert_eq!(nth_occurrence("fiel, fiel", "fiel", 0), Some((0, 4)));
        assert_eq!(nth_occurrence("fiel, fiel", "fiel", 1), Some((6, 10)));
        assert_eq!(
            nth_occurrence("under_score fiel", "fiel", 0),
            Some((12, 16))
        );
        // Apostrophe is part of the word, so a match inside "don't" does
        // not split the token.
        assert_eq!(nth_occurrence("don't teh", "teh", 0), Some((6, 9)));
    }

    #[test]
    fn test_byte_to_utf16() {
        // BMP and astral characters.
        assert_eq!(byte_to_utf16("ab\u{1F600}c", 0), 0);
        assert_eq!(byte_to_utf16("ab\u{1F600}c", 2), 2);
        assert_eq!(byte_to_utf16("ab\u{1F600}c", 6), 4); // emoji = 2 units
        assert_eq!(byte_to_utf16("abcdef", 3), 3);
    }

    #[test]
    fn test_word_char() {
        assert!(word_char('a'));
        assert!(word_char('_'));
        assert!(word_char('-'));
        assert!(word_char('\''));
        assert!(word_char('\u{2019}'));
        assert!(!word_char(' '));
        assert!(!word_char('.'));
    }

    #[test]
    fn test_range_contains() {
        let range = Range::new(Position::new(1, 2), Position::new(1, 5));
        assert!(range_contains(&range, &Position::new(1, 3)));
        assert!(range_contains(&range, &Position::new(1, 2)));
        assert!(!range_contains(&range, &Position::new(1, 6)));
        assert!(!range_contains(&range, &Position::new(2, 0)));
    }
}
