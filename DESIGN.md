# Zed Codespell Extension: Design Document

## 1. Overview

A [Zed](https://zed.dev) extension that brings the
[codespell](https://github.com/codespell-project/codespell) spell checker to
the editor as a language server. The extension follows the same shape as the
author's `zed-xml-language-server` (extension host in the repo root, native
language server fetched/launched by the host) and mirrors the feature set of
the companion [VS Code extension](https://github.com/impure0xntk/vscode-codespell-extension).

## 2. Key decision: a thin LSP adapter, not a Rust reimplementation

`codespell` is a batch CLI. As of v2.4.3 it has **no LSP mode**: it reads
files (or stdin) and prints `file:line: word ==> suggestion` lines; there is
no JSON-RPC/stdin server mode. The user's VS Code extension already exploits
this by spawning `codespell` per check.

Reimplementing codespell's checking engine (dictionary parsing, case fixing,
`[sic]` handling, ignore directives, URI handling, …) in Rust would duplicate
a large, living codebase and risk behavior drift. Instead, this repo contains:

- `codespell-lsp` (`lsp/` crate): a **thin adapter** that
  1. speaks LSP over stdio (via `lsp-server`),
  2. spawns the real `codespell` executable per document with the document
     text on stdin and `--stdin --stdin-single-line`,
  3. parses the `line: word ==> suggestion` output,
  4. publishes diagnostics and answers code-action/hover requests by mapping
     reported words back to LSP ranges in the document.
- the extension host (`src/lib.rs`, root crate): manages `codespell-lsp`
  the same way `codebook-zed` manages `codebook-lsp`
  (dev binary -> PATH -> GitHub release download).

All spelling semantics are therefore exactly those of the installed
`codespell` binary.

## 3. Components

```text
zed-codespell-language-server/
├── extension.toml          # extension manifest (language server wiring)
├── src/lib.rs              # extension host (WASM component)
├── lsp/                    # codespell-lsp adapter (native binary)
│   ├── src/
│   │   ├── main.rs         # CLI: `serve --root=<path>`, --version, --help
│   │   ├── config.rs       # LSP settings + codespell config file `skip`
│   │   ├── codespell.rs    # process spawn, output parsing, PATH lookup
│   │   └── server.rs       # LSP loop: diagnostics/codeAction/hover/commands
│   └── tests/lsp_integration.rs  # end-to-end JSON-RPC test over stdio
├── Makefile
├── flake.nix
└── .github/workflows/{ci,release}.yml
```

## 4. Extension host (`src/lib.rs`)

The adapter binary is managed automatically, like `codebook-lsp`. Resolution
order (each step is skipped when the previous one fails):

1. A development binary `./codespell-lsp` (also `codespell-lsp.exe` on
   Windows) in the extension directory — `make dev` documents copying the
   locally built adapter there.
2. `codespell-lsp` on `PATH`.
3. The cached binary from a previous lookup in this process.
4. Download from GitHub releases of this repository: assets named
   `codespell-lsp-<arch>-<os>.{tar.gz,zip}` are extracted into a
   versioned directory `codespell-lsp-<version>/` and older versions are
   removed.

`language_server_command` starts the adapter with `serve --root=<project>`;
`language_server_initialization_options` and
`language_server_workspace_configuration` forward the user's
`initializationOptions` / `settings` to the adapter. The standard Zed
`lsp.codespell-lsp.binary` block (`path`, `arguments`, `env`) configures the
wrapped `codespell` executable and is injected into both channels as a
`binary` object, so the adapter never needs to re-read Zed settings.

## 5. Adapter (`lsp/`)

### 5.1 LSP capabilities

- `textDocumentSync`: open/close, full sync, save.
- `codeActionProvider` + `executeCommandProvider`
  (`codespell.addToIgnoreList`).
- `hoverProvider`.

### 5.2 Checking pipeline

For each event (open/save immediately; edits debounced 300 ms in `onType`
mode) the adapter:

1. Applies `skip` globs server-side. `codespell` cannot apply `--skip` to
   stdin input (it has no filename), so the adapter matches the skip globs
   against the document path, its basename, and its path relative to the
   workspace root (using `globset`). The globs come from the LSP settings
   and, when the settings do not override them, from `.codespellrc` /
   `setup.cfg` / `pyproject.toml` (`[tool.codespell]`) — only the `skip`
   key is parsed there; everything else is applied by `codespell` itself.
2. Runs `codespell` with `--stdin --stdin-single-line`, plus flags mapped
   from the settings (`-L`, `-I`, `-S`, `-D`, `--builtin`, `--ignore-regex`,
   `--ignore-sic`, `--uri-ignore-words-list`, `--config` and user `args`),
   feeding the document text on stdin. The working directory is the
   workspace root, so codespell discovers project config files exactly as it
   does on the command line.
3. Parses `line: word ==> suggestion [| reason]` output. `--stdin-single-line`
   keeps the output unambiguous per occurrence (duplicates are reported per
   occurrence by codespell and reproduced as separate diagnostics).
4. Maps each word back to a precise LSP range by scanning the line for the
   n-th case-insensitive occurrence bounded by word characters
   (`[\w\-'’]`), converting byte offsets to UTF-16 code units.
5. Publishes `publishDiagnostics` (warning severity, source `codespell`,
   message `Possible misspelling: "word" -> "suggestion"`, machine-readable
   `data` for code actions).

Missing `codespell` executable or a usage/config error produces a single
error diagnostic that explains how to install it.

### 5.3 Code actions

Per diagnostic:

- **Fix spelling** — one action per suggestion (codespell often lists
  several: `fiel -> feel, field, file, phial`).
- **Ignore `<word>` (this session)** — runs `codespell.addToIgnoreList`,
  adding the word to `-L` for the server lifetime. Zed has no
  `settings.update` for LSP servers, so this session-scoped behavior is
  documented as such; persistent ignores belong in settings or a codespell
  config file.
- **Ignore `<word>` on this line / on the next line** — inserts the
  codespell directives `# codespell:ignore <word>` /
  `# codespell:ignore-next-line <word>` at the end of the line.

### 5.4 Hover

Hovering a reported word shows `**codespell**: "word" -> suggestions`.

### 5.5 Config sources and precedence

1. Defaults (`codespell` on PATH, run `onSave`, builtin dictionaries
   `clear,rare` decided by the codespell binary).
2. LSP settings: `initializationOptions` from the initialize request and the
   `workspace/configuration` response (which Zed answers from the extension
   host's `settings`), merged field by field, workspace configuration taking
   precedence. `binary.path` names the wrapped `codespell` executable and
   `binary.env` its environment (inherited from the adapter); `codespell.args`
   adds codespell CLI arguments (with `codespell.path` as a legacy fallback).
   The `codespell.*` settings namespace carries the checking options
   (`run`, `ignoreWords`, `skip`, `builtin`, `ignoreSic`, and friends).
3. codespell project config files (applied by the `codespell` process via its
   working directory; `skip` additionally mirrored server-side).

## 6. Grammar

No tree-sitter grammar is implemented. Neither `codebook` nor `typos` uses a
grammar for spell checking — they tokenize at the word level — and codespell
itself tokenizes with the regex `[\w\-'’]+`. Since the extension drives the
real codespell binary, tokenization is exactly codespell's.

## 7. Testing

- Unit tests: output parsing (including the `| reason` tail), word
  occurrence mapping, UTF-16 conversion, skip glob parsing (INI + TOML),
  settings merging, CLI parsing, asset naming.
- Integration tests (`lsp/tests/lsp_integration.rs`): spawn the compiled
  adapter and exercise the full LSP conversation over stdio — initialize,
  `workspace/configuration`, didOpen with `skip` settings, diagnostics
  contents and ranges, code actions, hover, missing-binary error path. The
  tests self-skip when `codespell` is not installed.

## 8. Known limitations / future work

- One `codespell` process spawn per check. Fine for interactive use with the
  debounce, but a persistent daemon (e.g. codespell's Python API, issue
  codespell#3419) would remove startup cost on huge files.
- `-w` / write-changes ("format on save") is not exposed; quick fixes cover
  the same use case one edit at a time.
- Session ignore list is not written back to workspace settings.
