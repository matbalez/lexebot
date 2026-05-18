# LexeBot

A deterministic Sprout bot that runs locally, receives encrypted auto-kudos
commands, and optionally lets one configured owner control a Lexe wallet from a
private Sprout channel.

LexeBot is not AI-powered. It holds a Nostr bot key for Sprout messages and a
Lexe SDK client credential string supplied locally by the user. It only accepts
wallet commands from the configured owner. In the normal owner-attested setup,
the owner pubkey is derived from `SPROUT_OWNER_PRIVATE_KEY`.

## Commands

Commands require an explicit mention tag for the bot pubkey. In Sprout, select
LexeBot from mention autocomplete. If the bot profile is personalized, the
visible mention includes the owner display name without spaces, for example
`@LexeBot[Mat] get balance`.

```text
@LexeBot get balance
@LexeBot get BOLT12
@LexeBot create invoice for ₿1,000
@LexeBot send ₿500 to <payment-target>
```

`get BOLT12` creates and returns a reusable Lexe BOLT12 offer with no minimum
amount.

`get balance` and `create invoice` return sensitive wallet details by opening a
Sprout DM with the owner and sending the full response there. The public channel
only receives a short acknowledgement.

`<payment-target>` is passed to Lexe's generic payment parser. Use whatever the
installed Lexe Rust SDK accepts, such as a BOLT11 invoice, BOLT12 offer, Human
Bitcoin Address, Lightning Address, or on-chain/BIP321 URI.

## Run Locally

LexeBot uses `lexe v0.1.10`, which requires Rust 1.90 or newer.

### Public Binary

For Apple Silicon Macs:

```bash
curl -L -o lexebot-v0.1.8-aarch64-apple-darwin.tar.gz \
  https://github.com/matbalez/lexebot/releases/download/v0.1.8/lexebot-v0.1.8-aarch64-apple-darwin.tar.gz
curl -L -o lexebot-v0.1.8-aarch64-apple-darwin.tar.gz.sha256 \
  https://github.com/matbalez/lexebot/releases/download/v0.1.8/lexebot-v0.1.8-aarch64-apple-darwin.tar.gz.sha256
shasum -a 256 -c lexebot-v0.1.8-aarch64-apple-darwin.tar.gz.sha256
mkdir -p ~/.local/bin
tar -xzf lexebot-v0.1.8-aarch64-apple-darwin.tar.gz
mv lexebot ~/.local/bin/lexebot
chmod 700 ~/.local/bin/lexebot
```

Make sure `~/.local/bin` is on your `PATH`.

### Sprout Install Script

For Sprout users on Apple Silicon Macs, the repo includes an installer that
sets up LexeBot locally, creates a private `lexebot` home channel for manual
wallet commands, and starts LexeBot:

```bash
LEXE_CLIENT_CREDENTIALS='paste-client-credential-here' \
  bash <(curl -fsSL https://raw.githubusercontent.com/matbalez/lexebot/main/scripts/install.sh)
run-lexebot
```

The installer downloads the binary, generates a local LexeBot identity, reads
the local Sprout owner key from `~/Library/Application Support/xyz.block.sprout.app/identity.key`,
reads the Lexe SDK client credentials from `LEXE_CLIENT_CREDENTIALS`, stores the local run config in
`~/.config/lexebot/lexebot.env` with file mode `600`, installs the runner at
`~/.local/bin/run-lexebot`, installs the channel helper at
`~/.local/bin/lexebot-add-channel`, installs the channel listing helper at
`~/.local/bin/lexebot-list-channels`, creates a private `lexebot` home channel,
and adds the bot to that channel as role `bot` using the invoking user's Sprout
identity. On a fresh install, it prints
the command to start LexeBot later, then runs LexeBot in the foreground in the
current terminal so startup logs are visible. On rerun, it detects an existing
local install and asks whether to start that existing bot instead of generating
a new identity. LexeBot derives the owner display name from the owner's public
Sprout profile on startup and publishes the bot profile as
`LexeBot[<owner display name without spaces>]`.
If the installer is newer than the local installed version, rerunning the
installer upgrades the binary and runner while preserving the existing local
config and bot identity. During that upgrade, the legacy Flint Alpha channel
subscription is removed from `SPROUT_CHANNEL_IDS`; auto-kudos does not need
LexeBot to subscribe to work channels. If the resulting config has no channel
subscriptions, the normal installer path creates a private `lexebot` home
channel and configures LexeBot to listen there for manual wallet commands.

If you do not want the installer to use or build Sprout CLI to create the
private home channel, use manual-add mode:

```bash
LEXE_CLIENT_CREDENTIALS='paste-client-credential-here' \
  bash <(curl -fsSL https://raw.githubusercontent.com/matbalez/lexebot/main/scripts/install.sh) --manual-add
```

Manual-add mode installs LexeBot, writes local config with no channel
subscriptions, prints the LexeBot pubkey, and exits without starting LexeBot.
Auto-kudos can run channel-free. For manual wallet commands, create a private
`lexebot` channel in Sprout, add the printed LexeBot pubkey as role `bot`, then
configure that channel with `lexebot-add-channel <channel-uuid>`.

After the first install, start LexeBot from any directory with:

```bash
run-lexebot
```

If `~/.local/bin` is not on your `PATH`, use this from any directory instead:

```bash
bash ~/.local/bin/run-lexebot
```

Add the same installed LexeBot identity to another channel with:

```bash
lexebot-add-channel <channel-uuid>
```

Then restart LexeBot with `run-lexebot`. The helper adds the bot pubkey to the
channel as role `bot` and appends the channel UUID to
`~/.config/lexebot/lexebot.env`. Prefer using this for a private `lexebot`
home/control channel, not for every work channel where Kudos may be used.

Until Sprout exposes a copy button for channel UUIDs in the UI, use the
installed helper to list channels and copy the target `id`/UUID:

```bash
lexebot-list-channels
```

If `~/.local/bin` is not on your `PATH`, use this from any directory instead:

```bash
bash ~/.local/bin/lexebot-list-channels
```

### From Source

Generate a bot identity:

```bash
cargo +1.90.0 run --manifest-path /Users/mattyb/.sprout/REPOS/lexebot/Cargo.toml -- --generate-key
```

Run LexeBot:

```bash
SPROUT_RELAY_URL=wss://sprout.up.railway.app \
SPROUT_CHANNEL_IDS=<optional-home-channel-uuid>[,<channel-uuid>...] \
SPROUT_BOT_PRIVATE_KEY=<bot-nsec-or-hex-secret> \
SPROUT_OWNER_PRIVATE_KEY=<your-sprout-owner-or-agent-secret> \
SPROUT_BOT_AUTH_MODE=owner-attested \
LEXE_CLIENT_CREDENTIALS=<lexe-client-credentials> \
cargo +1.90.0 run --manifest-path /Users/mattyb/.sprout/REPOS/lexebot/Cargo.toml
```

`SPROUT_CHANNEL_IDS` is optional. Leave it unset or empty for auto-kudos-only
mode; configure a private home/control channel only if the owner should send
manual wallet commands to LexeBot.

With `SPROUT_BOT_AUTH_MODE=owner-attested`, LexeBot derives the owner pubkey
from `SPROUT_OWNER_PRIVATE_KEY`. `LEXEBOT_OWNER_PUBKEY` is optional in this
mode and is only used as a consistency check if supplied. If you use a
precomputed `SPROUT_AUTH_TAG` without `SPROUT_OWNER_PRIVATE_KEY`, LexeBot
derives the owner from that verified auth tag instead.

Optional:

```bash
LEXEBOT_MAX_SEND_AMOUNT=100000
LEXEBOT_INVOICE_EXPIRATION_SECS=3600
LEXEBOT_NETWORK=mainnet
LEXEBOT_OWNER_DISPLAY_NAME=Mat
LEXEBOT_KUDOS_BOT_PUBKEY=<kudos-bot-pubkey-hex>
```

`LEXEBOT_MAX_SEND_AMOUNT` is expressed in ₿ base units. If set, `send` commands
above that amount are rejected.

On startup, LexeBot tries to read the owner's Sprout/Nostr profile and publishes
its bot profile as `LexeBot[<owner display name without spaces>]`, for example
`LexeBot[Mat]`. Set `LEXEBOT_OWNER_DISPLAY_NAME` only as an optional override or
fallback if profile lookup is unavailable.

The published profile also includes a public `lexebot` discovery object:

```json
{
  "owner_pubkey": "<owner-pubkey-hex>",
  "owner_auth": ["auth", "<owner-pubkey-hex>", "", "<owner-signature>"],
  "bolt12_offer": "lno1..."
}
```

Other bots can verify `owner_auth` against the LexeBot pubkey before treating
the profile as an owner-to-LexeBot mapping. `bolt12_offer` is a reusable
no-minimum BOLT12 offer generated on startup.

If `LEXEBOT_KUDOS_BOT_PUBKEY` is set, LexeBot accepts a narrow encrypted
machine command from that Kudos bot on global ephemeral event kind `21000`. The
event is p-tagged to this LexeBot and its JSON command body is encrypted to the
LexeBot pubkey with NIP-44. The command only executes when its encrypted
`sender_pubkey` is this LexeBot's configured owner.

Successful auto-kudos payments are silent in chat, then LexeBot sends an
encrypted global ephemeral kind `21001` result back to Kudos so Kudos can post a
terse public confirmation. The result event is only p-tagged to Kudos; request
correlation stays inside the encrypted payload. Failures are logged to the
LexeBot terminal. Normal wallet commands still require the owner pubkey and
still happen in a configured channel/DM.

Because auto-kudos commands are direct encrypted machine events, LexeBot does
not need to be a member of every public/private channel where Kudos is present.
It only needs to be running locally and publishing its verified profile/BOLT12
offer.

The Flint Alpha installer sets `LEXEBOT_KUDOS_BOT_PUBKEY` to the Flint Alpha
Kudos bot pubkey by default, and backfills that value for existing installs.

If startup logs say LexeBot is authenticated but is not a channel member, either
remove that channel id from `~/.config/lexebot/lexebot.env` or add the printed
bot pubkey to that channel as a bot using a Sprout identity allowed to add
members. For the local-only auto-kudos model, work channels should normally not
be listed there.

```bash
SPROUT_PRIVATE_KEY=<your-existing-sprout-nsec-or-hex-secret> \
sprout channels add-member \
  --channel <channel-uuid> \
  --pubkey <lexebot-pubkey-hex> \
  --role bot
```

If you are running the CLI from the Sprout repo instead of an installed `sprout`
binary:

```bash
cd /Users/mattyb/.sprout/REPOS/sprout
SPROUT_PRIVATE_KEY=<your-existing-sprout-nsec-or-hex-secret> \
cargo run -p sprout-cli -- \
  --relay wss://sprout.up.railway.app \
  channels add-member \
  --channel <channel-uuid> \
  --pubkey <lexebot-pubkey-hex> \
  --role bot
```

## Safety Model

- The bot only responds when the event has a `p` tag for the LexeBot pubkey.
- The bot only executes commands from the configured owner pubkey. In
  owner-attested mode this is derived from `SPROUT_OWNER_PRIVATE_KEY` or
  verified `SPROUT_AUTH_TAG`; explicit `LEXEBOT_OWNER_PUBKEY` is optional.
- The bot ignores its own messages and messages from before startup.
- Relay disconnects are retried with bounded backoff.
- Lexe client credentials are read from the local environment and never posted
  to Sprout.

## Packaging

Today, the simplest local install path is Cargo:

```bash
cargo +1.90.0 install --path /Users/mattyb/.sprout/REPOS/lexebot --locked
```

Once this repo is hosted, the recommended public install command is:

```bash
cargo +1.90.0 install --git https://github.com/<owner>/lexebot --locked
```

The next packaging step should be a small `lexebot init` flow that:

- generates a Sprout bot key
- derives the owner's Sprout pubkey from the owner auth material
- writes a local `.env` file outside the repo
- prints the exact `sprout channels add-member` command for the bot pubkey
- never stores or uploads Lexe client credentials anywhere except the user's
  local machine

After that is stable, ship signed GitHub release binaries and a Homebrew tap for
non-Rust users.

## Sources

- Lexe Rust quickstart: https://docs.lexe.tech/rust/quickstart/
- Lexe Rust API docs: https://rust.lexe.tech/
