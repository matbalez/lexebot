#!/usr/bin/env bash
set -euo pipefail

CHANNEL_ID="1df37399-3c25-4019-8bc7-faacd53587d0"
RELAY_WS_URL="wss://sprout.up.railway.app"
RELAY_HTTP_URL="https://sprout.up.railway.app"
LEXEBOT_VERSION="v0.1.2"
LEXEBOT_ARCHIVE="lexebot-${LEXEBOT_VERSION#v}-aarch64-apple-darwin.tar.gz"
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

say() {
  printf '%s\n' "$*"
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

download_lexebot() {
  need_cmd curl
  need_cmd shasum
  need_cmd tar

  [ "$(uname -s)" = "Darwin" ] || fail "this installer is for macOS"
  [ "$(uname -m)" = "arm64" ] || fail "the current public LexeBot binary is Apple Silicon only"

  local tmpdir
  tmpdir="$(mktemp -d)"
  trap 'rm -rf "$tmpdir"' EXIT

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

read_lexe_credentials() {
  local creds
  say "Paste your Lexe SDK client credentials. Input is hidden."
  printf 'Lexe SDK client: '
  IFS= read -r -s creds
  printf '\n'
  creds="$(printf '%s' "$creds" | trim)"
  [ -n "$creds" ] || fail "Lexe SDK client credentials cannot be empty"
  printf '%s\n' "$creds"
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

add_bot_to_channel() {
  local owner_key="$1"
  local bot_pubkey="$2"

  say "Adding LexeBot to Flint Alpha as role bot..."
  if command -v sprout >/dev/null 2>&1; then
    SPROUT_PRIVATE_KEY="$owner_key" sprout \
      --relay "$RELAY_HTTP_URL" \
      add-channel-member \
      --channel "$CHANNEL_ID" \
      --pubkey "$bot_pubkey" \
      --role bot
    say "LexeBot was added to Flint Alpha."
    return 0
  fi

  if command -v cargo >/dev/null 2>&1 && command -v git >/dev/null 2>&1; then
    say "sprout CLI was not found. Falling back to cargo + github.com/block/sprout."
    mkdir -p "$(dirname "$SPROUT_REPO_DIR")"
    if [ ! -d "$SPROUT_REPO_DIR/.git" ]; then
      git clone --depth 1 https://github.com/block/sprout.git "$SPROUT_REPO_DIR"
    fi
    (
      cd "$SPROUT_REPO_DIR"
      SPROUT_PRIVATE_KEY="$owner_key" cargo run -q -p sprout-cli -- \
        --relay "$RELAY_HTTP_URL" \
        add-channel-member \
        --channel "$CHANNEL_ID" \
        --pubkey "$bot_pubkey" \
        --role bot
    )
    say "LexeBot was added to Flint Alpha."
    return 0
  fi

  fail "install sprout CLI, or install cargo+git so the installer can build sprout-cli automatically"
}

main() {
  need_cmd awk
  need_cmd sed

  download_lexebot
  install_runner

  local owner_key_file owner_key bot_pubkey bot_nsec lexe_credentials generated
  owner_key_file="$(find_owner_key_file)" || fail "Sprout identity key not found. Launch Sprout once, then rerun this installer."
  owner_key="$(read_secret_file "$owner_key_file")" || fail "could not read ${owner_key_file}"
  [ -n "$owner_key" ] || fail "Sprout identity key is empty"

  generated="$(generate_bot_identity)"
  bot_pubkey="$(printf '%s\n' "$generated" | sed -n '1p')"
  bot_nsec="$(printf '%s\n' "$generated" | sed -n '2p')"
  lexe_credentials="$(read_lexe_credentials)"

  write_config "$owner_key" "$bot_nsec" "$lexe_credentials"
  add_bot_to_channel "$owner_key" "$bot_pubkey"

  say
  say "Done. Start LexeBot with:"
  say "$RUNNER_BIN"
  say
  say "If ${INSTALL_DIR} is not on your PATH, run:"
  say "bash $RUNNER_BIN"
}

main "$@"
