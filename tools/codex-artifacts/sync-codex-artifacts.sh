#!/usr/bin/env bash
set -euo pipefail

# Release-side entrypoint only. Customer installers read the published
# codex-artifacts/latest.json and must never execute this synchronizer.
script_dir="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
exec python3 "$script_dir/sync_codex_artifacts.py" "$@"
