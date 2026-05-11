//! A deterministic Sprout bot for controlling one owner's Lexe wallet.

use std::str::FromStr;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use lexe::{
    config::WalletEnvConfig,
    types::{
        auth::{ClientCredentials, CredentialsRef},
        bitcoin::Amount,
        command::{CreateInvoiceRequest, CreateOfferRequest, PayRequest},
    },
    wallet::LexeWallet,
};
use nostr::bitcoin::hashes::sha256::Hash as Sha256Hash;
use nostr::bitcoin::hashes::Hash;
use nostr::bitcoin::secp256k1::schnorr::Signature;
use nostr::bitcoin::secp256k1::{Message as SecpMessage, XOnlyPublicKey};
use nostr::{
    Alphabet, Event, EventBuilder, Filter, JsonUtil, Keys, Kind, PublicKey, SingleLetterTag, Tag,
    ToBech32, Url, SECP256K1,
};
use serde_json::{json, Value};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use url::Url as WsUrl;

const DEFAULT_RELAY_URL: &str = "ws://localhost:3000";
const SUBSCRIPTION_ID: &str = "lexebot";
const OWNER_PROFILE_SUBSCRIPTION_ID: &str = "lexebot-owner-profile";
const BOT_NAME: &str = "lexebot";
const BOT_DISPLAY_NAME: &str = "LexeBot";
const BOT_ABOUT: &str = "A deterministic Sprout bot that lets one owner control a Lexe wallet.";
const BOT_ICON_DATA_URL: &str = "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 128 128'%3E%3Crect width='128' height='128' rx='28' fill='%23111618'/%3E%3Cpath d='M35 84 64 28l29 56H76L64 58 52 84H35Z' fill='%23f4c542'/%3E%3Cpath d='M43 96h42' stroke='%238fd4ff' stroke-width='10' stroke-linecap='round'/%3E%3C/svg%3E";
const INITIAL_RECONNECT_DELAY: Duration = Duration::from_secs(1);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(60);

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::args().any(|arg| arg == "--generate-key") {
        print_generated_key()?;
        return Ok(());
    }

    let config = Config::from_env()?;
    let lexe = LexeClient::from_env()?;
    let runtime = Runtime { config, lexe };

    eprintln!(
        "lexebot pubkey: {}",
        runtime.config.bot_keys.public_key().to_hex()
    );
    eprintln!(
        "lexebot owner pubkey: {}",
        runtime.config.owner_pubkey.to_hex()
    );

    run_forever(runtime).await
}

struct Runtime {
    config: Config,
    lexe: LexeClient,
}

struct LexeClient {
    wallet: LexeWallet,
    invoice_expiration_secs: u32,
    max_send_amount: Option<u64>,
}

impl LexeClient {
    fn from_env() -> Result<Self> {
        let credentials = ClientCredentials::from_string(&required_env("LEXE_CLIENT_CREDENTIALS")?)
            .context("LEXE_CLIENT_CREDENTIALS is not a valid Lexe client credentials string")?;
        let env_config = lexe_env_config()?;
        let wallet =
            LexeWallet::load_or_fresh(env_config, CredentialsRef::from(&credentials), None)
                .context("failed to load Lexe wallet")?;

        let invoice_expiration_secs =
            optional_u32_env("LEXEBOT_INVOICE_EXPIRATION_SECS")?.unwrap_or(3600);
        let max_send_amount = optional_u64_env("LEXEBOT_MAX_SEND_AMOUNT")?;

        Ok(Self {
            wallet,
            invoice_expiration_secs,
            max_send_amount,
        })
    }

    async fn execute(&self, command: BotCommand) -> Result<String> {
        match command {
            BotCommand::GetBalance => self.get_balance().await,
            BotCommand::GetBolt12 => self.get_bolt12().await,
            BotCommand::CreateInvoice { amount } => self.create_invoice(amount).await,
            BotCommand::Send { amount, payable } => self.send_payment(amount, payable).await,
            BotCommand::AutoKudosSend {
                amount, payable, ..
            } => {
                let payment = self.send_payment(amount, payable).await?;
                Ok(format!("Auto-kudos {payment}"))
            }
        }
    }

    async fn get_balance(&self) -> Result<String> {
        let info = self.wallet.node_info().await?;
        Ok(format!(
            "Balance: {} total; {} Lightning; {} Lightning spendable; {} on-chain trusted.",
            format_amount(info.balance.sats_u64()),
            format_amount(info.lightning_balance.sats_u64()),
            format_amount(info.lightning_sendable_balance.sats_u64()),
            format_amount(info.onchain_trusted_balance.sats_u64()),
        ))
    }

    async fn create_invoice(&self, amount: u64) -> Result<String> {
        let amount = amount_from_base_units(amount)?;
        let response = self
            .wallet
            .create_invoice(CreateInvoiceRequest {
                expiration_secs: Some(self.invoice_expiration_secs),
                amount: Some(amount),
                description: Some("LexeBot invoice".to_string()),
                partner_pk: None,
                partner_prop_fee: None,
                partner_base_fee: None,
            })
            .await?;

        Ok(format!(
            "Invoice for {}:\n{}",
            format_amount(amount.sats_u64()),
            response.invoice
        ))
    }

    async fn get_bolt12(&self) -> Result<String> {
        Ok(format!(
            "BOLT12 offer:\n{}",
            self.create_bolt12_offer().await?
        ))
    }

    async fn create_bolt12_offer(&self) -> Result<String> {
        let response = self
            .wallet
            .create_offer(CreateOfferRequest {
                description: Some("LexeBot BOLT12 offer".to_string()),
                min_amount: None,
                expiration_secs: None,
            })
            .await?;

        Ok(response.offer.to_string())
    }

    async fn send_payment(&self, amount: u64, payable: String) -> Result<String> {
        if let Some(max) = self.max_send_amount {
            if amount > max {
                bail!(
                    "send amount {} exceeds LEXEBOT_MAX_SEND_AMOUNT {}",
                    format_amount(amount),
                    format_amount(max)
                );
            }
        }

        let amount = amount_from_base_units(amount)?;
        let response = self
            .wallet
            .pay(PayRequest {
                payable,
                amount: Some(amount),
                message: None,
                personal_note: Some("LexeBot channel command".to_string()),
            })
            .await?;

        Ok(format!(
            "Sent {}. Payment id: {}",
            format_amount(amount.sats_u64()),
            response.index
        ))
    }
}

fn lexe_env_config() -> Result<WalletEnvConfig> {
    match std::env::var("LEXEBOT_NETWORK")
        .unwrap_or_else(|_| "mainnet".to_string())
        .as_str()
    {
        "mainnet" => Ok(WalletEnvConfig::mainnet()),
        "testnet3" | "testnet" => Ok(WalletEnvConfig::testnet3()),
        other => bail!("LEXEBOT_NETWORK must be 'mainnet' or 'testnet3', got {other:?}"),
    }
}

async fn run_forever(runtime: Runtime) -> Result<()> {
    let mut reconnect_delay = INITIAL_RECONNECT_DELAY;

    loop {
        eprintln!("connecting to {}", runtime.config.relay_url);
        let session_started = Instant::now();
        match run_session(&runtime).await {
            Ok(()) => return Ok(()),
            Err(err) if is_unrecoverable_session_error(&err) => return Err(err),
            Err(err) => {
                if session_started.elapsed() > Duration::from_secs(30) {
                    reconnect_delay = INITIAL_RECONNECT_DELAY;
                }
                eprintln!("connection ended: {err:#}");
                eprintln!("reconnecting in {} second(s)", reconnect_delay.as_secs());
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {
                        eprintln!("shutting down");
                        return Ok(());
                    }
                    _ = tokio::time::sleep(reconnect_delay) => {}
                }
                reconnect_delay = next_reconnect_delay(reconnect_delay);
            }
        }
    }
}

async fn run_session(runtime: &Runtime) -> Result<()> {
    let mut ws = connect_and_authenticate(&runtime.config).await?;
    let bot_display_name = resolve_bot_display_name(&mut ws, &runtime.config).await;
    let bolt12_offer = runtime.lexe.create_bolt12_offer().await?;
    publish_profile(&mut ws, &runtime.config, &bot_display_name, &bolt12_offer).await?;
    announce_channel_membership(&mut ws, &runtime.config).await?;
    subscribe_to_channel(&mut ws, &runtime.config.channel_id).await?;

    let started_at = nostr::Timestamp::now();

    eprintln!(
        "listening in channel {} for @LexeBot commands",
        runtime.config.channel_id
    );

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                eprintln!("shutting down");
                return Ok(());
            }
            next = ws.next() => {
                let Some(message) = next else { bail!("relay closed the WebSocket"); };
                match message? {
                    Message::Text(text) => handle_relay_text(&mut ws, runtime, started_at, &text).await?,
                    Message::Ping(bytes) => ws.send(Message::Pong(bytes)).await?,
                    Message::Close(frame) => bail!("relay closed connection: {frame:?}"),
                    _ => {}
                }
            }
        }
    }
}

fn next_reconnect_delay(current: Duration) -> Duration {
    current.saturating_mul(2).min(MAX_RECONNECT_DELAY)
}

fn is_unrecoverable_session_error(err: &anyhow::Error) -> bool {
    let message = format!("{err:#}");
    message.contains("not a member of this channel") || message.contains("not a channel member")
}

fn print_generated_key() -> Result<()> {
    let keys = Keys::generate();
    println!("LexeBot key generated.");
    println!("Public key hex: {}", keys.public_key().to_hex());
    println!("Public key npub: {}", keys.public_key().to_bech32()?);
    println!("Private key nsec: {}", keys.secret_key().to_bech32()?);
    println!();
    println!("Keep the nsec private. Use it as SPROUT_BOT_PRIVATE_KEY when running LexeBot.");
    Ok(())
}

struct Config {
    relay_url: String,
    channel_id: String,
    bot_keys: Keys,
    owner_pubkey: PublicKey,
    owner_display_name_override: Option<String>,
    owner_auth_tag: Option<Tag>,
    kudos_bot_pubkey: Option<PublicKey>,
}

impl Config {
    fn from_env() -> Result<Self> {
        let relay_url =
            std::env::var("SPROUT_RELAY_URL").unwrap_or_else(|_| DEFAULT_RELAY_URL.to_string());
        let channel_id = required_env("SPROUT_CHANNEL_ID")?;
        let bot_keys = Keys::parse(&required_env("SPROUT_BOT_PRIVATE_KEY")?)
            .context("SPROUT_BOT_PRIVATE_KEY must be an nsec or hex private key")?;
        let owner_display_name_override = std::env::var("LEXEBOT_OWNER_DISPLAY_NAME")
            .ok()
            .and_then(|value| clean_display_name(&value));
        let kudos_bot_pubkey = optional_pubkey_env("LEXEBOT_KUDOS_BOT_PUBKEY")?;

        let auth_mode =
            std::env::var("SPROUT_BOT_AUTH_MODE").unwrap_or_else(|_| "standalone".to_string());
        let (owner_pubkey, owner_auth_tag) = match auth_mode.as_str() {
            "standalone" => (owner_pubkey_from_env()?, None),
            "owner-attested" => owner_identity_from_attestation(&bot_keys)?,
            other => bail!(
                "SPROUT_BOT_AUTH_MODE must be 'standalone' or 'owner-attested', got {other:?}"
            ),
        };

        Ok(Self {
            relay_url,
            channel_id,
            bot_keys,
            owner_pubkey,
            owner_display_name_override,
            owner_auth_tag,
            kudos_bot_pubkey,
        })
    }
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect_and_authenticate(config: &Config) -> Result<Ws> {
    let parsed = WsUrl::parse(&config.relay_url)?;
    let (mut ws, _) = connect_async(parsed.as_str()).await?;

    let challenge = wait_for_auth_challenge(&mut ws).await?;
    let auth_event = build_auth_event(config, &challenge)?;
    let auth_event_id = auth_event.id.to_hex();

    send_json(&mut ws, json!(["AUTH", auth_event])).await?;
    wait_for_ok(&mut ws, &auth_event_id).await?;
    Ok(ws)
}

fn build_auth_event(config: &Config, challenge: &str) -> Result<Event> {
    let relay_url: Url = config.relay_url.parse()?;
    if let Some(auth_tag) = &config.owner_auth_tag {
        let tags = vec![
            Tag::parse(&["relay", config.relay_url.as_str()])?,
            Tag::parse(&["challenge", challenge])?,
            auth_tag.clone(),
        ];
        Ok(EventBuilder::new(Kind::Authentication, "", tags).sign_with_keys(&config.bot_keys)?)
    } else {
        Ok(EventBuilder::auth(challenge, relay_url).sign_with_keys(&config.bot_keys)?)
    }
}

async fn publish_profile(
    ws: &mut Ws,
    config: &Config,
    display_name: &str,
    bolt12_offer: &str,
) -> Result<()> {
    let profile_event =
        build_profile(config, display_name, bolt12_offer).sign_with_keys(&config.bot_keys)?;
    let profile_event_id = profile_event.id.to_hex();

    send_json(ws, json!(["EVENT", profile_event])).await?;
    wait_for_ok(ws, &profile_event_id).await?;
    eprintln!("published kind:0 profile for {display_name}");
    Ok(())
}

fn build_profile(config: &Config, display_name: &str, bolt12_offer: &str) -> EventBuilder {
    EventBuilder::new(
        Kind::Custom(0),
        json!({
            "display_name": display_name,
            "name": BOT_NAME,
            "picture": BOT_ICON_DATA_URL,
            "about": BOT_ABOUT,
            "lexebot": {
                "version": 1,
                "owner_pubkey": config.owner_pubkey.to_hex(),
                "owner_auth": config.owner_auth_tag.as_ref().map(auth_tag_json),
                "bolt12_offer": bolt12_offer,
            },
        })
        .to_string(),
        [],
    )
}

async fn resolve_bot_display_name(ws: &mut Ws, config: &Config) -> String {
    if let Some(owner_name) = &config.owner_display_name_override {
        return owner_lexebot_display_name(owner_name);
    }

    match fetch_owner_profile_display_name(ws, config.owner_pubkey).await {
        Ok(Some(owner_name)) => owner_lexebot_display_name(&owner_name),
        Ok(None) => {
            eprintln!("owner profile display name not found; publishing default LexeBot profile");
            BOT_DISPLAY_NAME.to_string()
        }
        Err(err) => {
            eprintln!("could not resolve owner profile display name: {err:#}");
            BOT_DISPLAY_NAME.to_string()
        }
    }
}

async fn fetch_owner_profile_display_name(
    ws: &mut Ws,
    owner_pubkey: PublicKey,
) -> Result<Option<String>> {
    let filter = Filter::new()
        .kind(Kind::Custom(0))
        .author(owner_pubkey)
        .limit(1);
    send_json(ws, json!(["REQ", OWNER_PROFILE_SUBSCRIPTION_ID, filter])).await?;

    loop {
        let text = next_text(ws, Duration::from_secs(5)).await?;
        let value: Value = serde_json::from_str(&text)?;
        match value.get(0).and_then(Value::as_str) {
            Some("EVENT")
                if value.get(1).and_then(Value::as_str) == Some(OWNER_PROFILE_SUBSCRIPTION_ID) =>
            {
                let event_value = value
                    .get(2)
                    .ok_or_else(|| anyhow!("EVENT message missing event payload"))?;
                let event = Event::from_json(event_value.to_string())?;
                if event.pubkey == owner_pubkey && event.kind == Kind::Custom(0) {
                    close_subscription(ws, OWNER_PROFILE_SUBSCRIPTION_ID).await?;
                    return Ok(owner_display_name_from_metadata(&event.content));
                }
            }
            Some("EOSE")
                if value.get(1).and_then(Value::as_str) == Some(OWNER_PROFILE_SUBSCRIPTION_ID) =>
            {
                close_subscription(ws, OWNER_PROFILE_SUBSCRIPTION_ID).await?;
                return Ok(None);
            }
            Some("NOTICE") => eprintln!("relay: {value}"),
            Some("CLOSED")
                if value.get(1).and_then(Value::as_str) == Some(OWNER_PROFILE_SUBSCRIPTION_ID) =>
            {
                let reason = value.get(2).and_then(Value::as_str).unwrap_or("");
                eprintln!("relay closed owner profile lookup: {reason}");
                return Ok(None);
            }
            _ => {}
        }
    }
}

fn owner_display_name_from_metadata(content: &str) -> Option<String> {
    let value: Value = serde_json::from_str(content).ok()?;
    ["display_name", "displayName", "name"]
        .iter()
        .find_map(|field| value.get(field).and_then(Value::as_str))
        .and_then(clean_display_name)
}

fn owner_lexebot_display_name(owner_name: &str) -> String {
    clean_display_name(owner_name)
        .map(|name| format!("{name}'s LexeBot"))
        .unwrap_or_else(|| BOT_DISPLAY_NAME.to_string())
}

fn clean_display_name(value: &str) -> Option<String> {
    let collapsed = value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|ch| !ch.is_control())
        .take(80)
        .collect::<String>();
    (!collapsed.is_empty()).then_some(collapsed)
}

fn auth_tag_json(tag: &Tag) -> Value {
    Value::Array(
        tag.as_slice()
            .iter()
            .cloned()
            .map(Value::String)
            .collect::<Vec<_>>(),
    )
}

async fn announce_channel_membership(ws: &mut Ws, config: &Config) -> Result<()> {
    let builder = EventBuilder::new(
        Kind::Custom(9000),
        "",
        [
            Tag::parse(&["h", config.channel_id.as_str()])?,
            Tag::parse(&["p", &config.bot_keys.public_key().to_hex()])?,
            Tag::parse(&["role", "bot"])?,
        ],
    );
    let event = builder.sign_with_keys(&config.bot_keys)?;
    let event_id = event.id.to_hex();

    send_json(ws, json!(["EVENT", event])).await?;
    match wait_for_ok(ws, &event_id).await {
        Ok(()) => eprintln!("announced {BOT_DISPLAY_NAME} as a channel bot member"),
        Err(err) => print_membership_help(config, &format!("could not self-add: {err}")),
    }
    Ok(())
}

async fn subscribe_to_channel(ws: &mut Ws, channel_id: &str) -> Result<()> {
    let filter = Filter::new().kind(Kind::Custom(9)).custom_tag(
        SingleLetterTag::lowercase(Alphabet::H),
        [channel_id.to_string()],
    );
    send_json(ws, json!(["REQ", SUBSCRIPTION_ID, filter])).await
}

async fn handle_relay_text(
    ws: &mut Ws,
    runtime: &Runtime,
    started_at: nostr::Timestamp,
    text: &str,
) -> Result<()> {
    let value: Value = serde_json::from_str(text)?;
    match value.get(0).and_then(Value::as_str) {
        Some("EVENT") => {
            let event_value = value
                .get(2)
                .ok_or_else(|| anyhow!("EVENT message missing event payload"))?;
            let event = Event::from_json(event_value.to_string())?;
            maybe_reply(ws, runtime, started_at, &event).await?;
        }
        Some("EOSE") => {}
        Some("NOTICE") => eprintln!("relay: {value}"),
        Some("CLOSED") => handle_closed(&runtime.config, &value)?,
        Some(other) => eprintln!("ignored relay message type: {other}"),
        None => eprintln!("ignored malformed relay message: {text}"),
    }
    Ok(())
}

fn handle_closed(config: &Config, value: &Value) -> Result<()> {
    let subscription = value.get(1).and_then(Value::as_str).unwrap_or("<unknown>");
    let reason = value.get(2).and_then(Value::as_str).unwrap_or("");
    eprintln!("relay closed subscription {subscription}: {reason}");

    if subscription == SUBSCRIPTION_ID && reason.contains("not a channel member") {
        print_membership_help(config, reason);
        bail!("LexeBot is authenticated on the relay but is not a member of this channel");
    }

    Ok(())
}

fn print_membership_help(config: &Config, reason: &str) {
    let bot_pubkey = config.bot_keys.public_key().to_hex();
    eprintln!("{reason}");
    eprintln!(
        "Owner-attested auth can admit the bot to the relay, but the channel still checks the bot pubkey itself for membership."
    );
    eprintln!("Add this bot pubkey to the channel, then restart LexeBot:");
    eprintln!("  bot pubkey: {bot_pubkey}");
    eprintln!("  sprout add-channel-member \\");
    eprintln!("    --channel {} \\", config.channel_id);
    eprintln!("    --pubkey {bot_pubkey} \\");
    eprintln!("    --role bot");
}

async fn maybe_reply(
    ws: &mut Ws,
    runtime: &Runtime,
    started_at: nostr::Timestamp,
    event: &Event,
) -> Result<()> {
    if event.pubkey == runtime.config.bot_keys.public_key() || event.created_at < started_at {
        return Ok(());
    }
    if !event_mentions_bot(event, &runtime.config) {
        return Ok(());
    }

    let reply = match parse_command(&event.content) {
        Ok(command) if command_authorized(&command, event.pubkey, &runtime.config) => {
            match runtime.lexe.execute(command).await {
                Ok(reply) => reply,
                Err(err) => format!("Lexe command failed: {err:#}"),
            }
        }
        Ok(_) => {
            "Only this LexeBot's configured owner or configured Kudos bot can use wallet commands."
                .to_string()
        }
        Err(err) => format!("Invalid LexeBot command: {err}"),
    };

    let reply_event = build_message(
        &runtime.config.channel_id,
        &reply,
        &reply_mentions(event, &runtime.config),
    )?
    .sign_with_keys(&runtime.config.bot_keys)?;
    let reply_event_id = reply_event.id.to_hex();

    send_json(ws, json!(["EVENT", reply_event])).await?;
    eprintln!("replied to {} with {}", event.id.to_hex(), reply_event_id);
    Ok(())
}

fn command_authorized(command: &BotCommand, sender: PublicKey, config: &Config) -> bool {
    if sender == config.owner_pubkey {
        return true;
    }

    let BotCommand::AutoKudosSend {
        sender: kudos_sender,
        ..
    } = command
    else {
        return false;
    };

    config.kudos_bot_pubkey == Some(sender) && *kudos_sender == config.owner_pubkey
}

fn build_message(channel_id: &str, content: &str, mentions: &[String]) -> Result<EventBuilder> {
    if content.len() > 64 * 1024 {
        bail!("message content exceeds 64 KiB");
    }

    let mut tags = vec![Tag::parse(&["h", channel_id])?];
    for pubkey in mentions.iter().take(50) {
        tags.push(Tag::parse(&["p", pubkey])?);
    }
    Ok(EventBuilder::new(Kind::Custom(9), content, tags))
}

#[derive(Debug, Eq, PartialEq)]
enum BotCommand {
    GetBalance,
    GetBolt12,
    CreateInvoice {
        amount: u64,
    },
    Send {
        amount: u64,
        payable: String,
    },
    AutoKudosSend {
        sender: PublicKey,
        receiver: PublicKey,
        amount: u64,
        payable: String,
    },
}

fn parse_command(content: &str) -> std::result::Result<BotCommand, String> {
    let tokens = content.split_whitespace().collect::<Vec<_>>();
    if !content.trim_start().starts_with('@') {
        return Err(command_help());
    }

    let Some(mention_end_index) = tokens.iter().position(|token| is_bot_mention_token(token))
    else {
        return Err(command_help());
    };
    let [command, rest @ ..] = tokens
        .get(mention_end_index + 1..)
        .filter(|tokens| !tokens.is_empty())
        .ok_or_else(command_help)?
    else {
        return Err(command_help());
    };

    match command.to_ascii_lowercase().as_str() {
        "get" => parse_get_command(rest),
        "create" => parse_create_command(rest),
        "send" => parse_send_command(rest),
        "auto-kudos" => parse_auto_kudos_command(rest),
        _ => Err(command_help()),
    }
}

fn parse_get_command(tokens: &[&str]) -> std::result::Result<BotCommand, String> {
    match tokens {
        ["balance"] => Ok(BotCommand::GetBalance),
        [offer] if offer.eq_ignore_ascii_case("bolt12") => Ok(BotCommand::GetBolt12),
        _ => Err("expected `@LexeBot get balance` or `@LexeBot get BOLT12`".to_string()),
    }
}

fn parse_create_command(tokens: &[&str]) -> std::result::Result<BotCommand, String> {
    match tokens {
        ["invoice", "for", amount] => Ok(BotCommand::CreateInvoice {
            amount: parse_amount_token(amount)?,
        }),
        _ => Err("expected `@LexeBot create invoice for ₿1,000`".to_string()),
    }
}

fn parse_send_command(tokens: &[&str]) -> std::result::Result<BotCommand, String> {
    match tokens {
        [amount, "to", payable] => Ok(BotCommand::Send {
            amount: parse_amount_token(amount)?,
            payable: (*payable).to_string(),
        }),
        _ => Err("expected `@LexeBot send ₿500 to <payment-target>`".to_string()),
    }
}

fn parse_auto_kudos_command(tokens: &[&str]) -> std::result::Result<BotCommand, String> {
    match tokens {
        [sender, receiver, amount, "to", payable] => Ok(BotCommand::AutoKudosSend {
            sender: PublicKey::from_hex(sender)
                .map_err(|_| "auto-kudos sender must be a hex pubkey".to_string())?,
            receiver: PublicKey::from_hex(receiver)
                .map_err(|_| "auto-kudos receiver must be a hex pubkey".to_string())?,
            amount: parse_amount_token(amount)?,
            payable: (*payable).to_string(),
        }),
        _ => Err(
            "expected `@LexeBot auto-kudos <sender-pubkey> <receiver-pubkey> ₿500 to <payment-target>`"
                .to_string(),
        ),
    }
}

fn command_help() -> String {
    "use `@LexeBot get balance`, `@LexeBot get BOLT12`, `@LexeBot create invoice for ₿1,000`, or `@LexeBot send ₿500 to <payment-target>`; personalized names like `@Mat's LexeBot` work too".to_string()
}

fn parse_amount_token(token: &str) -> std::result::Result<u64, String> {
    let Some(amount) = token.strip_prefix('₿') else {
        return Err("amounts must use the ₿ base-unit format, e.g. ₿500".to_string());
    };
    let normalized = amount.replace(',', "");
    if normalized.is_empty() || !normalized.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("amount must be an integer, e.g. ₿1,000".to_string());
    }
    if normalized.len() > 1 && normalized.starts_with('0') {
        return Err("amount must not have leading zeros".to_string());
    }
    normalized
        .parse::<u64>()
        .map_err(|_| "amount is too large".to_string())
}

fn amount_from_base_units(amount: u64) -> Result<Amount> {
    Amount::try_from_sats_u64(amount).context("amount is outside Lexe's supported range")
}

fn format_amount(amount: u64) -> String {
    let digits = amount.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    out.push('₿');
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn is_bot_mention_token(token: &str) -> bool {
    let normalized = token
        .trim_start_matches('@')
        .trim_end_matches(|ch: char| ch.is_ascii_punctuation())
        .to_ascii_lowercase();
    normalized == BOT_NAME || normalized == "lexe-bot"
}

fn event_mentions_bot(event: &Event, config: &Config) -> bool {
    let bot_pubkey = config.bot_keys.public_key().to_hex();
    event.tags.iter().any(|tag| {
        let parts = tag.as_slice();
        parts.first().map(String::as_str) == Some("p")
            && parts.get(1).map(String::as_str) == Some(bot_pubkey.as_str())
    })
}

fn reply_mentions(event: &Event, config: &Config) -> Vec<String> {
    let bot_pubkey = config.bot_keys.public_key().to_hex();
    let mut pubkeys: Vec<String> = Vec::new();

    for tag in event.tags.iter() {
        let parts = tag.as_slice();
        if parts.first().map(String::as_str) != Some("p") {
            continue;
        }
        let Some(pubkey) = parts.get(1) else {
            continue;
        };
        if pubkey == &bot_pubkey || pubkeys.contains(pubkey) {
            continue;
        }
        pubkeys.push(pubkey.clone());
    }

    pubkeys
}

fn owner_identity_from_attestation(bot_keys: &Keys) -> Result<(PublicKey, Option<Tag>)> {
    let (owner, tag_json) = match std::env::var("SPROUT_AUTH_TAG") {
        Ok(value) if !value.trim().is_empty() => {
            let owner = verify_auth_tag(&value, &bot_keys.public_key())
                .context("SPROUT_AUTH_TAG is not valid for SPROUT_BOT_PRIVATE_KEY")?;
            (owner, value)
        }
        _ => {
            let owner_keys = Keys::parse(&required_env("SPROUT_OWNER_PRIVATE_KEY")?)
                .context("SPROUT_OWNER_PRIVATE_KEY must be an nsec or hex private key")?;
            let owner = owner_keys.public_key();
            let tag_json = compute_auth_tag(&owner_keys, &bot_keys.public_key(), "")?;
            (owner, tag_json)
        }
    };

    if let Some(explicit_owner) = optional_pubkey_env("LEXEBOT_OWNER_PUBKEY")? {
        if explicit_owner != owner {
            bail!(
                "LEXEBOT_OWNER_PUBKEY {} does not match attested owner {}",
                explicit_owner.to_hex(),
                owner.to_hex()
            );
        }
    }

    eprintln!("owner-attested auth tag verified; owner={}", owner.to_hex());
    Ok((owner, Some(parse_auth_tag(&tag_json)?)))
}

fn owner_pubkey_from_env() -> Result<PublicKey> {
    if let Some(owner_pubkey) = optional_pubkey_env("LEXEBOT_OWNER_PUBKEY")? {
        return Ok(owner_pubkey);
    }

    let owner_keys = Keys::parse(&required_env("SPROUT_OWNER_PRIVATE_KEY")?)
        .context("SPROUT_OWNER_PRIVATE_KEY must be an nsec or hex private key")?;
    Ok(owner_keys.public_key())
}

fn compute_auth_tag(owner_keys: &Keys, bot_pubkey: &PublicKey, conditions: &str) -> Result<String> {
    if owner_keys.public_key() == *bot_pubkey {
        bail!("owner and bot pubkeys must differ");
    }
    validate_conditions(conditions)?;

    let message = auth_message(bot_pubkey, conditions);
    let signature = owner_keys.sign_schnorr(&message);
    Ok(json!([
        "auth",
        owner_keys.public_key().to_hex(),
        conditions,
        signature.to_string()
    ])
    .to_string())
}

fn verify_auth_tag(tag_json: &str, bot_pubkey: &PublicKey) -> Result<PublicKey> {
    let parts = parse_auth_tag_parts(tag_json)?;
    let owner_pubkey = PublicKey::from_hex(parts.owner_pubkey)
        .context("auth tag owner pubkey is not valid hex")?;
    if owner_pubkey == *bot_pubkey {
        bail!("owner and bot pubkeys must differ");
    }

    let signature = Signature::from_str(&parts.signature)
        .context("auth tag signature is not a valid Schnorr signature")?;
    let message = auth_message(bot_pubkey, &parts.conditions);
    let xonly: &XOnlyPublicKey = &owner_pubkey;
    SECP256K1
        .verify_schnorr(&signature, &message, xonly)
        .context("auth tag signature verification failed")?;
    Ok(owner_pubkey)
}

fn parse_auth_tag(tag_json: &str) -> Result<Tag> {
    let parts = parse_auth_tag_parts(tag_json)?;
    Ok(Tag::parse(&[
        "auth",
        parts.owner_pubkey.as_str(),
        parts.conditions.as_str(),
        parts.signature.as_str(),
    ])?)
}

struct AuthTagParts {
    owner_pubkey: String,
    conditions: String,
    signature: String,
}

fn parse_auth_tag_parts(tag_json: &str) -> Result<AuthTagParts> {
    let value: Value = serde_json::from_str(tag_json).context("auth tag must be valid JSON")?;
    let arr = value
        .as_array()
        .ok_or_else(|| anyhow!("auth tag must be a JSON array"))?;
    if arr.len() != 4 {
        bail!("auth tag must have 4 elements");
    }
    if arr[0].as_str() != Some("auth") {
        bail!("auth tag first element must be \"auth\"");
    }

    let owner_pubkey = arr[1]
        .as_str()
        .ok_or_else(|| anyhow!("auth tag owner pubkey must be a string"))?;
    let conditions = arr[2]
        .as_str()
        .ok_or_else(|| anyhow!("auth tag conditions must be a string"))?;
    let signature = arr[3]
        .as_str()
        .ok_or_else(|| anyhow!("auth tag signature must be a string"))?;

    if !is_lower_hex(owner_pubkey, 64) {
        bail!("auth tag owner pubkey must be 64 lowercase hex chars");
    }
    if !is_lower_hex(signature, 128) {
        bail!("auth tag signature must be 128 lowercase hex chars");
    }
    validate_conditions(conditions)?;

    Ok(AuthTagParts {
        owner_pubkey: owner_pubkey.to_string(),
        conditions: conditions.to_string(),
        signature: signature.to_string(),
    })
}

fn auth_message(bot_pubkey: &PublicKey, conditions: &str) -> SecpMessage {
    let preimage = format!("nostr:agent-auth:{}:{}", bot_pubkey.to_hex(), conditions);
    let digest = Sha256Hash::hash(preimage.as_bytes());
    SecpMessage::from_digest(digest.to_byte_array())
}

fn validate_conditions(conditions: &str) -> Result<()> {
    if conditions.is_empty() {
        return Ok(());
    }
    if conditions.bytes().any(|byte| byte.is_ascii_whitespace()) {
        bail!("auth tag conditions must not contain whitespace");
    }

    for clause in conditions.split('&') {
        if clause.is_empty() {
            bail!("auth tag conditions contain an empty clause");
        }

        if let Some(value) = clause.strip_prefix("kind=") {
            validate_canonical_decimal(value, 0, 65535, "kind")?;
        } else if let Some(value) = clause.strip_prefix("created_at<") {
            validate_canonical_decimal(value, 0, 4_294_967_295, "created_at<")?;
        } else if let Some(value) = clause.strip_prefix("created_at>") {
            validate_canonical_decimal(value, 0, 4_294_967_295, "created_at>")?;
        } else {
            bail!("unsupported auth tag condition clause: {clause:?}");
        }
    }
    Ok(())
}

fn validate_canonical_decimal(value: &str, min: u64, max: u64, label: &str) -> Result<()> {
    if value.is_empty() {
        bail!("{label} value must not be empty");
    }
    if value.len() > 1 && value.starts_with('0') {
        bail!("{label} value must not have leading zeros");
    }
    if !value.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("{label} value must be a decimal integer");
    }

    let parsed = value
        .parse::<u64>()
        .with_context(|| format!("{label} value is out of range"))?;
    if parsed < min || parsed > max {
        bail!("{label} value {parsed} is outside [{min}, {max}]");
    }
    Ok(())
}

fn is_lower_hex(value: &str, expected_len: usize) -> bool {
    value.len() == expected_len
        && value
            .chars()
            .all(|ch| ch.is_ascii_digit() || matches!(ch, 'a'..='f'))
}

async fn wait_for_ok(ws: &mut Ws, event_id: &str) -> Result<()> {
    loop {
        let text = next_text(ws, Duration::from_secs(5)).await?;
        let value: Value = serde_json::from_str(&text)?;
        if value.get(0).and_then(Value::as_str) != Some("OK") {
            continue;
        }
        if value.get(1).and_then(Value::as_str) != Some(event_id) {
            continue;
        }
        if value.get(2).and_then(Value::as_bool) == Some(true) {
            return Ok(());
        }
        let reason = value
            .get(3)
            .and_then(Value::as_str)
            .unwrap_or("unknown reason");
        bail!("relay rejected event {event_id}: {reason}");
    }
}

async fn wait_for_auth_challenge(ws: &mut Ws) -> Result<String> {
    loop {
        let text = next_text(ws, Duration::from_secs(5)).await?;
        let value: Value = serde_json::from_str(&text)?;
        if value.get(0).and_then(Value::as_str) == Some("AUTH") {
            return value
                .get(1)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| anyhow!("AUTH message missing challenge"));
        }
    }
}

async fn next_text(ws: &mut Ws, timeout: Duration) -> Result<String> {
    loop {
        let message = tokio::time::timeout(timeout, ws.next())
            .await
            .context("timed out waiting for relay message")?
            .ok_or_else(|| anyhow!("relay closed the WebSocket"))??;
        match message {
            Message::Text(text) => return Ok(text.to_string()),
            Message::Ping(bytes) => ws.send(Message::Pong(bytes)).await?,
            Message::Close(frame) => bail!("relay closed connection: {frame:?}"),
            _ => {}
        }
    }
}

async fn send_json(ws: &mut Ws, value: Value) -> Result<()> {
    ws.send(Message::Text(value.to_string().into())).await?;
    Ok(())
}

async fn close_subscription(ws: &mut Ws, subscription_id: &str) -> Result<()> {
    send_json(ws, json!(["CLOSE", subscription_id])).await
}

fn required_env(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("{name} is required"))
}

fn optional_u64_env(name: &str) -> Result<Option<u64>> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| {
            value
                .parse::<u64>()
                .with_context(|| format!("{name} must be an integer"))
        })
        .transpose()
}

fn optional_u32_env(name: &str) -> Result<Option<u32>> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| {
            value
                .parse::<u32>()
                .with_context(|| format!("{name} must be an integer"))
        })
        .transpose()
}

fn optional_pubkey_env(name: &str) -> Result<Option<PublicKey>> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| {
            PublicKey::from_hex(&value).with_context(|| format!("{name} must be a hex pubkey"))
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_balance_command() {
        assert_eq!(
            parse_command("@LexeBot get balance"),
            Ok(BotCommand::GetBalance)
        );
        assert_eq!(
            parse_command("@Mat's LexeBot get balance"),
            Ok(BotCommand::GetBalance)
        );
    }

    #[test]
    fn parses_bolt12_command() {
        assert_eq!(
            parse_command("@LexeBot get BOLT12"),
            Ok(BotCommand::GetBolt12)
        );
        assert_eq!(
            parse_command("@lexebot get bolt12"),
            Ok(BotCommand::GetBolt12)
        );
    }

    #[test]
    fn parses_create_invoice_command() {
        assert_eq!(
            parse_command("@lexebot create invoice for ₿1,000"),
            Ok(BotCommand::CreateInvoice { amount: 1_000 })
        );
    }

    #[test]
    fn parses_send_command() {
        assert_eq!(
            parse_command("@lexebot send ₿500 to lno1abc"),
            Ok(BotCommand::Send {
                amount: 500,
                payable: "lno1abc".to_string()
            })
        );
    }

    #[test]
    fn parses_auto_kudos_command() {
        let sender = Keys::generate().public_key();
        let receiver = Keys::generate().public_key();
        assert_eq!(
            parse_command(&format!(
                "@lexebot auto-kudos {} {} ₿500 to lno1abc",
                sender.to_hex(),
                receiver.to_hex()
            )),
            Ok(BotCommand::AutoKudosSend {
                sender,
                receiver,
                amount: 500,
                payable: "lno1abc".to_string()
            })
        );
    }

    #[test]
    fn rejects_non_strict_commands() {
        assert!(parse_command("hey @lexebot get balance").is_err());
        assert!(parse_command("@lexebot balance").is_err());
        assert!(parse_command("hey @Mat's LexeBot get balance").is_err());
        assert!(parse_command("@Mat's wallet get balance").is_err());
        assert!(parse_command("@lexebot send 500 to lno1abc").is_err());
        assert!(parse_command("@lexebot send ₿500").is_err());
        assert!(parse_command("@lexebot create invoice for ₿001").is_err());
    }

    #[test]
    fn formats_base_unit_amounts() {
        assert_eq!(format_amount(0), "₿0");
        assert_eq!(format_amount(500), "₿500");
        assert_eq!(format_amount(1_000), "₿1,000");
        assert_eq!(format_amount(12_345_678), "₿12,345,678");
    }

    #[test]
    fn bot_p_tag_and_owner_are_independent_checks() {
        let bot_keys = Keys::generate();
        let owner_keys = Keys::generate();
        let event = EventBuilder::new(
            Kind::Custom(9),
            "@LexeBot get balance",
            [Tag::parse(&["p", &bot_keys.public_key().to_hex()]).unwrap()],
        )
        .sign_with_keys(&owner_keys)
        .unwrap();
        let config = Config {
            relay_url: DEFAULT_RELAY_URL.to_string(),
            channel_id: "test-channel".to_string(),
            bot_keys,
            owner_pubkey: owner_keys.public_key(),
            owner_display_name_override: None,
            owner_auth_tag: None,
            kudos_bot_pubkey: None,
        };

        assert!(event_mentions_bot(&event, &config));
        assert_eq!(event.pubkey, config.owner_pubkey);
    }

    #[test]
    fn text_mention_without_bot_p_tag_does_not_trigger() {
        let bot_keys = Keys::generate();
        let owner_keys = Keys::generate();
        let event = EventBuilder::new(Kind::Custom(9), "@LexeBot get balance", [])
            .sign_with_keys(&owner_keys)
            .unwrap();
        let config = Config {
            relay_url: DEFAULT_RELAY_URL.to_string(),
            channel_id: "test-channel".to_string(),
            bot_keys,
            owner_pubkey: owner_keys.public_key(),
            owner_display_name_override: None,
            owner_auth_tag: None,
            kudos_bot_pubkey: None,
        };

        assert!(!event_mentions_bot(&event, &config));
    }

    #[test]
    fn auto_kudos_requires_configured_kudos_bot_and_owner_sender() {
        let owner_keys = Keys::generate();
        let bot_keys = Keys::generate();
        let kudos_keys = Keys::generate();
        let receiver = Keys::generate().public_key();
        let config = Config {
            relay_url: DEFAULT_RELAY_URL.to_string(),
            channel_id: "test-channel".to_string(),
            bot_keys,
            owner_pubkey: owner_keys.public_key(),
            owner_display_name_override: None,
            owner_auth_tag: None,
            kudos_bot_pubkey: Some(kudos_keys.public_key()),
        };
        let command = BotCommand::AutoKudosSend {
            sender: owner_keys.public_key(),
            receiver,
            amount: 1,
            payable: "lno1abc".to_string(),
        };
        assert!(command_authorized(
            &command,
            kudos_keys.public_key(),
            &config
        ));
        assert!(!command_authorized(
            &command,
            Keys::generate().public_key(),
            &config
        ));
    }

    #[test]
    fn owner_auth_tag_round_trips() {
        let owner_keys = Keys::generate();
        let bot_keys = Keys::generate();
        let tag_json = compute_auth_tag(&owner_keys, &bot_keys.public_key(), "").unwrap();

        let owner = verify_auth_tag(&tag_json, &bot_keys.public_key()).unwrap();
        let tag = parse_auth_tag(&tag_json).unwrap();

        assert_eq!(owner, owner_keys.public_key());
        assert_eq!(tag.as_slice().first().map(String::as_str), Some("auth"));
    }

    #[test]
    fn owner_lexebot_display_name_uses_owner_profile_name() {
        assert_eq!(owner_lexebot_display_name("Mat"), "Mat's LexeBot");
        assert_eq!(
            owner_lexebot_display_name("  Mat   Balez  "),
            "Mat Balez's LexeBot"
        );
        assert_eq!(owner_lexebot_display_name(" \n\t "), "LexeBot");
    }

    #[test]
    fn owner_display_name_from_metadata_prefers_display_name() {
        assert_eq!(
            owner_display_name_from_metadata(
                r#"{"name":"mattyb","display_name":"Mat","displayName":"Mat B"}"#
            ),
            Some("Mat".to_string())
        );
        assert_eq!(
            owner_display_name_from_metadata(r#"{"name":"mattyb"}"#),
            Some("mattyb".to_string())
        );
        assert_eq!(owner_display_name_from_metadata("not json"), None);
    }

    #[test]
    fn reconnect_delay_is_bounded() {
        assert_eq!(
            next_reconnect_delay(Duration::from_secs(1)),
            Duration::from_secs(2)
        );
        assert_eq!(
            next_reconnect_delay(Duration::from_secs(60)),
            Duration::from_secs(60)
        );
    }
}
