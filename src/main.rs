//! A deterministic Sprout bot for controlling one owner's Lexe wallet.

use std::collections::{HashSet, VecDeque};
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use image::{ImageFormat, Luma};
use lexe::{
    config::WalletEnvConfig,
    types::{
        auth::{ClientCredentials, CredentialsRef},
        bitcoin::Amount,
        command::{CreateInvoiceRequest, CreateOfferRequest, PayRequest},
        payment::{Order, Payment, PaymentDirection, PaymentFilter, PaymentStatus},
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
use qrcode::{EcLevel, QrCode};
use serde_json::{json, Value};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use url::Url as WsUrl;

const DEFAULT_RELAY_URL: &str = "ws://localhost:3000";
const AUTO_KUDOS_SUBSCRIPTION_ID: &str = "lexebot-auto-kudos";
const OWNER_DM_SUBSCRIPTION_ID: &str = "lexebot-owner-dm";
const AUTO_KUDOS_COMMAND_KIND: u16 = 21_000;
const AUTO_KUDOS_RESULT_KIND: u16 = 21_001;
const AUTO_KUDOS_PROTOCOL_VERSION: u64 = 2;
const PRESENCE_UPDATE_KIND: u16 = 20_001;
const PRESENCE_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const AUTO_KUDOS_RECEIVE_POLL_INTERVAL: Duration = Duration::from_secs(1);
const AUTO_KUDOS_RECEIVE_TIMEOUT: Duration = Duration::from_secs(60);
const LATEST_RELEASE_API_URL: &str =
    "https://api.github.com/repos/matbalez/lexebot/releases/latest";
const AUTO_KUDOS_SOURCE_DEDUP_MAX_LEN: usize = 1_024;
const INBOUND_PAYMENT_DEDUP_MAX_LEN: usize = 1_024;
const BOT_NAME: &str = "lexebot";
const BOT_DISPLAY_NAME: &str = "LexeBot";
const BOT_ABOUT: &str = "A deterministic Sprout bot that lets one owner control a Lexe wallet.";
const BOT_ICON_DATA_URL: &str = "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 128 128'%3E%3Crect width='128' height='128' rx='28' fill='%23111618'/%3E%3Cpath d='M35 84 64 28l29 56H76L64 58 52 84H35Z' fill='%23f4c542'/%3E%3Cpath d='M43 96h42' stroke='%238fd4ff' stroke-width='10' stroke-linecap='round'/%3E%3C/svg%3E";
const INITIAL_RECONNECT_DELAY: Duration = Duration::from_secs(1);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(60);
const AUTO_KUDOS_PAYMENT_TIMEOUT: Duration = Duration::from_secs(25);
const LEXE_MIN_FUNDED_BALANCE: u64 = 2_500;

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
        let lexe = LexeClient::from_env()?;
        send_install_welcome_once(&config, &lexe, &channel_id).await?;
        return Ok(());
    }

    let config = Config::from_env()?;
    let lexe = LexeClient::from_env()?;
    let runtime = Runtime {
        config,
        lexe,
        processed_auto_kudos_sources: Mutex::new(RecentEventIds::new(
            AUTO_KUDOS_SOURCE_DEDUP_MAX_LEN,
        )),
        seen_inbound_payments: Mutex::new(RecentEventIds::new(INBOUND_PAYMENT_DEDUP_MAX_LEN)),
        pending_inbound_kudos: Mutex::new(Vec::new()),
    };

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
    processed_auto_kudos_sources: Mutex<RecentEventIds>,
    seen_inbound_payments: Mutex<RecentEventIds>,
    pending_inbound_kudos: Mutex<Vec<PendingInboundKudos>>,
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
            BotCommand::FundWallet => self.fund_wallet().await.map(Some),
            BotCommand::GetTransactions => self.get_transactions().await.map(Some),
            BotCommand::CreateInvoice { amount } => self.create_invoice(amount).await.map(Some),
            BotCommand::Send { amount, payable } => self
                .send_payment(
                    amount,
                    payable,
                    None,
                    Some("LexeBot channel command".to_string()),
                )
                .await
                .map(Some),
            BotCommand::AutoKudosSend {
                amount,
                payable,
                receiver_display_name,
                ..
            } => {
                self.send_payment(
                    amount,
                    payable,
                    Some("Kudos!".to_string()),
                    Some(auto_kudos_personal_note(&receiver_display_name)),
                )
                .await?;
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

    async fn get_transactions(&self) -> Result<String> {
        Ok(format_recent_lexe_payments(&self.recent_payments(5).await?))
    }

    async fn recent_payments(&self, limit: usize) -> Result<Vec<Payment>> {
        self.wallet
            .sync_payments()
            .await
            .context("failed to sync Lexe payment history")?;
        let response = self
            .wallet
            .list_payments(&PaymentFilter::All, Some(Order::Desc), Some(limit), None)
            .context("failed to list Lexe payment history")?;

        Ok(response.payments)
    }

    async fn funding_prompt_if_needed(&self) -> Result<Option<String>> {
        let info = self.wallet.node_info().await?;
        let balance = info.balance.sats_u64();
        if balance >= LEXE_MIN_FUNDED_BALANCE {
            return Ok(None);
        }

        let offer = self.create_bolt12_offer().await?;
        Ok(Some(format_bolt12_offer_message(
            &format!(
                "Your Lexe wallet balance is {}, below the {} minimum needed for small sends. Fund it with this reusable BOLT12 offer.",
            format_amount(balance),
            format_amount(LEXE_MIN_FUNDED_BALANCE),
            ),
            &offer,
        )))
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

    async fn fund_wallet(&self) -> Result<String> {
        let offer = self.create_bolt12_offer().await?;
        Ok(format_bolt12_offer_message(
            "Fund your Lexe wallet with this reusable BOLT12 offer.",
            &offer,
        ))
    }

    async fn get_bolt12(&self) -> Result<String> {
        let offer = self.create_bolt12_offer().await?;
        Ok(format_bolt12_offer_message("BOLT12 offer.", &offer))
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

    async fn send_payment(
        &self,
        amount: u64,
        payable: String,
        message: Option<String>,
        personal_note: Option<String>,
    ) -> Result<String> {
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
                message,
                personal_note,
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
    let mut update_check_interval = tokio::time::interval(UPDATE_CHECK_INTERVAL);
    update_check_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    update_check_interval.tick().await;
    let mut auto_kudos_receive_interval = tokio::time::interval(AUTO_KUDOS_RECEIVE_POLL_INTERVAL);
    auto_kudos_receive_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    auto_kudos_receive_interval.tick().await;

    eprintln!("listening for owner DM commands and encrypted auto-kudos commands");
    if runtime.config.channel_ids.len() > 1 {
        eprintln!(
            "ignoring {} extra configured plaintext channel(s); manual wallet commands only run in the owner DM",
            runtime.config.channel_ids.len() - 1
        );
    }
    if let Err(err) = maybe_notify_update_available(&mut ws, &runtime.config).await {
        eprintln!("update check failed: {err:#}");
    }
    if let Err(err) = remember_recent_inbound_payments(runtime).await {
        eprintln!("incoming payment notification initialization failed: {err:#}");
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
            _ = update_check_interval.tick() => {
                if let Err(err) = maybe_notify_update_available(&mut ws, &runtime.config).await {
                    eprintln!("update check failed: {err:#}");
                }
            }
            _ = auto_kudos_receive_interval.tick() => {
                if let Err(err) = check_pending_inbound_kudos(&mut ws, runtime).await {
                    eprintln!("pending auto-kudos receive check failed: {err:#}");
                }
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
    owner_auth_tag: Option<Tag>,
    kudos_bot_pubkey: Option<PublicKey>,
    update_check_enabled: bool,
}

impl Config {
    fn from_env() -> Result<Self> {
        let relay_url =
            std::env::var("SPROUT_RELAY_URL").unwrap_or_else(|_| DEFAULT_RELAY_URL.to_string());
        let channel_ids = channel_ids_from_env()?;
        let bot_keys = Keys::parse(&required_env("SPROUT_BOT_PRIVATE_KEY")?)
            .context("SPROUT_BOT_PRIVATE_KEY must be an nsec or hex private key")?;
        let kudos_bot_pubkey = optional_pubkey_env("LEXEBOT_KUDOS_BOT_PUBKEY")?;
        let update_check_enabled = update_check_enabled_from_env()?;

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
            owner_auth_tag,
            kudos_bot_pubkey,
            update_check_enabled,
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

async fn resolve_bot_display_name(_ws: &mut Ws, _config: &Config) -> String {
    BOT_DISPLAY_NAME.to_string()
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
            handle_encrypted_auto_kudos_message(ws, runtime, event).await?;
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

    send_owner_dm_message(ws, &runtime.config, &reply).await?;
    eprintln!("replied to plaintext owner DM {command_event_id}");
    Ok(())
}

async fn handle_encrypted_auto_kudos_message(
    ws: &mut Ws,
    runtime: &Runtime,
    event: &Event,
) -> Result<()> {
    let command_event_id = event.id.to_hex();
    let message = match encrypted_auto_kudos_message(&runtime.config, event) {
        Ok(message) if auto_kudos_message_authorized(&message, event.pubkey, &runtime.config) => {
            message
        }
        Ok(AutoKudosMessage::Send(_)) => {
            send_auto_kudos_result(ws, &runtime.config, event, false).await?;
            eprintln!("rejected unauthorized encrypted auto-kudos command {command_event_id}");
            return Ok(());
        }
        Ok(AutoKudosMessage::ReceiveNotice(_)) => {
            eprintln!(
                "rejected unauthorized encrypted auto-kudos receive notice {command_event_id}"
            );
            return Ok(());
        }
        Err(err) => {
            send_auto_kudos_result(ws, &runtime.config, event, false).await?;
            eprintln!("invalid encrypted auto-kudos message {command_event_id}: {err:#}");
            return Ok(());
        }
    };

    let command = match message {
        AutoKudosMessage::Send(command) => command,
        AutoKudosMessage::ReceiveNotice(notice) => {
            remember_pending_inbound_kudos(runtime, notice)?;
            eprintln!("accepted encrypted auto-kudos receive notice {command_event_id}");
            return Ok(());
        }
    };

    let receipt = auto_kudos_receipt(&command);
    let source_event_id = auto_kudos_source_event_id(&command).map(str::to_string);

    if let Some(source_event_id) = source_event_id.as_deref() {
        if auto_kudos_source_processed(runtime, source_event_id)? {
            send_auto_kudos_result(ws, &runtime.config, event, true).await?;
            eprintln!(
                "ignored duplicate encrypted auto-kudos command {command_event_id} for source {source_event_id}"
            );
            return Ok(());
        }
    }

    match tokio::time::timeout(AUTO_KUDOS_PAYMENT_TIMEOUT, runtime.lexe.execute(command)).await {
        Ok(Ok(_)) => {
            if let Some(source_event_id) = source_event_id.as_deref() {
                remember_auto_kudos_source(runtime, &command_event_id, source_event_id)?;
            }
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

fn auto_kudos_source_processed(runtime: &Runtime, source_event_id: &str) -> Result<bool> {
    let processed = runtime
        .processed_auto_kudos_sources
        .lock()
        .map_err(|_| anyhow!("auto-kudos dedup state lock poisoned"))?;
    Ok(processed.contains(source_event_id))
}

fn remember_auto_kudos_source(
    runtime: &Runtime,
    command_event_id: &str,
    source_event_id: &str,
) -> Result<()> {
    let mut processed = runtime
        .processed_auto_kudos_sources
        .lock()
        .map_err(|_| anyhow!("auto-kudos dedup state lock poisoned"))?;
    processed.insert_new(source_event_id);
    eprintln!("remembered auto-kudos source {source_event_id} from command {command_event_id}");
    Ok(())
}

struct RecentEventIds {
    max_len: usize,
    order: VecDeque<String>,
    ids: HashSet<String>,
}

impl RecentEventIds {
    fn new(max_len: usize) -> Self {
        Self {
            max_len,
            order: VecDeque::new(),
            ids: HashSet::new(),
        }
    }

    fn contains(&self, event_id: &str) -> bool {
        self.ids.contains(event_id)
    }

    fn insert_new(&mut self, event_id: &str) -> bool {
        if self.ids.contains(event_id) {
            return false;
        }

        let event_id = event_id.to_string();
        self.ids.insert(event_id.clone());
        self.order.push_back(event_id);

        while self.order.len() > self.max_len {
            if let Some(oldest) = self.order.pop_front() {
                self.ids.remove(&oldest);
            }
        }

        true
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AutoKudosReceiveNotice {
    sender: PublicKey,
    receiver: PublicKey,
    sender_display_name: String,
    amount: u64,
    source_event_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingInboundKudos {
    notice: AutoKudosReceiveNotice,
    requested_at: Instant,
}

async fn send_owner_dm_message(ws: &mut Ws, config: &Config, content: &str) -> Result<()> {
    let Some(channel_id) = owner_dm_channel_id(config) else {
        return Ok(());
    };
    send_dm_message(ws, config, channel_id, content, false).await
}

async fn send_dm_message(
    ws: &mut Ws,
    config: &Config,
    channel_id: &str,
    content: &str,
    wait_for_ack: bool,
) -> Result<()> {
    let (content, media_tags) = prepare_dm_message(config, content).await;
    let event = build_message_with_tags(
        channel_id,
        &content,
        &[config.owner_pubkey.to_hex()],
        media_tags,
    )?
    .sign_with_keys(&config.bot_keys)?;
    let event_id = event.id.to_hex();

    send_json(ws, json!(["EVENT", event])).await?;
    if wait_for_ack {
        wait_for_ok(ws, &event_id).await?;
    }
    Ok(())
}

async fn prepare_dm_message(config: &Config, content: &str) -> (String, Vec<Tag>) {
    let Some(offer) = extract_bolt12_offer(content) else {
        return (content.to_string(), Vec::new());
    };

    match upload_bolt12_qr(config, offer).await {
        Ok(image) => (
            format_bolt12_offer_message_with_image(content, &image.url),
            image.tags,
        ),
        Err(err) => {
            eprintln!("could not upload BOLT12 QR image: {err:#}");
            (content.to_string(), Vec::new())
        }
    }
}

struct UploadedQrImage {
    url: String,
    tags: Vec<Tag>,
}

async fn upload_bolt12_qr(config: &Config, offer: &str) -> Result<UploadedQrImage> {
    let png = render_bolt12_qr_png(offer)?;
    let sha256 = Sha256Hash::hash(&png).to_string();
    let auth = build_media_upload_auth(config, &sha256)?;
    let auth_header = format!(
        "Nostr {}",
        URL_SAFE_NO_PAD.encode(auth.as_json().as_bytes())
    );
    let upload_url = format!("{}/media/upload", relay_http_url(&config.relay_url)?);

    let response = reqwest::Client::new()
        .put(upload_url)
        .header("Authorization", auth_header)
        .header("Content-Type", "image/png")
        .header("X-SHA-256", &sha256)
        .body(png)
        .send()
        .await
        .context("failed to upload BOLT12 QR image")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!("QR image upload rejected with {status}: {body}");
    }

    let descriptor: Value = response
        .json()
        .await
        .context("QR image upload response was not JSON")?;
    let url = descriptor
        .get("url")
        .and_then(Value::as_str)
        .filter(|url| !url.trim().is_empty())
        .ok_or_else(|| anyhow!("QR image upload response missing url"))?
        .to_string();
    let size = descriptor
        .get("size")
        .and_then(Value::as_u64)
        .map(|size| size.to_string());

    let mut imeta = vec![
        "imeta".to_string(),
        format!("url {url}"),
        "m image/png".to_string(),
        format!("x {sha256}"),
        "dim 640x640".to_string(),
    ];
    if let Some(size) = size {
        imeta.push(format!("size {size}"));
    }

    Ok(UploadedQrImage {
        url,
        tags: vec![Tag::parse(&imeta)?],
    })
}

fn render_bolt12_qr_png(offer: &str) -> Result<Vec<u8>> {
    let code = QrCode::with_error_correction_level(offer.as_bytes(), EcLevel::M)
        .context("failed to render BOLT12 QR code")?;
    let image = code
        .render::<Luma<u8>>()
        .quiet_zone(true)
        .min_dimensions(640, 640)
        .dark_color(Luma([0]))
        .light_color(Luma([255]))
        .build();

    let mut cursor = Cursor::new(Vec::new());
    image
        .write_to(&mut cursor, ImageFormat::Png)
        .context("failed to encode BOLT12 QR PNG")?;
    Ok(cursor.into_inner())
}

fn build_media_upload_auth(config: &Config, sha256: &str) -> Result<Event> {
    let expiration = nostr::Timestamp::now().as_u64() + 300;
    let tags = vec![
        Tag::parse(&["t", "upload"])?,
        Tag::parse(&["x", sha256])?,
        Tag::parse(&["expiration", &expiration.to_string()])?,
    ];
    Ok(
        EventBuilder::new(Kind::from(24242), "Upload BOLT12 QR", tags)
            .sign_with_keys(&config.bot_keys)?,
    )
}

fn relay_http_url(relay_url: &str) -> Result<String> {
    let mut url = WsUrl::parse(relay_url).context("SPROUT_RELAY_URL is not a valid URL")?;
    let scheme = match url.scheme() {
        "ws" => "http",
        "wss" => "https",
        "http" => "http",
        "https" => "https",
        other => bail!("unsupported relay URL scheme for media upload: {other}"),
    };
    url.set_scheme(scheme)
        .map_err(|_| anyhow!("failed to convert relay URL to HTTP URL"))?;
    url.set_path("");
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.to_string().trim_end_matches('/').to_string())
}

async fn remember_recent_inbound_payments(runtime: &Runtime) -> Result<()> {
    let payments = runtime.lexe.recent_payments(20).await?;
    for payment in payments
        .iter()
        .filter(|payment| should_notify_inbound_payment(payment))
    {
        remember_inbound_payment(runtime, payment)?;
    }
    Ok(())
}

async fn check_pending_inbound_kudos(ws: &mut Ws, runtime: &Runtime) -> Result<()> {
    if !prune_and_has_pending_inbound_kudos(runtime)? {
        return Ok(());
    }

    let payments = runtime.lexe.recent_payments(20).await?;
    let mut notices = Vec::new();

    for payment in payments
        .iter()
        .rev()
        .filter(|payment| should_notify_inbound_payment(payment))
    {
        let payment_id = payment.index.to_string();
        if inbound_payment_seen(runtime, &payment_id)? {
            continue;
        }

        if let Some(pending) = take_matching_pending_inbound_kudos(runtime, payment)? {
            remember_inbound_payment_id(runtime, &payment_id)?;
            if let Some(notice) = inbound_payment_notice(payment, &pending.notice) {
                notices.push(notice);
            }
        }
    }

    for notice in notices {
        send_owner_dm_message(ws, &runtime.config, &notice).await?;
    }

    Ok(())
}

fn remember_inbound_payment(runtime: &Runtime, payment: &Payment) -> Result<bool> {
    remember_inbound_payment_id(runtime, &payment.index.to_string())
}

fn remember_inbound_payment_id(runtime: &Runtime, payment_id: &str) -> Result<bool> {
    let mut seen = runtime
        .seen_inbound_payments
        .lock()
        .map_err(|_| anyhow!("incoming payment dedup state lock poisoned"))?;
    Ok(seen.insert_new(payment_id))
}

fn inbound_payment_seen(runtime: &Runtime, payment_id: &str) -> Result<bool> {
    let seen = runtime
        .seen_inbound_payments
        .lock()
        .map_err(|_| anyhow!("incoming payment dedup state lock poisoned"))?;
    Ok(seen.contains(payment_id))
}

fn remember_pending_inbound_kudos(runtime: &Runtime, notice: AutoKudosReceiveNotice) -> Result<()> {
    let mut pending = runtime
        .pending_inbound_kudos
        .lock()
        .map_err(|_| anyhow!("pending auto-kudos receive state lock poisoned"))?;

    if let Some(source_event_id) = notice.source_event_id.as_deref() {
        if pending
            .iter()
            .any(|pending| pending.notice.source_event_id.as_deref() == Some(source_event_id))
        {
            eprintln!("ignored duplicate pending auto-kudos receive notice for {source_event_id}");
            return Ok(());
        }
    }

    pending.push(PendingInboundKudos {
        notice,
        requested_at: Instant::now(),
    });
    Ok(())
}

fn prune_and_has_pending_inbound_kudos(runtime: &Runtime) -> Result<bool> {
    let mut pending = runtime
        .pending_inbound_kudos
        .lock()
        .map_err(|_| anyhow!("pending auto-kudos receive state lock poisoned"))?;
    let now = Instant::now();
    pending.retain(|pending| {
        let keep = now.duration_since(pending.requested_at) <= AUTO_KUDOS_RECEIVE_TIMEOUT;
        if !keep {
            if let Some(source_event_id) = pending.notice.source_event_id.as_deref() {
                eprintln!("timed out waiting for inbound auto-kudos payment {source_event_id}");
            } else {
                eprintln!("timed out waiting for inbound auto-kudos payment");
            }
        }
        keep
    });
    Ok(!pending.is_empty())
}

fn take_matching_pending_inbound_kudos(
    runtime: &Runtime,
    payment: &Payment,
) -> Result<Option<PendingInboundKudos>> {
    let mut pending = runtime
        .pending_inbound_kudos
        .lock()
        .map_err(|_| anyhow!("pending auto-kudos receive state lock poisoned"))?;
    let Some(index) = pending
        .iter()
        .position(|pending| inbound_payment_matches_pending_kudos(payment, pending))
    else {
        return Ok(None);
    };
    Ok(Some(pending.remove(index)))
}

fn inbound_payment_matches_pending_kudos(payment: &Payment, pending: &PendingInboundKudos) -> bool {
    payment
        .amount
        .map(|amount| amount.sats_u64() == pending.notice.amount)
        .unwrap_or(false)
        && payment
            .message
            .as_deref()
            .and_then(clean_payment_note)
            .as_deref()
            == Some("Kudos!")
}

async fn maybe_notify_update_available(ws: &mut Ws, config: &Config) -> Result<()> {
    if !config.update_check_enabled {
        return Ok(());
    }

    let Some(latest_version) = fetch_latest_release_version().await? else {
        return Ok(());
    };
    let current_version = format!("v{}", env!("CARGO_PKG_VERSION"));
    if !version_is_newer(&latest_version, &current_version) {
        return Ok(());
    }

    let state_path = update_notification_state_path()?;
    if last_notified_update_version(&state_path).as_deref() == Some(latest_version.as_str()) {
        return Ok(());
    }

    let notice = update_available_notice(&latest_version, &current_version);
    if owner_dm_channel_id(config).is_some() {
        send_owner_dm_message(ws, config, &notice).await?;
    } else {
        eprintln!("{notice}");
    }
    remember_notified_update_version(&state_path, &latest_version)?;
    Ok(())
}

async fn fetch_latest_release_version() -> Result<Option<String>> {
    let user_agent = format!("LexeBot/{}", env!("CARGO_PKG_VERSION"));
    let output = tokio::task::spawn_blocking(move || {
        Command::new("curl")
            .arg("-fsSL")
            .arg("-H")
            .arg("Accept: application/vnd.github+json")
            .arg("-H")
            .arg(format!("User-Agent: {user_agent}"))
            .arg(LATEST_RELEASE_API_URL)
            .output()
    })
    .await
    .context("update check task failed")?
    .context("failed to run curl for update check")?;

    if !output.status.success() {
        bail!(
            "GitHub latest release request failed with {}",
            output.status
        );
    }

    let body =
        String::from_utf8(output.stdout).context("GitHub latest release response is not UTF-8")?;
    parse_latest_release_version(&body)
}

fn parse_latest_release_version(body: &str) -> Result<Option<String>> {
    let value: Value =
        serde_json::from_str(body).context("GitHub latest release response is not JSON")?;
    Ok(value
        .get("tag_name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .map(ToString::to_string))
}

fn version_is_newer(candidate: &str, current: &str) -> bool {
    let Some(candidate) = parse_version_tag(candidate) else {
        return false;
    };
    let Some(current) = parse_version_tag(current) else {
        return false;
    };
    candidate > current
}

fn parse_version_tag(tag: &str) -> Option<(u64, u64, u64)> {
    let tag = tag.trim().strip_prefix('v').unwrap_or_else(|| tag.trim());
    let mut parts = tag.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((major, minor, patch))
}

fn update_available_notice(latest_version: &str, current_version: &str) -> String {
    format!(
        "LexeBot {latest_version} is available. You are running {current_version}.\n\
Upgrade:\n\
curl -fsSL https://raw.githubusercontent.com/matbalez/lexebot/main/scripts/install.sh | bash"
    )
}

fn update_notification_state_path() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("LEXEBOT_UPDATE_STATE_FILE") {
        if !path.trim().is_empty() {
            return Ok(PathBuf::from(path));
        }
    }
    let home = std::env::var("HOME").context("HOME is required for LexeBot update state")?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("lexebot")
        .join("last-notified-update"))
}

fn last_notified_update_version(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn remember_notified_update_version(path: &Path, version: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create update state dir {}", parent.display()))?;
    }
    fs::write(path, format!("{version}\n"))
        .with_context(|| format!("failed to write update state file {}", path.display()))
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

async fn send_install_welcome_once(
    config: &Config,
    lexe: &LexeClient,
    channel_id: &str,
) -> Result<()> {
    let mut ws = connect_and_authenticate(config).await?;
    send_install_dm_message(&mut ws, config, channel_id, &install_welcome_message()).await?;

    if let Some(message) = lexe.funding_prompt_if_needed().await? {
        send_install_dm_message(&mut ws, config, channel_id, &message).await?;
    }

    Ok(())
}

async fn send_install_dm_message(
    ws: &mut Ws,
    config: &Config,
    channel_id: &str,
    content: &str,
) -> Result<()> {
    send_dm_message(ws, config, channel_id, content, true).await
}

fn install_welcome_message() -> String {
    format!(
        "LexeBot v{} is installed and ready.\n\n\
Supported commands:\n\
get balance\n\
get BOLT12\n\
fund wallet\n\
get transactions\n\
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

fn auto_kudos_message_authorized(
    message: &AutoKudosMessage,
    sender: PublicKey,
    config: &Config,
) -> bool {
    match message {
        AutoKudosMessage::Send(command) => command_authorized(command, sender, config),
        AutoKudosMessage::ReceiveNotice(notice) => {
            config.kudos_bot_pubkey == Some(sender) && notice.receiver == config.owner_pubkey
        }
    }
}

fn encrypted_auto_kudos_message(config: &Config, event: &Event) -> Result<AutoKudosMessage> {
    let decrypted = nip44::decrypt(
        config.bot_keys.secret_key(),
        &event.pubkey,
        event.content.as_str(),
    )?;
    let value: Value = serde_json::from_str(&decrypted)?;

    if value.get("version").and_then(Value::as_u64) != Some(AUTO_KUDOS_PROTOCOL_VERSION) {
        bail!("unsupported auto-kudos message version");
    }

    match value.get("type").and_then(Value::as_str) {
        Some("auto-kudos") => {
            parse_encrypted_auto_kudos_command(&value).map(AutoKudosMessage::Send)
        }
        Some("auto-kudos-receive-notice") => {
            parse_encrypted_auto_kudos_receive_notice(&value).map(AutoKudosMessage::ReceiveNotice)
        }
        Some(other) => bail!("unexpected auto-kudos message type: {other}"),
        None => bail!("auto-kudos message missing type"),
    }
}

fn parse_encrypted_auto_kudos_command(value: &Value) -> Result<BotCommand> {
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
    let source_event_id = value
        .get("source_event_id")
        .and_then(Value::as_str)
        .filter(|source_event_id| !source_event_id.trim().is_empty())
        .map(str::to_string);
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
        source_event_id,
    })
}

fn parse_encrypted_auto_kudos_receive_notice(value: &Value) -> Result<AutoKudosReceiveNotice> {
    let sender = value
        .get("sender_pubkey")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("auto-kudos receive notice missing sender_pubkey"))
        .and_then(|pubkey| {
            PublicKey::from_hex(pubkey)
                .context("auto-kudos receive notice sender_pubkey is invalid")
        })?;
    let receiver = value
        .get("receiver_pubkey")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("auto-kudos receive notice missing receiver_pubkey"))
        .and_then(|pubkey| {
            PublicKey::from_hex(pubkey)
                .context("auto-kudos receive notice receiver_pubkey is invalid")
        })?;
    let amount = value
        .get("amount")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("auto-kudos receive notice missing amount"))?;
    let sender_display_name = auto_kudos_receiver_label(
        value.get("sender_display_name").and_then(Value::as_str),
        sender,
    );
    let source_event_id = value
        .get("source_event_id")
        .and_then(Value::as_str)
        .filter(|source_event_id| !source_event_id.trim().is_empty())
        .map(str::to_string);

    Ok(AutoKudosReceiveNotice {
        sender,
        receiver,
        sender_display_name,
        amount,
        source_event_id,
    })
}

fn auto_kudos_source_event_id(command: &BotCommand) -> Option<&str> {
    let BotCommand::AutoKudosSend {
        source_event_id, ..
    } = command
    else {
        return None;
    };
    source_event_id.as_deref()
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

fn auto_kudos_personal_note(receiver_display_name: &str) -> String {
    let name = receiver_display_name
        .trim()
        .trim_start_matches('@')
        .trim_end_matches([',', '.', '!', '?', ':', ';']);
    let name = if name.is_empty() {
        receiver_display_name.trim()
    } else {
        name
    };
    format!("You sent a Kudos to {name}")
}

fn build_message_with_tags(
    channel_id: &str,
    content: &str,
    mentions: &[String],
    mut extra_tags: Vec<Tag>,
) -> Result<EventBuilder> {
    if content.len() > 64 * 1024 {
        bail!("message content exceeds 64 KiB");
    }

    let mut tags = vec![Tag::parse(&["h", channel_id])?];
    for pubkey in mentions.iter().take(50) {
        tags.push(Tag::parse(&["p", pubkey])?);
    }
    tags.append(&mut extra_tags);
    Ok(EventBuilder::new(Kind::Custom(9), content, tags))
}

#[derive(Debug, Eq, PartialEq)]
enum AutoKudosMessage {
    Send(BotCommand),
    ReceiveNotice(AutoKudosReceiveNotice),
}

#[derive(Debug, Eq, PartialEq)]
enum BotCommand {
    GetBalance,
    GetBolt12,
    FundWallet,
    GetTransactions,
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
        source_event_id: Option<String>,
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
        "fund" => parse_fund_command(rest),
        "create" => parse_create_command(rest),
        "send" => parse_send_command(rest),
        "auto-kudos" => parse_auto_kudos_command(rest),
        _ => Err(command_help()),
    }
}

fn parse_fund_command(tokens: &[&str]) -> std::result::Result<BotCommand, String> {
    match tokens {
        ["wallet"] => Ok(BotCommand::FundWallet),
        _ => Err("expected `fund wallet`".to_string()),
    }
}

fn parse_get_command(tokens: &[&str]) -> std::result::Result<BotCommand, String> {
    match tokens {
        ["balance"] => Ok(BotCommand::GetBalance),
        [offer] if offer.eq_ignore_ascii_case("bolt12") => Ok(BotCommand::GetBolt12),
        ["transactions"] => Ok(BotCommand::GetTransactions),
        _ => Err("expected `get balance`, `get BOLT12`, or `get transactions`".to_string()),
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
            source_event_id: None,
        }),
        _ => Err(
            "expected `@LexeBot auto-kudos <sender-pubkey> <receiver-pubkey> ₿500 to <payment-target>`"
                .to_string(),
        ),
    }
}

fn command_help() -> String {
    "use `get balance`, `get BOLT12`, `fund wallet`, `get transactions`, `create invoice for ₿1,000`, or `send ₿500 to <payment-target>`; explicit mentions like `@LexeBot get balance` work too".to_string()
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

fn format_bolt12_offer_message(intro: &str, offer: &str) -> String {
    let mut message = format!("{intro}\n\nBOLT12 offer:\n");
    message.push_str(offer);
    message
}

fn extract_bolt12_offer(content: &str) -> Option<&str> {
    content
        .split_once("BOLT12 offer:\n")
        .map(|(_, offer)| offer.trim())
        .filter(|offer| offer.starts_with("lno1"))
}

fn format_bolt12_offer_message_with_image(content: &str, image_url: &str) -> String {
    let Some(offer) = extract_bolt12_offer(content) else {
        return content.to_string();
    };
    let intro = content
        .split_once("\n\nBOLT12 offer:\n")
        .map(|(intro, _)| intro)
        .unwrap_or("BOLT12 offer.");
    format!("{intro}\n\n![BOLT12 QR code]({image_url})\n\nBOLT12 offer:\n{offer}")
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

fn format_recent_lexe_payments(payments: &[Payment]) -> String {
    if payments.is_empty() {
        return "No recent transactions found.".to_string();
    }

    let mut lines = Vec::with_capacity(payments.len() + 1);
    lines.push("Recent transactions:".to_string());
    for (index, payment) in payments.iter().enumerate() {
        lines.push(format!("{}. {}", index + 1, format_lexe_payment(payment)));
    }
    lines.join("\n")
}

fn format_lexe_payment(payment: &Payment) -> String {
    let amount = payment
        .amount
        .map(|amount| format_amount(amount.sats_u64()))
        .unwrap_or_else(|| "amountless".to_string());
    let fee = payment.fees.sats_u64();
    let mut parts = vec![
        format_lexe_payment_timestamp(payment),
        payment.direction.to_string(),
        payment.status.to_string(),
        payment.kind.to_string(),
        amount,
    ];
    if fee > 0 {
        parts.push(format!("fee {}", format_amount(fee)));
    }
    if !payment.status_msg.trim().is_empty() {
        parts.push(payment.status_msg.trim().to_string());
    }
    parts.join(" - ")
}

fn format_lexe_payment_timestamp(payment: &Payment) -> String {
    let timestamp = payment.finalized_at.unwrap_or(payment.created_at);
    format_timestamp_ms(timestamp.to_millis())
}

fn should_notify_inbound_payment(payment: &Payment) -> bool {
    payment.direction == PaymentDirection::Inbound
        && payment.status == PaymentStatus::Completed
        && payment.amount.is_some()
}

fn inbound_payment_notice(payment: &Payment, notice: &AutoKudosReceiveNotice) -> Option<String> {
    let amount = payment.amount?;
    let message = payment.message.as_deref().and_then(clean_payment_note);
    let mut lines = vec![if message.as_deref() == Some("Kudos!") {
        format!("Received {} kudos.", format_amount(amount.sats_u64()))
    } else {
        format!("Received {}.", format_amount(amount.sats_u64()))
    }];
    lines.push(format!("From: {}", notice.sender_display_name));
    if let Some(message) = message {
        lines.push(format!("Message: {message}"));
    }
    Some(lines.join("\n"))
}

fn clean_payment_note(value: &str) -> Option<String> {
    let cleaned = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.is_empty() {
        return None;
    }
    Some(cleaned.chars().take(240).collect())
}

fn format_timestamp_ms(ms_since_epoch: u64) -> String {
    let Some(secs_since_epoch) = i64::try_from(ms_since_epoch / 1000).ok() else {
        return format!("{ms_since_epoch}ms since Unix epoch");
    };

    OffsetDateTime::from_unix_timestamp(secs_since_epoch)
        .ok()
        .and_then(|timestamp| timestamp.format(&Rfc3339).ok())
        .unwrap_or_else(|| format!("{ms_since_epoch}ms since Unix epoch"))
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

fn update_check_enabled_from_env() -> Result<bool> {
    match std::env::var("LEXEBOT_UPDATE_CHECK") {
        Ok(value) if value.trim().is_empty() => Ok(true),
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "0" | "false" | "no" | "off" => Ok(false),
            "1" | "true" | "yes" | "on" => Ok(true),
            _ => bail!("LEXEBOT_UPDATE_CHECK must be 0/1, true/false, yes/no, or on/off"),
        },
        Err(std::env::VarError::NotPresent) => Ok(true),
        Err(err) => Err(err).context("LEXEBOT_UPDATE_CHECK is not valid Unicode"),
    }
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
    fn parses_transactions_command() {
        assert_eq!(
            parse_command("get transactions"),
            Ok(BotCommand::GetTransactions)
        );
        assert_eq!(
            parse_command("@LexeBot get transactions"),
            Ok(BotCommand::GetTransactions)
        );
    }

    #[test]
    fn parses_fund_wallet_command() {
        assert_eq!(parse_command("fund wallet"), Ok(BotCommand::FundWallet));
        assert_eq!(
            parse_command("@LexeBot fund wallet"),
            Ok(BotCommand::FundWallet)
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
                payable: "lno1abc".to_string(),
                source_event_id: None,
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
            owner_auth_tag: None,
            kudos_bot_pubkey: None,
            update_check_enabled: true,
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
            owner_auth_tag: None,
            kudos_bot_pubkey: None,
            update_check_enabled: true,
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
            owner_auth_tag: None,
            kudos_bot_pubkey: None,
            update_check_enabled: true,
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
            owner_auth_tag: None,
            kudos_bot_pubkey: None,
            update_check_enabled: true,
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
            owner_auth_tag: None,
            kudos_bot_pubkey: Some(kudos_keys.public_key()),
            update_check_enabled: true,
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
            owner_auth_tag: None,
            kudos_bot_pubkey: Some(kudos_keys.public_key()),
            update_check_enabled: true,
        };
        let command = BotCommand::AutoKudosSend {
            sender: owner_keys.public_key(),
            receiver,
            receiver_display_name: "@Receiver".to_string(),
            amount: 1,
            payable: "lno1abc".to_string(),
            source_event_id: None,
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
            owner_auth_tag: None,
            kudos_bot_pubkey: Some(kudos_keys.public_key()),
            update_check_enabled: true,
        };

        let AutoKudosMessage::Send(command) =
            encrypted_auto_kudos_message(&config, &event).unwrap()
        else {
            panic!("expected auto-kudos send command");
        };
        assert_eq!(
            command,
            BotCommand::AutoKudosSend {
                sender: owner_keys.public_key(),
                receiver,
                receiver_display_name: "@DK".to_string(),
                amount: 21,
                payable: "lno1abc".to_string(),
                source_event_id: Some(
                    "c87370826bf84cbf48bd8e5c50d485a5df251a8b76595cc60916783ab4f5a422".to_string()
                ),
            }
        );
        assert!(command_authorized(&command, event.pubkey, &config));
    }

    #[test]
    fn encrypted_auto_kudos_receive_notice_round_trips() {
        let bot_keys = Keys::generate();
        let owner_keys = Keys::generate();
        let kudos_keys = Keys::generate();
        let sender = Keys::generate().public_key();
        let payload = json!({
            "version": AUTO_KUDOS_PROTOCOL_VERSION,
            "type": "auto-kudos-receive-notice",
            "sender_pubkey": sender.to_hex(),
            "sender_display_name": "@Mat",
            "receiver_pubkey": owner_keys.public_key().to_hex(),
            "amount": 21,
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
            owner_auth_tag: None,
            kudos_bot_pubkey: Some(kudos_keys.public_key()),
            update_check_enabled: true,
        };

        let message = encrypted_auto_kudos_message(&config, &event).unwrap();
        assert_eq!(
            message,
            AutoKudosMessage::ReceiveNotice(AutoKudosReceiveNotice {
                sender,
                receiver: owner_keys.public_key(),
                sender_display_name: "@Mat".to_string(),
                amount: 21,
                source_event_id: Some(
                    "c87370826bf84cbf48bd8e5c50d485a5df251a8b76595cc60916783ab4f5a422".to_string()
                ),
            })
        );
        assert!(auto_kudos_message_authorized(
            &message,
            event.pubkey,
            &config
        ));
    }

    #[test]
    fn auto_kudos_receipt_uses_receiver_display_name() {
        let command = BotCommand::AutoKudosSend {
            sender: Keys::generate().public_key(),
            receiver: Keys::generate().public_key(),
            receiver_display_name: "@DK".to_string(),
            amount: 21,
            payable: "lno1abc".to_string(),
            source_event_id: None,
        };

        assert_eq!(
            auto_kudos_receipt(&command).as_deref(),
            Some("₿21 kudos sent to @DK")
        );
    }

    #[test]
    fn auto_kudos_personal_note_uses_receiver_display_name_without_at_prefix() {
        assert_eq!(
            auto_kudos_personal_note("@DK"),
            "You sent a Kudos to DK".to_string()
        );
        assert_eq!(
            auto_kudos_personal_note("@Moneyball,"),
            "You sent a Kudos to Moneyball".to_string()
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
    fn parses_latest_release_version_from_github_json() {
        assert_eq!(
            parse_latest_release_version(r#"{"tag_name":"v0.1.17"}"#).unwrap(),
            Some("v0.1.17".to_string())
        );
        assert_eq!(
            parse_latest_release_version(r#"{"tag_name":""}"#).unwrap(),
            None
        );
    }

    #[test]
    fn compares_semver_release_tags() {
        assert!(version_is_newer("v0.1.17", "v0.1.16"));
        assert!(version_is_newer("v0.2.0", "v0.1.99"));
        assert!(!version_is_newer("v0.1.16", "v0.1.16"));
        assert!(!version_is_newer("v0.1.15", "v0.1.16"));
        assert!(!version_is_newer("latest", "v0.1.16"));
    }

    #[test]
    fn formats_update_available_notice() {
        assert_eq!(
            update_available_notice("v0.1.17", "v0.1.16"),
            "LexeBot v0.1.17 is available. You are running v0.1.16.\n\
Upgrade:\n\
curl -fsSL https://raw.githubusercontent.com/matbalez/lexebot/main/scripts/install.sh | bash"
        );
    }

    #[test]
    fn formats_bolt12_offer_message_with_raw_offer() {
        let message = format_bolt12_offer_message("BOLT12 offer.", "lno1qtest");
        assert_eq!(message, "BOLT12 offer.\n\nBOLT12 offer:\nlno1qtest");
    }

    #[test]
    fn formats_bolt12_offer_message_with_uploaded_image() {
        let message = format_bolt12_offer_message_with_image(
            "BOLT12 offer.\n\nBOLT12 offer:\nlno1qtest",
            "https://example.test/qr.png",
        );
        assert_eq!(
            message,
            "BOLT12 offer.\n\n![BOLT12 QR code](https://example.test/qr.png)\n\nBOLT12 offer:\nlno1qtest"
        );
    }

    #[test]
    fn renders_bolt12_qr_png() {
        let png = render_bolt12_qr_png("lno1qtest").unwrap();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
    }

    #[test]
    fn recent_event_ids_rejects_duplicates_and_bounds_memory() {
        let mut ids = RecentEventIds::new(2);
        assert!(ids.insert_new("a"));
        assert!(!ids.insert_new("a"));
        assert!(ids.contains("a"));
        assert!(ids.insert_new("b"));
        assert!(ids.insert_new("c"));
        assert!(!ids.contains("a"));
        assert!(ids.contains("b"));
        assert!(ids.contains("c"));
    }

    #[test]
    fn formats_transaction_timestamps_as_utc_rfc3339() {
        assert_eq!(format_timestamp_ms(0), "1970-01-01T00:00:00Z");
        assert_eq!(
            format_timestamp_ms(1_700_000_000_999),
            "2023-11-14T22:13:20Z"
        );
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
