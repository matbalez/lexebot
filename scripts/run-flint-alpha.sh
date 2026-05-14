#!/usr/bin/env bash
set -euo pipefail

CONFIG_FILE="${HOME}/.config/lexebot/flint-alpha.env"

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

[ -f "$CONFIG_FILE" ] || fail "missing ${CONFIG_FILE}; run install-flint-alpha.sh first"

# shellcheck disable=SC1090
. "$CONFIG_FILE"

: "${SPROUT_RELAY_URL:?missing SPROUT_RELAY_URL in config}"
: "${SPROUT_CHANNEL_ID:?missing SPROUT_CHANNEL_ID in config}"
: "${SPROUT_OWNER_PRIVATE_KEY:?missing SPROUT_OWNER_PRIVATE_KEY in config}"
: "${SPROUT_BOT_PRIVATE_KEY:?missing SPROUT_BOT_PRIVATE_KEY in config}"
: "${SPROUT_BOT_AUTH_MODE:?missing SPROUT_BOT_AUTH_MODE in config}"
: "${LEXE_CLIENT_CREDENTIALS:?missing LEXE_CLIENT_CREDENTIALS in config}"
: "${LEXEBOT_BIN:?missing LEXEBOT_BIN in config}"

[ -x "$LEXEBOT_BIN" ] || fail "LexeBot binary is not executable at ${LEXEBOT_BIN}"

export SPROUT_RELAY_URL
export SPROUT_CHANNEL_ID
export SPROUT_OWNER_PRIVATE_KEY
export SPROUT_BOT_PRIVATE_KEY
export SPROUT_BOT_AUTH_MODE
export LEXEBOT_OWNER_DISPLAY_NAME
export LEXE_CLIENT_CREDENTIALS

exec "$LEXEBOT_BIN"
