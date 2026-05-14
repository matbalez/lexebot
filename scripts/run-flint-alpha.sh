#!/usr/bin/env bash
set -euo pipefail

CONFIG_FILE="${HOME}/.config/lexebot/flint-alpha.env"

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

trim() {
  awk '{$1=$1; print}'
}

[ -f "$CONFIG_FILE" ] || fail "missing ${CONFIG_FILE}; run install-flint-alpha.sh first"

# shellcheck disable=SC1090
. "$CONFIG_FILE"

: "${SPROUT_RELAY_URL:?missing SPROUT_RELAY_URL in config}"
: "${SPROUT_CHANNEL_ID:?missing SPROUT_CHANNEL_ID in config}"
: "${SPROUT_OWNER_PRIVATE_KEY_FILE:?missing SPROUT_OWNER_PRIVATE_KEY_FILE in config}"
: "${LEXEBOT_BIN:?missing LEXEBOT_BIN in config}"
: "${KEYCHAIN_SERVICE:?missing KEYCHAIN_SERVICE in config}"

keychain_get() {
  security find-generic-password -a "$1" -s "$KEYCHAIN_SERVICE" -w 2>/dev/null || true
}

read_secret_file() {
  local path="$1"
  [ -f "$path" ] || return 1
  LC_ALL=C tr -d '\r\n' <"$path" | trim
}

[ -x "$LEXEBOT_BIN" ] || fail "LexeBot binary is not executable at ${LEXEBOT_BIN}"

SPROUT_BOT_PRIVATE_KEY="$(keychain_get bot-nsec)"
LEXE_CLIENT_CREDENTIALS="$(keychain_get lexe-client-credentials)"
SPROUT_OWNER_PRIVATE_KEY="$(read_secret_file "$SPROUT_OWNER_PRIVATE_KEY_FILE")" || fail "could not read ${SPROUT_OWNER_PRIVATE_KEY_FILE}"

[ -n "$SPROUT_BOT_PRIVATE_KEY" ] || fail "missing LexeBot bot key in Keychain; rerun installer"
[ -n "$LEXE_CLIENT_CREDENTIALS" ] || fail "missing Lexe SDK client credentials in Keychain; rerun installer"
[ -n "$SPROUT_OWNER_PRIVATE_KEY" ] || fail "Sprout owner identity key is empty"

export SPROUT_RELAY_URL
export SPROUT_CHANNEL_ID
export SPROUT_BOT_PRIVATE_KEY
export SPROUT_OWNER_PRIVATE_KEY
export SPROUT_BOT_AUTH_MODE="owner-attested"
export LEXE_CLIENT_CREDENTIALS

exec "$LEXEBOT_BIN"
