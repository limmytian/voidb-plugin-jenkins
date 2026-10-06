# VoidB Jenkins Plugin (`voidb-plugin-jenkins`)

[![License](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)

Independent process plugin for [VoidB](https://github.com/limmytian/voidb) to connect, inspect, and trigger jobs on Jenkins CI/CD automation servers.

## Features

- **Autonomous Process Architecture**: Runs in an isolated OS process communicating with VoidB via `stdio-jsonrpc`.
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
- **Dual Mode**: Can run as a JSON-RPC worker server (`voidb-plugin-jenkins serve`) or standalone interactive TUI.

## Quick Start

### Installation

Place this plugin directory or a packaged release archive under your VoidB plugins directory:

```bash
mkdir -p ~/.config/voidb/plugins/jenkins
cp -r plugin.toml bin schemas ~/.config/voidb/plugins/jenkins/
```

Verify discovery via `voidb`:

```bash
voidb-cli plugin list
voidb-cli plugin describe jenkins
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
