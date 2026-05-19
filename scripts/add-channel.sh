#!/usr/bin/env bash
set -euo pipefail

RELAY_HTTP_URL_DEFAULT="https://sprout.up.railway.app"
CONFIG_FILE="${HOME}/.config/lexebot/lexebot.env"
LEGACY_CONFIG_FILE="${HOME}/.config/lexebot/flint-alpha.env"
SPROUT_REPO_DIR="${HOME}/.cache/lexebot/sprout"
SPROUT_CLI_PATH="${SPROUT_CLI_PATH:-}"

say() {
  printf '%s\n' "$*" >&2
}

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

trim() {
  awk '{$1=$1; print}'
}

shell_quote() {
  printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\''/g")"
}

is_sprout_cli() {
  local candidate="$1"
  [ -n "$candidate" ] || return 1
  [ -x "$candidate" ] || return 1
  "$candidate" --help 2>/dev/null | grep -q "Sprout CLI"
}

find_sprout_cli() {
  local candidate resolved seen search_dir
  seen="|"

  if [ -n "$SPROUT_CLI_PATH" ] && is_sprout_cli "$SPROUT_CLI_PATH"; then
    printf '%s\n' "$SPROUT_CLI_PATH"
    return 0
  fi

  resolved="$(command -v sprout 2>/dev/null || true)"
  if [ -n "$resolved" ] && is_sprout_cli "$resolved"; then
    printf '%s\n' "$resolved"
    return 0
  fi

  for candidate in \
    "${HOME}/.local/bin/sprout" \
    "${HOME}/.cargo/bin/sprout" \
    "/opt/homebrew/bin/sprout" \
    "/usr/local/bin/sprout" \
    "${HOME}/.sprout/REPOS/sprout/target/release/sprout" \
    "${HOME}/.sprout/REPOS/sprout/target/debug/sprout" \
    "${SPROUT_REPO_DIR}/target/release/sprout" \
    "${SPROUT_REPO_DIR}/target/debug/sprout"
  do
    case "$seen" in
      *"|${candidate}|"*) continue ;;
    esac
    seen="${seen}${candidate}|"
    if is_sprout_cli "$candidate"; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done

  for search_dir in "${HOME}/.sprout" "${HOME}/.cache/lexebot" "${HOME}/.cache"; do
    [ -d "$search_dir" ] || continue
    while IFS= read -r candidate; do
      case "$seen" in
        *"|${candidate}|"*) continue ;;
      esac
      seen="${seen}${candidate}|"
      if is_sprout_cli "$candidate"; then
        printf '%s\n' "$candidate"
        return 0
      fi
    done <<EOF
$(find "$search_dir" -maxdepth 8 -type f -name sprout 2>/dev/null)
EOF
  done

  return 1
}

usage() {
  cat >&2 <<EOF
Usage:
  lexebot-add-channel <channel-uuid>

Adds the already-installed LexeBot identity to another Sprout channel when
needed, then updates ~/.config/lexebot/lexebot.env so run-lexebot listens there
too. For DMs, the bot is already a participant, so a failed add-member attempt
is tolerated and the config is still updated. If Sprout CLI is not available,
the config is still updated; this is sufficient for a DM that already includes
LexeBot.
EOF
}

load_config() {
  if [ ! -f "$CONFIG_FILE" ] && [ -f "$LEGACY_CONFIG_FILE" ]; then
    CONFIG_FILE="$LEGACY_CONFIG_FILE"
  fi
  [ -f "$CONFIG_FILE" ] || fail "missing ${CONFIG_FILE}; run install.sh first"

  # shellcheck disable=SC1090
  . "$CONFIG_FILE"

  if [ -z "${SPROUT_CHANNEL_IDS:-}" ] && [ -n "${SPROUT_CHANNEL_ID:-}" ]; then
    SPROUT_CHANNEL_IDS="$SPROUT_CHANNEL_ID"
  fi

  : "${SPROUT_OWNER_PRIVATE_KEY:?missing SPROUT_OWNER_PRIVATE_KEY in config}"
  : "${SPROUT_BOT_PUBKEY:?missing SPROUT_BOT_PUBKEY in config; rerun the LexeBot installer to upgrade config}"
}

update_channel_config() {
  local channel_id="$1"
  local current normalized tmp quoted_channels

  current="${SPROUT_CHANNEL_IDS:-}"
  normalized="$current"

  for existing in $current; do
    if [ "$existing" = "$channel_id" ]; then
      say "Channel ${channel_id} is already in ${CONFIG_FILE}."
      return 0
    fi
  done

  normalized="$(printf '%s %s' "$current" "$channel_id" | trim)"
  quoted_channels="$(shell_quote "$normalized")"
  tmp="${CONFIG_FILE}.tmp.$$"
  awk -v quoted_channels="$quoted_channels" '
    BEGIN { updated = 0 }
    /^SPROUT_CHANNEL_IDS=/ {
      print "SPROUT_CHANNEL_IDS=" quoted_channels
      updated = 1
      next
    }
    /^SPROUT_CHANNEL_ID=/ {
      if (!updated) {
        print "SPROUT_CHANNEL_IDS=" quoted_channels
        updated = 1
      }
      next
    }
    { print }
    END {
      if (!updated) {
        print "SPROUT_CHANNEL_IDS=" quoted_channels
      }
    }
  ' "$CONFIG_FILE" >"$tmp"
  chmod 600 "$tmp"
  mv "$tmp" "$CONFIG_FILE"
  say "Added channel ${channel_id} to ${CONFIG_FILE}."
}

sprout_add_bot_to_channel() {
  local sprout_cli="$1"
  local channel_id="$2"
  local relay_http_url="${SPROUT_HTTP_RELAY_URL:-$RELAY_HTTP_URL_DEFAULT}"

  say "Adding LexeBot pubkey ${SPROUT_BOT_PUBKEY} to channel ${channel_id} as role bot..."
  if "$sprout_cli" channels add-member --help >/dev/null 2>&1; then
    SPROUT_PRIVATE_KEY="$SPROUT_OWNER_PRIVATE_KEY" "$sprout_cli" \
      --relay "$relay_http_url" \
      channels add-member \
      --channel "$channel_id" \
      --pubkey "$SPROUT_BOT_PUBKEY" \
      --role bot
    return 0
  fi

  if "$sprout_cli" add-channel-member --help >/dev/null 2>&1; then
    SPROUT_PRIVATE_KEY="$SPROUT_OWNER_PRIVATE_KEY" "$sprout_cli" \
      --relay "$relay_http_url" \
      add-channel-member \
      --channel "$channel_id" \
      --pubkey "$SPROUT_BOT_PUBKEY" \
      --role bot
    return 0
  fi

  fail "Sprout CLI at ${sprout_cli} does not support adding channel members"
}

main() {
  if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
    usage
    exit 0
  fi
  [ "$#" -eq 1 ] || {
    usage
    exit 1
  }

  local channel_id="$1"
  [ -n "$channel_id" ] || fail "channel UUID is required"

  load_config
  local sprout_cli
  if sprout_cli="$(find_sprout_cli)"; then
    if ! sprout_add_bot_to_channel "$sprout_cli" "$channel_id"; then
      say "Could not add LexeBot as a normal channel member."
      say "Continuing because this is expected for a DM where LexeBot is already a participant."
    fi
  else
    say "Sprout CLI not found; skipping channel member add."
    say "This is OK for a DM where LexeBot is already a participant."
  fi
  update_channel_config "$channel_id"
  say "Done. Restart LexeBot so it subscribes to the new channel:"
  say "  run-lexebot"
}

main "$@"
