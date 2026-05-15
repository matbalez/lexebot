#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd)"
if [ -f "${SCRIPT_DIR}/install-flint-alpha.sh" ]; then
  exec "${SCRIPT_DIR}/install-flint-alpha.sh" "$@"
fi

exec bash <(curl -fsSL https://raw.githubusercontent.com/matbalez/lexebot/main/scripts/install-flint-alpha.sh) "$@"
