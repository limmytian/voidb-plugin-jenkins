# VoidB Jenkins Plugin (`voidb-plugin-jenkins`)

[![License](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)

Independent process plugin for [VoidB](https://github.com/limmytian/voidb) to connect, inspect, and trigger jobs on Jenkins CI/CD automation servers.

## Features

- **Autonomous Process Architecture**: Runs in an isolated OS process communicating with VoidB via `stdio-jsonrpc`.
- **Tri-Modal Operation**:
  - `test`: Automated connectivity verification.
  - `tui`: Standalone full-screen interactive terminal interface.
  - `serve`: `stdio-jsonrpc` protocol server for AI Agents and automated orchestration.
  - Domain commands: Rich CI/CD job querying and console streaming (`jobs`, `console`).
- **Capability Surface**:
  - `diagnostics`: Secret-safe diagnostic verification without opening remote sockets.
  - `jobs`: List jobs and multi-branch folders.
  - `job_detail`: Retrieve job metadata and recent build runs.
  - `activity`: Snapshot currently running builds and queued build items.
  - `console`: Stream and view bounded build console logs.
  - `pipeline`: Inspect declarative and scripted pipeline stages.
  - `trigger_build`: Trigger build jobs with optional parameters.
  - `abort_build`: Abort active build executions.
  - `cancel_queue_item`: Cancel queued builds.

## Usage & Operational Modes

When installed, `voidb-cli` automatically discovers this plugin and provides seamless transparent dispatch:

### 1. Human Operations (CLI & TUI)

```bash
# Connectivity test
voidb-cli jenkins test --profile my-jenkins

# Launch standalone interactive fullscreen TUI
voidb-cli jenkins tui --profile my-jenkins

# Inspect jobs and view build console logs
voidb-cli jenkins jobs --profile my-jenkins
voidb-cli jenkins console "build-job" 42 --profile my-jenkins
```

### 2. Machine Communication (AI Agents & JSON-RPC)

```bash
# Run the stdio-jsonrpc server directly or via voidb-cli
voidb-cli jenkins serve
# Or directly run the binary:
./bin/voidb-plugin-jenkins serve
```

## Quick Start

### Installation

Place this plugin directory or a packaged release archive under your VoidB plugins directory (`~/Library/Application Support/voidb/plugins/jenkins` on macOS, or `~/.local/share/voidb/plugins/jenkins` on Linux):

```bash
mkdir -p "$HOME/Library/Application Support/voidb/plugins/jenkins"
cp -r plugin.toml bin schemas "$HOME/Library/Application Support/voidb/plugins/jenkins/"
```

Verify discovery via `voidb`:

```bash
voidb-cli plugin list
voidb-cli jenkins --help
```

### Development & Build

```bash
cargo build --release
mkdir -p bin
cp target/release/voidb-plugin-jenkins bin/
```

## Protocol Specifications

Complies with the [VoidB Process Plugin Protocol](https://github.com/limmytian/voidb/blob/main/docs/quickstart-process-plugin.md) specification (v1.0).

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE) for details.

## Optional local SDK development

Release and CI builds resolve SDK/core from the immutable Git revision in Cargo.toml. To test against a sibling `../voidb` checkout, explicitly opt in with `cargo --config .cargo/config.local.example.toml test --all-targets`. Do not commit an active `.cargo/config.toml` path override.

## Authenticated releases

Starting with v0.3.4, managed installation in VoidB v0.3.5 requires a signed
`provenance.json` covering identity, target, executable, manifest and schemas.
Build jobs produce unsigned staging archives. The separate release signing job
signs all four native targets in the protected `plugin-release-signing` environment
and validates the complete payload before publishing a new immutable release.
The owner must configure environment protection and its signing secret before
publication. Existing v0.3.3 release assets are never replaced.

`VOIDB_PLUGIN_SIGNING_KEY_PEM` belongs only to the protected signing step. Do not
put private keys in this repository, packages, command arguments or logs. Core
contains the trusted public key; registry fields and editable installation records
cannot add runtime trust. Local unsigned development requires both an explicit
`VOIDB_PLUGIN_PATH` directory and `VOIDB_ALLOW_UNSIGNED_PLUGINS=1`.
