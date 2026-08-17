#!/usr/bin/env bash
# Build and install mem8, stopping a running server first.
#
# Windows holds an exclusive lock on a running executable, so `cargo install`
# fails with "Access is denied (os error 5)" whenever an MCP client has the
# server open — which is most of the time, since that is the normal state.
# Stopping it first is the whole reason this script exists.
#
# Usage:
#   ./install.sh            build and install the binary
#   ./install.sh --plugin   also reinstall the Claude Code plugin

set -euo pipefail

cd "$(dirname "$0")"

stop_running_server() {
  case "$(uname -s)" in
    MINGW* | MSYS* | CYGWIN*)
      # //F and //IM are the Git Bash escaping for taskkill's /F and /IM.
      taskkill //IM mem8.exe //F >/dev/null 2>&1 || true
      ;;
    *)
      pkill -f '^mem8 serve' >/dev/null 2>&1 || true
      ;;
  esac
}

echo "Stopping any running mem8 server..."
stop_running_server
sleep 1

echo "Installing the binary..."
cargo install --path . --force

if [[ "${1:-}" == "--plugin" ]]; then
  if ! command -v claude >/dev/null 2>&1; then
    echo "The claude CLI is not on PATH; skipping the plugin." >&2
  else
    echo "Reinstalling the Claude Code plugin..."
    claude plugin uninstall mem8@mem8 >/dev/null 2>&1 || true
    claude plugin marketplace add ./ >/dev/null 2>&1 || true
    claude plugin install mem8@mem8
  fi
fi

echo
echo "Installed: $(mem8 --version)"
echo "Restart Claude Code to pick up the new binary."
