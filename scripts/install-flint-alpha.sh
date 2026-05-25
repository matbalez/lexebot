#!/usr/bin/env bash
set -euo pipefail

LEGACY_DEFAULT_CHANNEL_ID="1df37399-3c25-4019-8bc7-faacd53587d0"
HOME_SURFACE_NAME="LexeBot DM"
DEFAULT_KUDOS_BOT_PUBKEY="84c220e52abb478dad3b96796383bea7dd989fb1540ea26b3478f706f78b7f8c"
RELAY_WS_URL="wss://sprout.up.railway.app"
RELAY_HTTP_URL="https://sprout.up.railway.app"
LEXEBOT_VERSION="v0.1.19"
LEXEBOT_ARCHIVE="lexebot-${LEXEBOT_VERSION}-aarch64-apple-darwin.tar.gz"
LEXEBOT_RELEASE_BASE="https://github.com/matbalez/lexebot/releases/download/${LEXEBOT_VERSION}"
RUNNER_URL="https://raw.githubusercontent.com/matbalez/lexebot/main/scripts/run.sh"
ADD_CHANNEL_URL="https://raw.githubusercontent.com/matbalez/lexebot/main/scripts/add-channel.sh"
LIST_CHANNELS_URL="https://raw.githubusercontent.com/matbalez/lexebot/main/scripts/list-channels.sh"

INSTALL_DIR="${HOME}/.local/bin"
LEXEBOT_BIN="${INSTALL_DIR}/lexebot"
RUNNER_BIN="${INSTALL_DIR}/run-lexebot"
ADD_CHANNEL_BIN="${INSTALL_DIR}/lexebot-add-channel"
LIST_CHANNELS_BIN="${INSTALL_DIR}/lexebot-list-channels"
LEGACY_RUNNER_BIN="${INSTALL_DIR}/run-lexebot-flint-alpha"
CONFIG_DIR="${HOME}/.config/lexebot"
CONFIG_FILE="${CONFIG_DIR}/lexebot.env"
LEGACY_CONFIG_FILE="${CONFIG_DIR}/flint-alpha.env"
SPROUT_IDENTITY_KEY="${HOME}/Library/Application Support/xyz.block.sprout.app/identity.key"
LEGACY_SPROUT_IDENTITY_KEY="${HOME}/Library/Application Support/com.wesb.sprout/identity.key"
SPROUT_REPO_DIR="${HOME}/.cache/lexebot/sprout"
SPROUT_CLI_PATH="${SPROUT_CLI_PATH:-}"
LEXEBOT_DOWNLOAD_TMPDIR=""
MANUAL_ADD=0

cleanup() {
  if [ -n "${LEXEBOT_DOWNLOAD_TMPDIR:-}" ] && [ -d "$LEXEBOT_DOWNLOAD_TMPDIR" ]; then
    rm -rf "$LEXEBOT_DOWNLOAD_TMPDIR"
  fi
}

trap cleanup EXIT

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

usage() {
  cat >&2 <<EOF
Usage:
  install.sh [--manual-add]

Options:
  --manual-add   Create/install the LexeBot identity and print its pubkey.
                 Do not open a LexeBot DM and do not start LexeBot.
EOF
}

parse_args() {
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --manual-add)
        MANUAL_ADD=1
        ;;
      -h|--help)
        usage
        exit 0
        ;;
      *)
        usage
        fail "unknown argument: $1"
        ;;
    esac
    shift
  done
}

start_command() {
  if command -v run-lexebot >/dev/null 2>&1; then
    printf 'run-lexebot\n'
  else
    printf 'bash %s\n' "$RUNNER_BIN"
  fi
}

add_channel_command() {
  if command -v lexebot-add-channel >/dev/null 2>&1; then
    printf 'lexebot-add-channel <channel-uuid>\n'
  else
    printf 'bash %s <channel-uuid>\n' "$ADD_CHANNEL_BIN"
  fi
}

list_channels_command() {
  if command -v lexebot-list-channels >/dev/null 2>&1; then
    printf 'lexebot-list-channels\n'
  else
    printf 'bash %s\n' "$LIST_CHANNELS_BIN"
  fi
}

print_start_command() {
  say "Future start command:"
  say "$(start_command)"
}

print_manual_add_instructions() {
  local bot_pubkey="$1"

  say
  say "Manual setup requested."
  say "LexeBot pubkey:"
  say
  say "  ${bot_pubkey}"
  say
  say "Auto-kudos does not require adding LexeBot to work channels."
  say "For manual wallet commands, open a Direct Message with the LexeBot pubkey above,"
  say "then configure that DM channel with:"
  say "  $(add_channel_command)"
  say
  say "After that, start LexeBot with:"
  say "  $(start_command)"
}

configured_bot_pubkey() {
  [ -f "$CONFIG_FILE" ] || return 1
  (
    # shellcheck disable=SC1090
    . "$CONFIG_FILE" 2>/dev/null
    printf '%s\n' "${SPROUT_BOT_PUBKEY:-}"
  ) | trim
}

configured_channel_ids() {
  [ -f "$CONFIG_FILE" ] || return 1
  (
    # shellcheck disable=SC1090
    . "$CONFIG_FILE" 2>/dev/null
    if [ -n "${SPROUT_CHANNEL_IDS:-}" ]; then
      printf '%s\n' "$SPROUT_CHANNEL_IDS"
    else
      printf '%s\n' "${SPROUT_CHANNEL_ID:-}"
    fi
  ) | trim
}

configured_owner_key() {
  [ -f "$CONFIG_FILE" ] || return 1
  (
    # shellcheck disable=SC1090
    . "$CONFIG_FILE" 2>/dev/null
    printf '%s\n' "${SPROUT_OWNER_PRIVATE_KEY:-}"
  ) | trim
}

configured_bot_private_key() {
  [ -f "$CONFIG_FILE" ] || return 1
  (
    # shellcheck disable=SC1090
    . "$CONFIG_FILE" 2>/dev/null
    printf '%s\n' "${SPROUT_BOT_PRIVATE_KEY:-}"
  ) | trim
}

lexebot_pid() {
  pgrep -f "$LEXEBOT_BIN" 2>/dev/null | head -n 1 || true
}

existing_install_present() {
  [ -x "$LEXEBOT_BIN" ] && [ -f "$CONFIG_FILE" ] && { [ -x "$RUNNER_BIN" ] || [ -x "$LEGACY_RUNNER_BIN" ]; }
}

installed_config_version() {
  [ -f "$CONFIG_FILE" ] || return 1
  (
    # shellcheck disable=SC1090
    . "$CONFIG_FILE"
    printf '%s\n' "${LEXEBOT_VERSION:-}"
  ) 2>/dev/null || true
}

update_config_version() {
  local tmp quoted_version
  tmp="${CONFIG_FILE}.tmp.$$"
  quoted_version="$(shell_quote "$LEXEBOT_VERSION")"
  awk -v quoted_version="$quoted_version" '
    BEGIN { updated = 0 }
    /^LEXEBOT_VERSION=/ {
      print "LEXEBOT_VERSION=" quoted_version
      updated = 1
      next
    }
    { print }
    END {
      if (!updated) {
        print "LEXEBOT_VERSION=" quoted_version
      }
    }
  ' "$CONFIG_FILE" >"$tmp"
  chmod 600 "$tmp"
  mv "$tmp" "$CONFIG_FILE"
}

migrate_legacy_config() {
  if [ -f "$CONFIG_FILE" ] || [ ! -f "$LEGACY_CONFIG_FILE" ]; then
    return 0
  fi

  mkdir -p "$CONFIG_DIR"
  cp "$LEGACY_CONFIG_FILE" "$CONFIG_FILE"
  chmod 600 "$CONFIG_FILE"
  normalize_config_channels
  say "Migrated ${LEGACY_CONFIG_FILE} to ${CONFIG_FILE}"
}

normalize_config_channels() {
  [ -f "$CONFIG_FILE" ] || return 0
  local existing_channel_ids existing_channel_id normalized channel_id quoted_channels tmp

  existing_channel_ids="$(
    # shellcheck disable=SC1090
    . "$CONFIG_FILE" 2>/dev/null
    printf '%s\n' "${SPROUT_CHANNEL_IDS:-}"
  )"
  existing_channel_id="$(
    # shellcheck disable=SC1090
    . "$CONFIG_FILE" 2>/dev/null
    printf '%s\n' "${SPROUT_CHANNEL_ID:-}"
  )"

  if [ -z "$existing_channel_ids" ] && [ -n "$existing_channel_id" ]; then
    existing_channel_ids="$existing_channel_id"
  fi

  normalized=""
  for channel_id in $existing_channel_ids; do
    [ "$channel_id" != "$LEGACY_DEFAULT_CHANNEL_ID" ] || continue
    case " $normalized " in
      *" $channel_id "*) ;;
      *) normalized="$(printf '%s %s' "$normalized" "$channel_id" | trim)" ;;
    esac
  done

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

  if [ "$existing_channel_ids" != "$normalized" ]; then
    say "Updated ${CONFIG_FILE} channel subscriptions for the local-only auto-kudos model."
  fi
}

ensure_config_bot_pubkey() {
  [ -f "$CONFIG_FILE" ] || return 0
  local existing_pubkey bot_nsec derived_pubkey quoted_pubkey tmp

  existing_pubkey="$(
    # shellcheck disable=SC1090
    . "$CONFIG_FILE" 2>/dev/null
    printf '%s\n' "${SPROUT_BOT_PUBKEY:-}"
  )"
  [ -z "$existing_pubkey" ] || return 0

  bot_nsec="$(
    # shellcheck disable=SC1090
    . "$CONFIG_FILE" 2>/dev/null
    printf '%s\n' "${SPROUT_BOT_PRIVATE_KEY:-}"
  )"
  [ -n "$bot_nsec" ] || return 0
  [ -x "$LEXEBOT_BIN" ] || return 0

  derived_pubkey="$(SPROUT_BOT_PRIVATE_KEY="$bot_nsec" "$LEXEBOT_BIN" --print-pubkey | trim)"
  [ -n "$derived_pubkey" ] || return 0

  quoted_pubkey="$(shell_quote "$derived_pubkey")"
  tmp="${CONFIG_FILE}.tmp.$$"
  awk -v quoted_pubkey="$quoted_pubkey" '
    BEGIN { inserted = 0 }
    /^SPROUT_BOT_PRIVATE_KEY=/ {
      if (!inserted) {
        print "SPROUT_BOT_PUBKEY=" quoted_pubkey
        inserted = 1
      }
      print
      next
    }
    { print }
    END {
      if (!inserted) {
        print "SPROUT_BOT_PUBKEY=" quoted_pubkey
      }
    }
  ' "$CONFIG_FILE" >"$tmp"
  chmod 600 "$tmp"
  mv "$tmp" "$CONFIG_FILE"
  say "Stored LexeBot pubkey in ${CONFIG_FILE}."
}

ensure_config_kudos_bot_pubkey() {
  [ -f "$CONFIG_FILE" ] || return 0
  local existing_pubkey quoted_pubkey tmp

  existing_pubkey="$(
    # shellcheck disable=SC1090
    . "$CONFIG_FILE" 2>/dev/null
    printf '%s\n' "${LEXEBOT_KUDOS_BOT_PUBKEY:-}"
  )"
  [ -z "$existing_pubkey" ] || return 0

  quoted_pubkey="$(shell_quote "$DEFAULT_KUDOS_BOT_PUBKEY")"
  tmp="${CONFIG_FILE}.tmp.$$"
  awk -v quoted_pubkey="$quoted_pubkey" '
    BEGIN { inserted = 0 }
    /^LEXE_CLIENT_CREDENTIALS=/ {
      if (!inserted) {
        print "LEXEBOT_KUDOS_BOT_PUBKEY=" quoted_pubkey
        inserted = 1
      }
      print
      next
    }
    { print }
    END {
      if (!inserted) {
        print "LEXEBOT_KUDOS_BOT_PUBKEY=" quoted_pubkey
      }
    }
  ' "$CONFIG_FILE" >"$tmp"
  chmod 600 "$tmp"
  mv "$tmp" "$CONFIG_FILE"
  say "Stored Flint Alpha Kudos bot pubkey in ${CONFIG_FILE}."
}

confirm_yes() {
  local prompt="$1"
  local default_answer="$2"
  local answer suffix

  case "$default_answer" in
    yes) suffix="[Y/n]" ;;
    no) suffix="[y/N]" ;;
    *) fail "invalid default answer: ${default_answer}" ;;
  esac

  printf '%s %s ' "$prompt" "$suffix" >&2
  IFS= read -r answer || answer=""
  answer="$(printf '%s' "$answer" | trim | tr '[:upper:]' '[:lower:]')"

  if [ -z "$answer" ]; then
    [ "$default_answer" = "yes" ]
    return
  fi

  case "$answer" in
    y|yes) return 0 ;;
    n|no) return 1 ;;
    *) fail "please answer yes or no" ;;
  esac
}

trim() {
  awk '{$1=$1; print}'
}

shell_quote() {
  printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\''/g")"
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

start_lexebot() {
  local pid

  pid="$(lexebot_pid)"
  if [ -n "$pid" ]; then
    say "LexeBot is already running with pid ${pid}."
    print_start_command
    return 0
  fi

  [ -x "$RUNNER_BIN" ] || fail "runner is not executable at ${RUNNER_BIN}"
  print_start_command
  say
  say "Starting LexeBot in this terminal. Leave this process running."
  say "Press Ctrl-C to stop LexeBot."
  exec "$RUNNER_BIN"
}

handle_existing_install() {
  local installed_version bot_pubkey

  if ! existing_install_present; then
    return 0
  fi

  say "Existing LexeBot install found:"
  say "- ${LEXEBOT_BIN}"
  say "- ${RUNNER_BIN}"
  say "- ${CONFIG_FILE}"
  install_runner
  normalize_config_channels
  ensure_config_kudos_bot_pubkey

  installed_version="$(installed_config_version)"
  if [ "$installed_version" != "$LEXEBOT_VERSION" ]; then
    say "Existing LexeBot version is ${installed_version:-unknown}; upgrading to ${LEXEBOT_VERSION}."
    if [ -n "$(lexebot_pid)" ]; then
      fail "LexeBot is currently running. Stop it with Ctrl-C, then rerun this installer to upgrade."
    fi
    download_lexebot
    install_runner
    update_config_version
    ensure_config_bot_pubkey
    ensure_config_kudos_bot_pubkey
    say "Upgraded existing LexeBot install to ${LEXEBOT_VERSION}."
    if [ "$MANUAL_ADD" -eq 1 ]; then
      bot_pubkey="$(configured_bot_pubkey)"
      [ -n "$bot_pubkey" ] || fail "could not determine LexeBot pubkey from ${CONFIG_FILE}"
      print_manual_add_instructions "$bot_pubkey"
      exit 0
    fi
    ensure_home_channel_for_existing_install
    start_lexebot
    exit 0
  fi

  ensure_config_bot_pubkey

  if [ "$MANUAL_ADD" -eq 1 ]; then
    bot_pubkey="$(configured_bot_pubkey)"
    [ -n "$bot_pubkey" ] || fail "could not determine LexeBot pubkey from ${CONFIG_FILE}"
    print_manual_add_instructions "$bot_pubkey"
    exit 0
  fi

  ensure_home_channel_for_existing_install

  if [ -n "$(lexebot_pid)" ]; then
    start_lexebot
    exit 0
  fi

  if confirm_yes "Start the existing LexeBot now?" yes; then
    start_lexebot
  else
    say "Leaving the existing LexeBot install unchanged."
    print_start_command
  fi
  exit 0
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

  say "Searching for an existing Sprout CLI..."
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

download_lexebot() {
  need_cmd curl
  need_cmd shasum
  need_cmd tar

  [ "$(uname -s)" = "Darwin" ] || fail "this installer is for macOS"
  [ "$(uname -m)" = "arm64" ] || fail "the current public LexeBot binary is Apple Silicon only"

  local tmpdir
  tmpdir="$(mktemp -d)"
  LEXEBOT_DOWNLOAD_TMPDIR="$tmpdir"

  say "Downloading LexeBot ${LEXEBOT_VERSION}..."
  curl -fL -o "${tmpdir}/${LEXEBOT_ARCHIVE}" "${LEXEBOT_RELEASE_BASE}/${LEXEBOT_ARCHIVE}"
  curl -fL -o "${tmpdir}/${LEXEBOT_ARCHIVE}.sha256" "${LEXEBOT_RELEASE_BASE}/${LEXEBOT_ARCHIVE}.sha256"

  (
    cd "$tmpdir"
    shasum -a 256 -c "${LEXEBOT_ARCHIVE}.sha256"
    tar -xzf "$LEXEBOT_ARCHIVE"
  )

  mkdir -p "$INSTALL_DIR"
  install -m 700 "${tmpdir}/lexebot" "$LEXEBOT_BIN"
  xattr -dr com.apple.quarantine "$LEXEBOT_BIN" 2>/dev/null || true
  say "Installed ${LEXEBOT_BIN}"
}

install_runner() {
  local script_source script_dir local_runner local_add_channel local_list_channels
  script_source="${BASH_SOURCE[0]-}"
  script_dir=""
  if [ -n "$script_source" ] && [ "$script_source" != "bash" ] && [ "$script_source" != "-" ]; then
    script_dir="$(cd "$(dirname "$script_source")" >/dev/null 2>&1 && pwd || true)"
  fi
  local_runner="${script_dir:+${script_dir}/run.sh}"
  local_add_channel="${script_dir:+${script_dir}/add-channel.sh}"
  local_list_channels="${script_dir:+${script_dir}/list-channels.sh}"
  mkdir -p "$INSTALL_DIR"

  if [ -n "$local_runner" ] && [ -f "$local_runner" ]; then
    install -m 700 "$local_runner" "$RUNNER_BIN"
  else
    need_cmd curl
    curl -fL -o "$RUNNER_BIN" "$RUNNER_URL"
    chmod 700 "$RUNNER_BIN"
  fi

  say "Installed runner ${RUNNER_BIN}"

  if [ -n "$local_add_channel" ] && [ -f "$local_add_channel" ]; then
    install -m 700 "$local_add_channel" "$ADD_CHANNEL_BIN"
  else
    need_cmd curl
    curl -fL -o "$ADD_CHANNEL_BIN" "$ADD_CHANNEL_URL"
    chmod 700 "$ADD_CHANNEL_BIN"
  fi

  say "Installed channel helper ${ADD_CHANNEL_BIN}"

  if [ -n "$local_list_channels" ] && [ -f "$local_list_channels" ]; then
    install -m 700 "$local_list_channels" "$LIST_CHANNELS_BIN"
  else
    need_cmd curl
    curl -fL -o "$LIST_CHANNELS_BIN" "$LIST_CHANNELS_URL"
    chmod 700 "$LIST_CHANNELS_BIN"
  fi

  say "Installed channel list helper ${LIST_CHANNELS_BIN}"

  cat >"$LEGACY_RUNNER_BIN" <<EOF
#!/usr/bin/env bash
exec "$RUNNER_BIN" "\$@"
EOF
  chmod 700 "$LEGACY_RUNNER_BIN"
}

generate_bot_identity() {
  local key_output bot_nsec bot_pubkey

  say "Creating a local LexeBot identity..."
  key_output="$("$LEXEBOT_BIN" --generate-key)"
  bot_pubkey="$(printf '%s\n' "$key_output" | awk -F': ' '/Public key hex:/ {print $2}' | trim)"
  bot_nsec="$(printf '%s\n' "$key_output" | awk -F': ' '/Private key nsec:/ {print $2}' | trim)"

  [ -n "$bot_pubkey" ] || fail "could not parse generated LexeBot public key"
  [ -n "$bot_nsec" ] || fail "could not parse generated LexeBot private key"

  printf '%s\n%s\n' "$bot_pubkey" "$bot_nsec"
}

normalize_lexe_credentials() {
  local creds="$1"
  creds="${creds//$'\e[200~'/}"
  creds="${creds//$'\e[201~'/}"
  printf '%s' "$creds" | LC_ALL=C tr -d '\r\n' | trim
}

read_lexe_credentials() {
  local creds

  if [ -n "${LEXE_CLIENT_CREDENTIALS:-}" ]; then
    creds="$(normalize_lexe_credentials "$LEXE_CLIENT_CREDENTIALS")"
    [ -n "$creds" ] || fail "LEXE_CLIENT_CREDENTIALS cannot be empty"
    say "Read Lexe SDK client credentials from LEXE_CLIENT_CREDENTIALS."
    printf '%s\n' "$creds"
    return 0
  fi

  fail "set LEXE_CLIENT_CREDENTIALS before running the installer, e.g. LEXE_CLIENT_CREDENTIALS='paste-client-credential-here' bash <(curl -fsSL https://raw.githubusercontent.com/matbalez/lexebot/main/scripts/install.sh)"
}

write_config() {
  local owner_key="$1"
  local bot_pubkey="$2"
  local bot_nsec="$3"
  local lexe_credentials="$4"

  mkdir -p "$CONFIG_DIR"
  umask 077
  {
    printf 'SPROUT_RELAY_URL=%s\n' "$(shell_quote "$RELAY_WS_URL")"
    printf 'SPROUT_HTTP_RELAY_URL=%s\n' "$(shell_quote "$RELAY_HTTP_URL")"
    printf 'SPROUT_CHANNEL_IDS=%s\n' "$(shell_quote "")"
    printf 'SPROUT_OWNER_PRIVATE_KEY=%s\n' "$(shell_quote "$owner_key")"
    printf 'SPROUT_BOT_PUBKEY=%s\n' "$(shell_quote "$bot_pubkey")"
    printf 'SPROUT_BOT_PRIVATE_KEY=%s\n' "$(shell_quote "$bot_nsec")"
    printf 'SPROUT_BOT_AUTH_MODE=%s\n' "$(shell_quote "owner-attested")"
    printf 'LEXEBOT_KUDOS_BOT_PUBKEY=%s\n' "$(shell_quote "$DEFAULT_KUDOS_BOT_PUBKEY")"
    printf 'LEXE_CLIENT_CREDENTIALS=%s\n' "$(shell_quote "$lexe_credentials")"
    printf 'LEXEBOT_BIN=%s\n' "$(shell_quote "$LEXEBOT_BIN")"
    printf 'LEXEBOT_VERSION=%s\n' "$(shell_quote "$LEXEBOT_VERSION")"
  } >"$CONFIG_FILE"
  chmod 600 "$CONFIG_FILE"
  say "Wrote ${CONFIG_FILE}"
}

channel_id_from_dm_open_output() {
  local output="$1"
  local channel_id

  channel_id="$(printf '%s\n' "$output" | sed -n 's/.*"channel_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1 | trim)"
  if [ -n "$channel_id" ]; then
    printf '%s\n' "$channel_id"
    return 0
  fi

  printf '%s\n' "$output" | sed -n 's/.*\\"channel_id\\"[[:space:]]*:[[:space:]]*\\"\([^\\"]*\)\\".*/\1/p' | head -n 1 | trim
}

sprout_open_home_dm() {
  local sprout_cli="$1"
  local owner_key="$2"
  local bot_pubkey="$3"
  local output channel_id

  say "Opening ${HOME_SURFACE_NAME}..."
  if "$sprout_cli" open-dm --help >/dev/null 2>&1; then
    output="$(SPROUT_PRIVATE_KEY="$owner_key" "$sprout_cli" \
      --relay "$RELAY_HTTP_URL" \
      open-dm \
      --pubkey "$bot_pubkey")"
  elif "$sprout_cli" dms open --help >/dev/null 2>&1; then
    output="$(SPROUT_PRIVATE_KEY="$owner_key" "$sprout_cli" \
      --relay "$RELAY_HTTP_URL" \
      dms open \
      --pubkey "$bot_pubkey")"
  else
    fail "Sprout CLI at ${sprout_cli} does not support opening DMs"
  fi

  channel_id="$(channel_id_from_dm_open_output "$output")"
  [ -n "$channel_id" ] || fail "could not parse ${HOME_SURFACE_NAME} channel id from Sprout CLI output: ${output}"
  printf '%s\n' "$channel_id"
}

set_config_channels() {
  local channel_ids="$1"
  local quoted_channels tmp

  quoted_channels="$(shell_quote "$channel_ids")"
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
}

send_install_welcome_message() {
  local bot_nsec="$1"
  local owner_key="$2"
  local channel_id="$3"

  [ -n "$bot_nsec" ] || {
    say "Skipping welcome message: missing LexeBot private key."
    return 0
  }
  [ -n "$owner_key" ] || {
    say "Skipping welcome message: missing Sprout owner key."
    return 0
  }

  say "Sending LexeBot welcome message..."
  if ! (
    set -a
    . "$CONFIG_FILE"
    set +a
    SPROUT_RELAY_URL="$RELAY_WS_URL" \
      SPROUT_BOT_PRIVATE_KEY="$bot_nsec" \
      SPROUT_OWNER_PRIVATE_KEY="$owner_key" \
      SPROUT_BOT_AUTH_MODE="owner-attested" \
      "$LEXEBOT_BIN" --send-install-welcome "$channel_id" >/dev/null
  )
  then
    say "Warning: install completed, but the welcome message could not be sent."
  fi
}

ensure_home_channel_for_existing_install() {
  local existing_channels bot_pubkey bot_nsec owner_key sprout_cli home_channel_id

  existing_channels="$(configured_channel_ids || true)"
  bot_pubkey="$(configured_bot_pubkey)"
  [ -n "$bot_pubkey" ] || fail "could not determine LexeBot pubkey from ${CONFIG_FILE}"
  bot_nsec="$(configured_bot_private_key)"
  [ -n "$bot_nsec" ] || fail "could not determine LexeBot private key from ${CONFIG_FILE}"
  owner_key="$(configured_owner_key)"
  [ -n "$owner_key" ] || fail "could not determine Sprout owner key from ${CONFIG_FILE}"

  if [ -n "$existing_channels" ]; then
    # The first configured channel is the owner control surface. For current
    # installs that is the LexeBot DM; older Flint Alpha work-channel entries
    # were removed by normalize_config_channels before this runs.
    home_channel_id="$(printf '%s\n' "$existing_channels" | awk '{print $1}')"
    send_install_welcome_message "$bot_nsec" "$owner_key" "$home_channel_id"
    return 0
  fi

  sprout_cli="$(ensure_sprout_cli)"
  home_channel_id="$(sprout_open_home_dm "$sprout_cli" "$owner_key" "$bot_pubkey")"
  set_config_channels "$home_channel_id"
  send_install_welcome_message "$bot_nsec" "$owner_key" "$home_channel_id"
  say "${HOME_SURFACE_NAME} configured: ${home_channel_id}"
}

main() {
  parse_args "$@"

  need_cmd awk
  need_cmd sed

  migrate_legacy_config
  handle_existing_install

  download_lexebot
  install_runner

  local owner_key_file owner_key bot_pubkey bot_nsec lexe_credentials generated
  local sprout_cli home_channel_id
  owner_key_file="$(find_owner_key_file)" || fail "Sprout identity key not found. Launch Sprout once, then rerun this installer."
  owner_key="$(read_secret_file "$owner_key_file")" || fail "could not read ${owner_key_file}"
  [ -n "$owner_key" ] || fail "Sprout identity key is empty"

  generated="$(generate_bot_identity)"
  bot_pubkey="$(printf '%s\n' "$generated" | sed -n '1p')"
  bot_nsec="$(printf '%s\n' "$generated" | sed -n '2p')"
  lexe_credentials="$(read_lexe_credentials)"

  write_config "$owner_key" "$bot_pubkey" "$bot_nsec" "$lexe_credentials"

  if [ "$MANUAL_ADD" -eq 1 ]; then
    say
    say "Install complete."
    say "No channel subscriptions configured."
    print_manual_add_instructions "$bot_pubkey"
    exit 0
  fi

  sprout_cli="$(ensure_sprout_cli)"
  home_channel_id="$(sprout_open_home_dm "$sprout_cli" "$owner_key" "$bot_pubkey")"
  set_config_channels "$home_channel_id"
  send_install_welcome_message "$bot_nsec" "$owner_key" "$home_channel_id"

  say
  say "Install complete."
  say "${HOME_SURFACE_NAME} configured: ${home_channel_id}"
  say "List available channels with:"
  say "$(list_channels_command)"
  say "Add another channel later with:"
  say "$(add_channel_command)"
  start_lexebot
}

main "$@"
