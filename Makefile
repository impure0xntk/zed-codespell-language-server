.PHONY: dev test build build-native check fmt clippy clean dict-update

# Default target: build the release WASM component + native adapter
all: build build-native

# Development: install as a dev extension in Zed
dev:
	@echo "To install as a dev extension:"
	@echo "  1. Open Zed"
	@echo "  2. Press Cmd+Shift+P (Ctrl+Shift+P on Linux/Windows)"
	@echo "  3. Type 'zed: install dev extension'"
	@echo "  4. Select directory: $(CURDIR)"
	@echo ""
	@echo "The extension needs the codespell-lsp adapter binary. For local"
	@echo "development, build it and copy it into this directory:"
	@echo "    make build-native && cp lsp/target/release/codespell-lsp ."
	@echo "The binary is resolved in this order: settings path -> ./codespell-lsp"
	@echo "-> PATH -> GitHub release download."
	@echo ""
	@echo "The adapter drives the real codespell executable. Install it with:"
	@echo "    pip install codespell   # or pipx/uvx"

# Run tests for both crates (requires codespell on PATH for integration tests)
test:
	cargo test
	cargo test --manifest-path lsp/Cargo.toml

# Build the extension host as a release WASM component (wasm32-wasip2)
build:
	cargo build --release --target wasm32-wasip2

# Build the codespell-lsp adapter natively (for local testing)
build-native:
	cargo build --release --manifest-path lsp/Cargo.toml

# Quality checks: format, clippy, test for both crates
check: fmt clippy test

fmt:
	cargo fmt --all -- --check
	cargo fmt --manifest-path lsp/Cargo.toml -- --check

clippy:
	cargo clippy --all-targets -- -D warnings
	cargo clippy --manifest-path lsp/Cargo.toml --all-targets -- -D warnings

# Clean build artifacts
clean:
	cargo clean
	cargo clean --manifest-path lsp/Cargo.toml
	rm -rf target/wasm32-wasip2

# Refresh the pinned dependency locks
update:
	cargo update
	cargo update --manifest-path lsp/Cargo.toml
