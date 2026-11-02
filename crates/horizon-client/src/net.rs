//! Multiplayer session: a network thread that connects to a server, logs in,
//! runs the configuration phase and then forwards decoded play packets to the
//! game thread. Keep-alives are answered on the network thread; pings wait for
//! the game thread, like ClientCommonPacketListenerImpl.handlePing, so a pong
//! cannot overtake an earlier world update or teleport acknowledgement.
//! Online-mode servers need the Microsoft account the launcher signed in
//! (crate::auth): the login joins the server through the session service and
//! encrypts the connection (ClientHandshakePacketListenerImpl.handleHello).
use crate::auth::AuthSession;
use horizon_protocol::{
    PROTOCOL_VERSION,
    connection::{ConnReader, ConnWriter, Connection, Counters},
    crypto::{generate_secret, offline_uuid, server_hash},
    packets::{Empty, configuration as cfg, handshake, login, play},
    registry::Dynamic,
    rsa::RsaPublicKey,
    servers::parse_address,
    text::{Language, plain, translatable},
};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, Sender, channel},
    },
    thread::JoinHandle,
    time::Duration,
};

/// `CommonListenerCookie.serverCookies`: cookies the server asked the client
/// to store (`ClientboundStoreCookiePacket`), answered to
/// `ClientboundCookieRequestPacket`. A `transfer` carries them to the next
/// connection (`TransferState`), so a proxy's backend switch keeps them.
pub type Cookies = Arc<Mutex<HashMap<String, Vec<u8>>>>;

/// What the network thread reports to the game.
pub enum Event {
    /// Progress text for the connecting screen.
    Status(String),
    /// The server's `minecraft:brand` custom payload.
    Brand(String),
    /// The server runs mods' quiet commands (`horizon_protocol::quiet`).
    QuietCommands,
    /// Configuration finished: the play state begins with these registries.
    Joined {
        registries: Arc<Dynamic>,
        uuid: u128,
        name: String,
    },
    Packet(Box<play::Clientbound>),
    /// A packet that failed to decode (skipped, like vanilla's skippable packets).
    DecodeError(String),
    /// The server sent us back to configuration and finished it again.
    Reconfigured(Arc<Dynamic>),
    /// ClientboundResourcePackPushPacket, in configuration or in play.
    PackPush(cfg::ResourcePackPush),
    /// ClientboundResourcePackPopPacket: one pack, or every pack.
    PackPop(Option<u128>),
    /// `show_dialog` in configuration: the dialog itself.
    ShowDialog(horizon_world::nbt::Tag),
    /// `clear_dialog` in configuration.
    ClearDialog,
    /// `ClientboundTransferPacket`: the server sent us to another address.
    /// The connection ends right after; the game reconnects there with the
    /// stored cookies.
    Transfer {
        host: String,
        port: i32,
    },
    /// `ClientboundServerLinksPacket`: the links the pause menu's "Server
    /// Links..." lists (ClientCommonPacketListenerImpl.handleServerLinks).
    ServerLinks(Vec<cfg::ServerLink>),
    /// The connection ended. `from_server`: the server gave the reason (or
    /// the connection was refused for good), rather than the connection
    /// failing (auto-reconnect retries only failures and shutdowns).
    Disconnected {
        reason: String,
        from_server: bool,
    },
}

pub struct Session {
    events: Receiver<Event>,
    writer: Arc<Mutex<Option<ConnWriter>>>,
    /// The connection is in configuration (else in play). Changed only
    /// with `writer` locked, as the packet that switches is sent.
    configuring: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    pub address: String,
    pub name: String,
    pub counters: Arc<Mutex<Option<Arc<Counters>>>>,
    /// The cookies this server stored on us (`ClientboundStoreCookiePacket`).
    pub cookies: Cookies,
    /// HORIZON_NET_DEBUG=1 logs every packet sent from the game thread.
    debug: bool,
    required: Option<crate::lan::login_negotiation::RequiredNegotiation>,
}
impl Session {
    /// Start connecting in the background, as `auth`'s account when there is one.
    pub fn connect(
        address: &str,
        name: &str,
        auth: Option<Arc<AuthSession>>,
        view_distance: i8,
        lang: Arc<Language>,
    ) -> Self {
        Self::start(address, name, auth, view_distance, lang, None, None, None)
    }
    /// `ConnectScreen.startConnecting` for a `transfer`: the cookies the
    /// server stored carry to the new connection (`TransferState`).
    pub fn connect_transferred(
        address: &str,
        name: &str,
        auth: Option<Arc<AuthSession>>,
        view_distance: i8,
        lang: Arc<Language>,
        cookies: Cookies,
    ) -> Self {
        Self::start(
            address,
            name,
            auth,
            view_distance,
            lang,
            None,
            Some(cookies),
            None,
        )
    }
    /// Join an integrated server through memory (`Connection.connectToLocalServer`).
    pub fn connect_local(
        pipe: horizon_protocol::connection::Pipe,
        name: &str,
        view_distance: i8,
        lang: Arc<Language>,
    ) -> Self {
        Self::start(
            "local",
            name,
            None,
            view_distance,
            lang,
            Some(pipe),
            None,
            None,
        )
    }
    /// Explicit bounded compatibility attempt. It never publishes Joined or
    /// client registries, including when an unsupported peer stays silent.
    pub fn connect_required_negotiation(
        address: &str,
        name: &str,
        auth: Option<Arc<AuthSession>>,
        view_distance: i8,
        lang: Arc<Language>,
        required: crate::lan::login_negotiation::RequiredNegotiation,
    ) -> Self {
        Self::start(
            address,
            name,
            auth,
            view_distance,
            lang,
            None,
            None,
            Some(required),
        )
    }
    fn start(
        address: &str,
        name: &str,
        auth: Option<Arc<AuthSession>>,
        view_distance: i8,
        lang: Arc<Language>,
        local: Option<horizon_protocol::connection::Pipe>,
        cookies: Option<Cookies>,
        required: Option<crate::lan::login_negotiation::RequiredNegotiation>,
    ) -> Self {
        let (send, events) = channel();
        let writer = Arc::new(Mutex::new(None));
        let configuring = Arc::new(AtomicBool::new(true));
        let counters = Arc::new(Mutex::new(None));
        // Connection.initiateServerboundPlayConnection: a transfer's
        // handshake says so (ClientIntent.TRANSFER).
        let transferred = cookies.is_some();
        let cookies = cookies.unwrap_or_default();
        let (thread_writer, thread_counters, thread_cookies) =
            (writer.clone(), counters.clone(), cookies.clone());
        let thread_configuring = configuring.clone();
        let (address_owned, name_owned) = (address.to_owned(), name.to_owned());
        let thread_required = required.clone();
        let thread = std::thread::Builder::new()
            .name("horizon-net".into())
            .spawn(move || {
                let result = run(
                    local,
                    &address_owned,
                    &name_owned,
                    auth.as_deref(),
                    view_distance,
                    &lang,
                    &send,
                    &thread_writer,
                    &thread_configuring,
                    &thread_counters,
                    &thread_cookies,
                    transferred,
                    thread_required,
                );
                let from_server = result.is_ok();
                let reason = result.unwrap_or_else(|e| e);
                if let Some(w) = thread_writer.lock().unwrap().take() {
                    w.shutdown();
                }
                let _ = send.send(Event::Disconnected {
                    reason,
                    from_server,
                });
            })
            .ok();
        let debug = std::env::var_os("HORIZON_NET_DEBUG").is_some();
        Self {
            events,
            writer,
            configuring,
            thread,
            address: address.to_owned(),
            name: name.to_owned(),
            counters,
            cookies,
            debug,
            required,
        }
    }
    /// Everything received since the last poll.
    pub fn poll(&self) -> Vec<Event> {
        self.events.try_iter().collect()
    }
    /// Send a play packet; silently dropped once the connection is gone,
    /// and while the server has the connection back in configuration (a
    /// proxy switching servers). Vanilla's `handleConfigurationStart` clears
    /// the level before it acknowledges, so no play packet follows the
    /// acknowledgement; one that did would reach the server as a
    /// configuration packet, and Velocity drops such a client.
    pub fn send(&self, packet: play::Serverbound) {
        if self.debug
            && !matches!(
                packet,
                play::Serverbound::ClientTickEnd(_)
                    | play::Serverbound::MovePlayerPos(_)
                    | play::Serverbound::MovePlayerPosRot(_)
            )
        {
            eprintln!("[net] -> {packet:?}");
        }
        let mut writer = self.writer.lock().unwrap();
        if self.configuring.load(Ordering::Relaxed) {
            return;
        }
        if let Some(w) = writer.as_mut()
            && w.send(&packet.encode()).is_err()
        {
            w.shutdown();
        }
    }
    /// ServerboundResourcePackPacket, in whichever phase the connection is.
    pub fn send_pack_status(&self, id: u128, action: i32) {
        let status = cfg::ResourcePackResponse { id, action };
        let mut writer = self.writer.lock().unwrap();
        let body = if self.configuring.load(Ordering::Relaxed) {
            cfg::Serverbound::ResourcePack(status).encode()
        } else {
            play::Serverbound::ResourcePack(status).encode()
        };
        if let Some(w) = writer.as_mut()
            && w.send(&body).is_err()
        {
            w.shutdown();
        }
    }
    /// ServerboundCustomClickActionPacket (a dialog's answer), in whichever
    /// phase the connection is.
    pub fn send_custom_click(&self, id: &str, payload: Option<horizon_world::nbt::Tag>) {
        let click = cfg::CustomClickAction {
            id: id.to_owned(),
            payload,
        };
        let mut writer = self.writer.lock().unwrap();
        let body = if self.configuring.load(Ordering::Relaxed) {
            cfg::Serverbound::CustomClickAction(click).encode()
        } else {
            play::Serverbound::CustomClickAction(click).encode()
        };
        if let Some(w) = writer.as_mut()
            && w.send(&body).is_err()
        {
            w.shutdown();
        }
    }
    pub fn disconnect(&mut self) {
        if let Some(required) = &self.required {
            required.close();
        }
        if let Some(w) = self.writer.lock().unwrap().take() {
            w.shutdown();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.disconnect();
    }
}

/// An address of the form `friend:K7QM-3XPD` joins a friend's world through
/// its invite code (horizon-friends): the session goes through a tunnel the
/// client reaches on a loopback port, so everything after is a plain join.
pub const FRIEND: &str = horizon_friends::ADDRESS_PREFIX;

/// An address as players and logs see it: a friend's invite code stays
/// out of it (its secret is in there).
pub fn shown_address(address: &str) -> String {
    match address.strip_prefix(FRIEND) {
        Some(code) => match horizon_friends::invite::parse(code) {
            Some(horizon_friends::invite::Parsed::Short(c)) => {
                format!("Friend's world ({}-****)", c.nameplate)
            }
            _ => "Friend's world".to_owned(),
        },
        None => address.to_owned(),
    }
}

/// The relay the player set (`friendsRelay` in horizon.txt, beside
/// options.txt), else `HORIZON_RELAY_URL`. None by default.
pub fn friends_relay_setting() -> Option<String> {
    let dir = std::env::current_dir().ok()?;
    crate::ui_theme::preference(&dir, "friendsRelay")
        .filter(|r| !r.is_empty())
        .or_else(horizon_friends::relay_url)
}
/// Remember the relay typed on Join a friend (empty forgets it).
pub fn save_friends_relay(relay: &str) {
    if let Ok(dir) = std::env::current_dir() {
        crate::ui_theme::set_preference(&dir, "friendsRelay", (!relay.is_empty()).then_some(relay));
    }
}

/// Reach the friend's world from the code (directly, or through a relay)
/// and open the tunnel: the loopback address to connect to.
fn join_friend(code: &str, send: &Sender<Event>) -> Result<(String, u16), String> {
    use horizon_friends::{
        PathMode,
        guest::{self, GuestConfig},
    };
    status(send, "Finding your friend's world...");
    let tunnel = guest::join(&GuestConfig {
        code: code.to_owned(),
        relay: friends_relay_setting(),
        protocol: PROTOCOL_VERSION,
        game: horizon_protocol::GAME_VERSION.to_owned(),
        path: PathMode::from_env(),
    })
    .map_err(|e| e.to_string())?;
    status(
        send,
        if tunnel.direct {
            "Connected straight to your friend..."
        } else {
            "Connected to your friend through the relay..."
        },
    );
    let (port, _) = tunnel.serve_loopback().map_err(|e| e.to_string())?;
    Ok(("127.0.0.1".to_owned(), port))
}

fn status(send: &Sender<Event>, text: &str) {
    let _ = send.send(Event::Status(text.to_owned()));
}

/// Shown when an online-mode server asks a client without an account to authenticate.
pub const NOT_SIGNED_IN: &str = "This server is in online mode. Sign in with your Microsoft account in the Horizon launcher to join it.";

/// A translation, or the key itself (Language.getOrDefault).
fn tr(lang: &Language, key: &str) -> String {
    lang.get(key).cloned().unwrap_or_else(|| key.to_owned())
}

/// The whole connection. Ok: why it ended for good (the server's reason, or
/// a refusal no retry fixes); Err: the connection failed.
fn run(
    local: Option<horizon_protocol::connection::Pipe>,
    address: &str,
    name: &str,
    auth: Option<&AuthSession>,
    view_distance: i8,
    lang: &Language,
    send: &Sender<Event>,
    shared_writer: &Mutex<Option<ConnWriter>>,
    configuring: &AtomicBool,
    shared_counters: &Mutex<Option<Arc<Counters>>>,
    cookies: &Cookies,
    transferred: bool,
    required: Option<crate::lan::login_negotiation::RequiredNegotiation>,
) -> Result<String, String> {
    let (host, port, mut c) = if let Some(pipe) = local {
        (address.to_owned(), 0, Connection::local(pipe))
    } else {
        let (host, port) = match address.strip_prefix(FRIEND) {
            Some(code) => join_friend(code, send)?,
            None => {
                let Some(target) = parse_address(address) else {
                    return Ok(format!("Invalid server address: {address}"));
                };
                status(send, "Connecting to the server...");
                target
            }
        };
        let c = if address.starts_with(FRIEND) {
            // The friends relay supplies a concrete endpoint, not a Minecraft hostname.
            Connection::connect(&host, port, Duration::from_secs(10))
        } else {
            Connection::connect_server(&host, port, Duration::from_secs(10))
        }
        .map_err(|e| e.to_string())?;
        (host, port, c)
    };
    *shared_counters.lock().unwrap() = Some(c.reader.counters.clone());
    let e = |e: horizon_protocol::Error| e.to_string();
    let required_deadline = required.as_ref().map(|r| r.deadline());
    let send_login = |c: &mut Connection, body: &[u8]| match required_deadline {
        Some(deadline) => c.send_until(deadline, body),
        None => c.send(body),
    };
    send_login(
        &mut c,
        &handshake::Serverbound::Intention(handshake::Intention {
            protocol: PROTOCOL_VERSION,
            host: host.clone(),
            port,
            intent: if transferred {
                handshake::TRANSFER
            } else {
                handshake::LOGIN
            },
        })
        .encode(),
    )
    .map_err(e)?;
    // Vanilla's login sends the account's profile id, or the offline one.
    let uuid = auth.map_or_else(|| offline_uuid(name), |a| a.uuid);
    send_login(
        &mut c,
        &login::Serverbound::Hello(login::ServerHello {
            name: name.to_owned(),
            uuid,
        })
        .encode(),
    )
    .map_err(e)?;
    let mut required_auth_seen = false;
    let profile = loop {
        let (id, payload) = match required_deadline {
            Some(deadline) => c.recv_until(deadline),
            None => c.recv(),
        }
        .map_err(e)?;
        match login::Clientbound::decode(id, &payload).map_err(e)? {
            login::Clientbound::Compression(p) => {
                if required.is_some() {
                    return Err("paired login: compression before agreement".into());
                }
                c.set_compression(p.threshold);
            }
            login::Clientbound::LoginFinished(p) => {
                if required.is_some() {
                    return Err("paired login: LoginFinished before agreement".into());
                }
                c.send(&login::Serverbound::LoginAcknowledged(Empty).encode())
                    .map_err(e)?;
                break p.profile;
            }
            login::Clientbound::Disconnect(p) => {
                let reason = serde_json::from_str::<serde_json::Value>(&p.reason_json)
                    .map(|v| json_text(&v, lang))
                    .unwrap_or(p.reason_json);
                return Ok(reason);
            }
            login::Clientbound::Hello(hello) => {
                if required.is_some() && required_auth_seen {
                    return Err("paired login: repeated authentication hello".into());
                }
                required_auth_seen = true;
                status(send, &tr(lang, "connect.authorizing"));
                let key = RsaPublicKey::from_der(&hello.public_key)
                    .map_err(|err| format!("Protocol error: {err}"))?;
                let secret = generate_secret().map_err(e)?;
                if hello.should_authenticate {
                    let account = auth.ok_or_else(|| NOT_SIGNED_IN.to_owned())?;
                    let hash = server_hash(&hello.server_id, &secret, &hello.public_key);
                    if let Err(err) = crate::auth::join_server(account, &hash) {
                        let reason = match err.reason() {
                            Ok(key) => tr(lang, key),
                            Err(message) => message.to_owned(),
                        };
                        return Err(plain(
                            &translatable("disconnect.loginFailedInfo", &[&reason]),
                            lang,
                        ));
                    }
                }
                status(send, &tr(lang, "connect.encrypting"));
                send_login(
                    &mut c,
                    &login::Serverbound::Key(login::Key {
                        key: key.encrypt(&secret).map_err(e)?,
                        challenge: key.encrypt(&hello.challenge).map_err(e)?,
                    })
                    .encode(),
                )
                .map_err(e)?;
                c.enable_encryption(&secret);
            }
            login::Clientbound::CustomQuery(q) => {
                if let Some(required) = &required {
                    use crate::lan::login_negotiation::{
                        CONFIGURATION_DISABLED, NegotiationConnection, negotiate_client_from_query,
                    };
                    let mut scope = NegotiationConnection::for_required(
                        c,
                        required,
                        required_deadline.expect("required deadline"),
                    )
                    .map_err(|e| e.to_string())?;
                    let receipt = negotiate_client_from_query(
                        &mut scope,
                        required.expectation.clone(),
                        uuid,
                        q,
                    )
                    .map_err(|e| e.to_string())?;
                    let observed = scope
                        .observe_configuration_refusal()
                        .map_err(|e| e.to_string());
                    drop(receipt);
                    observed?;
                    return Ok(CONFIGURATION_DISABLED.into());
                }
                c.send(
                    &login::Serverbound::CustomQueryAnswer(login::CustomQueryAnswer {
                        transaction: q.transaction,
                        payload: None,
                    })
                    .encode(),
                )
                .map_err(e)?;
            }
            // ClientHandshakePacketListenerImpl.handleRequestCookie: the
            // cookies a transfer carried over (none on a plain join).
            login::Clientbound::CookieRequest(q) => {
                if required.is_some() {
                    return Err("paired login: cookie request before agreement".into());
                }
                let payload = cookies.lock().unwrap().get(&q.key).cloned();
                c.send(
                    &login::Serverbound::CookieResponse(login::CookieResponse {
                        key: q.key,
                        payload,
                    })
                    .encode(),
                )
                .map_err(e)?;
            }
            login::Clientbound::Unknown(..) => {
                if required.is_some() {
                    return Err("paired login: unexpected login packet".into());
                }
            }
        }
    };
    status(send, "Joining world...");
    c.send(
        &cfg::Serverbound::ClientInformation(cfg::ClientInformation::new(view_distance)).encode(),
    )
    .map_err(e)?;
    c.send(
        &cfg::Serverbound::CustomPayload(cfg::CustomPayload {
            channel: "minecraft:brand".into(),
            data: brand(),
        })
        .encode(),
    )
    .map_err(e)?;
    let (mut reader, writer) = c.split();
    *shared_writer.lock().unwrap() = Some(writer);
    let registries = configure(&mut reader, shared_writer, configuring, lang, send, cookies)?;
    let _ = send.send(Event::Joined {
        registries: Arc::new(registries),
        uuid: profile.uuid,
        name: profile.name,
    });
    receive_play(&mut reader, shared_writer, configuring, lang, send, cookies)
}

fn receive_play(
    reader: &mut ConnReader,
    shared_writer: &Mutex<Option<ConnWriter>>,
    configuring: &AtomicBool,
    lang: &Language,
    send: &Sender<Event>,
    cookies: &Cookies,
) -> Result<String, String> {
    loop {
        let (id, payload) = reader.recv().map_err(|e| e.to_string())?;
        let packet = match play::Clientbound::decode(id, &payload) {
            Ok(p) => p,
            Err(err) => {
                let name = horizon_protocol::ids::name(
                    horizon_protocol::State::Play,
                    horizon_protocol::Direction::Clientbound,
                    id,
                )
                .unwrap_or("?");
                let _ = send.send(Event::DecodeError(format!("{name}: {err}")));
                continue;
            }
        };
        let reply = |p: play::Serverbound| {
            if let Some(w) = shared_writer.lock().unwrap().as_mut() {
                let _ = w.send(&p.encode());
            }
        };
        match packet {
            play::Clientbound::KeepAlive(k) => reply(play::Serverbound::KeepAlive(k)),
            play::Clientbound::BundleDelimiter(_) => {}
            play::Clientbound::ResourcePackPush(p) => {
                let _ = send.send(Event::PackPush(p));
            }
            play::Clientbound::ResourcePackPop(p) => {
                let _ = send.send(Event::PackPop(p.id));
            }
            play::Clientbound::StartConfiguration(_) => {
                if let Some(w) = shared_writer.lock().unwrap().as_mut() {
                    let _ = w.send(&play::Serverbound::ConfigurationAcknowledged(Empty).encode());
                    configuring.store(true, Ordering::Relaxed);
                }
                let registries =
                    configure(reader, shared_writer, configuring, lang, send, cookies)?;
                let _ = send.send(Event::Reconfigured(Arc::new(registries)));
            }
            // ClientCommonPacketListenerImpl.handleRequestCookie: the stored
            // cookie (or none) goes straight back.
            play::Clientbound::CookieRequest(q) => {
                let payload = cookies.lock().unwrap().get(&q.key).cloned();
                reply(play::Serverbound::CookieResponse(cfg::CookieResponse {
                    key: q.key,
                    payload,
                }));
            }
            play::Clientbound::StoreCookie(p) => {
                cookies.lock().unwrap().insert(p.key, p.payload);
            }
            play::Clientbound::ServerLinks(p) => {
                let _ = send.send(Event::ServerLinks(p.links));
            }
            // ClientCommonPacketListenerImpl.handleTransfer: the connection
            // ends and the game connects to the other address.
            play::Clientbound::Transfer(p) => {
                let _ = send.send(Event::Transfer {
                    host: p.host,
                    port: p.port,
                });
                return Ok(tr(lang, "disconnect.transfer"));
            }
            play::Clientbound::Disconnect(d) => return Ok(plain(&d.reason, lang)),
            play::Clientbound::CustomPayload(p) if p.channel == "minecraft:brand" => {
                receive_brand(&p, send);
            }
            other => {
                if send.send(Event::Packet(Box::new(other))).is_err() {
                    return Ok("Disconnected".into());
                }
            }
        }
    }
}

/// The configuration phase: registries, tags and known packs until the
/// server finishes it (ClientConfigurationPacketListenerImpl).
fn configure(
    reader: &mut ConnReader,
    writer: &Mutex<Option<ConnWriter>>,
    configuring: &AtomicBool,
    lang: &Language,
    send: &Sender<Event>,
    cookies: &Cookies,
) -> Result<Dynamic, String> {
    let mut registries = Dynamic::default();
    let reply = |p: cfg::Serverbound| -> Result<(), String> {
        match writer.lock().unwrap().as_mut() {
            Some(w) => w.send(&p.encode()).map_err(|e| e.to_string()),
            None => Err("Disconnected".into()),
        }
    };
    loop {
        let (id, payload) = reader.recv().map_err(|e| e.to_string())?;
        match cfg::Clientbound::decode(id, &payload).map_err(|e| e.to_string())? {
            cfg::Clientbound::KeepAlive(k) => reply(cfg::Serverbound::KeepAlive(k))?,
            cfg::Clientbound::CustomPayload(p) => receive_brand(&p, send),
            cfg::Clientbound::Ping(p) => reply(cfg::Serverbound::Pong(p))?,
            // Know no packs: the server then sends every registry entry in full.
            cfg::Clientbound::SelectKnownPacks(_) => {
                reply(cfg::Serverbound::SelectKnownPacks(cfg::SelectKnownPacks {
                    packs: vec![],
                }))?
            }
            cfg::Clientbound::RegistryData(r) => registries.add(r),
            cfg::Clientbound::CodeOfConduct(_) => {
                reply(cfg::Serverbound::AcceptCodeOfConduct(Empty))?
            }
            // The game answers: it may prompt the player first.
            cfg::Clientbound::ResourcePackPush(p) => {
                let _ = send.send(Event::PackPush(p));
            }
            cfg::Clientbound::ResourcePackPop(p) => {
                let _ = send.send(Event::PackPop(p.id));
            }
            // The game shows it; the answer goes back from there.
            cfg::Clientbound::ShowDialog(d) => {
                let _ = send.send(Event::ShowDialog(d.dialog));
            }
            cfg::Clientbound::ClearDialog(_) => {
                let _ = send.send(Event::ClearDialog);
            }
            cfg::Clientbound::CookieRequest(q) => {
                let payload = cookies.lock().unwrap().get(&q.key).cloned();
                reply(cfg::Serverbound::CookieResponse(cfg::CookieResponse {
                    key: q.key,
                    payload,
                }))?
            }
            cfg::Clientbound::StoreCookie(p) => {
                cookies.lock().unwrap().insert(p.key, p.payload);
            }
            cfg::Clientbound::ServerLinks(p) => {
                let _ = send.send(Event::ServerLinks(p.links));
            }
            cfg::Clientbound::Transfer(p) => {
                let _ = send.send(Event::Transfer {
                    host: p.host,
                    port: p.port,
                });
                return Err(tr(lang, "disconnect.transfer"));
            }
            cfg::Clientbound::Disconnect(d) => return Err(plain(&d.reason, lang)),
            cfg::Clientbound::FinishConfiguration(_) => {
                let mut w = writer.lock().unwrap();
                let w = w.as_mut().ok_or("Disconnected")?;
                w.send(&cfg::Serverbound::FinishConfiguration(Empty).encode())
                    .map_err(|e| e.to_string())?;
                configuring.store(false, Ordering::Relaxed);
                return Ok(registries);
            }
            _ => {}
        }
    }
}

fn receive_brand(payload: &cfg::CustomPayload, send: &Sender<Event>) {
    if payload.channel == "minecraft:brand"
        && let Ok(brand) = horizon_protocol::buf::Reader::new(&payload.data).string(32767)
    {
        let _ = send.send(Event::Brand(brand));
    } else if payload.channel == horizon_protocol::quiet::CHANNEL {
        let _ = send.send(Event::QuietCommands);
    }
}

/// The minecraft:brand payload: a string naming the client.
fn brand() -> Vec<u8> {
    let mut w = horizon_protocol::buf::Writer::new();
    w.string("horizon");
    w.data
}

/// Flatten a JSON chat component (login disconnects are JSON).
pub fn json_text(v: &serde_json::Value, lang: &Language) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(a) => a.iter().map(|e| json_text(e, lang)).collect(),
        serde_json::Value::Object(o) => {
            let mut out = if let Some(t) = o.get("text").and_then(|t| t.as_str()) {
                t.to_owned()
            } else if let Some(key) = o.get("translate").and_then(|t| t.as_str()) {
                let args: Vec<String> = o
                    .get("with")
                    .and_then(|w| w.as_array())
                    .map(|a| a.iter().map(|e| json_text(e, lang)).collect())
                    .unwrap_or_default();
                let pattern = lang
                    .get(key)
                    .cloned()
                    .or_else(|| {
                        o.get("fallback")
                            .and_then(|f| f.as_str())
                            .map(str::to_owned)
                    })
                    .unwrap_or_else(|| key.to_owned());
                horizon_protocol::text::format_java(&pattern, &args)
            } else {
                String::new()
            };
            if let Some(extra) = o.get("extra").and_then(|e| e.as_array()) {
                for e in extra {
                    out.push_str(&json_text(e, lang));
                }
            }
            out
        }
        other => other.to_string(),
    }
}

/// The language a session resolves server-sent components with: the selected
/// one as `lang` loaded it (`en_us` under it, packs included), so a menu title
/// the server sends as a key follows the language screen. A process with no
/// language set (a dedicated server's log) reads en_us.json.
pub fn load_language(resources: &std::path::Path) -> Language {
    if let Some(lang) = crate::lang::current() {
        return lang.map().clone();
    }
    std::fs::read(resources.join("assets/minecraft/lang/en_us.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Ask a server for its status (ServerStatusPinger): MOTD, players and the
/// round trip time in milliseconds.
pub fn ping(address: &str, timeout: Duration) -> Result<(serde_json::Value, u128), String> {
    use horizon_protocol::packets::status;
    let (host, port) = parse_address(address).ok_or("Invalid address")?;
    let mut c = Connection::connect_server(&host, port, timeout).map_err(|e| e.to_string())?;
    c.reader
        .set_read_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    let e = |e: horizon_protocol::Error| e.to_string();
    c.send(
        &handshake::Serverbound::Intention(handshake::Intention {
            protocol: PROTOCOL_VERSION,
            host: host.clone(),
            port,
            intent: handshake::STATUS,
        })
        .encode(),
    )
    .map_err(e)?;
    c.send(&status::Serverbound::StatusRequest(Empty).encode())
        .map_err(e)?;
    let (id, payload) = c.recv().map_err(e)?;
    let json = match status::Clientbound::decode(id, &payload).map_err(e)? {
        status::Clientbound::StatusResponse(s) => {
            serde_json::from_str(&s.json).map_err(|e| e.to_string())?
        }
        _ => return Err("unexpected status reply".into()),
    };
    let start = std::time::Instant::now();
    c.send(&status::Serverbound::PingRequest(status::Ping { time: 1 }).encode())
        .map_err(e)?;
    let _ = c.recv().map_err(e)?;
    Ok((json, start.elapsed().as_millis()))
}

/// Explicit experimental ownership; ordinary `Session` remains the default.
#[cfg(feature = "paired-packages")]
pub use retained_client::*;

#[cfg(feature = "paired-packages")]
mod retained_client {
    use super::*;
    use horizon_protocol::connection::{ConnectionShutdown, Pipe, PipeShutdown};
    use std::{collections::VecDeque, sync::Condvar, time::Instant};

    pub const RETAINED_QUEUE_BYTES: usize = 8 << 20;
    pub const RETAINED_QUEUE_BODIES: usize = 256;

    pub enum RetainedTarget {
        Address(String),
        Memory(Pipe),
        /// Fixture-only original endpoint, configured before codec wrapping.
        #[cfg(test)]
        Connected(std::net::TcpStream),
    }

    pub struct RetainedSettings {
        pub name: String,
        pub view_distance: i8,
        pub language: Arc<Language>,
        pub queue_bytes: usize,
        pub queue_bodies: usize,
    }
    impl RetainedSettings {
        pub fn offline(name: String, view_distance: i8, language: Arc<Language>) -> Self {
            Self {
                name,
                view_distance,
                language,
                queue_bytes: RETAINED_QUEUE_BYTES,
                queue_bodies: RETAINED_QUEUE_BODIES,
            }
        }
    }

    /// Refusal returns the original input, including an already supplied Pipe.
    pub struct RetainedValidation {
        pub reason: String,
        pub target: RetainedTarget,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum RetainedPhase {
        Login,
        Configuration,
        Play,
    }
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum RetainedActor {
        Reader,
        Writer,
    }
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum RetainedOutcome {
        Closed,
        Receive(String),
        Encode(String),
        Io(String),
        Spawn(String),
        Panic,
        ServerDisconnect(String),
        Transfer,
        Refused(String),
        QueueRefused,
    }
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct RetainedTerminal {
        pub actor: RetainedActor,
        pub outcome: RetainedOutcome,
    }
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum SendRefusal {
        Closed,
        Phase,
        Budget,
    }
    #[derive(Debug)]
    pub enum SendAdmission {
        Admitted { sequence: u64 },
        Refused { reason: SendRefusal, body: Vec<u8> },
    }
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum RetainedClose {
        Open,
        Closing,
        Blocked,
        Quiescent,
    }
    #[derive(Clone, Debug)]
    pub struct RetainedObservation {
        pub close: RetainedClose,
        pub started: bool,
        pub acquired: bool,
        pub reader_registered: bool,
        pub writer_registered: bool,
        pub reader_joined: bool,
        pub writer_joined: bool,
        pub planned_phase: RetainedPhase,
        pub last_ack_planned: u64,
        pub last_ack_written: u64,
        pub queued_bodies: usize,
        pub active_bytes: usize,
        pub retained_bytes: usize,
        pub failed_bodies: usize,
        pub queued_events: usize,
        pub packets_sent: i64,
        pub packets_received: i64,
        pub bytes_in: i64,
        pub bytes_out: i64,
        pub terminals: Vec<RetainedTerminal>,
    }
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct RetainedTransfer {
        pub host: String,
        pub port: i32,
    }
    /// A value snapshot; no mutable shared cookie store escapes.
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    pub struct RetainedCookies(pub HashMap<String, Vec<u8>>);

    struct Body {
        sequence: u64,
        bytes: Arc<[u8]>,
        ack: bool,
    }
    impl Clone for Body {
        fn clone(&self) -> Self {
            Self {
                sequence: self.sequence,
                bytes: self.bytes.clone(),
                ack: self.ack,
            }
        }
    }
    enum Interrupt {
        Connection(ConnectionShutdown),
        Memory(PipeShutdown),
        #[cfg(test)]
        Tcp(std::net::TcpStream),
    }
    impl Interrupt {
        fn shutdown(&self) -> std::io::Result<()> {
            match self {
                Self::Connection(c) => c.shutdown(),
                Self::Memory(p) => {
                    p.shutdown();
                    Ok(())
                }
                #[cfg(test)]
                Self::Tcp(s) => s.shutdown(std::net::Shutdown::Both),
            }
        }
    }
    struct State {
        input: Option<RetainedTarget>,
        pending_writer: Option<ConnWriter>,
        interrupt: Option<Arc<Interrupt>>,
        counters: Option<Arc<Counters>>,
        stopped: bool,
        deadline: Option<Instant>,
        started: bool,
        acquired: bool,
        reader: Option<JoinHandle<RetainedOutcome>>,
        writer: Option<JoinHandle<RetainedOutcome>>,
        reader_registered: bool,
        writer_registered: bool,
        reader_joined: bool,
        writer_joined: bool,
        phase: RetainedPhase,
        next_sequence: u64,
        ack_planned: u64,
        ack_written: u64,
        queue: VecDeque<Body>,
        active: Option<Body>,
        retained_bytes: usize,
        failed: Vec<Body>,
        events: VecDeque<Event>,
        terminals: Vec<RetainedTerminal>,
        cookies: HashMap<String, Vec<u8>>,
        transfer: Option<RetainedTransfer>,
        #[cfg(test)]
        controls: Controls,
    }
    struct Controller {
        state: Mutex<State>,
        ready: Condvar,
        settings: RetainedSettings,
        transferred: bool,
    }
    impl Controller {
        fn lock(&self) -> std::sync::MutexGuard<'_, State> {
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }
        fn event(&self, event: Event) {
            self.lock().events.push_back(event);
        }
        fn stop(&self) {
            let cap = {
                let mut s = self.lock();
                s.stopped = true;
                s.interrupt.clone()
            };
            self.ready.notify_all();
            if let Some(cap) = cap {
                // The capability targets only the original native transport.
                // An interrupt request is never completion evidence.
                if let Err(e) = cap.shutdown() {
                    self.lock().terminals.push(RetainedTerminal {
                        actor: RetainedActor::Reader,
                        outcome: RetainedOutcome::Io(e.to_string()),
                    });
                }
            }
        }
        fn terminal(&self, actor: RetainedActor, outcome: RetainedOutcome) {
            let mut s = self.lock();
            if actor == RetainedActor::Writer
                && outcome == RetainedOutcome::Panic
                && let Some(body) = s.active.clone()
            {
                s.failed.push(body);
            }
            if actor == RetainedActor::Reader {
                let from_server = matches!(
                    outcome,
                    RetainedOutcome::ServerDisconnect(_) | RetainedOutcome::Refused(_)
                );
                s.events.push_back(Event::Disconnected {
                    reason: format!("{outcome:?}"),
                    from_server,
                });
            }
            s.terminals.push(RetainedTerminal { actor, outcome });
            drop(s);
            self.stop();
        }
        fn register(&self, c: &Connection) -> bool {
            let cap = Arc::new(Interrupt::Connection(c.shutdown_handle()));
            let stopped = {
                let mut s = self.lock();
                s.acquired = true;
                s.counters = Some(c.reader.counters.clone());
                s.interrupt = Some(cap.clone());
                s.stopped
            };
            if stopped {
                let _ = cap.shutdown();
            }
            !stopped
        }
        fn admit_locked(&self, s: &mut State, bytes: Vec<u8>, ack: bool) -> SendAdmission {
            let reason = if s.stopped {
                Some(SendRefusal::Closed)
            } else if s.queue.len() + usize::from(s.active.is_some()) >= self.settings.queue_bodies
                || bytes.len() > self.settings.queue_bytes.saturating_sub(s.retained_bytes)
            {
                Some(SendRefusal::Budget)
            } else {
                None
            };
            if let Some(reason) = reason {
                return SendAdmission::Refused {
                    reason,
                    body: bytes,
                };
            }
            s.next_sequence += 1;
            let sequence = s.next_sequence;
            s.retained_bytes += bytes.len();
            s.queue.push_back(Body {
                sequence,
                bytes: bytes.into(),
                ack,
            });
            if ack {
                s.ack_planned = sequence;
            }
            self.ready.notify_all();
            SendAdmission::Admitted { sequence }
        }
        fn automatic(
            &self,
            body: Vec<u8>,
            next: Option<RetainedPhase>,
            ack: bool,
        ) -> Result<u64, RetainedOutcome> {
            let mut s = self.lock();
            match self.admit_locked(&mut s, body, ack) {
                SendAdmission::Admitted { sequence } => {
                    if let Some(next) = next {
                        s.phase = next;
                    }
                    Ok(sequence)
                }
                SendAdmission::Refused { body, .. } => {
                    // The rejected automatic reply remains owned with its raw
                    // failure; it was never admitted or claimed delivered.
                    s.failed.push(Body {
                        sequence: 0,
                        bytes: body.into(),
                        ack,
                    });
                    s.stopped = true;
                    drop(s);
                    self.stop();
                    Err(RetainedOutcome::QueueRefused)
                }
            }
        }
        fn await_ack(&self, sequence: u64) -> Result<(), RetainedOutcome> {
            let mut s = self.lock();
            loop {
                if s.ack_written >= sequence {
                    return Ok(());
                }
                if s.stopped {
                    return Err(RetainedOutcome::Closed);
                }
                s = self
                    .ready
                    .wait(s)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }
        fn cookie(&self, key: &str) -> Option<Vec<u8>> {
            self.lock().cookies.get(key).cloned()
        }
        fn transfer(&self, p: cfg::Transfer) -> RetainedOutcome {
            let mut s = self.lock();
            s.transfer = Some(RetainedTransfer {
                host: p.host.clone(),
                port: p.port,
            });
            s.events.push_back(Event::Transfer {
                host: p.host,
                port: p.port,
            });
            RetainedOutcome::Transfer
        }
    }

    pub struct RetainedSession {
        owner: Arc<Controller>,
        resolved: bool,
    }
    pub struct ClosedSession {
        owner: Arc<Controller>,
    }
    impl RetainedSession {
        pub fn new_offline(
            target: RetainedTarget,
            settings: RetainedSettings,
        ) -> Result<Self, RetainedValidation> {
            Self::construct(target, settings, None)
        }
        pub fn new_offline_transferred(
            target: RetainedTarget,
            settings: RetainedSettings,
            cookies: RetainedCookies,
        ) -> Result<Self, RetainedValidation> {
            Self::construct(target, settings, Some(cookies))
        }
        fn construct(
            target: RetainedTarget,
            settings: RetainedSettings,
            cookies: Option<RetainedCookies>,
        ) -> Result<Self, RetainedValidation> {
            let valid_target = match &target {
                RetainedTarget::Address(a) => !a.starts_with(FRIEND) && parse_address(a).is_some(),
                RetainedTarget::Memory(_) => true,
                #[cfg(test)]
                RetainedTarget::Connected(_) => true,
            };
            let valid_name = !settings.name.is_empty()
                && settings.name.len() <= 16
                && settings
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_');
            if !valid_target
                || !valid_name
                || !(2..=32).contains(&settings.view_distance)
                || settings.queue_bytes == 0
                || settings.queue_bytes > RETAINED_QUEUE_BYTES
                || settings.queue_bodies == 0
                || settings.queue_bodies > RETAINED_QUEUE_BODIES
            {
                return Err(RetainedValidation {
                    reason: "invalid offline target, profile, view distance or queue budget".into(),
                    target,
                });
            }
            let interrupt = match &target {
                RetainedTarget::Memory(p) => Some(Arc::new(Interrupt::Memory(p.shutdown_handle()))),
                RetainedTarget::Address(_) => None,
                #[cfg(test)]
                RetainedTarget::Connected(s) => match s.try_clone() {
                    Ok(c) => Some(Arc::new(Interrupt::Tcp(c))),
                    Err(e) => {
                        return Err(RetainedValidation {
                            reason: e.to_string(),
                            target,
                        });
                    }
                },
            };
            let transferred = cookies.is_some();
            Ok(Self {
                owner: Arc::new(Controller {
                    state: Mutex::new(State {
                        input: Some(target),
                        pending_writer: None,
                        interrupt,
                        counters: None,
                        stopped: false,
                        deadline: None,
                        started: false,
                        acquired: false,
                        reader: None,
                        writer: None,
                        reader_registered: false,
                        writer_registered: false,
                        reader_joined: false,
                        writer_joined: false,
                        phase: RetainedPhase::Login,
                        next_sequence: 0,
                        ack_planned: 0,
                        ack_written: 0,
                        queue: VecDeque::new(),
                        active: None,
                        retained_bytes: 0,
                        failed: vec![],
                        events: VecDeque::new(),
                        terminals: vec![],
                        cookies: cookies.unwrap_or_default().0,
                        transfer: None,
                        #[cfg(test)]
                        controls: Controls::default(),
                    }),
                    ready: Condvar::new(),
                    settings,
                    transferred,
                }),
                resolved: false,
            })
        }
        /// The same owner retains the pending input through spawn refusal.
        pub fn start(&mut self) -> Result<(), RetainedOutcome> {
            {
                let mut s = self.owner.lock();
                if s.stopped {
                    return Err(RetainedOutcome::Closed);
                }
                if s.started {
                    return Err(RetainedOutcome::Refused("already started".into()));
                }
                s.started = true;
            }
            let (release, latch) = channel();
            let owner = self.owner.clone();
            #[cfg(test)]
            let fail = self.owner.lock().controls.reader_spawn_failure;
            #[cfg(not(test))]
            let fail = false;
            let spawned = if fail {
                Err(std::io::Error::other("injected reader spawn failure"))
            } else {
                std::thread::Builder::new()
                    .name("horizon-retained-client".into())
                    .spawn(move || {
                        if latch.recv().is_err() {
                            return RetainedOutcome::Closed;
                        }
                        let outcome =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                reader_main(&owner)
                            }))
                            .unwrap_or(RetainedOutcome::Panic);
                        owner.terminal(RetainedActor::Reader, outcome.clone());
                        outcome
                    })
            };
            match spawned {
                Ok(handle) => {
                    let mut s = self.owner.lock();
                    s.reader = Some(handle);
                    s.reader_registered = true;
                    drop(s);
                    let _ = release.send(());
                    Ok(())
                }
                Err(e) => {
                    let outcome = RetainedOutcome::Spawn(e.to_string());
                    self.owner.terminal(RetainedActor::Reader, outcome.clone());
                    Err(outcome)
                }
            }
        }
        pub fn poll_events(&self, max: usize) -> Vec<Event> {
            let mut s = self.owner.lock();
            let take = max.min(s.events.len());
            s.events.drain(..take).collect()
        }
        pub fn send(&self, packet: play::Serverbound) -> SendAdmission {
            let mut s = self.owner.lock();
            let body = packet.encode();
            if !s.stopped && s.phase != RetainedPhase::Play {
                return SendAdmission::Refused {
                    reason: SendRefusal::Phase,
                    body,
                };
            }
            self.owner.admit_locked(&mut s, body, false)
        }
        pub fn send_pack_status(&self, id: u128, action: i32) -> SendAdmission {
            let mut s = self.owner.lock();
            let p = cfg::ResourcePackResponse { id, action };
            let body = match s.phase {
                RetainedPhase::Configuration => cfg::Serverbound::ResourcePack(p).encode(),
                RetainedPhase::Play => play::Serverbound::ResourcePack(p).encode(),
                RetainedPhase::Login => {
                    return SendAdmission::Refused {
                        reason: if s.stopped {
                            SendRefusal::Closed
                        } else {
                            SendRefusal::Phase
                        },
                        body: cfg::Serverbound::ResourcePack(p).encode(),
                    };
                }
            };
            self.owner.admit_locked(&mut s, body, false)
        }
        pub fn send_custom_click(
            &self,
            id: &str,
            payload: Option<horizon_world::nbt::Tag>,
        ) -> SendAdmission {
            let mut s = self.owner.lock();
            let p = cfg::CustomClickAction {
                id: id.into(),
                payload,
            };
            let body = match s.phase {
                RetainedPhase::Configuration => cfg::Serverbound::CustomClickAction(p).encode(),
                RetainedPhase::Play => play::Serverbound::CustomClickAction(p).encode(),
                RetainedPhase::Login => {
                    return SendAdmission::Refused {
                        reason: if s.stopped {
                            SendRefusal::Closed
                        } else {
                            SendRefusal::Phase
                        },
                        body: cfg::Serverbound::CustomClickAction(p).encode(),
                    };
                }
            };
            self.owner.admit_locked(&mut s, body, false)
        }
        pub fn begin_close(&mut self, budget: Duration) -> RetainedClose {
            self.owner
                .lock()
                .deadline
                .get_or_insert_with(|| Instant::now() + budget);
            self.owner.stop();
            self.poll_close()
        }
        pub fn retry_close(&mut self, budget: Duration) -> RetainedClose {
            if self.owner.lock().stopped {
                self.owner.lock().deadline = Some(Instant::now() + budget);
                self.owner.stop();
            }
            self.poll_close()
        }
        pub fn poll_close(&mut self) -> RetainedClose {
            // Only positively finished original handles are taken. No lock is
            // held during a checked join, and each handle can be joined once.
            for actor in [RetainedActor::Writer, RetainedActor::Reader] {
                let handle = {
                    let mut s = self.owner.lock();
                    let slot = match actor {
                        RetainedActor::Reader => &mut s.reader,
                        RetainedActor::Writer => &mut s.writer,
                    };
                    if slot.as_ref().is_some_and(JoinHandle::is_finished) {
                        slot.take()
                    } else {
                        None
                    }
                };
                if let Some(handle) = handle {
                    let joined = handle.join();
                    let mut s = self.owner.lock();
                    match actor {
                        RetainedActor::Reader => s.reader_joined = true,
                        RetainedActor::Writer => s.writer_joined = true,
                    }
                    if joined.is_err() {
                        s.terminals.push(RetainedTerminal {
                            actor,
                            outcome: RetainedOutcome::Panic,
                        });
                    }
                }
            }
            close_state(&self.owner.lock())
        }
        pub fn observe(&self) -> RetainedObservation {
            observation(&self.owner.lock())
        }
        pub fn try_closed(mut self) -> Result<ClosedSession, Self> {
            if self.poll_close() != RetainedClose::Quiescent {
                return Err(self);
            }
            self.resolved = true;
            Ok(ClosedSession {
                owner: self.owner.clone(),
            })
        }
    }
    impl Drop for RetainedSession {
        fn drop(&mut self) {
            if !self.resolved {
                use std::io::Write;
                let _ = std::io::stderr()
                    .write_all(b"fatal: unresolved retained client Session owner\n");
                std::process::abort();
            }
        }
    }
    impl ClosedSession {
        pub fn observe(&self) -> RetainedObservation {
            observation(&self.owner.lock())
        }
        pub fn cookies(&self) -> RetainedCookies {
            RetainedCookies(self.owner.lock().cookies.clone())
        }
        pub fn transfer(&self) -> Option<RetainedTransfer> {
            self.owner.lock().transfer.clone()
        }
        pub fn poll_events(&self, max: usize) -> Vec<Event> {
            let mut s = self.owner.lock();
            let take = max.min(s.events.len());
            s.events.drain(..take).collect()
        }
    }
    fn close_state(s: &State) -> RetainedClose {
        if !s.stopped {
            RetainedClose::Open
        } else if s.reader.is_none()
            && s.writer.is_none()
            && (!s.reader_registered || s.reader_joined)
            && (!s.writer_registered || s.writer_joined)
        {
            RetainedClose::Quiescent
        } else if s.deadline.is_some_and(|d| Instant::now() >= d) {
            RetainedClose::Blocked
        } else {
            RetainedClose::Closing
        }
    }
    fn observation(s: &State) -> RetainedObservation {
        RetainedObservation {
            close: close_state(s),
            started: s.started,
            acquired: s.acquired,
            reader_registered: s.reader_registered,
            writer_registered: s.writer_registered,
            reader_joined: s.reader_joined,
            writer_joined: s.writer_joined,
            planned_phase: s.phase,
            last_ack_planned: s.ack_planned,
            last_ack_written: s.ack_written,
            queued_bodies: s.queue.len(),
            active_bytes: s.active.as_ref().map_or(0, |b| b.bytes.len()),
            retained_bytes: s.retained_bytes,
            failed_bodies: s.failed.len(),
            queued_events: s.events.len(),
            packets_sent: s
                .counters
                .as_ref()
                .map_or(0, |c| c.sent.load(Ordering::Relaxed)),
            packets_received: s
                .counters
                .as_ref()
                .map_or(0, |c| c.received.load(Ordering::Relaxed)),
            bytes_in: s
                .counters
                .as_ref()
                .map_or(0, |c| c.bytes_in.load(Ordering::Relaxed)),
            bytes_out: s
                .counters
                .as_ref()
                .map_or(0, |c| c.bytes_out.load(Ordering::Relaxed)),
            terminals: s.terminals.clone(),
        }
    }

    fn write_native(writer: &mut ConnWriter, body: &[u8]) -> Result<(), RetainedOutcome> {
        let contents = horizon_protocol::frame::contents(body, writer.threshold)
            .map_err(|e| RetainedOutcome::Encode(e.to_string()))?;
        writer
            .send_contents(&contents)
            .map_err(|e| RetainedOutcome::Io(e.to_string()))
    }
    fn reader_main(owner: &Arc<Controller>) -> RetainedOutcome {
        match retained_run(owner) {
            Ok(outcome) | Err(outcome) => outcome,
        }
    }
    fn retained_run(owner: &Arc<Controller>) -> Result<RetainedOutcome, RetainedOutcome> {
        #[cfg(test)]
        test_gate(owner, Gate::BeforeAcquire);
        let target = {
            let mut s = owner.lock();
            if s.stopped {
                return Err(RetainedOutcome::Closed);
            }
            s.input
                .take()
                .ok_or_else(|| RetainedOutcome::Refused("missing original target".into()))?
        };
        let (host, port, mut c) = match target {
            RetainedTarget::Address(address) => {
                let (host, port) = parse_address(&address)
                    .ok_or_else(|| RetainedOutcome::Refused("invalid target".into()))?;
                owner.event(Event::Status("Connecting to the server...".into()));
                let c = Connection::connect_server(&host, port, Duration::from_secs(10))
                    .map_err(|e| RetainedOutcome::Receive(e.to_string()))?;
                (host, port, c)
            }
            RetainedTarget::Memory(pipe) => ("local".into(), 0, Connection::local(pipe)),
            #[cfg(test)]
            RetainedTarget::Connected(socket) => (
                "127.0.0.1".into(),
                25565,
                Connection::from_stream(socket)
                    .map_err(|e| RetainedOutcome::Receive(e.to_string()))?,
            ),
        };
        #[cfg(test)]
        test_gate(owner, Gate::BeforeRegister);
        if !owner.register(&c) {
            return Err(RetainedOutcome::Closed);
        }
        #[cfg(test)]
        test_gate(owner, Gate::AfterRegister);
        if owner.lock().stopped {
            return Err(RetainedOutcome::Closed);
        }
        let settings = &owner.settings;
        write_native(
            &mut c.writer,
            &handshake::Serverbound::Intention(handshake::Intention {
                protocol: PROTOCOL_VERSION,
                host,
                port,
                intent: if owner.transferred {
                    handshake::TRANSFER
                } else {
                    handshake::LOGIN
                },
            })
            .encode(),
        )?;
        write_native(
            &mut c.writer,
            &login::Serverbound::Hello(login::ServerHello {
                name: settings.name.clone(),
                uuid: offline_uuid(&settings.name),
            })
            .encode(),
        )?;
        let receive = |e: horizon_protocol::Error| RetainedOutcome::Receive(e.to_string());
        let profile = loop {
            let (id, body) = c.recv().map_err(receive)?;
            match login::Clientbound::decode(id, &body).map_err(receive)? {
                login::Clientbound::Compression(p) => c.set_compression(p.threshold),
                login::Clientbound::LoginFinished(p) => {
                    write_native(
                        &mut c.writer,
                        &login::Serverbound::LoginAcknowledged(Empty).encode(),
                    )?;
                    break p.profile;
                }
                login::Clientbound::Disconnect(p) => {
                    let reason = serde_json::from_str::<serde_json::Value>(&p.reason_json)
                        .map(|v| json_text(&v, &settings.language))
                        .unwrap_or(p.reason_json);
                    return Ok(RetainedOutcome::ServerDisconnect(reason));
                }
                login::Clientbound::Hello(_) => {
                    return Err(RetainedOutcome::Refused(NOT_SIGNED_IN.into()));
                }
                login::Clientbound::CustomQuery(q) => write_native(
                    &mut c.writer,
                    &login::Serverbound::CustomQueryAnswer(login::CustomQueryAnswer {
                        transaction: q.transaction,
                        payload: None,
                    })
                    .encode(),
                )?,
                login::Clientbound::CookieRequest(q) => write_native(
                    &mut c.writer,
                    &login::Serverbound::CookieResponse(login::CookieResponse {
                        payload: owner.cookie(&q.key),
                        key: q.key,
                    })
                    .encode(),
                )?,
                login::Clientbound::Unknown(..) => {}
            }
        };
        let (mut reader, writer) = c.split();
        start_writer(owner, writer)?;
        owner.automatic(
            cfg::Serverbound::ClientInformation(cfg::ClientInformation::new(
                settings.view_distance,
            ))
            .encode(),
            None,
            false,
        )?;
        owner.automatic(
            cfg::Serverbound::CustomPayload(cfg::CustomPayload {
                channel: "minecraft:brand".into(),
                data: brand(),
            })
            .encode(),
            Some(RetainedPhase::Configuration),
            false,
        )?;
        let registries = retained_configure(&mut reader, owner)?;
        owner.event(Event::Joined {
            registries: Arc::new(registries),
            uuid: profile.uuid,
            name: profile.name,
        });
        loop {
            #[cfg(test)]
            if owner.lock().controls.reader_panic {
                panic!("injected retained reader panic");
            }
            let (id, body) = reader.recv().map_err(receive)?;
            let packet = match play::Clientbound::decode(id, &body) {
                Ok(p) => p,
                Err(e) => {
                    owner.event(Event::DecodeError(e.to_string()));
                    continue;
                }
            };
            match packet {
                play::Clientbound::KeepAlive(k) => {
                    owner.automatic(play::Serverbound::KeepAlive(k).encode(), None, false)?;
                }
                play::Clientbound::BundleDelimiter(_) => {}
                play::Clientbound::ResourcePackPush(p) => owner.event(Event::PackPush(p)),
                play::Clientbound::ResourcePackPop(p) => owner.event(Event::PackPop(p.id)),
                play::Clientbound::StartConfiguration(_) => {
                    let ack = owner.automatic(
                        play::Serverbound::ConfigurationAcknowledged(Empty).encode(),
                        Some(RetainedPhase::Configuration),
                        true,
                    )?;
                    owner.await_ack(ack)?;
                    let registries = retained_configure(&mut reader, owner)?;
                    owner.event(Event::Reconfigured(Arc::new(registries)));
                }
                play::Clientbound::CookieRequest(q) => {
                    owner.automatic(
                        play::Serverbound::CookieResponse(cfg::CookieResponse {
                            payload: owner.cookie(&q.key),
                            key: q.key,
                        })
                        .encode(),
                        None,
                        false,
                    )?;
                }
                play::Clientbound::StoreCookie(p) => {
                    owner.lock().cookies.insert(p.key, p.payload);
                }
                play::Clientbound::ServerLinks(p) => owner.event(Event::ServerLinks(p.links)),
                play::Clientbound::Transfer(p) => return Ok(owner.transfer(p)),
                play::Clientbound::Disconnect(d) => {
                    return Ok(RetainedOutcome::ServerDisconnect(plain(
                        &d.reason,
                        &settings.language,
                    )));
                }
                play::Clientbound::CustomPayload(p)
                    if p.channel == "minecraft:brand"
                        || p.channel == horizon_protocol::quiet::CHANNEL =>
                {
                    retained_brand(&p, owner)
                }
                other => owner.event(Event::Packet(Box::new(other))),
            }
        }
    }
    fn retained_brand(p: &cfg::CustomPayload, owner: &Controller) {
        if p.channel == "minecraft:brand" {
            if let Ok(brand) = horizon_protocol::buf::Reader::new(&p.data).string(32767) {
                owner.event(Event::Brand(brand));
            }
        } else if p.channel == horizon_protocol::quiet::CHANNEL {
            owner.event(Event::QuietCommands);
        }
    }
    fn retained_configure(
        reader: &mut ConnReader,
        owner: &Controller,
    ) -> Result<Dynamic, RetainedOutcome> {
        let mut registries = Dynamic::default();
        loop {
            let (id, body) = reader
                .recv()
                .map_err(|e| RetainedOutcome::Receive(e.to_string()))?;
            let packet = cfg::Clientbound::decode(id, &body)
                .map_err(|e| RetainedOutcome::Receive(e.to_string()))?;
            let reply = match packet {
                cfg::Clientbound::KeepAlive(k) => Some(cfg::Serverbound::KeepAlive(k)),
                cfg::Clientbound::Ping(p) => Some(cfg::Serverbound::Pong(p)),
                cfg::Clientbound::SelectKnownPacks(_) => {
                    Some(cfg::Serverbound::SelectKnownPacks(cfg::SelectKnownPacks {
                        packs: vec![],
                    }))
                }
                cfg::Clientbound::CodeOfConduct(_) => {
                    Some(cfg::Serverbound::AcceptCodeOfConduct(Empty))
                }
                cfg::Clientbound::RegistryData(r) => {
                    registries.add(r);
                    None
                }
                cfg::Clientbound::CustomPayload(p) => {
                    retained_brand(&p, owner);
                    None
                }
                cfg::Clientbound::ResourcePackPush(p) => {
                    owner.event(Event::PackPush(p));
                    None
                }
                cfg::Clientbound::ResourcePackPop(p) => {
                    owner.event(Event::PackPop(p.id));
                    None
                }
                cfg::Clientbound::ShowDialog(d) => {
                    owner.event(Event::ShowDialog(d.dialog));
                    None
                }
                cfg::Clientbound::ClearDialog(_) => {
                    owner.event(Event::ClearDialog);
                    None
                }
                cfg::Clientbound::CookieRequest(q) => {
                    Some(cfg::Serverbound::CookieResponse(cfg::CookieResponse {
                        payload: owner.cookie(&q.key),
                        key: q.key,
                    }))
                }
                cfg::Clientbound::StoreCookie(p) => {
                    owner.lock().cookies.insert(p.key, p.payload);
                    None
                }
                cfg::Clientbound::ServerLinks(p) => {
                    owner.event(Event::ServerLinks(p.links));
                    None
                }
                cfg::Clientbound::Transfer(p) => return Err(owner.transfer(p)),
                cfg::Clientbound::Disconnect(d) => {
                    return Err(RetainedOutcome::ServerDisconnect(plain(
                        &d.reason,
                        &owner.settings.language,
                    )));
                }
                cfg::Clientbound::FinishConfiguration(_) => {
                    let ack = owner.automatic(
                        cfg::Serverbound::FinishConfiguration(Empty).encode(),
                        Some(RetainedPhase::Play),
                        true,
                    )?;
                    owner.await_ack(ack)?;
                    return Ok(registries);
                }
                _ => None,
            };
            if let Some(reply) = reply {
                owner.automatic(reply.encode(), None, false)?;
            }
        }
    }
    fn start_writer(owner: &Arc<Controller>, writer: ConnWriter) -> Result<(), RetainedOutcome> {
        owner.lock().pending_writer = Some(writer);
        let (release, latch) = channel();
        let worker_owner = owner.clone();
        #[cfg(test)]
        let fail = owner.lock().controls.writer_spawn_failure;
        #[cfg(not(test))]
        let fail = false;
        let spawned = if fail {
            Err(std::io::Error::other("injected writer spawn failure"))
        } else {
            std::thread::Builder::new()
                .name("horizon-retained-client-writer".into())
                .spawn(move || {
                    if latch.recv().is_err() {
                        return RetainedOutcome::Closed;
                    }
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        writer_main(&worker_owner)
                    }))
                    .unwrap_or(RetainedOutcome::Panic);
                    worker_owner.terminal(RetainedActor::Writer, outcome.clone());
                    outcome
                })
        };
        match spawned {
            Ok(handle) => {
                let mut s = owner.lock();
                s.writer = Some(handle);
                s.writer_registered = true;
                drop(s);
                let _ = release.send(());
                Ok(())
            }
            Err(e) => {
                let outcome = RetainedOutcome::Spawn(e.to_string());
                owner.terminal(RetainedActor::Writer, outcome.clone());
                Err(outcome)
            }
        }
    }
    fn writer_main(owner: &Controller) -> RetainedOutcome {
        let mut writer = match owner.lock().pending_writer.take() {
            Some(w) => w,
            None => return RetainedOutcome::Refused("missing original writer".into()),
        };
        loop {
            let body = {
                let mut s = owner.lock();
                while s.queue.is_empty() && !s.stopped {
                    s = owner
                        .ready
                        .wait(s)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
                if s.stopped {
                    return RetainedOutcome::Closed;
                }
                let body = s.queue.pop_front().expect("nonempty retained FIFO");
                s.active = Some(body.clone());
                body
            };
            #[cfg(test)]
            {
                test_gate(owner, Gate::WriterActive);
                if owner.lock().controls.writer_panic {
                    panic!("injected retained writer panic");
                }
            }
            if let Err(e) = write_native(&mut writer, &body.bytes) {
                // Active body stays in the registry; queued bodies stay FIFO.
                owner.lock().failed.push(body);
                return e;
            }
            let mut s = owner.lock();
            s.retained_bytes -= body.bytes.len();
            s.active = None;
            if body.ack {
                s.ack_written = body.sequence;
            }
            owner.ready.notify_all();
        }
    }

    #[cfg(test)]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Gate {
        BeforeAcquire,
        BeforeRegister,
        AfterRegister,
        WriterActive,
    }
    #[cfg(test)]
    #[derive(Default)]
    struct Controls {
        gate: Option<Gate>,
        reached: bool,
        release: bool,
        reader_spawn_failure: bool,
        writer_spawn_failure: bool,
        reader_panic: bool,
        writer_panic: bool,
    }
    #[cfg(test)]
    fn test_gate(owner: &Controller, gate: Gate) {
        let mut s = owner.lock();
        if s.controls.gate == Some(gate) {
            s.controls.reached = true;
            owner.ready.notify_all();
            while !s.controls.release {
                s = owner
                    .ready
                    .wait(s)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::{
            io::{Read, Write},
            net::{Shutdown, TcpListener, TcpStream},
        };
        type R<T = ()> = Result<T, String>;
        const WAIT: Duration = Duration::from_secs(5);

        struct Fixture(Option<RetainedSession>);
        impl Fixture {
            fn target(target: RetainedTarget) -> Self {
                let settings =
                    RetainedSettings::offline("RetainedGuest".into(), 2, Arc::new(Language::new()));
                Self(Some(
                    RetainedSession::new_offline(target, settings)
                        .unwrap_or_else(|e| panic!("required fixture: {}", e.reason)),
                ))
            }
            fn pipe() -> (Self, Connection) {
                let (client, server) = Pipe::pair();
                let server = Connection::local(server);
                server
                    .reader
                    .set_read_timeout(Some(WAIT))
                    .expect("required pipe timeout");
                (Self::target(RetainedTarget::Memory(client)), server)
            }
            fn session(&mut self) -> &mut RetainedSession {
                self.0.as_mut().expect("fixture owner")
            }
            fn owner(&self) -> &Arc<Controller> {
                &self.0.as_ref().expect("fixture owner").owner
            }
            fn start(&mut self) -> R {
                self.session().start().map_err(|e| format!("{e:?}"))
            }
            fn wait(&mut self, predicate: impl Fn(&RetainedObservation) -> bool) -> bool {
                let end = Instant::now() + WAIT;
                loop {
                    self.session().poll_close();
                    if predicate(&self.session().observe()) {
                        return true;
                    }
                    if Instant::now() >= end {
                        return false;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            fn gate(&self, gate: Gate) {
                let mut s = self.owner().lock();
                s.controls.gate = Some(gate);
                s.controls.reached = false;
                s.controls.release = false;
            }
            fn reached(&mut self) -> bool {
                let owner = self.owner().clone();
                self.wait(|_| owner.lock().controls.reached)
            }
            fn release(&self) {
                self.owner().lock().controls.release = true;
                self.owner().ready.notify_all();
            }
            fn close(&mut self) -> ClosedSession {
                self.release();
                self.session().begin_close(WAIT);
                let _ = self.wait(|o| o.close == RetainedClose::Quiescent);
                let session = self.0.take().expect("fixture owner");
                match session.try_closed() {
                    Ok(closed) => closed,
                    Err(owner) => {
                        // No abandonment or false completion on a fixture failure.
                        // Root's direct-process watchdog remains a failure backstop.
                        eprintln!(
                            "required retained fixture failed to reap: {:?}",
                            owner.observe()
                        );
                        std::process::abort();
                    }
                }
            }
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                if self.0.is_some() {
                    let _ = self.close();
                }
            }
        }
        fn check(ok: bool, message: &str) -> R {
            if ok { Ok(()) } else { Err(message.into()) }
        }
        fn receive(c: &mut Connection) -> R<(i32, Vec<u8>)> {
            c.recv().map_err(|e| e.to_string())
        }
        fn send(c: &mut Connection, body: &[u8]) -> R {
            c.send(body).map_err(|e| e.to_string())
        }
        fn cfg_recv(c: &mut Connection) -> R<cfg::Serverbound> {
            let (id, body) = receive(c)?;
            cfg::Serverbound::decode(id, &body).map_err(|e| e.to_string())
        }
        fn play_recv(c: &mut Connection) -> R<play::Serverbound> {
            let (id, body) = receive(c)?;
            play::Serverbound::decode(id, &body).map_err(|e| e.to_string())
        }
        fn login(c: &mut Connection, compression: bool) -> R {
            let (id, body) = receive(c)?;
            check(
                matches!(
                    handshake::Serverbound::decode(id, &body).map_err(|e| e.to_string())?,
                    handshake::Serverbound::Intention(_)
                ),
                "handshake",
            )?;
            receive(c)?;
            send(
                c,
                &login::Clientbound::CustomQuery(login::CustomQuery {
                    transaction: 17,
                    channel: "fixture:query".into(),
                    data: vec![7],
                })
                .encode(),
            )?;
            let (id, body) = receive(c)?;
            check(
                matches!(login::Serverbound::decode(id, &body).map_err(|e| e.to_string())?, login::Serverbound::CustomQueryAnswer(p) if p.transaction == 17 && p.payload.is_none()),
                "query None",
            )?;
            if compression {
                send(
                    c,
                    &login::Clientbound::Compression(login::Compression { threshold: 64 }).encode(),
                )?;
                c.set_compression(64);
            }
            send(
                c,
                &login::Clientbound::LoginFinished(login::LoginFinished {
                    profile: login::GameProfile {
                        uuid: 42,
                        name: "RetainedGuest".into(),
                        properties: vec![],
                    },
                    session: 42,
                })
                .encode(),
            )?;
            let (id, body) = receive(c)?;
            check(
                matches!(
                    login::Serverbound::decode(id, &body).map_err(|e| e.to_string())?,
                    login::Serverbound::LoginAcknowledged(_)
                ),
                "login ack",
            )?;
            check(
                matches!(cfg_recv(c)?, cfg::Serverbound::ClientInformation(_)),
                "client information",
            )?;
            check(
                matches!(cfg_recv(c)?, cfg::Serverbound::CustomPayload(p) if p.channel == "minecraft:brand"),
                "brand",
            )?;
            Ok(())
        }
        fn finish(f: &mut Fixture, c: &mut Connection, reconfigured: bool) -> R {
            send(c, &cfg::Clientbound::FinishConfiguration(Empty).encode())?;
            check(
                matches!(cfg_recv(c)?, cfg::Serverbound::FinishConfiguration(_)),
                "config ack",
            )?;
            let mut seen = false;
            check(
                f.wait(|o| {
                    o.last_ack_written == o.last_ack_planned
                        && o.planned_phase == RetainedPhase::Play
                }),
                "actual ack receipt",
            )?;
            for e in f.session().poll_events(256) {
                seen |= if reconfigured {
                    matches!(e, Event::Reconfigured(_))
                } else {
                    matches!(e, Event::Joined { uuid: 42, .. })
                };
            }
            // The ack syscall may return before the reader publishes its event.
            let until = Instant::now() + WAIT;
            while !seen && Instant::now() < until {
                seen = f.session().poll_events(256).into_iter().any(|e| {
                    if reconfigured {
                        matches!(e, Event::Reconfigured(_))
                    } else {
                        matches!(e, Event::Joined { uuid: 42, .. })
                    }
                });
                std::thread::yield_now();
            }
            check(seen, "reader event after actual acknowledgment")
        }
        fn tcp_pair() -> R<(TcpStream, TcpStream)> {
            let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
            let a = TcpStream::connect(listener.local_addr().map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
            let (b, _) = listener.accept().map_err(|e| e.to_string())?;
            // The actual client matches production: no IO timeout can provide
            // its passing termination. Peer waits and root's watchdog fail hard.
            a.set_read_timeout(None).map_err(|e| e.to_string())?;
            a.set_write_timeout(None).map_err(|e| e.to_string())?;
            b.set_read_timeout(Some(WAIT)).map_err(|e| e.to_string())?;
            b.set_write_timeout(Some(WAIT)).map_err(|e| e.to_string())?;
            Ok((a, b))
        }
        fn address_original(gate: Option<Gate>) -> R<(Fixture, TcpStream)> {
            let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
            listener.set_nonblocking(true).map_err(|e| e.to_string())?;
            let mut f = Fixture::target(RetainedTarget::Address(
                listener
                    .local_addr()
                    .map_err(|e| e.to_string())?
                    .to_string(),
            ));
            if let Some(gate) = gate {
                f.gate(gate);
            }
            f.start()?;
            let end = Instant::now() + WAIT;
            let socket = loop {
                match listener.accept() {
                    Ok((s, _)) => break s,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < end =>
                    {
                        std::thread::sleep(Duration::from_millis(1))
                    }
                    Err(e) => return Err(e.to_string()),
                }
            };
            socket
                .set_read_timeout(Some(WAIT))
                .map_err(|e| e.to_string())?;
            socket
                .set_write_timeout(Some(WAIT))
                .map_err(|e| e.to_string())?;
            Ok((f, socket))
        }
        fn address_fixture() -> R<(Fixture, Connection)> {
            let (f, socket) = address_original(None)?;
            Ok((
                f,
                Connection::from_stream(socket).map_err(|e| e.to_string())?,
            ))
        }
        fn read_native_frame(peer: &mut impl Read) -> R<Vec<u8>> {
            let mut length = 0usize;
            for shift in [0, 7, 14] {
                let mut byte = [0u8];
                peer.read_exact(&mut byte).map_err(|e| e.to_string())?;
                length |= usize::from(byte[0] & 0x7f) << shift;
                if byte[0] & 0x80 == 0 {
                    let mut body = vec![0; length];
                    peer.read_exact(&mut body).map_err(|e| e.to_string())?;
                    return Ok(body);
                }
            }
            Err("invalid native frame length".into())
        }
        fn outcome(
            closed: &ClosedSession,
            actor: RetainedActor,
            predicate: impl Fn(&RetainedOutcome) -> bool,
        ) -> bool {
            closed
                .observe()
                .terminals
                .iter()
                .any(|t| t.actor == actor && predicate(&t.outcome))
        }

        #[test]
        fn retained_invalid_input_returns_original_pipe_and_excludes_friends() {
            let (client, server) = Pipe::pair();
            let mut settings =
                RetainedSettings::offline("bad name".into(), 2, Arc::new(Language::new()));
            let refused =
                match RetainedSession::new_offline(RetainedTarget::Memory(client), settings) {
                    Err(e) => e,
                    Ok(mut s) => {
                        s.begin_close(WAIT);
                        let mut f = Fixture(Some(s));
                        f.close();
                        panic!("invalid name accepted");
                    }
                };
            let mut c = match refused.target {
                RetainedTarget::Memory(p) => Connection::local(p),
                _ => panic!("original Pipe missing"),
            };
            let mut peer = Connection::local(server);
            peer.reader.set_read_timeout(Some(WAIT)).unwrap();
            let sent = c.send(&[0]);
            let received = peer.recv();
            c.writer.shutdown();
            peer.writer.shutdown();
            settings = RetainedSettings::offline("Guest".into(), 2, Arc::new(Language::new()));
            let excluded = RetainedSession::new_offline(
                RetainedTarget::Address("friend:K7QM-3XPD".into()),
                settings,
            );
            let excluded = match excluded {
                Err(e) => matches!(e.target, RetainedTarget::Address(_)),
                Ok(mut s) => {
                    s.begin_close(WAIT);
                    Fixture(Some(s)).close();
                    false
                }
            };
            assert!(sent.is_ok() && received.is_ok() && excluded);
        }

        #[test]
        fn retained_close_before_start_owns_original_pipe() {
            let (mut f, mut peer) = Fixture::pipe();
            let state = f.session().begin_close(Duration::ZERO);
            let refused = f.session().start();
            let received = peer.recv();
            let closed = f.close();
            peer.writer.shutdown();
            assert_eq!(state, RetainedClose::Quiescent);
            assert!(refused.is_err() && received.is_err());
            assert!(!closed.observe().reader_registered);
        }

        #[test]
        fn retained_stalled_pipe_login_closes_partial_read() {
            let (client, mut peer) = Pipe::pair();
            let mut f = Fixture::target(RetainedTarget::Memory(client));
            let result = (|| -> R {
                f.start()?;
                check(
                    f.wait(|o| o.packets_sent == 2 && !o.writer_registered),
                    "actual Pipe handshake and login writes",
                )?;
                peer.write_all(&[0x80]).map_err(|e| e.to_string())?;
                check(
                    f.wait(|o| o.acquired && !o.writer_registered && o.bytes_in == 1),
                    "actual partial native Pipe read",
                )
            })();
            let closed = f.close();
            peer.shutdown();
            assert!(result.is_ok(), "{result:?}");
            assert!(closed.observe().reader_joined && !closed.observe().writer_registered);
            assert!(outcome(&closed, RetainedActor::Reader, |o| matches!(
                o,
                RetainedOutcome::Receive(_)
            )));
        }

        #[test]
        fn retained_stalled_tcp_login_closes_partial_read() {
            let (mut f, mut peer) = address_original(None).expect("required address fixture");
            let result = (|| -> R {
                read_native_frame(&mut peer)?;
                read_native_frame(&mut peer)?;
                peer.write_all(&[0x80]).map_err(|e| e.to_string())?;
                check(
                    f.wait(|o| o.acquired && !o.writer_registered && o.bytes_in == 1),
                    "actual partial native TCP read",
                )
            })();
            let closed = f.close();
            let _ = peer.shutdown(Shutdown::Both);
            drop(peer);
            assert!(result.is_ok(), "{result:?}");
            assert!(closed.observe().reader_joined && !closed.observe().writer_registered);
        }

        #[test]
        fn retained_close_before_late_registration_keeps_same_actor() {
            let (mut f, mut peer) = address_original(Some(Gate::BeforeRegister))
                .expect("required production address acquisition");
            let result = (|| -> R {
                check(
                    f.reached(),
                    "late registration gate after actual address acquisition",
                )?;
                let original = f.owner().lock().reader.as_ref().map(|h| h.thread().id());
                check(
                    f.session().begin_close(Duration::ZERO) == RetainedClose::Blocked,
                    "deadline cannot cancel acquisition",
                )?;
                check(
                    !f.session().observe().acquired,
                    "no acquisition receipt before registration",
                )?;
                f.session().retry_close(WAIT);
                check(
                    f.owner().lock().reader.as_ref().map(|h| h.thread().id()) == original,
                    "retry replaced original actor",
                )?;
                f.release();
                Ok(())
            })();
            let closed = f.close();
            let mut byte = [0u8];
            let native_end = peer.read(&mut byte);
            let _ = peer.shutdown(Shutdown::Both);
            drop(peer);
            assert!(result.is_ok(), "{result:?}");
            assert!(closed.observe().acquired && closed.observe().reader_joined);
            assert!(matches!(native_end, Ok(0)));
            assert!(outcome(&closed, RetainedActor::Reader, |o| *o
                == RetainedOutcome::Closed));
        }

        #[test]
        fn retained_close_after_registration_deadline_retry_same_owner() {
            let (mut f, mut peer) = Fixture::pipe();
            f.gate(Gate::AfterRegister);
            let result = (|| -> R {
                f.start()?;
                check(f.reached(), "registered gate")?;
                let original = f.owner().lock().reader.as_ref().map(|h| h.thread().id());
                check(
                    f.session().begin_close(Duration::ZERO) == RetainedClose::Blocked,
                    "actual unfinished original",
                )?;
                for _ in 0..3 {
                    f.session().poll_close();
                    f.session().retry_close(Duration::ZERO);
                }
                check(
                    f.owner().lock().reader.as_ref().map(|h| h.thread().id()) == original,
                    "same actor",
                )?;
                check(peer.recv().is_err(), "early registered native interruption")
            })();
            let closed = f.close();
            peer.writer.shutdown();
            assert!(result.is_ok(), "{result:?}");
            assert!(closed.observe().reader_joined);
            assert_eq!(
                closed
                    .observe()
                    .terminals
                    .iter()
                    .filter(|t| t.actor == RetainedActor::Reader
                        && t.outcome == RetainedOutcome::Closed)
                    .count(),
                1
            );
        }

        #[test]
        fn retained_close_before_acquisition_retains_pending_input_and_actor() {
            let (mut f, mut peer) = Fixture::pipe();
            f.gate(Gate::BeforeAcquire);
            let result = (|| -> R {
                f.start()?;
                check(f.reached(), "original setup actor before acquisition")?;
                let original = f.owner().lock().reader.as_ref().map(|h| h.thread().id());
                check(
                    matches!(f.owner().lock().input, Some(RetainedTarget::Memory(_))),
                    "original input remains registered",
                )?;
                check(
                    f.session().begin_close(Duration::ZERO) == RetainedClose::Blocked,
                    "pending setup not finished",
                )?;
                let deadline = f.owner().lock().deadline;
                f.session().begin_close(WAIT);
                check(
                    f.owner().lock().deadline == deadline,
                    "repeated begin_close cannot renew deadline",
                )?;
                f.session().retry_close(Duration::ZERO);
                check(
                    f.owner().lock().reader.as_ref().map(|h| h.thread().id()) == original,
                    "retry keeps original setup actor",
                )?;
                check(
                    peer.recv().is_err(),
                    "pending supplied endpoint interrupted",
                )
            })();
            let closed = f.close();
            peer.writer.shutdown();
            assert!(result.is_ok(), "{result:?}");
            assert!(!closed.observe().acquired && closed.observe().reader_joined);
            assert!(matches!(
                closed.owner.lock().input,
                Some(RetainedTarget::Memory(_))
            ));
        }

        #[test]
        fn retained_address_login_compression_query_known_packs() {
            let (mut f, mut peer) = address_fixture().expect("required address fixture");
            let result = (|| -> R {
                login(&mut peer, true)?;
                send(
                    &mut peer,
                    &cfg::Clientbound::SelectKnownPacks(cfg::SelectKnownPacks { packs: vec![] })
                        .encode(),
                )?;
                check(
                    matches!(cfg_recv(&mut peer)?, cfg::Serverbound::SelectKnownPacks(p) if p.packs.is_empty()),
                    "empty known packs",
                )?;
                finish(&mut f, &mut peer, false)
            })();
            peer.writer.shutdown();
            let closed = f.close();
            assert!(result.is_ok(), "{result:?}");
            assert!(closed.observe().reader_joined && closed.observe().writer_joined);
        }

        fn phase_case(reconfiguration: bool) -> R {
            let (mut f, mut peer) = Fixture::pipe();
            let result = (|| -> R {
                f.start()?;
                login(&mut peer, false)?;
                if reconfiguration {
                    finish(&mut f, &mut peer, false)?;
                    check(
                        matches!(
                            f.session().send(play::Serverbound::ClientTickEnd(Empty)),
                            SendAdmission::Admitted { .. }
                        ),
                        "old play admission",
                    )?;
                    check(
                        matches!(play_recv(&mut peer)?, play::Serverbound::ClientTickEnd(_)),
                        "old play FIFO",
                    )?;
                    send(
                        &mut peer,
                        &play::Clientbound::StartConfiguration(Empty).encode(),
                    )?;
                    check(
                        matches!(
                            play_recv(&mut peer)?,
                            play::Serverbound::ConfigurationAcknowledged(_)
                        ),
                        "play phase ack",
                    )?;
                    check(
                        f.wait(|o| o.planned_phase == RetainedPhase::Configuration),
                        "planned configuration",
                    )?;
                }
                check(
                    matches!(
                        f.session().send(play::Serverbound::ClientTickEnd(Empty)),
                        SendAdmission::Refused {
                            reason: SendRefusal::Phase,
                            ..
                        }
                    ),
                    "play refused in config",
                )?;
                check(
                    matches!(
                        f.session().send_pack_status(11, 2),
                        SendAdmission::Admitted { .. }
                    ),
                    "config pack",
                )?;
                check(
                    matches!(
                        f.session().send_custom_click("fixture:before", None),
                        SendAdmission::Admitted { .. }
                    ),
                    "config click",
                )?;
                check(
                    matches!(cfg_recv(&mut peer)?, cfg::Serverbound::ResourcePack(p) if p.id == 11),
                    "config pack wire",
                )?;
                check(
                    matches!(cfg_recv(&mut peer)?, cfg::Serverbound::CustomClickAction(p) if p.id == "fixture:before"),
                    "config click wire",
                )?;
                f.gate(Gate::WriterActive);
                send(
                    &mut peer,
                    &cfg::Clientbound::FinishConfiguration(Empty).encode(),
                )?;
                check(f.reached(), "native ack active gate")?;
                let o = f.session().observe();
                check(
                    o.planned_phase == RetainedPhase::Play
                        && o.last_ack_planned > o.last_ack_written,
                    "planned vs actual receipt",
                )?;
                check(
                    !f.session()
                        .poll_events(256)
                        .into_iter()
                        .any(|e| matches!(e, Event::Joined { .. } | Event::Reconfigured(_))),
                    "no early publication",
                )?;
                check(
                    matches!(
                        f.session().send(play::Serverbound::ClientTickEnd(Empty)),
                        SendAdmission::Admitted { .. }
                    ),
                    "play admission behind ack",
                )?;
                check(
                    matches!(
                        f.session().send_pack_status(12, 3),
                        SendAdmission::Admitted { .. }
                    ),
                    "play pack admission behind ack",
                )?;
                check(
                    matches!(
                        f.session().send_custom_click("fixture:after", None),
                        SendAdmission::Admitted { .. }
                    ),
                    "play click admission behind ack",
                )?;
                f.release();
                check(
                    matches!(
                        cfg_recv(&mut peer)?,
                        cfg::Serverbound::FinishConfiguration(_)
                    ),
                    "actual ack first",
                )?;
                check(
                    matches!(play_recv(&mut peer)?, play::Serverbound::ClientTickEnd(_)),
                    "play FIFO",
                )?;
                check(
                    matches!(play_recv(&mut peer)?, play::Serverbound::ResourcePack(p) if p.id == 12),
                    "pack phase FIFO",
                )?;
                check(
                    matches!(play_recv(&mut peer)?, play::Serverbound::CustomClickAction(p) if p.id == "fixture:after"),
                    "click phase FIFO",
                )?;
                check(
                    f.wait(|o| o.last_ack_written == o.last_ack_planned),
                    "written receipt",
                )?;
                let until = Instant::now() + WAIT;
                loop {
                    let seen = f.session().poll_events(256).into_iter().any(|e| {
                        if reconfiguration {
                            matches!(e, Event::Reconfigured(_))
                        } else {
                            matches!(e, Event::Joined { .. })
                        }
                    });
                    if seen {
                        break;
                    }
                    if Instant::now() >= until {
                        return Err("post-write publication absent".into());
                    }
                    std::thread::yield_now();
                }
                Ok(())
            })();
            f.release();
            peer.writer.shutdown();
            let _closed = f.close();
            result
        }
        #[test]
        fn retained_initial_phase_fifo_and_actual_ack_receipt() {
            let r = phase_case(false);
            assert!(r.is_ok(), "{r:?}");
        }
        #[test]
        fn retained_reconfiguration_phase_fifo_and_actual_ack_receipt() {
            let r = phase_case(true);
            assert!(r.is_ok(), "{r:?}");
        }

        #[test]
        fn retained_ping_teleport_keepalive_order() {
            let (mut f, mut peer) = Fixture::pipe();
            let result = (|| -> R {
                f.start()?;
                login(&mut peer, false)?;
                finish(&mut f, &mut peer, false)?;
                let teleport = play::Clientbound::PlayerPosition(play::PlayerPosition {
                    teleport: 42,
                    position: [0.0, 64.0, 0.0],
                    delta: [0.0; 3],
                    yaw: 0.0,
                    pitch: 0.0,
                    relatives: 0,
                });
                send(&mut peer, &teleport.encode())?;
                send(
                    &mut peer,
                    &play::Clientbound::Ping(cfg::Ping { id: 7 }).encode(),
                )?;
                send(
                    &mut peer,
                    &play::Clientbound::KeepAlive(cfg::KeepAlive { id: 9 }).encode(),
                )?;
                check(
                    matches!(play_recv(&mut peer)?, play::Serverbound::KeepAlive(p) if p.id == 9),
                    "automatic keepalive",
                )?;
                let events = f.session().poll_events(256);
                let packets: Vec<_> = events
                    .into_iter()
                    .filter_map(|e| match e {
                        Event::Packet(p) => Some(*p),
                        _ => None,
                    })
                    .collect();
                check(
                    packets == vec![teleport, play::Clientbound::Ping(cfg::Ping { id: 7 })],
                    "teleport before game ping",
                )?;
                f.session().send(play::Serverbound::AcceptTeleportation(
                    play::AcceptTeleportation {
                        id: 42,
                        x: 0.0,
                        y: 64.0,
                        z: 0.0,
                        yaw: 0.0,
                        pitch: 0.0,
                    },
                ));
                f.session()
                    .send(play::Serverbound::Pong(cfg::Ping { id: 7 }));
                check(
                    matches!(
                        play_recv(&mut peer)?,
                        play::Serverbound::AcceptTeleportation(_)
                    ),
                    "game teleport ack first",
                )?;
                check(
                    matches!(play_recv(&mut peer)?, play::Serverbound::Pong(p) if p.id == 7),
                    "game pong second",
                )
            })();
            peer.writer.shutdown();
            let _closed = f.close();
            assert!(result.is_ok(), "{result:?}");
        }

        #[test]
        fn retained_budget_counts_active_and_queued_and_returns_refusal() {
            let (mut f, mut peer) = Fixture::pipe();
            let result = (|| -> R {
                f.start()?;
                login(&mut peer, false)?;
                finish(&mut f, &mut peer, false)?;
                f.gate(Gate::WriterActive);
                let body = play::Serverbound::CustomPayload(cfg::CustomPayload {
                    channel: "fixture:budget".into(),
                    data: vec![0xa5; 2_000_000],
                });
                let bytes = body.encode().len();
                check(
                    matches!(
                        f.session().send(body.clone()),
                        SendAdmission::Admitted { .. }
                    ),
                    "active admitted",
                )?;
                check(f.reached(), "active body owned")?;
                for _ in 0..3 {
                    check(
                        matches!(
                            f.session().send(body.clone()),
                            SendAdmission::Admitted { .. }
                        ),
                        "queued admitted",
                    )?;
                }
                let refused = f.session().send(body.clone());
                check(
                    matches!(refused, SendAdmission::Refused { reason: SendRefusal::Budget, body: ref b } if *b == body.encode()),
                    "full original refused body",
                )?;
                let o = f.session().observe();
                check(
                    o.active_bytes == bytes
                        && o.retained_bytes == bytes * 4
                        && o.queued_bodies == 3,
                    "active+queued budget",
                )
            })();
            f.session().begin_close(WAIT);
            f.release();
            peer.writer.shutdown();
            let closed = f.close();
            assert!(result.is_ok(), "{result:?}");
            assert!(closed.observe().retained_bytes <= RETAINED_QUEUE_BYTES);
        }

        #[test]
        fn retained_automatic_ack_budget_refusal_keeps_body_without_joined() {
            let (mut f, mut peer) = Fixture::pipe();
            let result = (|| -> R {
                f.start()?;
                login(&mut peer, false)?;
                f.gate(Gate::WriterActive);
                check(
                    matches!(
                        f.session().send_pack_status(1, 2),
                        SendAdmission::Admitted { .. }
                    ),
                    "first config body admitted",
                )?;
                check(f.reached(), "original active config body")?;
                for id in 2..=RETAINED_QUEUE_BODIES {
                    check(
                        matches!(
                            f.session().send_pack_status(id as u128, 2),
                            SendAdmission::Admitted { .. }
                        ),
                        "bounded queue fills",
                    )?;
                }
                send(
                    &mut peer,
                    &cfg::Clientbound::FinishConfiguration(Empty).encode(),
                )?;
                check(
                    f.wait(|o| {
                        o.terminals.iter().any(|t| {
                            t.actor == RetainedActor::Reader
                                && t.outcome == RetainedOutcome::QueueRefused
                        })
                    }),
                    "explicit automatic ack refusal",
                )?;
                let s = f.owner().lock();
                check(
                    s.phase == RetainedPhase::Configuration
                        && s.ack_planned == 0
                        && s.ack_written == 0,
                    "refused ack cannot change phase or receipt",
                )?;
                check(
                    s.failed.iter().any(|b| {
                        b.bytes.as_ref() == cfg::Serverbound::FinishConfiguration(Empty).encode()
                    }),
                    "original automatic refused body retained",
                )?;
                check(
                    !s.events.iter().any(|e| matches!(e, Event::Joined { .. })),
                    "refused actual ack cannot publish Joined",
                )
            })();
            f.release();
            peer.writer.shutdown();
            let closed = f.close();
            assert!(result.is_ok(), "{result:?}");
            assert!(
                closed.observe().reader_joined
                    && closed.observe().writer_joined
                    && closed.observe().failed_bodies > 0
            );
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn retained_actual_tcp_backpressure_has_original_active_frame() {
            let (a, b) = tcp_pair().expect("required native pair");
            rustix::net::sockopt::set_socket_send_buffer_size(&a, 4096)
                .expect("required native send buffer");
            rustix::net::sockopt::set_socket_recv_buffer_size(&b, 4096)
                .expect("required native receive buffer");
            let send_bytes = rustix::net::sockopt::socket_send_buffer_size(&a)
                .expect("required actual send buffer");
            let recv_bytes = rustix::net::sockopt::socket_recv_buffer_size(&b)
                .expect("required actual receive buffer");
            let mut observer = b.try_clone().expect("required observer clone");
            let mut f = Fixture::target(RetainedTarget::Connected(a));
            let mut peer = Connection::from_stream(b).expect("required original peer");
            let mut original = None;
            let result = (|| -> R {
                f.start()?;
                login(&mut peer, false)?;
                finish(&mut f, &mut peer, false)?;
                let marker = b"s1tc1-first-native-active-frame-unique-v1";
                let mut data = vec![0xa5; 1_999_900];
                data[..marker.len()].copy_from_slice(marker);
                let packet = play::Serverbound::CustomPayload(cfg::CustomPayload {
                    channel: "fixture:blocked".into(),
                    data,
                });
                let body = packet.encode();
                check(
                    send_bytes > 0
                        && recv_bytes > 0
                        && (send_bytes + recv_bytes + 131) * 8 < body.len(),
                    "actual socket capacity",
                )?;
                check(
                    matches!(f.session().send(packet), SendAdmission::Admitted { .. }),
                    "native frame admitted",
                )?;
                let mut length = 0usize;
                let mut ended = false;
                for shift in [0, 7, 14] {
                    let mut b = [0];
                    observer.read_exact(&mut b).map_err(|e| e.to_string())?;
                    length |= usize::from(b[0] & 0x7f) << shift;
                    if b[0] & 0x80 == 0 {
                        ended = true;
                        break;
                    }
                }
                let mut prefix = [0u8; 128];
                observer
                    .read_exact(&mut prefix)
                    .map_err(|e| e.to_string())?;
                check(
                    ended && length == body.len() && prefix.as_slice() == &body[..128],
                    "same first actual native frame prefix",
                )?;
                let s = f.owner().lock();
                let active = s.active.as_ref().ok_or("original active body absent")?;
                check(
                    active.bytes.as_ref() == body.as_slice(),
                    "same original owned body",
                )?;
                check(
                    s.writer.as_ref().is_some_and(|h| !h.is_finished()) && s.failed.is_empty(),
                    "original writer unfinished BEFORE interruption",
                )?;
                original = Some(active.bytes.clone());
                drop(s);
                // These caller operations only encode/admit/observe; the real
                // writer is still inside its first native send_contents write.
                check(
                    matches!(
                        f.session().send_pack_status(7, 2),
                        SendAdmission::Admitted { .. }
                    ),
                    "status admission independent of blocked IO",
                )?;
                check(
                    matches!(
                        f.session().send(play::Serverbound::ClientTickEnd(Empty)),
                        SendAdmission::Admitted { .. }
                    ),
                    "play admission independent of blocked IO",
                )?;
                check(
                    f.session().observe().active_bytes == body.len(),
                    "same active frame remains",
                )
            })();
            f.session().begin_close(WAIT);
            let _ = observer.shutdown(Shutdown::Both);
            drop(observer);
            peer.writer.shutdown();
            drop(peer);
            let closed = f.close();
            assert!(result.is_ok(), "{result:?}");
            let original = original.expect("native defining frame");
            assert!(Arc::ptr_eq(
                &original,
                &closed
                    .owner
                    .lock()
                    .active
                    .as_ref()
                    .expect("retained original active")
                    .bytes
            ));
            assert!(outcome(&closed, RetainedActor::Writer, |o| matches!(
                o,
                RetainedOutcome::Io(_)
            )));
            assert!(closed.observe().queued_bodies >= 2);
        }

        #[test]
        fn retained_reader_spawn_failure_retains_original_input() {
            let (mut f, mut peer) = Fixture::pipe();
            f.owner().lock().controls.reader_spawn_failure = true;
            let refused = f.session().start();
            let retained = matches!(f.owner().lock().input, Some(RetainedTarget::Memory(_)));
            let ended = peer.recv();
            peer.writer.shutdown();
            let closed = f.close();
            assert!(refused.is_err() && retained && ended.is_err());
            assert!(!closed.observe().reader_registered && !closed.observe().writer_registered);
            assert!(outcome(&closed, RetainedActor::Reader, |o| matches!(
                o,
                RetainedOutcome::Spawn(_)
            )));
        }

        #[test]
        fn retained_writer_spawn_failure_and_panic_keep_independent_custody() {
            for spawn_failure in [true, false] {
                let (mut f, mut peer) = Fixture::pipe();
                f.owner().lock().controls.writer_spawn_failure = spawn_failure;
                if !spawn_failure {
                    f.gate(Gate::WriterActive);
                }
                let result = (|| -> R {
                    f.start()?;
                    if spawn_failure {
                        receive(&mut peer)?;
                        receive(&mut peer)?;
                        send(
                            &mut peer,
                            &login::Clientbound::LoginFinished(login::LoginFinished {
                                profile: login::GameProfile {
                                    uuid: 42,
                                    name: "RetainedGuest".into(),
                                    properties: vec![],
                                },
                                session: 42,
                            })
                            .encode(),
                        )?;
                        receive(&mut peer)?;
                        check(
                            f.wait(|o| {
                                o.terminals.iter().any(|t| t.actor == RetainedActor::Writer)
                            }),
                            "writer spawn outcome",
                        )?;
                        check(
                            f.owner().lock().pending_writer.is_some(),
                            "unstarted original writer retained",
                        )
                    } else {
                        receive(&mut peer)?;
                        receive(&mut peer)?;
                        send(
                            &mut peer,
                            &login::Clientbound::LoginFinished(login::LoginFinished {
                                profile: login::GameProfile {
                                    uuid: 42,
                                    name: "RetainedGuest".into(),
                                    properties: vec![],
                                },
                                session: 42,
                            })
                            .encode(),
                        )?;
                        receive(&mut peer)?;
                        check(f.reached(), "registered active writer")?;
                        check(
                            f.session().observe().writer_registered,
                            "independent registry handle",
                        )?;
                        f.owner().lock().controls.writer_panic = true;
                        f.release();
                        check(
                            f.wait(|o| {
                                o.terminals.iter().any(|t| {
                                    t.actor == RetainedActor::Writer
                                        && t.outcome == RetainedOutcome::Panic
                                })
                            }),
                            "writer panic history",
                        )
                    }
                })();
                f.release();
                peer.writer.shutdown();
                let closed = f.close();
                assert!(result.is_ok(), "{result:?}");
                assert!(closed.observe().reader_joined);
                if !spawn_failure {
                    assert!(
                        closed.observe().writer_joined
                            && closed.observe().failed_bodies > 0
                            && closed.observe().active_bytes > 0
                    );
                }
            }
        }

        #[test]
        fn retained_reader_panic_retry_events_and_finished_only_joins() {
            let (mut f, mut peer) = Fixture::pipe();
            let result = (|| -> R {
                f.start()?;
                login(&mut peer, false)?;
                finish(&mut f, &mut peer, false)?;
                f.owner().lock().controls.reader_panic = true;
                send(
                    &mut peer,
                    &play::Clientbound::KeepAlive(cfg::KeepAlive { id: 31 }).encode(),
                )?;
                check(
                    f.wait(|o| {
                        o.terminals.iter().any(|t| {
                            t.actor == RetainedActor::Reader && t.outcome == RetainedOutcome::Panic
                        })
                    }),
                    "reader panic terminal",
                )?;
                for _ in 0..3 {
                    f.session().retry_close(WAIT);
                    f.session().poll_close();
                }
                check(
                    matches!(
                        f.session().send(play::Serverbound::ClientTickEnd(Empty)),
                        SendAdmission::Refused {
                            reason: SendRefusal::Closed,
                            ..
                        }
                    ),
                    "permanent seal",
                )
            })();
            peer.writer.shutdown();
            let closed = f.close();
            let terminals = closed.observe().terminals;
            let queued = closed.poll_events(1);
            let rest = closed.poll_events(256);
            let empty = closed.poll_events(1);
            assert!(result.is_ok(), "{result:?}");
            assert!(closed.observe().reader_joined && closed.observe().writer_joined);
            assert!(queued.len() <= 1 && empty.is_empty());
            assert!(
                queued
                    .into_iter()
                    .chain(rest)
                    .any(|e| matches!(e, Event::Disconnected { .. }))
            );
            assert_eq!(terminals, closed.observe().terminals);
        }

        #[test]
        fn retained_encode_and_native_failures_preserve_wire_bodies() {
            for encoding in [true, false] {
                let (mut f, mut peer) = Fixture::pipe();
                let result = (|| -> R {
                    f.start()?;
                    login(&mut peer, false)?;
                    finish(&mut f, &mut peer, false)?;
                    f.gate(Gate::WriterActive);
                    let data = vec![0xa5; if encoding { 3_000_000 } else { 128 }];
                    check(
                        matches!(
                            f.session().send(play::Serverbound::CustomPayload(
                                cfg::CustomPayload {
                                    channel: "fixture:failure".into(),
                                    data
                                }
                            )),
                            SendAdmission::Admitted { .. }
                        ),
                        "failure body admitted",
                    )?;
                    check(f.reached(), "active body")?;
                    f.session().send(play::Serverbound::ClientTickEnd(Empty));
                    if !encoding {
                        peer.writer.shutdown();
                    }
                    f.release();
                    check(
                        f.wait(|o| o.terminals.iter().any(|t| t.actor == RetainedActor::Writer)),
                        "writer native terminal",
                    )
                })();
                f.release();
                peer.writer.shutdown();
                let closed = f.close();
                assert!(result.is_ok(), "{result:?}");
                assert!(outcome(&closed, RetainedActor::Writer, |o| if encoding {
                    matches!(o, RetainedOutcome::Encode(_))
                } else {
                    matches!(o, RetainedOutcome::Io(_))
                }));
                assert!(
                    closed.observe().active_bytes > 0
                        && closed.observe().failed_bodies > 0
                        && closed.observe().queued_bodies > 0
                );
            }
        }

        #[test]
        fn retained_transfer_cookie_snapshot_both_phases() {
            for play_phase in [false, true] {
                let (mut f, mut peer) = Fixture::pipe();
                let result = (|| -> R {
                    f.start()?;
                    login(&mut peer, false)?;
                    if play_phase {
                        finish(&mut f, &mut peer, false)?;
                    }
                    let store = cfg::StoreCookie {
                        key: "proxy:session".into(),
                        payload: vec![1, 2, 3],
                    };
                    let ask = cfg::CookieRequest {
                        key: "proxy:session".into(),
                    };
                    let transfer = cfg::Transfer {
                        host: "127.0.0.1".into(),
                        port: 25566,
                    };
                    if play_phase {
                        send(&mut peer, &play::Clientbound::StoreCookie(store).encode())?;
                        send(&mut peer, &play::Clientbound::CookieRequest(ask).encode())?;
                        check(
                            matches!(play_recv(&mut peer)?, play::Serverbound::CookieResponse(p) if p.payload == Some(vec![1,2,3])),
                            "play cookie",
                        )?;
                        send(&mut peer, &play::Clientbound::Transfer(transfer).encode())?;
                    } else {
                        send(&mut peer, &cfg::Clientbound::StoreCookie(store).encode())?;
                        send(&mut peer, &cfg::Clientbound::CookieRequest(ask).encode())?;
                        check(
                            matches!(cfg_recv(&mut peer)?, cfg::Serverbound::CookieResponse(p) if p.payload == Some(vec![1,2,3])),
                            "config cookie",
                        )?;
                        send(&mut peer, &cfg::Clientbound::Transfer(transfer).encode())?;
                    }
                    check(
                        f.wait(|o| {
                            o.terminals
                                .iter()
                                .any(|t| t.outcome == RetainedOutcome::Transfer)
                        }),
                        "old transfer terminal",
                    )
                })();
                peer.writer.shutdown();
                let closed = f.close();
                let mut snapshot = closed.cookies();
                snapshot.0.insert("proxy:session".into(), vec![9]);
                let original = closed.cookies();
                let target = closed.transfer();
                let transfer_event = closed
                    .poll_events(256)
                    .into_iter()
                    .any(|e| matches!(e, Event::Transfer { port: 25566, .. }));
                let (client, server) = Pipe::pair();
                let settings =
                    RetainedSettings::offline("NextGuest".into(), 2, Arc::new(Language::new()));
                let next = RetainedSession::new_offline_transferred(
                    RetainedTarget::Memory(client),
                    settings,
                    original.clone(),
                )
                .unwrap_or_else(|e| panic!("required typed transfer construction: {}", e.reason));
                let mut next = Fixture(Some(next));
                let mut server = Connection::local(server);
                server
                    .reader
                    .set_read_timeout(Some(WAIT))
                    .expect("required handoff peer timeout");
                let handoff = (|| -> R {
                    next.start()?;
                    let (id, body) = receive(&mut server)?;
                    check(
                        matches!(handshake::Serverbound::decode(id, &body).map_err(|e| e.to_string())?, handshake::Serverbound::Intention(p) if p.intent == handshake::TRANSFER),
                        "typed transferred handshake",
                    )?;
                    receive(&mut server)?;
                    send(
                        &mut server,
                        &login::Clientbound::CookieRequest(login::CookieRequest {
                            key: "proxy:session".into(),
                        })
                        .encode(),
                    )?;
                    let (id, body) = receive(&mut server)?;
                    check(
                        matches!(login::Serverbound::decode(id, &body).map_err(|e| e.to_string())?, login::Serverbound::CookieResponse(p) if p.payload == Some(vec![1,2,3])),
                        "transferred login cookie snapshot",
                    )
                })();
                server.writer.shutdown();
                let next_closed = next.close();
                assert!(result.is_ok(), "{result:?}");
                assert!(handoff.is_ok(), "{handoff:?}");
                assert_eq!(original.0.get("proxy:session"), Some(&vec![1, 2, 3]));
                assert_eq!(
                    target,
                    Some(RetainedTransfer {
                        host: "127.0.0.1".into(),
                        port: 25566
                    })
                );
                assert!(
                    transfer_event
                        && closed.observe().reader_joined
                        && closed.observe().writer_joined
                );
                assert!(next_closed.observe().reader_joined);
            }
        }

        #[test]
        #[ignore = "root runs directly with exact SIGABRT/diagnostic and reaping checks"]
        fn retained_unresolved_session_drop_aborts() {
            let (mut f, mut peer) = Fixture::pipe();
            f.start().expect("required real owner start");
            receive(&mut peer).expect("required real handshake");
            receive(&mut peer).expect("required real login");
            eprintln!("S1TC1_UNRESOLVED_SESSION_DROP_ARMED");
            let session = f.0.take().expect("original owner");
            drop(session);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pings_wait_behind_teleports_but_keepalives_do_not_wait_for_the_game() {
        let (client, server) = horizon_protocol::connection::Pipe::pair();
        let (mut reader, writer) = Connection::local(client).split();
        let writer = Arc::new(Mutex::new(Some(writer)));
        let (send, events) = channel();
        let thread_writer = writer.clone();
        let thread = std::thread::spawn(move || {
            receive_play(
                &mut reader,
                &thread_writer,
                &AtomicBool::new(false),
                &Language::new(),
                &send,
                &Cookies::default(),
            )
        });
        let mut server = Connection::local(server);
        let teleport = play::Clientbound::PlayerPosition(play::PlayerPosition {
            teleport: 42,
            position: [10.0, 64.0, 10.0],
            delta: [0.0; 3],
            yaw: 90.0,
            pitch: 0.0,
            relatives: 0,
        });
        server.send(&teleport.encode()).unwrap();
        server
            .send(&play::Clientbound::Ping(cfg::Ping { id: 7 }).encode())
            .unwrap();
        // A reply to this proves the receive loop has passed the ping. The
        // old implementation instead replies with Pong before this keepalive.
        server
            .send(&play::Clientbound::KeepAlive(cfg::KeepAlive { id: 9 }).encode())
            .unwrap();
        let (id, body) = server.recv().unwrap();
        assert_eq!(
            play::Serverbound::decode(id, &body).unwrap(),
            play::Serverbound::KeepAlive(cfg::KeepAlive { id: 9 })
        );
        let timeout = Duration::from_secs(5);
        assert!(
            matches!(events.recv_timeout(timeout).unwrap(), Event::Packet(p) if *p == teleport)
        );
        assert!(
            matches!(events.recv_timeout(timeout).unwrap(), Event::Packet(p) if *p == play::Clientbound::Ping(cfg::Ping { id: 7 }))
        );
        writer.lock().unwrap().take().unwrap().shutdown();
        assert!(thread.join().unwrap().is_err());
    }

    #[test]
    fn play_packets_wait_out_a_reconfiguration() {
        let (client, server) = horizon_protocol::connection::Pipe::pair();
        let (mut reader, writer) = Connection::local(client).split();
        let (send, events) = channel();
        let session = Session {
            events,
            writer: Arc::new(Mutex::new(Some(writer))),
            configuring: Arc::new(AtomicBool::new(false)),
            thread: None,
            address: "local".into(),
            name: "Player".into(),
            counters: Arc::new(Mutex::new(None)),
            cookies: Cookies::default(),
            debug: false,
            required: None,
        };
        let (thread_writer, thread_configuring) =
            (session.writer.clone(), session.configuring.clone());
        let thread = std::thread::spawn(move || {
            receive_play(
                &mut reader,
                &thread_writer,
                &thread_configuring,
                &Language::new(),
                &send,
                &Cookies::default(),
            )
        });
        let mut server = Connection::local(server);
        let tick_end = play::Serverbound::ClientTickEnd(Empty);
        server
            .send(&play::Clientbound::StartConfiguration(Empty).encode())
            .unwrap();
        let (id, body) = server.recv().unwrap();
        assert_eq!(
            play::Serverbound::decode(id, &body).unwrap(),
            play::Serverbound::ConfigurationAcknowledged(Empty)
        );
        // The game thread still ticks the old level: nothing of it goes out.
        session.send(tick_end.clone());
        server
            .send(&cfg::Clientbound::FinishConfiguration(Empty).encode())
            .unwrap();
        let (id, body) = server.recv().unwrap();
        assert_eq!(
            cfg::Serverbound::decode(id, &body).unwrap(),
            cfg::Serverbound::FinishConfiguration(Empty)
        );
        // Back in play, the game's packets flow again.
        session.send(tick_end.clone());
        let (id, body) = server.recv().unwrap();
        assert_eq!(play::Serverbound::decode(id, &body).unwrap(), tick_end);
        drop(session);
        assert!(thread.join().unwrap().is_err());
    }

    #[test]
    fn default_login_keeps_query_none_compression_known_packs_join_and_reconfiguration() {
        let (client, server) = horizon_protocol::connection::Pipe::pair();
        let server = std::thread::spawn(move || {
            let mut c = Connection::local(server);
            c.reader
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            c.recv().unwrap();
            c.recv().unwrap();
            c.send(
                &login::Clientbound::CustomQuery(login::CustomQuery {
                    transaction: 17,
                    channel: crate::lan::login_negotiation::CHANNEL.into(),
                    data: vec![1, 2, 3],
                })
                .encode(),
            )
            .unwrap();
            let (id, body) = c.recv().unwrap();
            assert_eq!(
                login::Serverbound::decode(id, &body).unwrap(),
                login::Serverbound::CustomQueryAnswer(login::CustomQueryAnswer {
                    transaction: 17,
                    payload: None
                })
            );
            c.send(&login::Clientbound::Compression(login::Compression { threshold: 64 }).encode())
                .unwrap();
            c.set_compression(64);
            c.send(
                &login::Clientbound::LoginFinished(login::LoginFinished {
                    profile: login::GameProfile {
                        uuid: 42,
                        name: "DefaultGuest".into(),
                        properties: vec![],
                    },
                    session: 42,
                })
                .encode(),
            )
            .unwrap();
            let (id, body) = c.recv().unwrap();
            assert!(matches!(
                login::Serverbound::decode(id, &body).unwrap(),
                login::Serverbound::LoginAcknowledged(_)
            ));
            c.recv().unwrap();
            c.recv().unwrap();
            c.send(
                &cfg::Clientbound::SelectKnownPacks(cfg::SelectKnownPacks { packs: vec![] })
                    .encode(),
            )
            .unwrap();
            let (id, body) = c.recv().unwrap();
            assert!(
                matches!(cfg::Serverbound::decode(id,&body).unwrap(),cfg::Serverbound::SelectKnownPacks(p) if p.packs.is_empty())
            );
            c.send(&cfg::Clientbound::FinishConfiguration(Empty).encode())
                .unwrap();
            let (id, body) = c.recv().unwrap();
            assert!(matches!(
                cfg::Serverbound::decode(id, &body).unwrap(),
                cfg::Serverbound::FinishConfiguration(_)
            ));
            c.send(&play::Clientbound::StartConfiguration(Empty).encode())
                .unwrap();
            let (id, body) = c.recv().unwrap();
            assert!(matches!(
                play::Serverbound::decode(id, &body).unwrap(),
                play::Serverbound::ConfigurationAcknowledged(_)
            ));
            c.send(&cfg::Clientbound::FinishConfiguration(Empty).encode())
                .unwrap();
            let (id, body) = c.recv().unwrap();
            assert!(matches!(
                cfg::Serverbound::decode(id, &body).unwrap(),
                cfg::Serverbound::FinishConfiguration(_)
            ));
            c.writer.shutdown();
        });
        let mut session =
            Session::connect_local(client, "DefaultGuest", 2, Arc::new(Language::new()));
        let mut joined = false;
        let mut reconfigured = false;
        let mut ended = false;
        while !ended {
            match session.events.recv_timeout(Duration::from_secs(4)).unwrap() {
                Event::Joined { uuid, name, .. } => {
                    assert_eq!(uuid, 42);
                    assert_eq!(name, "DefaultGuest");
                    joined = true;
                }
                Event::Reconfigured(_) => reconfigured = true,
                Event::Disconnected { .. } => ended = true,
                _ => {}
            }
        }
        session.disconnect();
        server.join().unwrap();
        assert!(joined && reconfigured);
    }

    #[test]
    fn play_answers_cookie_requests_from_stored_cookies_and_follows_transfers() {
        let (client, server) = horizon_protocol::connection::Pipe::pair();
        let (mut reader, writer) = Connection::local(client).split();
        let writer = Arc::new(Mutex::new(Some(writer)));
        let (send, events) = channel();
        let cookies = Cookies::default();
        let (thread_writer, thread_cookies) = (writer.clone(), cookies.clone());
        let thread = std::thread::spawn(move || {
            receive_play(
                &mut reader,
                &thread_writer,
                &AtomicBool::new(false),
                &Language::new(),
                &send,
                &thread_cookies,
            )
        });
        let mut server = Connection::local(server);
        let ask = |server: &mut Connection, key: &str| {
            let request = play::Clientbound::CookieRequest(cfg::CookieRequest { key: key.into() });
            server.send(&request.encode()).unwrap();
            let (id, body) = server.recv().unwrap();
            play::Serverbound::decode(id, &body).unwrap()
        };
        // handleRequestCookie answers an unknown key with no payload.
        assert_eq!(
            ask(&mut server, "proxy:session"),
            play::Serverbound::CookieResponse(cfg::CookieResponse {
                key: "proxy:session".into(),
                payload: None,
            })
        );
        let store = play::Clientbound::StoreCookie(cfg::StoreCookie {
            key: "proxy:session".into(),
            payload: vec![1, 2, 3],
        });
        server.send(&store.encode()).unwrap();
        assert_eq!(
            ask(&mut server, "proxy:session"),
            play::Serverbound::CookieResponse(cfg::CookieResponse {
                key: "proxy:session".into(),
                payload: Some(vec![1, 2, 3]),
            })
        );
        let transfer = play::Clientbound::Transfer(cfg::Transfer {
            host: "example.net".into(),
            port: 25566,
        });
        server.send(&transfer.encode()).unwrap();
        assert!(thread.join().unwrap().is_ok());
        assert!(matches!(
            events.try_iter().last(),
            Some(Event::Transfer { host, port: 25566 }) if host == "example.net"
        ));
        // The stored cookie stays for the next connection (TransferState).
        assert_eq!(
            cookies.lock().unwrap().get("proxy:session"),
            Some(&vec![1, 2, 3])
        );
    }

    #[test]
    fn server_brand_payload_is_decoded_and_invalid_or_unrelated_payloads_are_ignored() {
        let (send, events) = channel();
        let mut writer = horizon_protocol::buf::Writer::new();
        writer.string("vanilla");
        let mut payload = cfg::CustomPayload {
            channel: "minecraft:brand".into(),
            data: writer.data,
        };
        receive_brand(&payload, &send);
        assert!(matches!(events.try_recv(), Ok(Event::Brand(brand)) if brand == "vanilla"));
        payload.channel = "example:other".into();
        receive_brand(&payload, &send);
        assert!(events.try_recv().is_err());
        payload.channel = "minecraft:brand".into();
        payload.data = vec![0xff];
        receive_brand(&payload, &send);
        assert!(events.try_recv().is_err());
    }
}
