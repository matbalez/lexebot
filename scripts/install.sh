#!/usr/bin/env bash
set -euo pipefail

SCRIPT_SOURCE="${BASH_SOURCE[0]-}"
SCRIPT_DIR=""
if [ -n "$SCRIPT_SOURCE" ] && [ "$SCRIPT_SOURCE" != "bash" ] && [ "$SCRIPT_SOURCE" != "-" ]; then
  SCRIPT_DIR="$(cd "$(dirname "$SCRIPT_SOURCE")" >/dev/null 2>&1 && pwd || true)"
fi

if [ -n "$SCRIPT_DIR" ] && [ -f "${SCRIPT_DIR}/install-flint-alpha.sh" ]; then
  exec "${SCRIPT_DIR}/install-flint-alpha.sh" "$@"
fi

exec bash <(curl -fsSL https://raw.githubusercontent.com/matbalez/lexebot/main/scripts/install-flint-alpha.sh) "$@"
