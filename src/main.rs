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
use nostr::nips::nip44::{self, Version as Nip44Version};
use nostr::{
    Alphabet, Event, EventBuilder, Filter, JsonUtil, Keys, Kind, PublicKey, SingleLetterTag, Tag,
    ToBech32, Url, SECP256K1,
};
use serde_json::{json, Value};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use url::Url as WsUrl;

const DEFAULT_RELAY_URL: &str = "ws://localhost:3000";
const AUTO_KUDOS_SUBSCRIPTION_ID: &str = "lexebot-auto-kudos";
const OWNER_DM_SUBSCRIPTION_ID: &str = "lexebot-owner-dm";
const OWNER_PROFILE_SUBSCRIPTION_ID: &str = "lexebot-owner-profile";
const AUTO_KUDOS_COMMAND_KIND: u16 = 21_000;
const AUTO_KUDOS_RESULT_KIND: u16 = 21_001;
const AUTO_KUDOS_PROTOCOL_VERSION: u64 = 2;
const PRESENCE_UPDATE_KIND: u16 = 20_001;
const PRESENCE_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const BOT_NAME: &str = "lexebot";
const BOT_DISPLAY_NAME: &str = "LexeBot";
const BOT_ABOUT: &str = "A deterministic Sprout bot that lets one owner control a Lexe wallet.";
const BOT_ICON_DATA_URL: &str = "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 128 128'%3E%3Crect width='128' height='128' rx='28' fill='%23111618'/%3E%3Cpath d='M35 84 64 28l29 56H76L64 58 52 84H35Z' fill='%23f4c542'/%3E%3Cpath d='M43 96h42' stroke='%238fd4ff' stroke-width='10' stroke-linecap='round'/%3E%3C/svg%3E";
const INITIAL_RECONNECT_DELAY: Duration = Duration::from_secs(1);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(60);
const AUTO_KUDOS_PAYMENT_TIMEOUT: Duration = Duration::from_secs(25);

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::args().any(|arg| arg == "--generate-key") {
        print_generated_key()?;
        return Ok(());
    }
    if std::env::args().any(|arg| arg == "--print-pubkey") {
        print_configured_pubkey()?;
        return Ok(());
    }
    if let Some(channel_id) = one_shot_arg("--send-install-welcome")? {
        let config = Config::from_env()?;
        send_install_welcome_once(&config, &channel_id).await?;
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

    async fn execute(&self, command: BotCommand) -> Result<Option<String>> {
        match command {
            BotCommand::GetBalance => self.get_balance().await.map(Some),
            BotCommand::GetBolt12 => self.get_bolt12().await.map(Some),
            BotCommand::CreateInvoice { amount } => self.create_invoice(amount).await.map(Some),
            BotCommand::Send { amount, payable } => {
                self.send_payment(amount, payable).await.map(Some)
            }
            BotCommand::AutoKudosSend {
                amount, payable, ..
            } => {
                self.send_payment(amount, payable).await?;
                Ok(None)
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
    publish_presence(&mut ws, &runtime.config, "online").await?;
    subscribe_to_owner_dm_channel(&mut ws, &runtime.config).await?;
    subscribe_to_auto_kudos_inbox(&mut ws, &runtime.config).await?;

    let started_at = nostr::Timestamp::now();
    let mut presence_interval = tokio::time::interval(PRESENCE_REFRESH_INTERVAL);
    presence_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    presence_interval.tick().await;

    eprintln!("listening for owner DM commands and encrypted auto-kudos commands");
    if runtime.config.channel_ids.len() > 1 {
        eprintln!(
            "ignoring {} extra configured plaintext channel(s); manual wallet commands only run in the owner DM",
            runtime.config.channel_ids.len() - 1
        );
    }

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                let _ = publish_presence(&mut ws, &runtime.config, "offline").await;
                eprintln!("shutting down");
                return Ok(());
            }
            _ = presence_interval.tick() => {
                publish_presence(&mut ws, &runtime.config, "online").await?;
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

fn print_configured_pubkey() -> Result<()> {
    let keys = Keys::parse(&required_env("SPROUT_BOT_PRIVATE_KEY")?)
        .context("SPROUT_BOT_PRIVATE_KEY must be an nsec or hex private key")?;
    println!("{}", keys.public_key().to_hex());
    Ok(())
}

fn one_shot_arg(flag: &str) -> Result<Option<String>> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == flag {
            return args
                .next()
                .map(Some)
                .ok_or_else(|| anyhow!("{flag} requires a value"));
        }
    }
    Ok(None)
}

struct Config {
    relay_url: String,
    channel_ids: Vec<String>,
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
        let channel_ids = channel_ids_from_env()?;
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
            channel_ids,
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
                "version": 2,
                "owner_pubkey": config.owner_pubkey.to_hex(),
                "owner_auth": config.owner_auth_tag.as_ref().map(auth_tag_json),
                "bolt12_offer": bolt12_offer,
                "auto_kudos": {
                    "encrypted": "nip44",
                    "command_kind": AUTO_KUDOS_COMMAND_KIND,
                    "result_kind": AUTO_KUDOS_RESULT_KIND,
                },
                "manual_commands": {
                    "transport": "sprout-dm-kind-9",
                    "encrypted": false,
                    "visibility": "scoped to DM participants; readable by the relay operator",
                },
            },
        })
        .to_string(),
        [],
    )
}

async fn publish_presence(ws: &mut Ws, config: &Config, status: &str) -> Result<()> {
    let event = EventBuilder::new(Kind::Custom(PRESENCE_UPDATE_KIND), status, [])
        .sign_with_keys(&config.bot_keys)?;
    send_json(ws, json!(["EVENT", event])).await
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
        .map(|name| format!("LexeBot[{}]", name.replace(' ', "")))
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

async fn subscribe_to_owner_dm_channel(ws: &mut Ws, config: &Config) -> Result<()> {
    let Some(channel_id) = config.channel_ids.first() else {
        return Ok(());
    };
    let filter = Filter::new().kind(Kind::Custom(9)).custom_tag(
        SingleLetterTag::lowercase(Alphabet::H),
        [channel_id.to_string()],
    );
    send_json(ws, json!(["REQ", OWNER_DM_SUBSCRIPTION_ID, filter])).await
}

async fn subscribe_to_auto_kudos_inbox(ws: &mut Ws, config: &Config) -> Result<()> {
    let bot_pubkey = config.bot_keys.public_key().to_hex();
    let filter = Filter::new()
        .kind(Kind::Ephemeral(AUTO_KUDOS_COMMAND_KIND))
        .custom_tag(SingleLetterTag::lowercase(Alphabet::P), [bot_pubkey]);
    send_json(ws, json!(["REQ", AUTO_KUDOS_SUBSCRIPTION_ID, filter])).await
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
        Some("OK") => {}
        Some("EOSE") => {}
        Some("NOTICE") => eprintln!("relay: {value}"),
        Some("CLOSED") => handle_closed(&value)?,
        Some(other) => eprintln!("ignored relay message type: {other}"),
        None => eprintln!("ignored malformed relay message: {text}"),
    }
    Ok(())
}

fn handle_closed(value: &Value) -> Result<()> {
    let subscription = value.get(1).and_then(Value::as_str).unwrap_or("<unknown>");
    let reason = value.get(2).and_then(Value::as_str).unwrap_or("");
    eprintln!("relay closed subscription {subscription}: {reason}");
    Ok(())
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
    if event.kind == Kind::Ephemeral(AUTO_KUDOS_COMMAND_KIND) {
        if event_addresses_bot(event, &runtime.config) {
            handle_encrypted_auto_kudos(ws, runtime, event).await?;
        }
        return Ok(());
    }
    if plaintext_owner_dm_addresses_bot(event, &runtime.config) {
        handle_plaintext_owner_dm(ws, runtime, event).await?;
    }
    Ok(())
}

async fn handle_plaintext_owner_dm(ws: &mut Ws, runtime: &Runtime, event: &Event) -> Result<()> {
    let command_event_id = event.id.to_hex();
    let reply = match parse_command(&event.content) {
        Ok(command) if command_authorized(&command, event.pubkey, &runtime.config) => {
            match runtime.lexe.execute(command).await {
                Ok(Some(reply)) => reply,
                Ok(None) => return Ok(()),
                Err(err) => format!("Lexe command failed: {err:#}"),
            }
        }
        Ok(_) => {
            eprintln!("ignored unauthorized plaintext owner DM command {command_event_id}");
            return Ok(());
        }
        Err(err) => format!("Invalid LexeBot command: {err}"),
    };

    let Some(channel_id) = owner_dm_channel_id(&runtime.config) else {
        return Ok(());
    };
    let reply_event = build_message(channel_id, &reply, &[runtime.config.owner_pubkey.to_hex()])?
        .sign_with_keys(&runtime.config.bot_keys)?;
    let reply_event_id = reply_event.id.to_hex();

    send_json(ws, json!(["EVENT", reply_event])).await?;
    eprintln!("replied to plaintext owner DM {command_event_id} with {reply_event_id}");
    Ok(())
}

async fn handle_encrypted_auto_kudos(ws: &mut Ws, runtime: &Runtime, event: &Event) -> Result<()> {
    let command_event_id = event.id.to_hex();
    let command = match encrypted_auto_kudos_command(&runtime.config, event) {
        Ok(command) if command_authorized(&command, event.pubkey, &runtime.config) => command,
        Ok(_) => {
            send_auto_kudos_result(ws, &runtime.config, event, false).await?;
            eprintln!("rejected unauthorized encrypted auto-kudos command {command_event_id}");
            return Ok(());
        }
        Err(err) => {
            send_auto_kudos_result(ws, &runtime.config, event, false).await?;
            eprintln!("invalid encrypted auto-kudos command {command_event_id}: {err:#}");
            return Ok(());
        }
    };
    let receipt = auto_kudos_receipt(&command);

    match tokio::time::timeout(AUTO_KUDOS_PAYMENT_TIMEOUT, runtime.lexe.execute(command)).await {
        Ok(Ok(_)) => {
            if let Some(receipt) = receipt {
                if let Err(err) = send_owner_dm_message(ws, &runtime.config, &receipt).await {
                    eprintln!("could not send auto-kudos owner DM receipt: {err:#}");
                }
            }
            send_auto_kudos_result(ws, &runtime.config, event, true).await?;
            eprintln!("processed encrypted auto-kudos command {command_event_id}");
        }
        Ok(Err(err)) => {
            send_auto_kudos_result(ws, &runtime.config, event, false).await?;
            eprintln!("encrypted auto-kudos command {command_event_id} failed: {err:#}");
        }
        Err(_) => {
            send_auto_kudos_result(ws, &runtime.config, event, false).await?;
            eprintln!(
                "encrypted auto-kudos command {command_event_id} timed out after {} seconds",
                AUTO_KUDOS_PAYMENT_TIMEOUT.as_secs()
            );
        }
    }

    Ok(())
}

async fn send_owner_dm_message(ws: &mut Ws, config: &Config, content: &str) -> Result<()> {
    let Some(channel_id) = owner_dm_channel_id(config) else {
        return Ok(());
    };
    let event = build_message(channel_id, content, &[config.owner_pubkey.to_hex()])?
        .sign_with_keys(&config.bot_keys)?;
    send_json(ws, json!(["EVENT", event])).await
}

async fn send_auto_kudos_result(
    ws: &mut Ws,
    config: &Config,
    request: &Event,
    sent: bool,
) -> Result<()> {
    let content = json!({
        "version": AUTO_KUDOS_PROTOCOL_VERSION,
        "type": "auto-kudos-result",
        "request_event_id": request.id.to_hex(),
        "status": if sent { "sent" } else { "failed" },
    })
    .to_string();
    let encrypted = nip44::encrypt(
        config.bot_keys.secret_key(),
        &request.pubkey,
        content,
        Nip44Version::default(),
    )?;
    let event = EventBuilder::new(
        Kind::Ephemeral(AUTO_KUDOS_RESULT_KIND),
        encrypted,
        [Tag::parse(&["p", &request.pubkey.to_hex()])?],
    )
    .sign_with_keys(&config.bot_keys)?;
    send_json(ws, json!(["EVENT", event])).await
}

async fn send_install_welcome_once(config: &Config, channel_id: &str) -> Result<()> {
    let mut ws = connect_and_authenticate(config).await?;
    let event = build_message(
        channel_id,
        &install_welcome_message(),
        &[config.owner_pubkey.to_hex()],
    )?
    .sign_with_keys(&config.bot_keys)?;
    let event_id = event.id.to_hex();

    send_json(&mut ws, json!(["EVENT", event])).await?;
    wait_for_ok(&mut ws, &event_id).await?;
    Ok(())
}

fn install_welcome_message() -> String {
    format!(
        "LexeBot v{} is installed and ready.\n\n\
Supported commands:\n\
get balance\n\
get BOLT12\n\
create invoice for ₿1,000\n\
send ₿500 to <payment-target>",
        env!("CARGO_PKG_VERSION")
    )
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

fn encrypted_auto_kudos_command(config: &Config, event: &Event) -> Result<BotCommand> {
    let decrypted = nip44::decrypt(
        config.bot_keys.secret_key(),
        &event.pubkey,
        event.content.as_str(),
    )?;
    let value: Value = serde_json::from_str(&decrypted)?;

    if value.get("version").and_then(Value::as_u64) != Some(AUTO_KUDOS_PROTOCOL_VERSION) {
        bail!("unsupported auto-kudos command version");
    }
    if value.get("type").and_then(Value::as_str) != Some("auto-kudos") {
        bail!("unexpected auto-kudos command type");
    }

    let sender = value
        .get("sender_pubkey")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("auto-kudos command missing sender_pubkey"))
        .and_then(|pubkey| {
            PublicKey::from_hex(pubkey).context("auto-kudos sender_pubkey is invalid")
        })?;
    let receiver = value
        .get("receiver_pubkey")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("auto-kudos command missing receiver_pubkey"))
        .and_then(|pubkey| {
            PublicKey::from_hex(pubkey).context("auto-kudos receiver_pubkey is invalid")
        })?;
    let amount = value
        .get("amount")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("auto-kudos command missing amount"))?;
    let payable = value
        .get("payable")
        .and_then(Value::as_str)
        .filter(|payable| !payable.trim().is_empty())
        .ok_or_else(|| anyhow!("auto-kudos command missing payable"))?
        .to_string();
    let receiver_display_name = auto_kudos_receiver_label(
        value.get("receiver_display_name").and_then(Value::as_str),
        receiver,
    );

    Ok(BotCommand::AutoKudosSend {
        sender,
        receiver,
        receiver_display_name,
        amount,
        payable,
    })
}

fn auto_kudos_receiver_label(value: Option<&str>, receiver: PublicKey) -> String {
    value
        .and_then(clean_auto_kudos_receiver_label)
        .unwrap_or_else(|| format!("@{}", receiver.to_hex().chars().take(8).collect::<String>()))
}

fn clean_auto_kudos_receiver_label(value: &str) -> Option<String> {
    let value = value.trim();
    let label = value.strip_prefix('@').unwrap_or(value);
    let label = label.trim_end_matches([',', '.', '!', '?', ':', ';']);
    if label.is_empty()
        || label.len() > 80
        || label
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return None;
    }
    Some(format!("@{label}"))
}

fn auto_kudos_receipt(command: &BotCommand) -> Option<String> {
    let BotCommand::AutoKudosSend {
        receiver_display_name,
        amount,
        ..
    } = command
    else {
        return None;
    };
    Some(format!(
        "{} kudos sent to {}",
        format_amount(*amount),
        receiver_display_name
    ))
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
        receiver_display_name: String,
        amount: u64,
        payable: String,
    },
}

fn parse_command(content: &str) -> std::result::Result<BotCommand, String> {
    let tokens = content.split_whitespace().collect::<Vec<_>>();
    let command_start_index = if content.trim_start().starts_with('@') {
        let Some(mention_end_index) = tokens.iter().position(|token| is_bot_mention_token(token))
        else {
            return Err(command_help());
        };
        mention_end_index + 1
    } else {
        0
    };
    let [command, rest @ ..] = tokens
        .get(command_start_index..)
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
        _ => Err("expected `get balance` or `get BOLT12`".to_string()),
    }
}

fn parse_create_command(tokens: &[&str]) -> std::result::Result<BotCommand, String> {
    match tokens {
        ["invoice", "for", amount] => Ok(BotCommand::CreateInvoice {
            amount: parse_amount_token(amount)?,
        }),
        _ => Err("expected `create invoice for ₿1,000`".to_string()),
    }
}

fn parse_send_command(tokens: &[&str]) -> std::result::Result<BotCommand, String> {
    match tokens {
        [amount, "to", payable] => Ok(BotCommand::Send {
            amount: parse_amount_token(amount)?,
            payable: (*payable).to_string(),
        }),
        _ => Err("expected `send ₿500 to <payment-target>`".to_string()),
    }
}

fn parse_auto_kudos_command(tokens: &[&str]) -> std::result::Result<BotCommand, String> {
    match tokens {
        [sender, receiver, amount, "to", payable] => Ok(BotCommand::AutoKudosSend {
            sender: PublicKey::from_hex(sender)
                .map_err(|_| "auto-kudos sender must be a hex pubkey".to_string())?,
            receiver: PublicKey::from_hex(receiver)
                .map_err(|_| "auto-kudos receiver must be a hex pubkey".to_string())?,
            receiver_display_name: format!(
                "@{}",
                receiver.chars().take(8).collect::<String>()
            ),
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
    "use `get balance`, `get BOLT12`, `create invoice for ₿1,000`, or `send ₿500 to <payment-target>`; explicit mentions like `@LexeBot get balance` work too".to_string()
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
        .trim_end_matches([',', '.', '!', '?', ':', ';'])
        .to_ascii_lowercase();
    normalized == BOT_NAME
        || normalized == "lexe-bot"
        || (normalized.starts_with("lexebot[") && normalized.ends_with(']'))
}

fn event_mentions_bot(event: &Event, config: &Config) -> bool {
    let bot_pubkey = config.bot_keys.public_key().to_hex();
    event.tags.iter().any(|tag| {
        let parts = tag.as_slice();
        parts.first().map(String::as_str) == Some("p")
            && parts.get(1).map(String::as_str) == Some(bot_pubkey.as_str())
    })
}

fn event_addresses_bot(event: &Event, config: &Config) -> bool {
    if event.kind == Kind::Ephemeral(AUTO_KUDOS_COMMAND_KIND) {
        return event_mentions_bot(event, config);
    }
    false
}

fn plaintext_owner_dm_addresses_bot(event: &Event, config: &Config) -> bool {
    event.kind == Kind::Custom(9)
        && event.pubkey == config.owner_pubkey
        && owner_dm_channel_id(config)
            .is_some_and(|channel_id| event_has_channel(event, channel_id))
}

fn owner_dm_channel_id(config: &Config) -> Option<&str> {
    config.channel_ids.first().map(String::as_str)
}

fn event_has_channel(event: &Event, expected_channel_id: &str) -> bool {
    event.tags.iter().any(|tag| {
        let parts = tag.as_slice();
        parts.first().map(String::as_str) == Some("h")
            && parts.get(1).map(String::as_str) == Some(expected_channel_id)
    })
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
    wait_for_ok_message(ws, event_id).await.map(|_| ())
}

async fn wait_for_ok_message(ws: &mut Ws, event_id: &str) -> Result<String> {
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
            return Ok(value
                .get(3)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string());
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

fn channel_ids_from_env() -> Result<Vec<String>> {
    let value = std::env::var("SPROUT_CHANNEL_IDS")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("SPROUT_CHANNEL_ID")
                .ok()
                .filter(|value| !value.trim().is_empty())
        });
    Ok(channel_ids_from_value(value.as_deref()))
}

fn channel_ids_from_value(value: Option<&str>) -> Vec<String> {
    let mut channel_ids = Vec::new();

    let Some(value) = value else {
        return channel_ids;
    };

    for raw in value.split([',', '\n', ' ', '\t']) {
        let channel_id = raw.trim();
        if channel_id.is_empty() || channel_ids.iter().any(|existing| existing == channel_id) {
            continue;
        }
        channel_ids.push(channel_id.to_string());
    }

    channel_ids
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
        assert_eq!(parse_command("get balance"), Ok(BotCommand::GetBalance));
        assert_eq!(
            parse_command("@LexeBot get balance"),
            Ok(BotCommand::GetBalance)
        );
        assert_eq!(
            parse_command("@Mat's LexeBot get balance"),
            Ok(BotCommand::GetBalance)
        );
        assert_eq!(
            parse_command("@LexeBot[Mat] get balance"),
            Ok(BotCommand::GetBalance)
        );
    }

    #[test]
    fn parses_bolt12_command() {
        assert_eq!(parse_command("get BOLT12"), Ok(BotCommand::GetBolt12));
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
            parse_command("create invoice for ₿1,000"),
            Ok(BotCommand::CreateInvoice { amount: 1_000 })
        );
        assert_eq!(
            parse_command("@lexebot create invoice for ₿1,000"),
            Ok(BotCommand::CreateInvoice { amount: 1_000 })
        );
    }

    #[test]
    fn parses_send_command() {
        assert_eq!(
            parse_command("send ₿500 to lno1abc"),
            Ok(BotCommand::Send {
                amount: 500,
                payable: "lno1abc".to_string()
            })
        );
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
                receiver_display_name: format!(
                    "@{}",
                    receiver.to_hex().chars().take(8).collect::<String>()
                ),
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
            channel_ids: vec!["test-channel".to_string()],
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
            channel_ids: vec!["test-channel".to_string()],
            bot_keys,
            owner_pubkey: owner_keys.public_key(),
            owner_display_name_override: None,
            owner_auth_tag: None,
            kudos_bot_pubkey: None,
        };

        assert!(!event_mentions_bot(&event, &config));
        assert!(!event_addresses_bot(&event, &config));
    }

    #[test]
    fn plaintext_owner_dm_command_triggers_without_mention() {
        let bot_keys = Keys::generate();
        let owner_keys = Keys::generate();
        let event = EventBuilder::new(
            Kind::Custom(9),
            "get balance",
            [Tag::parse(&["h", "control-channel"]).unwrap()],
        )
        .sign_with_keys(&owner_keys)
        .unwrap();
        let config = Config {
            relay_url: DEFAULT_RELAY_URL.to_string(),
            channel_ids: vec!["control-channel".to_string()],
            bot_keys,
            owner_pubkey: owner_keys.public_key(),
            owner_display_name_override: None,
            owner_auth_tag: None,
            kudos_bot_pubkey: None,
        };

        assert!(!event_mentions_bot(&event, &config));
        assert!(plaintext_owner_dm_addresses_bot(&event, &config));
    }

    #[test]
    fn plaintext_user_commands_only_trigger_in_owner_dm() {
        let bot_keys = Keys::generate();
        let owner_keys = Keys::generate();
        let stranger_keys = Keys::generate();
        let control_event = EventBuilder::new(
            Kind::Custom(9),
            "get balance",
            [Tag::parse(&["h", "control-channel"]).unwrap()],
        )
        .sign_with_keys(&owner_keys)
        .unwrap();
        let other_event = EventBuilder::new(
            Kind::Custom(9),
            "get balance",
            [Tag::parse(&["h", "other-channel"]).unwrap()],
        )
        .sign_with_keys(&owner_keys)
        .unwrap();
        let mentioned_other_event = EventBuilder::new(
            Kind::Custom(9),
            "@LexeBot get balance",
            [
                Tag::parse(&["h", "other-channel"]).unwrap(),
                Tag::parse(&["p", &bot_keys.public_key().to_hex()]).unwrap(),
            ],
        )
        .sign_with_keys(&owner_keys)
        .unwrap();
        let stranger_control_event = EventBuilder::new(
            Kind::Custom(9),
            "get balance",
            [Tag::parse(&["h", "control-channel"]).unwrap()],
        )
        .sign_with_keys(&stranger_keys)
        .unwrap();
        let config = Config {
            relay_url: DEFAULT_RELAY_URL.to_string(),
            channel_ids: vec!["control-channel".to_string(), "other-channel".to_string()],
            bot_keys,
            owner_pubkey: owner_keys.public_key(),
            owner_display_name_override: None,
            owner_auth_tag: None,
            kudos_bot_pubkey: None,
        };

        assert!(!event_addresses_bot(&control_event, &config));
        assert!(plaintext_owner_dm_addresses_bot(&control_event, &config));
        assert!(!plaintext_owner_dm_addresses_bot(&other_event, &config));
        assert!(!plaintext_owner_dm_addresses_bot(
            &mentioned_other_event,
            &config
        ));
        assert!(!plaintext_owner_dm_addresses_bot(
            &stranger_control_event,
            &config
        ));
    }

    #[test]
    fn encrypted_auto_kudos_still_uses_bot_mention_tag() {
        let bot_keys = Keys::generate();
        let kudos_keys = Keys::generate();
        let event = EventBuilder::new(
            Kind::Ephemeral(AUTO_KUDOS_COMMAND_KIND),
            "encrypted-payload",
            [Tag::parse(&["p", &bot_keys.public_key().to_hex()]).unwrap()],
        )
        .sign_with_keys(&kudos_keys)
        .unwrap();
        let config = Config {
            relay_url: DEFAULT_RELAY_URL.to_string(),
            channel_ids: vec!["control-channel".to_string()],
            bot_keys,
            owner_pubkey: Keys::generate().public_key(),
            owner_display_name_override: None,
            owner_auth_tag: None,
            kudos_bot_pubkey: Some(kudos_keys.public_key()),
        };

        assert!(event_addresses_bot(&event, &config));
    }

    #[test]
    fn no_channels_can_be_configured_for_auto_kudos_only_mode() {
        assert_eq!(channel_ids_from_value(None), Vec::<String>::new());
        assert_eq!(
            channel_ids_from_value(Some("  \n\t ")),
            Vec::<String>::new()
        );
    }

    #[test]
    fn auto_kudos_requires_configured_kudos_bot_and_owner_sender() {
        let owner_keys = Keys::generate();
        let bot_keys = Keys::generate();
        let kudos_keys = Keys::generate();
        let receiver = Keys::generate().public_key();
        let config = Config {
            relay_url: DEFAULT_RELAY_URL.to_string(),
            channel_ids: vec!["test-channel".to_string()],
            bot_keys,
            owner_pubkey: owner_keys.public_key(),
            owner_display_name_override: None,
            owner_auth_tag: None,
            kudos_bot_pubkey: Some(kudos_keys.public_key()),
        };
        let command = BotCommand::AutoKudosSend {
            sender: owner_keys.public_key(),
            receiver,
            receiver_display_name: "@Receiver".to_string(),
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
    fn encrypted_auto_kudos_command_round_trips() {
        let bot_keys = Keys::generate();
        let owner_keys = Keys::generate();
        let kudos_keys = Keys::generate();
        let receiver = Keys::generate().public_key();
        let payload = json!({
            "version": AUTO_KUDOS_PROTOCOL_VERSION,
            "type": "auto-kudos",
            "sender_pubkey": owner_keys.public_key().to_hex(),
            "receiver_pubkey": receiver.to_hex(),
            "receiver_display_name": "@DK",
            "amount": 21,
            "payable": "lno1abc",
            "source_event_id": "c87370826bf84cbf48bd8e5c50d485a5df251a8b76595cc60916783ab4f5a422",
        })
        .to_string();
        let encrypted = nip44::encrypt(
            kudos_keys.secret_key(),
            &bot_keys.public_key(),
            payload,
            Nip44Version::default(),
        )
        .unwrap();
        let event = EventBuilder::new(
            Kind::Ephemeral(AUTO_KUDOS_COMMAND_KIND),
            encrypted,
            [Tag::parse(&["p", &bot_keys.public_key().to_hex()]).unwrap()],
        )
        .sign_with_keys(&kudos_keys)
        .unwrap();
        let config = Config {
            relay_url: DEFAULT_RELAY_URL.to_string(),
            channel_ids: vec!["test-channel".to_string()],
            bot_keys,
            owner_pubkey: owner_keys.public_key(),
            owner_display_name_override: None,
            owner_auth_tag: None,
            kudos_bot_pubkey: Some(kudos_keys.public_key()),
        };

        let command = encrypted_auto_kudos_command(&config, &event).unwrap();
        assert_eq!(
            command,
            BotCommand::AutoKudosSend {
                sender: owner_keys.public_key(),
                receiver,
                receiver_display_name: "@DK".to_string(),
                amount: 21,
                payable: "lno1abc".to_string(),
            }
        );
        assert!(command_authorized(&command, event.pubkey, &config));
    }

    #[test]
    fn auto_kudos_receipt_uses_receiver_display_name() {
        let command = BotCommand::AutoKudosSend {
            sender: Keys::generate().public_key(),
            receiver: Keys::generate().public_key(),
            receiver_display_name: "@DK".to_string(),
            amount: 21,
            payable: "lno1abc".to_string(),
        };

        assert_eq!(
            auto_kudos_receipt(&command).as_deref(),
            Some("₿21 kudos sent to @DK")
        );
    }

    #[test]
    fn auto_kudos_receiver_label_is_sanitized() {
        let receiver = Keys::generate().public_key();
        assert_eq!(
            auto_kudos_receiver_label(Some("DK!"), receiver),
            "@DK".to_string()
        );
        assert_eq!(
            auto_kudos_receiver_label(Some("@D K"), receiver),
            format!("@{}", receiver.to_hex().chars().take(8).collect::<String>())
        );
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
        assert_eq!(owner_lexebot_display_name("Mat"), "LexeBot[Mat]");
        assert_eq!(
            owner_lexebot_display_name("  Mat   Balez  "),
            "LexeBot[MatBalez]"
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
