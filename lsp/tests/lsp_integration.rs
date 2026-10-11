// SPDX-License-Identifier: Apache-2.0

//! End-to-end LSP test: spawn the compiled `codespell-lsp` binary and
//! drive it over stdio like a language client would. The real `codespell`
//! executable must be on `PATH`; the test skips itself when it is missing.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use lsp_types::Url;
use serde_json::{Value, json};

const CODESPELL_BIN: &str = env!("CARGO_BIN_EXE_codespell-lsp");

struct LspClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl LspClient {
    fn start(root: &Path) -> Self {
        let mut child = Command::new(CODESPELL_BIN)
            .arg(format!("--root={}", root.display()))
            .arg("serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn codespell-lsp");

        let stdin = child.stdin.take().expect("child stdin");
        let stdout = child.stdout.take().expect("child stdout");
        LspClient {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 0,
        }
    }

    fn send(&mut self, value: &Value) {
        let body = serde_json::to_vec(value).expect("serialize message");
        write!(self.stdin, "Content-Length: {}\r\n\r\n", body.len()).expect("write header");
        self.stdin.write_all(&body).expect("write body");
        self.stdin.flush().expect("flush");
    }

    fn recv(&mut self) -> Value {
        let mut header = String::new();
        loop {
            let mut line = String::new();
            self.stdout.read_line(&mut line).expect("read header line");
            if line == "\r\n" || line == "\n" {
                break;
            }
            header.push_str(&line);
        }

        let length = header
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .expect("Content-Length header");

        let mut body = vec![0u8; length];
        self.stdout.read_exact(&mut body).expect("read body");
        serde_json::from_slice(&body).expect("parse message")
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }));
    }

    /// Send a request, answering any interleaved server requests with null,
    /// and return the response body for `method`.
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));

        loop {
            let message = self.recv();
            if message.get("id").and_then(Value::as_i64) == Some(id) {
                return message.get("result").cloned().unwrap_or(Value::Null);
            }
            if message.get("method").is_some() && message.get("id").is_some() {
                // Interleaved server request (e.g. workspace/configuration).
                let request_id = message.get("id").cloned().unwrap_or(Value::Null);
                self.send(&json!({
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "result": null,
                }));
            }
        }
    }

    /// Read messages until a `textDocument/publishDiagnostics`
    /// notification for `uri` arrives, returning the full params so callers
    /// can inspect both the diagnostics and the document version.
    fn await_diagnostics(&mut self, uri: &str) -> Value {
        loop {
            let message = self.recv();
            if message.get("method").is_some() && message.get("id").is_some() {
                // Interleaved server request; answer with null and continue.
                let request_id = message.get("id").cloned().unwrap_or(Value::Null);
                self.send(&json!({
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "result": null,
                }));
                continue;
            }
            if message.get("method").and_then(Value::as_str)
                == Some("textDocument/publishDiagnostics")
            {
                let params = &message["params"];
                if params.get("uri").and_then(Value::as_str) == Some(uri) {
                    return params.clone();
                }
            }
        }
    }

    fn shutdown_and_exit(mut self) {
        self.request("shutdown", Value::Null);
        self.notify("exit", Value::Null);
        let status = self.child.wait().expect("wait for child");
        assert!(status.success(), "codespell-lsp exited with {status}");
    }
}

fn codespell_available() -> bool {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path).any(|dir| dir.join("codespell").is_file())
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("codespell-lsp-test-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn file_uri(path: &Path) -> String {
    Url::from_file_path(path).expect("file uri").to_string()
}

#[test]
fn lsp_diagnostics_code_actions_and_hover() {
    if !codespell_available() {
        eprintln!("skipping: `codespell` not found on PATH");
        return;
    }

    let root = temp_dir("roundtrip");
    let uri = file_uri(&root.join("sample.txt"));

    let mut client = LspClient::start(&root);

    // initialize
    let response = client.request(
        "initialize",
        json!({
            "processId": null,
            "rootUri": file_uri(&root),
            "capabilities": {},
            "initializationOptions": {
                "codespell": { "run": "onType", "skip": ["*.txt"] }
            }
        }),
    );
    assert!(
        response["capabilities"]["codeActionProvider"].is_boolean()
            || response["capabilities"]["codeActionProvider"].is_object(),
        "code actions advertised"
    );
    // Real clients interleave tracing notifications with `initialized`;
    // the server must not require `initialized` to be the very next
    // message after the initialize response.
    client.notify("$/setTrace", json!({}));
    client.notify("initialized", json!({}));

    // A .txt file is skipped by the settings and produces no diagnostics.
    client.notify(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "plaintext",
                "version": 1,
                "text": "teh quikc\n",
            }
        }),
    );
    assert_eq!(
        client.await_diagnostics(&uri)["diagnostics"],
        json!([]),
        "skipped file must not produce diagnostics"
    );

    // A .rs file is checked and reports both misspellings.
    let rs_uri = file_uri(&root.join("main.rs"));
    client.notify(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": rs_uri,
                "languageId": "rust",
                "version": 1,
                "text": "teh quikc brown fox\nfiel on line two\n",
            }
        }),
    );
    let params = client.await_diagnostics(&rs_uri);
    assert_eq!(params["version"], 1);
    let diagnostics = params["diagnostics"].as_array().expect("diagnostics array");
    assert_eq!(diagnostics.len(), 3, "diagnostics: {diagnostics:?}");

    let teh = diagnostics
        .iter()
        .find(|d| {
            d["message"]
                .as_str()
                .is_some_and(|m| m.starts_with("Possible misspelling: \"teh\""))
        })
        .expect("teh diagnostic");
    assert_eq!(teh["range"]["start"]["line"], 0);
    assert_eq!(teh["range"]["start"]["character"], 0);
    assert_eq!(teh["range"]["end"]["character"], 3);
    assert_eq!(teh["source"], "codespell");
    assert_eq!(teh["data"]["word"], "teh");
    assert_eq!(teh["data"]["suggestions"], json!(["the"]));

    let fiel = diagnostics
        .iter()
        .find(|d| {
            d["message"]
                .as_str()
                .is_some_and(|m| m.starts_with("Possible misspelling: \"fiel\""))
        })
        .expect("fiel diagnostic");
    assert_eq!(fiel["range"]["start"]["line"], 1);
    assert_eq!(
        fiel["data"]["suggestions"],
        json!(["feel", "field", "file", "phial"])
    );

    // Code actions: one fix per suggestion plus ignore actions.
    let actions = client.request(
        "textDocument/codeAction",
        json!({
            "textDocument": { "uri": rs_uri },
            "range": teh["range"],
            "context": { "diagnostics": [teh] },
        }),
    );
    let actions = actions.as_array().expect("actions array");
    assert!(
        actions
            .iter()
            .any(|a| a["title"].as_str() == Some("Fix spelling: \"teh\" -> \"the\""))
    );
    assert!(
        actions
            .iter()
            .any(|a| a["title"].as_str() == Some("Ignore \"teh\" (this session)"))
    );
    assert!(
        actions
            .iter()
            .any(|a| a["title"].as_str() == Some("Ignore \"teh\" on this line"))
    );
    assert!(
        actions
            .iter()
            .any(|a| a["title"].as_str() == Some("Ignore \"teh\" on the next line"))
    );

    // "fiel" reports four comma-separated suggestions; each becomes its own
    // quick-fix action.
    let fiel_actions = client.request(
        "textDocument/codeAction",
        json!({
            "textDocument": { "uri": rs_uri },
            "range": fiel["range"],
            "context": { "diagnostics": [fiel] },
        }),
    );
    let fiel_fixes = fiel_actions
        .as_array()
        .expect("actions array")
        .iter()
        .filter(|a| {
            a["title"]
                .as_str()
                .is_some_and(|t| t.starts_with("Fix spelling: \"fiel\""))
        })
        .count();
    assert_eq!(fiel_fixes, 4, "one fix action per suggestion");

    // Hover over the misspelling shows the suggestion.
    let hover = client.request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": rs_uri },
            "position": { "line": 0, "character": 1 },
        }),
    );
    assert_eq!(hover["contents"]["value"], "**codespell**: \"teh\" -> the");

    // Full-sync edits re-check after the onType debounce. Replacing "teh"
    // with "the" leaves the two other misspellings behind.
    client.notify(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": rs_uri, "version": 2 },
            "contentChanges": [{
                "text": "the quikc brown fox\nfiel on line two\n",
            }],
        }),
    );
    let params = client.await_diagnostics(&rs_uri);
    assert_eq!(params["version"], 2, "publish reflects the edited revision");
    let diagnostics = params["diagnostics"].as_array().expect("diagnostics array");
    assert_eq!(diagnostics.len(), 2, "after edit: {diagnostics:?}");
    assert!(diagnostics.iter().all(|d| {
        d["message"]
            .as_str()
            .is_some_and(|m| !m.starts_with("Possible misspelling: \"teh\""))
    }));

    // Rapid consecutive edits coalesce into a single debounced re-check, so
    // the diagnostics must reflect the final revision, not an earlier one.
    for (version, text) in [
        (3, "the quikc brown fox\nfiel on line two\nfiel again\n"),
        (4, "the quikc brown fox\nfiel on line two\n"),
        (5, "the quikc brown fox\nfiel\n"),
    ] {
        client.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": rs_uri, "version": version },
                "contentChanges": [{ "text": text }],
            }),
        );
    }
    let params = client.await_diagnostics(&rs_uri);
    assert_eq!(
        params["version"], 5,
        "final revision checked after coalescing"
    );
    let diagnostics = params["diagnostics"].as_array().expect("diagnostics array");
    assert_eq!(
        diagnostics.len(),
        2,
        "final revision checked: {diagnostics:?}"
    );
    assert!(diagnostics.iter().all(|d| {
        d["message"]
            .as_str()
            .is_some_and(|m| !m.starts_with("Possible misspelling: \"teh\""))
    }));

    // The session ignore command re-checks and suppresses the word.
    client.request(
        "workspace/executeCommand",
        json!({
            "command": "codespell.addToIgnoreList",
            "arguments": ["quikc"],
        }),
    );
    let params = client.await_diagnostics(&rs_uri);
    assert_eq!(params["version"], 5);
    let diagnostics = params["diagnostics"].as_array().expect("diagnostics array");
    assert_eq!(diagnostics.len(), 1, "after ignore: {diagnostics:?}");
    assert!(
        diagnostics[0]["message"]
            .as_str()
            .is_some_and(|m| m.starts_with("Possible misspelling: \"fiel\""))
    );

    // workspace/didChangeConfiguration re-checks all open documents.
    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": {} }),
    );
    let params = client.await_diagnostics(&rs_uri);
    assert_eq!(params["version"], 5);
    assert_eq!(
        params["diagnostics"]
            .as_array()
            .expect("diagnostics array")
            .len(),
        1,
        "the ignored word stays ignored after a config refresh"
    );

    // didClose clears the diagnostics for the document.
    client.notify(
        "textDocument/didClose",
        json!({ "textDocument": { "uri": rs_uri } }),
    );
    let params = client.await_diagnostics(&rs_uri);
    assert_eq!(
        params["diagnostics"],
        json!([]),
        "diagnostics cleared on close"
    );

    client.shutdown_and_exit();
}

#[test]
fn lsp_reports_missing_codespell_as_error() {
    let root = temp_dir("missing");
    let uri = file_uri(&root.join("a.md"));

    let mut client = LspClient::start(&root);
    client.request(
        "initialize",
        json!({
            "processId": null,
            "rootUri": file_uri(&root),
            "capabilities": {},
            "initializationOptions": {
                "codespell": { "path": "/nonexistent/codespell" }
            }
        }),
    );
    // Real clients interleave tracing notifications with `initialized`;
    // the server must not require `initialized` to be the very next
    // message after the initialize response.
    client.notify("$/setTrace", json!({}));
    client.notify("initialized", json!({}));

    client.notify(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "markdown",
                "version": 1,
                "text": "teh\n",
            }
        }),
    );
    let params = client.await_diagnostics(&uri);
    assert_eq!(params["version"], 1);
    let diagnostics = params["diagnostics"].as_array().expect("diagnostics array");
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0]["severity"], 1, "error severity");
    assert!(
        diagnostics[0]["message"]
            .as_str()
            .unwrap()
            .contains("could not run"),
        "message: {diagnostics:?}"
    );

    client.shutdown_and_exit();
}
