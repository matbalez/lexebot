#!/usr/bin/env bash
set -euo pipefail

CONFIG_FILE="${HOME}/.config/lexebot/lexebot.env"
LEGACY_CONFIG_FILE="${HOME}/.config/lexebot/flint-alpha.env"

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

if [ ! -f "$CONFIG_FILE" ] && [ -f "$LEGACY_CONFIG_FILE" ]; then
  CONFIG_FILE="$LEGACY_CONFIG_FILE"
fi

[ -f "$CONFIG_FILE" ] || fail "missing ${CONFIG_FILE}; run install.sh first"

# shellcheck disable=SC1090
. "$CONFIG_FILE"

if [ -z "${SPROUT_CHANNEL_IDS:-}" ] && [ -n "${SPROUT_CHANNEL_ID:-}" ]; then
  SPROUT_CHANNEL_IDS="$SPROUT_CHANNEL_ID"
fi

: "${SPROUT_RELAY_URL:?missing SPROUT_RELAY_URL in config}"
: "${SPROUT_CHANNEL_IDS:?missing SPROUT_CHANNEL_IDS in config}"
: "${SPROUT_OWNER_PRIVATE_KEY:?missing SPROUT_OWNER_PRIVATE_KEY in config}"
: "${SPROUT_BOT_PRIVATE_KEY:?missing SPROUT_BOT_PRIVATE_KEY in config}"
: "${SPROUT_BOT_AUTH_MODE:?missing SPROUT_BOT_AUTH_MODE in config}"
: "${LEXE_CLIENT_CREDENTIALS:?missing LEXE_CLIENT_CREDENTIALS in config}"
: "${LEXEBOT_BIN:?missing LEXEBOT_BIN in config}"

[ -x "$LEXEBOT_BIN" ] || fail "LexeBot binary is not executable at ${LEXEBOT_BIN}"

export SPROUT_RELAY_URL
export SPROUT_CHANNEL_IDS
export SPROUT_OWNER_PRIVATE_KEY
export SPROUT_BOT_PRIVATE_KEY
export SPROUT_BOT_AUTH_MODE
export LEXEBOT_OWNER_DISPLAY_NAME
export LEXE_CLIENT_CREDENTIALS

exec "$LEXEBOT_BIN"
