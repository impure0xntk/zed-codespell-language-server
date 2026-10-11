// SPDX-License-Identifier: Apache-2.0

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde_json::Value;
use zed_extension_api::{
    self as zed, Architecture, LanguageServerId, LanguageServerInstallationStatus, Os, Worktree,
    register_extension, settings::LspSettings,
};

const NAME: &str = "codespell-lsp";
const GITHUB_REPO: &str = "impure0xntk/zed-codespell-language-server";

/// Pure mapping from a platform to the release asset name. Kept separate
/// from [`CodespellExtension::asset_name`] so the mapping is testable on
/// hosts where `zed::current_platform` is not available.
fn asset_name_for(platform: Os, arch: Architecture) -> Result<(String, String), String> {
    let (os_name, file_ext) = match platform {
        Os::Mac => ("apple-darwin", "tar.gz"),
        Os::Linux => ("unknown-linux-musl", "tar.gz"),
        Os::Windows => ("pc-windows-msvc", "zip"),
    };

    let arch_name = match arch {
        Architecture::X8664 => "x86_64",
        Architecture::Aarch64 => "aarch64",
        Architecture::X86 => return Err("x86 (32-bit) architecture is not supported".into()),
    };

    let descriptor = format!("{arch_name}-{os_name}");
    let name = format!("{NAME}-{descriptor}.{file_ext}");

    Ok((name, descriptor))
}

fn binary_filename_for(platform: Os) -> String {
    let mut binary = String::from(NAME);
    if platform == Os::Windows {
        binary.push_str(".exe");
    }
    binary
}

struct CodespellExtension {
    cached_binary: Option<String>,
}

impl CodespellExtension {
    fn new() -> Self {
        Self {
            cached_binary: None,
        }
    }

    fn lsp_settings(&self, worktree: &Worktree) -> Result<LspSettings, String> {
        LspSettings::for_worktree(NAME, worktree)
            .map_err(|e| format!("Failed to read LSP settings for {NAME}: {e}"))
    }

    fn server_env(&self, worktree: &Worktree) -> Vec<(String, String)> {
        let mut env = Vec::new();

        // Preserve the environment provided by the worktree so the
        // underlying `codespell` executable can be found on PATH.
        for (key, value) in worktree.shell_env() {
            if !env.iter().any(|(existing, _)| existing == &key) {
                env.push((key, value));
            }
        }

        // In Zed, `lsp.<name>.binary` configures the language-server
        // process itself, so `binary.env` applies to the adapter. It
        // inherits to the `codespell` child process it spawns.
        if let Ok(settings) = self.lsp_settings(worktree)
            && let Some(binary) = settings.binary
            && let Some(overrides) = binary.env
        {
            for (key, value) in overrides {
                if let Some((_, existing)) = env.iter_mut().find(|(k, _)| k == &key) {
                    *existing = value;
                } else {
                    env.push((key, value));
                }
            }
        }

        env
    }

    fn server_args(&self, worktree: &Worktree) -> Vec<String> {
        let mut args = vec![
            "serve".to_string(),
            format!("--root={}", worktree.root_path()),
        ];

        // Extra `binary.arguments` are appended after the defaults.
        if let Ok(settings) = self.lsp_settings(worktree)
            && let Some(binary) = settings.binary
            && let Some(arguments) = binary.arguments
        {
            args.extend(arguments);
        }

        args
    }

    fn initialization_options(&self, worktree: &Worktree) -> Option<Value> {
        self.lsp_settings(worktree)
            .ok()
            .and_then(|settings| settings.initialization_options)
    }

    fn workspace_configuration(&self, worktree: &Worktree) -> Option<Value> {
        self.lsp_settings(worktree)
            .ok()
            .and_then(|settings| settings.settings)
    }

    fn get_binary(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<String, String> {
        // Note: `lsp.codespell-lsp.binary.path` is intentionally NOT used
        // here. In Zed, setting `binary.path` makes Zed spawn that exact
        // executable as the language server, bypassing this extension; only
        // real LSP binaries belong there. The `codespell` CLI is configured
        // through `settings.codespell.path` and spawned by the adapter.
        //
        // 1. Prefer a development binary checked into the extension
        //    directory (see `make dev`).
        if let Some(path) = self.find_development_binary() {
            return Ok(path);
        }

        // 2. Prefer a system-managed binary, e.g. one installed by Nix.
        if let Some(path) = worktree.which(NAME) {
            return Ok(path);
        }

        // 3. Reuse the binary resolved by a previous call.
        if let Some(path) = self.cached_binary.as_deref()
            && Path::new(path).is_file()
            && self.binary_runs(path)
        {
            return Ok(path.to_owned());
        }

        // 4. Download a prebuilt binary from GitHub releases.
        let path = self.install_binary(language_server_id)?;
        self.cached_binary = Some(path.clone());
        Ok(path)
    }

    fn find_development_binary(&self) -> Option<String> {
        let mut path = PathBuf::from(NAME);
        if zed::current_platform().0 == Os::Windows {
            path.set_extension("exe");
        }
        if path.is_file() && self.binary_runs(path.to_str().unwrap_or(NAME)) {
            Some(path.to_string_lossy().into_owned())
        } else {
            None
        }
    }

    fn binary_runs(&self, path: &str) -> bool {
        let mut command = zed::process::Command::new(path.to_string());
        match command.output() {
            Ok(output) => output.status.unwrap_or(-1) == 0,
            Err(_) => false,
        }
    }

    fn install_binary(&mut self, language_server_id: &LanguageServerId) -> Result<String, String> {
        zed::set_language_server_installation_status(
            language_server_id,
            &LanguageServerInstallationStatus::CheckingForUpdate,
        );

        let release = zed::latest_github_release(
            GITHUB_REPO,
            zed::GithubReleaseOptions {
                require_assets: true,
                pre_release: false,
            },
        )?;

        let (asset_name, _descriptor) = self.asset_name()?;
        let asset = release
            .assets
            .iter()
            .find(|asset| asset.name == asset_name)
            .ok_or_else(|| {
                format!(
                    "{NAME} release {} does not contain asset {asset_name}",
                    release.version
                )
            })?;

        let (platform, _) = zed::current_platform();
        let version_dir = PathBuf::from(format!("{NAME}-{}", release.version));
        let binary_path = version_dir.join(self.binary_filename());

        // Reuse an existing installation of this exact version.
        if binary_path.is_file() && self.binary_runs(binary_path.to_str().unwrap_or(NAME)) {
            let binary_path_str = binary_path.to_str().ok_or("Invalid binary path")?;
            zed::make_file_executable(binary_path_str)?;
            self.cleanup_old_versions(&version_dir)?;
            return Ok(binary_path.to_string_lossy().into_owned());
        }
        if binary_path.is_file() {
            // The cached download exists but cannot execute on this system
            // (e.g. an ELF interpreter mismatch). Drop it and re-download.
            let _ = fs::remove_dir_all(&version_dir);
        }

        zed::set_language_server_installation_status(
            language_server_id,
            &LanguageServerInstallationStatus::Downloading,
        );

        zed::download_file(
            &asset.download_url,
            &version_dir.to_string_lossy(),
            if platform == Os::Windows {
                zed::DownloadedFileType::Zip
            } else {
                zed::DownloadedFileType::GzipTar
            },
        )?;

        if !binary_path.is_file() {
            // Remove only our own incomplete version directory. Never delete
            // unrelated entries from the extension directory.
            let _ = fs::remove_dir_all(&version_dir);
            return Err(format!(
                "Downloaded {asset_name} but could not find the expected executable in {}",
                version_dir.display()
            ));
        }

        zed::make_file_executable(binary_path.to_str().ok_or("Invalid binary path")?)?;
        self.cleanup_old_versions(&version_dir)?;

        Ok(binary_path.to_string_lossy().into_owned())
    }

    fn binary_filename(&self) -> String {
        binary_filename_for(zed::current_platform().0)
    }

    /// Map the current platform to the asset published by the release
    /// workflow. Returns the asset file name and a human-readable descriptor
    /// for error messages.
    fn asset_name(&self) -> Result<(String, String), String> {
        let (platform, arch) = zed::current_platform();
        asset_name_for(platform, arch)
    }

    /// Remove versioned install directories other than the current one so the
    /// extension directory does not accumulate stale binaries.
    fn cleanup_old_versions(&self, current_version_dir: &Path) -> Result<(), String> {
        let current_name = current_version_dir
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "Invalid version directory name".to_string())?;

        let entries = fs::read_dir(".").map_err(|e| format!("Failed to read directory: {e}"))?;

        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };

            if name.starts_with(&format!("{NAME}-"))
                && name != current_name
                && entry.path().is_dir()
                && let Err(e) = fs::remove_dir_all(entry.path())
            {
                // Log but do not fail the install on cleanup errors.
                eprintln!("Warning: failed to remove old version directory {name}: {e}");
            }
        }

        Ok(())
    }
}

impl zed::Extension for CodespellExtension {
    fn new() -> Self {
        Self::new()
    }

    fn language_server_command(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<zed::Command, String> {
        if language_server_id.as_ref() != NAME {
            return Err(format!(
                "Unknown language server: {}",
                language_server_id.as_ref()
            ));
        }

        let command = self.get_binary(language_server_id, worktree)?;
        let args = self.server_args(worktree);
        let env = self.server_env(worktree);

        Ok(zed::Command { command, args, env })
    }

    fn language_server_initialization_options(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<Option<Value>, String> {
        if language_server_id.as_ref() != NAME {
            return Ok(None);
        }

        Ok(self.initialization_options(worktree))
    }

    fn language_server_workspace_configuration(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<Option<Value>, String> {
        if language_server_id.as_ref() != NAME {
            return Ok(None);
        }

        Ok(self.workspace_configuration(worktree))
    }
}

register_extension!(CodespellExtension);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_has_no_cached_binary() {
        let extension = CodespellExtension::new();
        assert!(extension.cached_binary.is_none());
    }

    #[test]
    fn test_binary_filename_platform_suffix() {
        assert_eq!(binary_filename_for(Os::Linux), "codespell-lsp");
        assert_eq!(binary_filename_for(Os::Mac), "codespell-lsp");
        assert_eq!(binary_filename_for(Os::Windows), "codespell-lsp.exe");
    }

    #[test]
    fn test_asset_name_descriptors() {
        let expected = [
            (
                Os::Mac,
                Architecture::Aarch64,
                "codespell-lsp-aarch64-apple-darwin.tar.gz",
            ),
            (
                Os::Mac,
                Architecture::X8664,
                "codespell-lsp-x86_64-apple-darwin.tar.gz",
            ),
            (
                Os::Linux,
                Architecture::Aarch64,
                "codespell-lsp-aarch64-unknown-linux-musl.tar.gz",
            ),
            (
                Os::Linux,
                Architecture::X8664,
                "codespell-lsp-x86_64-unknown-linux-musl.tar.gz",
            ),
            (
                Os::Windows,
                Architecture::Aarch64,
                "codespell-lsp-aarch64-pc-windows-msvc.zip",
            ),
            (
                Os::Windows,
                Architecture::X8664,
                "codespell-lsp-x86_64-pc-windows-msvc.zip",
            ),
        ];

        for (platform, arch, expected_name) in expected {
            let (name, descriptor) =
                asset_name_for(platform, arch).expect("supported platform should map");
            assert_eq!(&name, expected_name, "asset name mismatch for {descriptor}");
        }
    }

    #[test]
    fn test_asset_name_unsupported_platforms() {
        assert!(
            asset_name_for(Os::Linux, Architecture::X86).is_err(),
            "32-bit x86 is not supported"
        );
    }
}
