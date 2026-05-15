#!/usr/bin/env bash
set -euo pipefail

CHANNEL_ID="1df37399-3c25-4019-8bc7-faacd53587d0"
RELAY_WS_URL="wss://sprout.up.railway.app"
RELAY_HTTP_URL="https://sprout.up.railway.app"
LEXEBOT_VERSION="v0.1.3"
LEXEBOT_ARCHIVE="lexebot-${LEXEBOT_VERSION}-aarch64-apple-darwin.tar.gz"
LEXEBOT_RELEASE_BASE="https://github.com/matbalez/lexebot/releases/download/${LEXEBOT_VERSION}"
RUNNER_URL="https://raw.githubusercontent.com/matbalez/lexebot/main/scripts/run-flint-alpha.sh"

INSTALL_DIR="${HOME}/.local/bin"
LEXEBOT_BIN="${INSTALL_DIR}/lexebot"
RUNNER_BIN="${INSTALL_DIR}/run-lexebot-flint-alpha"
CONFIG_DIR="${HOME}/.config/lexebot"
CONFIG_FILE="${CONFIG_DIR}/flint-alpha.env"
SPROUT_IDENTITY_KEY="${HOME}/Library/Application Support/xyz.block.sprout.app/identity.key"
LEGACY_SPROUT_IDENTITY_KEY="${HOME}/Library/Application Support/com.wesb.sprout/identity.key"
SPROUT_REPO_DIR="${HOME}/.cache/lexebot/sprout"
SPROUT_CLI_PATH="${SPROUT_CLI_PATH:-}"
LEXEBOT_DOWNLOAD_TMPDIR=""

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

start_command() {
  if command -v run-lexebot-flint-alpha >/dev/null 2>&1; then
    printf 'run-lexebot-flint-alpha\n'
  else
    printf 'bash %s\n' "$RUNNER_BIN"
  fi
}

print_start_command() {
  say "Future start command:"
  say "$(start_command)"
}

lexebot_pid() {
  pgrep -f "$LEXEBOT_BIN" 2>/dev/null | head -n 1 || true
}

existing_install_present() {
  [ -x "$LEXEBOT_BIN" ] && [ -x "$RUNNER_BIN" ] && [ -f "$CONFIG_FILE" ]
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
  if ! existing_install_present; then
    return 0
  fi

  say "Existing LexeBot install found:"
  say "- ${LEXEBOT_BIN}"
  say "- ${RUNNER_BIN}"
  say "- ${CONFIG_FILE}"

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
  local script_dir local_runner
  script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd || true)"
  local_runner="${script_dir}/run-flint-alpha.sh"
  mkdir -p "$INSTALL_DIR"

  if [ -f "$local_runner" ]; then
    install -m 700 "$local_runner" "$RUNNER_BIN"
  else
    need_cmd curl
    curl -fL -o "$RUNNER_BIN" "$RUNNER_URL"
    chmod 700 "$RUNNER_BIN"
  fi

  say "Installed runner ${RUNNER_BIN}"
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

  fail "set LEXE_CLIENT_CREDENTIALS before running the installer, e.g. LEXE_CLIENT_CREDENTIALS='paste-client-credential-here' bash <(curl -fsSL https://raw.githubusercontent.com/matbalez/lexebot/main/scripts/install-flint-alpha.sh)"
}

write_config() {
  local owner_key="$1"
  local bot_nsec="$2"
  local lexe_credentials="$3"

  mkdir -p "$CONFIG_DIR"
  umask 077
  {
    printf 'SPROUT_RELAY_URL=%s\n' "$(shell_quote "$RELAY_WS_URL")"
    printf 'SPROUT_HTTP_RELAY_URL=%s\n' "$(shell_quote "$RELAY_HTTP_URL")"
    printf 'SPROUT_CHANNEL_ID=%s\n' "$(shell_quote "$CHANNEL_ID")"
    printf 'SPROUT_OWNER_PRIVATE_KEY=%s\n' "$(shell_quote "$owner_key")"
    printf 'SPROUT_BOT_PRIVATE_KEY=%s\n' "$(shell_quote "$bot_nsec")"
    printf 'SPROUT_BOT_AUTH_MODE=%s\n' "$(shell_quote "owner-attested")"
    printf 'LEXE_CLIENT_CREDENTIALS=%s\n' "$(shell_quote "$lexe_credentials")"
    printf 'LEXEBOT_BIN=%s\n' "$(shell_quote "$LEXEBOT_BIN")"
    printf 'LEXEBOT_VERSION=%s\n' "$(shell_quote "$LEXEBOT_VERSION")"
  } >"$CONFIG_FILE"
  chmod 600 "$CONFIG_FILE"
  say "Wrote ${CONFIG_FILE}"
}

sprout_add_bot_to_channel() {
  local sprout_cli="$1"
  local owner_key="$2"
  local bot_pubkey="$3"

  say "Adding LexeBot to Flint Alpha as role bot..."
  if "$sprout_cli" channels add-member --help >/dev/null 2>&1; then
    SPROUT_PRIVATE_KEY="$owner_key" "$sprout_cli" \
      --relay "$RELAY_HTTP_URL" \
      channels add-member \
      --channel "$CHANNEL_ID" \
      --pubkey "$bot_pubkey" \
      --role bot
    say "LexeBot was added to Flint Alpha."
    return 0
  fi

  if "$sprout_cli" add-channel-member --help >/dev/null 2>&1; then
    SPROUT_PRIVATE_KEY="$owner_key" "$sprout_cli" \
      --relay "$RELAY_HTTP_URL" \
      add-channel-member \
      --channel "$CHANNEL_ID" \
      --pubkey "$bot_pubkey" \
      --role bot
    say "LexeBot was added to Flint Alpha."
    return 0
  fi

  fail "Sprout CLI at ${sprout_cli} does not support adding channel members"
}

main() {
  need_cmd awk
  need_cmd sed

  handle_existing_install

  download_lexebot
  install_runner

  local owner_key_file owner_key bot_pubkey bot_nsec lexe_credentials generated
  local sprout_cli
  owner_key_file="$(find_owner_key_file)" || fail "Sprout identity key not found. Launch Sprout once, then rerun this installer."
  owner_key="$(read_secret_file "$owner_key_file")" || fail "could not read ${owner_key_file}"
  [ -n "$owner_key" ] || fail "Sprout identity key is empty"

  generated="$(generate_bot_identity)"
  bot_pubkey="$(printf '%s\n' "$generated" | sed -n '1p')"
  bot_nsec="$(printf '%s\n' "$generated" | sed -n '2p')"
  lexe_credentials="$(read_lexe_credentials)"
  sprout_cli="$(ensure_sprout_cli)"

  write_config "$owner_key" "$bot_nsec" "$lexe_credentials"
  sprout_add_bot_to_channel "$sprout_cli" "$owner_key" "$bot_pubkey"

  say
  say "Install complete."
  start_lexebot
}

main "$@"
