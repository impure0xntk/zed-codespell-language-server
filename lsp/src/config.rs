// SPDX-License-Identifier: Apache-2.0
//
//! Configuration for the codespell language server.
//!
//! Settings are merged from, in increasing order of precedence:
//!   1. hard-coded defaults
//!   2. codespell project config files (`.codespellrc`, `setup.cfg`,
//!      `pyproject.toml`) read from the workspace root. Only the `skip`
//!      option is consumed here: every other option is applied by the
//!      `codespell` executable itself when it is spawned with its working
//!      directory set to the project root.
//!   3. LSP settings, received through `initializationOptions` and/or
//!      `workspace/configuration`. The standard Zed `lsp.<name>.binary`
//!      block (path / arguments / env) configures the wrapped `codespell`
//!      executable; the remaining options mirror the `codespell.*` settings
//!      of the companion VS Code extension.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunMode {
    /// Re-check on every edit (debounced).
    OnType,
    /// Re-check only when the document is saved or opened.
    #[default]
    OnSave,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Settings {
    /// Path to the `codespell` executable. When unset the executable is
    /// resolved through `PATH`.
    pub path: Option<String>,
    /// Extra arguments appended to every `codespell` invocation.
    pub args: Vec<String>,
    /// Extra environment variables for the `codespell` process.
    pub env: Vec<(String, String)>,
    /// When to re-check documents.
    pub run: RunMode,
    /// Replacements for `--ignore-words-list` (`-L`).
    pub ignore_words: Vec<String>,
    /// Replacement for `--ignore-words` (`-I`).
    pub ignore_words_file: Option<String>,
    /// Globs of files that must not be checked (`--skip` / `-S`). Applied
    /// both by `codespell` and server-side (stdin checks carry no filename,
    /// so `codespell` cannot apply them itself).
    pub skip: Vec<String>,
    /// Replacement for `--config`.
    pub config: Option<String>,
    /// Replacements for `--dictionary` (`-D`).
    pub dictionary: Vec<String>,
    /// Replacement for `--builtin`.
    pub builtin: Option<String>,
    /// Replacement for `--ignore-regex`.
    pub ignore_regex: Option<String>,
    /// Replacement for `--ignore-sic` (ignore `[sic]` markers).
    pub ignore_sic: bool,
    /// Replacement for `--uri-ignore-words-list`.
    pub uri_ignore_words: Vec<String>,
}

impl Settings {
    /// Effective skip globs: LSP settings first (they override the project
    /// config file), falling back to the config file.
    pub fn effective_skip(&self, config_file_skip: &[String]) -> Vec<String> {
        if self.skip.is_empty() {
            config_file_skip.to_vec()
        } else {
            self.skip.clone()
        }
    }
}

/// Parse an LSP settings object. Both `{"codespell": {...}}` and a flat
/// object whose keys are the setting names are accepted.
#[cfg(test)]
pub fn settings_from_json(value: &Value) -> Settings {
    let mut settings = Settings::default();
    merge_settings(&mut settings, value);
    settings
}

/// Overlay the setting fields present in `value` onto `base`. Fields absent
/// from `value` are left untouched, which makes repeated application of
/// several config sources (initialization options, workspace configuration)
/// order-independent at the field level.
pub fn merge_settings(base: &mut Settings, value: &Value) {
    let obj = match value {
        Value::Object(map) => map,
        _ => return,
    };

    let codespell = obj.get("codespell").and_then(|v| v.as_object());
    let map: &serde_json::Map<String, Value> = codespell.unwrap_or(obj);

    // The standard Zed `lsp.<name>.binary` block configures the wrapped
    // `codespell` executable.
    if let Some(binary) = obj.get("binary").and_then(Value::as_object) {
        if let Some(v) = binary.get("path").and_then(Value::as_str) {
            base.path = Some(v.to_string());
        }
        if let Some(v) = binary.get("arguments").and_then(Value::as_array) {
            base.args = v
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
        }
        if let Some(v) = binary.get("env").and_then(Value::as_object) {
            base.env = v
                .iter()
                .map(|(key, value)| (key.clone(), value.as_str().unwrap_or_default().to_string()))
                .collect();
        }
    }

    // Legacy fallback for users of the VS Code-compatible keys:
    // `codespell.path` and `codespell.args` apply when the standard
    // `binary` block above is absent.
    if base.path.is_none()
        && let Some(v) = map.get("path").and_then(Value::as_str)
    {
        base.path = Some(v.to_string());
    }
    if base.args.is_empty()
        && let Some(v) = map.get("args").and_then(Value::as_array)
    {
        base.args = v
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
    }

    // The remaining options live in the `codespell` namespace (or flat).
    if let Some(v) = map.get("run").and_then(Value::as_str) {
        base.run = if v == "onType" {
            RunMode::OnType
        } else {
            RunMode::OnSave
        };
    }
    if let Some(v) = map.get("ignoreWords").and_then(Value::as_array) {
        base.ignore_words = v
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
    }
    if let Some(v) = map.get("ignoreWordsFile").and_then(Value::as_str) {
        base.ignore_words_file = Some(v.to_string());
    }
    if let Some(v) = map.get("skip").and_then(Value::as_array) {
        base.skip = v
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
    }
    if let Some(v) = map.get("config").and_then(Value::as_str) {
        base.config = Some(v.to_string());
    }
    if let Some(v) = map.get("dictionary").and_then(Value::as_array) {
        base.dictionary = v
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
    }
    if let Some(v) = map.get("builtin").and_then(Value::as_str) {
        base.builtin = Some(v.to_string());
    }
    if let Some(v) = map.get("ignoreRegex").and_then(Value::as_str) {
        base.ignore_regex = Some(v.to_string());
    }
    if let Some(v) = map.get("ignoreSic").and_then(Value::as_bool) {
        base.ignore_sic = v;
    }
    if let Some(v) = map.get("uriIgnoreWords").and_then(Value::as_array) {
        base.uri_ignore_words = v
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
    }
}

/// Extract the codespell `skip` option from the project config files that
/// `codespell` itself would read: `pyproject.toml` (`[tool.codespell]`),
/// `.codespellrc` and `setup.cfg` (`[codespell]`).
///
/// The remaining options are intentionally not parsed here; `codespell`
/// applies them when spawned with the project root as its working
/// directory. `skip` is the only option that cannot be applied by
/// `codespell` for stdin checks, so it must be mirrored server-side.
pub fn config_file_skip(root: &Path, explicit_config: Option<&str>) -> Vec<String> {
    let mut skip = Vec::new();

    if let Some(config) = explicit_config {
        let path = if Path::new(config).is_absolute() {
            PathBuf::from(config)
        } else {
            root.join(config)
        };
        if let Ok(content) = std::fs::read_to_string(&path) {
            let is_toml = path
                .file_name()
                .is_some_and(|name| name == "pyproject.toml")
                || path.extension().is_some_and(|ext| ext == "toml");
            let parsed = if is_toml {
                toml_skip(&content)
            } else {
                ini_skip(&content, "codespell")
            };
            extend_skip(&mut skip, parsed);
        }
        return skip;
    }

    let pyproject = root.join("pyproject.toml");
    if pyproject.is_file()
        && let Ok(content) = std::fs::read_to_string(&pyproject)
    {
        extend_skip(&mut skip, toml_skip(&content));
    }

    for name in [".codespellrc", "setup.cfg"] {
        let path = root.join(name);
        if path.is_file()
            && let Ok(content) = std::fs::read_to_string(&path)
        {
            extend_skip(&mut skip, ini_skip(&content, "codespell"));
        }
    }

    skip
}

fn extend_skip(skip: &mut Vec<String>, parsed: Option<Vec<String>>) {
    if let Some(parsed) = parsed {
        for value in parsed {
            if !skip.contains(&value) {
                skip.push(value);
            }
        }
    }
}

fn toml_skip(content: &str) -> Option<Vec<String>> {
    let value: toml::Value = toml::from_str(content).ok()?;
    let codespell = value.get("tool")?.get("codespell")?;
    parse_skip_value(codespell.get("skip")?)
}

fn parse_skip_value(value: &toml::Value) -> Option<Vec<String>> {
    match value {
        toml::Value::String(s) => Some(split_skip_list(s)),
        toml::Value::Array(items) => Some(
            items
                .iter()
                .filter_map(|item| item.as_str())
                .flat_map(split_skip_list)
                .collect(),
        ),
        _ => None,
    }
}

/// Parse the `[section]` of a ConfigParser-style INI file (`.codespellrc`,
/// `setup.cfg`). Handles `key = value` pairs, `;`/`#` comments, and
/// continuation lines (indented lines that belong to the previous key).
fn ini_skip(content: &str, wanted: &str) -> Option<Vec<String>> {
    let section = parse_ini_section(content, wanted)?;
    section.get("skip").map(|value| split_skip_list(value))
}

fn parse_ini_section(content: &str, wanted: &str) -> Option<HashMap<String, String>> {
    let mut current_section: Option<String> = None;
    let mut pairs: HashMap<String, String> = HashMap::new();
    let mut last_key: Option<String> = None;

    for raw_line in content.lines() {
        let line = raw_line.trim_end();

        if line.trim_start().is_empty() {
            continue;
        }

        // Continuation lines: indented and not a key/value pair.
        if line.starts_with([' ', '\t'])
            && current_section.as_deref() == Some(wanted)
            && let Some(key) = last_key.clone()
        {
            if let Some(existing) = pairs.get_mut(&key) {
                existing.push('\n');
                existing.push_str(line.trim());
            }
            continue;
        }

        let trimmed = line.trim_start();

        // Comment.
        if trimmed.starts_with(['#', ';']) {
            continue;
        }

        // Section header.
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            current_section = Some(trimmed[1..trimmed.len() - 1].to_string());
            last_key = None;
            continue;
        }

        let Some((key, value)) = trimmed.split_once(['=', ':']) else {
            last_key = None;
            continue;
        };

        let key = key.trim().to_string();
        let value = value.trim().to_string();

        if current_section.as_deref() == Some(wanted) {
            pairs.insert(key.clone(), value);
            last_key = Some(key);
        } else {
            last_key = None;
        }
    }

    if pairs.is_empty() { None } else { Some(pairs) }
}

/// codespell flattens comma-separated values and drops empty entries.
fn split_skip_list(value: &str) -> Vec<String> {
    value
        .split([',', '\n'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

/// Root directory used as the working directory for `codespell` and as the
/// base for config file discovery.
pub fn root_for_document(roots: &[PathBuf], document_path: &Path) -> Option<PathBuf> {
    if let Some(root) = roots.iter().find(|root| document_path.starts_with(root)) {
        return Some(root.clone());
    }
    roots.first().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(text: &str) -> Value {
        serde_json::from_str(text).expect("valid JSON in test")
    }

    #[test]
    fn test_settings_flat_shape() {
        let value = json(r#"{"ignoreWords":["teh"],"skip":["*.md"],"run":"onType"}"#);
        let settings = settings_from_json(&value);
        assert_eq!(settings.ignore_words, vec!["teh"]);
        assert_eq!(settings.skip, vec!["*.md"]);
        assert_eq!(settings.run, RunMode::OnType);
    }

    #[test]
    fn test_settings_nested_shape() {
        let value = json(r#"{"codespell":{"ignoreSic":true,"builtin":"clear,rare,code"}}"#);
        let settings = settings_from_json(&value);
        assert!(settings.ignore_sic);
        assert_eq!(settings.builtin.as_deref(), Some("clear,rare,code"));
    }

    #[test]
    fn test_skip_list_splitting() {
        assert_eq!(split_skip_list("*.eps, *.txt"), vec!["*.eps", "*.txt"]);
        assert_eq!(split_skip_list("a\n b \n, c"), vec!["a", "b", "c"]);
        assert_eq!(split_skip_list(" , "), Vec::<String>::new());
    }

    #[test]
    fn test_ini_skip_parsing() {
        let content = "\
# comment
[some-other-section]
skip = nope

[codespell]
skip = *.mjs, *.min.js
ignore-words-list = teh
";
        // codespell itself applies ignore-words-list; we only surface skip.
        let skip = ini_skip(content, "codespell").expect("section found");
        assert_eq!(skip, vec!["*.mjs", "*.min.js"]);
    }

    #[test]
    fn test_ini_multiline_skip() {
        let content = "\
[codespell]
skip =
    *.min.js
    vendor/*
";
        let skip = ini_skip(content, "codespell").expect("section found");
        assert_eq!(skip, vec!["*.min.js", "vendor/*"]);
    }

    #[test]
    fn test_toml_skip_parsing() {
        let content = r#"
[tool.codespell]
skip = ["*.min.js", "vendor/*"]
"#;
        let skip = toml_skip(content).expect("toml parsed");
        assert_eq!(skip, vec!["*.min.js", "vendor/*"]);
    }

    #[test]
    fn test_toml_skip_string() {
        let content = r#"
[tool.codespell]
skip = "*.md, docs/*"
"#;
        let skip = toml_skip(content).expect("toml parsed");
        assert_eq!(skip, vec!["*.md", "docs/*"]);
    }

    #[test]
    fn test_config_file_skip_reads_from_disk() {
        let dir =
            std::env::temp_dir().join(format!("codespell-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create dir");

        std::fs::write(
            dir.join("pyproject.toml"),
            "[tool.codespell]\nskip = [\"*.min.js\", \"vendor/*\"]\n",
        )
        .expect("write pyproject.toml");
        std::fs::write(dir.join(".codespellrc"), "[codespell]\nskip = *.md\n")
            .expect("write .codespellrc");

        let skip = config_file_skip(&dir, None);
        // Merged from both files, deduplicated, sorted by file order.
        assert_eq!(skip, vec!["*.min.js", "vendor/*", "*.md"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_config_file_skip_explicit_config_wins() {
        let dir =
            std::env::temp_dir().join(format!("codespell-config-explicit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create dir");
        std::fs::write(
            dir.join("pyproject.toml"),
            "[tool.codespell]\nskip = [\"*.min.js\"]\n",
        )
        .expect("write pyproject.toml");
        std::fs::write(dir.join("custom.cfg"), "[codespell]\nskip = *.digests\n")
            .expect("write custom.cfg");

        // Auto-discovery still sees the pyproject skip...
        let skip = config_file_skip(&dir, None);
        assert_eq!(skip, vec!["*.min.js"]);

        // ...but an explicit --config path replaces discovery entirely.
        let skip = config_file_skip(&dir, Some("custom.cfg"));
        assert_eq!(skip, vec!["*.digests"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_effective_skip_settings_override_config() {
        let mut settings = Settings::default();
        assert_eq!(settings.effective_skip(&["*.md".to_string()]), vec!["*.md"]);

        settings.skip = vec!["*.txt".to_string()];
        assert_eq!(
            settings.effective_skip(&["*.md".to_string()]),
            vec!["*.txt"]
        );
    }

    #[test]
    fn test_binary_block_parsing() {
        let value = json(
            r#"{
                "binary": {
                    "path": "/opt/codespell/bin/codespell",
                    "arguments": ["--builtin=clear,rare,code", "--quiet-level=2"],
                    "env": { "FOO": "bar" }
                },
                "codespell": { "run": "onType" }
            }"#,
        );
        let settings = settings_from_json(&value);
        assert_eq!(
            settings.path.as_deref(),
            Some("/opt/codespell/bin/codespell")
        );
        assert_eq!(
            settings.args,
            vec!["--builtin=clear,rare,code", "--quiet-level=2"]
        );
        assert!(
            settings
                .env
                .contains(&("FOO".to_string(), "bar".to_string()))
        );
        assert_eq!(settings.run, RunMode::OnType);
    }

    #[test]
    fn test_codespell_namespace_path_args_fallback() {
        // `codespell.path` / `codespell.args` remain supported (VS Code
        // parity) when the standard `binary` block is absent...
        let value = json(r#"{"codespell": { "path": "/legacy", "args": ["--quiet-level=2"] } }"#);
        let settings = settings_from_json(&value);
        assert_eq!(settings.path.as_deref(), Some("/legacy"));
        assert_eq!(settings.args, vec!["--quiet-level=2"]);

        // ...but the standard `binary` block wins when both are present.
        let value =
            json(r#"{ "binary": { "path": "/standard" }, "codespell": { "path": "/legacy" } }"#);
        let settings = settings_from_json(&value);
        assert_eq!(settings.path.as_deref(), Some("/standard"));
    }

    #[test]
    fn test_merge_settings_precedence() {
        let base = settings_from_json(&json(r#"{"ignoreWords":["teh"]}"#));
        let override_value = json(r#"{"codespell":{"ignoreWords":["teh","recieve"]}}"#);
        let mut base = base;
        merge_settings(&mut base, &override_value);
        assert_eq!(base.ignore_words, vec!["teh", "recieve"]);
        // Fields absent from the override keep their previous value.
        assert_eq!(base.run, RunMode::OnSave);
    }

    #[test]
    fn test_root_for_document() {
        let roots = vec![PathBuf::from("/proj/a"), PathBuf::from("/proj/b")];
        let doc = Path::new("/proj/b/src/main.rs");
        assert_eq!(
            root_for_document(&roots, doc),
            Some(PathBuf::from("/proj/b"))
        );
        let outside = Path::new("/elsewhere/x.txt");
        assert_eq!(
            root_for_document(&roots, outside),
            Some(PathBuf::from("/proj/a"))
        );
    }
}
