# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Cargo workspace with all eight crates
- `reeve-model`: all domain entities and signal types
- `reeve-storage`: hot tier (ring buffer) + warm tier (SQLite) interfaces
- `reeve-ingestion`: four-stage pipeline skeleton
- `reeve-engine`: evaluation + policy engine skeleton
- `reeve-renderer`: Ratatui cockpit skeleton with panels and widgets
- `reeve-intervention`: gRPC control channel skeleton
- `reeve-sdk`: Rust agent SDK skeleton with `checkpoint()` primitive
- GitHub Actions CI (fmt check, clippy, tests, release build)
- Issue templates (bug report, feature request)
- PR template
- ADRs: 0001 (two-channel architecture), 0002 (local-first LLM judge),
  0003 (Apache 2.0 license)
- SQLite schema migration (`migrations/0001_initial.sql`)
- gRPC protocol definition (`proto/reeve.proto`)
- Eight color themes (Catppuccin Mocha/Latte/Frappe/Macchiato, Dracula, Nord,
  Tokyo Night, Gruvbox)
- Python SDK skeleton with framework adapter stubs
