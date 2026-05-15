#!/usr/bin/env bash
set -euo pipefail

RELAY_HTTP_URL_DEFAULT="https://sprout.up.railway.app"
SPROUT_REPO_DIR="${HOME}/.cache/lexebot/sprout"
SPROUT_CLI_PATH="${SPROUT_CLI_PATH:-}"
SPROUT_IDENTITY_KEY="${HOME}/Library/Application Support/xyz.block.sprout.app/identity.key"
LEGACY_SPROUT_IDENTITY_KEY="${HOME}/Library/Application Support/com.wesb.sprout/identity.key"

say() {
  printf '%s\n' "$*" >&2
}

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || fail "missing required command: $1"
}

trim() {
  awk '{$1=$1; print}'
}

read_secret_file() {
  local path="$1"
  [ -f "$path" ] || return 1
  LC_ALL=C tr -d '\r\n' <"$path" | trim
}

find_owner_key_file() {
  if [ -f "$SPROUT_IDENTITY_KEY" ]; then
    printf '%s\n' "$SPROUT_IDENTITY_KEY"
    return 0
  fi
  if [ -f "$LEGACY_SPROUT_IDENTITY_KEY" ]; then
    printf '%s\n' "$LEGACY_SPROUT_IDENTITY_KEY"
    return 0
  fi
  return 1
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

ensure_sprout_cli() {
  local sprout_cli

  if sprout_cli="$(find_sprout_cli)"; then
    say "Using Sprout CLI at ${sprout_cli}"
    printf '%s\n' "$sprout_cli"
    return 0
  fi

  need_cmd cargo
  need_cmd git
  say "No existing Sprout CLI was found. Building installer-managed Sprout CLI as a last resort."
  mkdir -p "$(dirname "$SPROUT_REPO_DIR")"
  if [ ! -d "$SPROUT_REPO_DIR/.git" ]; then
    git clone --depth 1 https://github.com/block/sprout.git "$SPROUT_REPO_DIR"
  fi
  (
    cd "$SPROUT_REPO_DIR"
    cargo build -q -p sprout-cli
  )

  sprout_cli="${SPROUT_REPO_DIR}/target/debug/sprout"
  is_sprout_cli "$sprout_cli" || fail "built Sprout CLI, but ${sprout_cli} is not executable"
  printf '%s\n' "$sprout_cli"
}

usage() {
  cat >&2 <<EOF
Usage:
  lexebot-list-channels

Lists Sprout channels visible to your local Sprout identity so you can copy a
channel UUID for lexebot-add-channel.
EOF
}

format_channel_list() {
  if ! command -v python3 >/dev/null 2>&1; then
    cat
    return 0
  fi

  python3 -c '
import json
import sys

data = json.load(sys.stdin)
ids = []
for event in data:
    for tag in event.get("tags", []):
        if len(tag) >= 2 and tag[0] == "d":
            ids.append(tag[1])
            break

if not ids:
    print("No member channels found.")
    sys.exit(0)

print("Channel UUID")
for channel_id in ids:
    print(channel_id)
'
}

list_channels() {
  local sprout_cli="$1"
  local owner_key="$2"
  local relay_http_url="${SPROUT_HTTP_RELAY_URL:-$RELAY_HTTP_URL_DEFAULT}"
  local output

  if "$sprout_cli" list-channels --help >/dev/null 2>&1; then
    output="$(SPROUT_PRIVATE_KEY="$owner_key" "$sprout_cli" --relay "$relay_http_url" list-channels --member)"
    printf '%s\n' "$output" | format_channel_list
    return 0
  fi

  if "$sprout_cli" channels list --help >/dev/null 2>&1; then
    output="$(SPROUT_PRIVATE_KEY="$owner_key" "$sprout_cli" --relay "$relay_http_url" channels list --member)"
    printf '%s\n' "$output" | format_channel_list
    return 0
  fi

  fail "Sprout CLI at ${sprout_cli} does not support listing channels"
}

main() {
  if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
    usage
    exit 0
  fi
  [ "$#" -eq 0 ] || {
    usage
    exit 1
  }

  local sprout_cli owner_key_file owner_key
  sprout_cli="$(ensure_sprout_cli)"
  owner_key_file="$(find_owner_key_file)" || fail "Sprout identity key not found. Launch Sprout once, then rerun lexebot-list-channels."
  owner_key="$(read_secret_file "$owner_key_file")" || fail "could not read ${owner_key_file}"
  [ -n "$owner_key" ] || fail "Sprout identity key is empty"
  list_channels "$sprout_cli" "$owner_key"
}

main "$@"
