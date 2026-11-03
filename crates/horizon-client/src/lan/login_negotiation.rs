//! Two-query login compatibility confirmation. Agreement is data, never admission.
#[cfg(feature = "paired-packages")]
use super::data::RegistrySnapshot;
use super::{
    data::SyncData,
    peer_contract::{ContractMismatch, WireContract},
};
#[cfg(feature = "paired-packages")]
use horizon_protocol::connection::{ConnectionShutdown, Counters};
use horizon_protocol::{connection::Connection, crypto::random_bytes, packets::login};
use horizon_world::registry::BlockRegistry;
use sha2::{Digest, Sha256};
use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub const CHANNEL: &str = "horizon:paired_login";
pub const CONFIGURATION_DISABLED: &str = "Paired configuration admission is not enabled";
pub const MAX_PAYLOAD: usize = 8192;
pub const MAX_BUDGET: Duration = Duration::from_secs(120);
const MAGIC: &[u8; 8] = b"HZPLOGIN";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    Budget,
    Entropy,
    Io(String),
    Syntax(&'static str),
    Order,
    Unsupported,
    Mismatch(ContractMismatch),
    Binding,
    Closed,
    ConfigurationDisabled,
}
impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "paired login: {self:?}")
    }
}
impl std::error::Error for Refusal {}
type Result<T> = std::result::Result<T, Refusal>;

/// Retains real native owners. A trusted native constructor is not a host-capture
/// capability; only the binary's frozen startup holder establishes its provenance.
pub struct NegotiationExpectation {
    sync: Arc<SyncData>,
    blocks: &'static BlockRegistry,
    #[cfg(feature = "paired-packages")]
    prepared: Arc<horizon_mods::paired::PreparedBundle>,
    contract: WireContract,
    canonical: Vec<u8>,
}
impl NegotiationExpectation {
    #[cfg(feature = "paired-packages")]
    pub fn from_retained(
        sync: Arc<SyncData>,
        blocks: &'static BlockRegistry,
        prepared: Arc<horizon_mods::paired::PreparedBundle>,
        contract: WireContract,
    ) -> std::result::Result<Arc<Self>, String> {
        let claims = contract.claims();
        let machines = prepared.content().machine_types().types();
        if claims.package_id != prepared.bundle().content_owner().id
            || claims.package != *prepared.identity()
            || machines.len() != 1
            || claims.machine_id != machines[0].id()
            || claims.schema != machines[0].save_schema()
        {
            return Err("negotiation retained package/type/schema projection differs".into());
        }
        let registries = RegistrySnapshot::capture(&sync, blocks, 0, 0)?;
        if claims.registries != &registries {
            return Err("negotiation actual registry/tag/feature projections differ".into());
        }
        let canonical = contract.encode().map_err(|e| e.to_string())?;
        if WireContract::decode(&canonical).map_err(|e| e.to_string())? != contract {
            return Err("negotiation contract is not canonical".into());
        }
        Ok(Arc::new(Self {
            sync,
            blocks,
            prepared,
            contract,
            canonical,
        }))
    }
    pub fn retains_sync(&self, sync: &Arc<SyncData>) -> bool {
        Arc::ptr_eq(&self.sync, sync)
    }
    pub fn retains_blocks(&self, blocks: &'static BlockRegistry) -> bool {
        std::ptr::eq(self.blocks, blocks)
    }
    #[cfg(feature = "paired-packages")]
    pub fn retains_prepared(&self, prepared: &Arc<horizon_mods::paired::PreparedBundle>) -> bool {
        Arc::ptr_eq(&self.prepared, prepared)
    }
    pub fn contract(&self) -> &WireContract {
        &self.contract
    }
}

/// One total budget. The coordinator cannot renew it between protocol phases.
#[derive(Clone)]
pub struct RequiredNegotiation {
    pub(crate) expectation: Arc<NegotiationExpectation>,
    budget: Duration,
    runtime_open: Arc<AtomicBool>,
}
impl RequiredNegotiation {
    pub fn new(expectation: Arc<NegotiationExpectation>, budget: Duration) -> Result<Self> {
        if budget.is_zero() || budget > MAX_BUDGET {
            return Err(Refusal::Budget);
        }
        Ok(Self {
            expectation,
            budget,
            runtime_open: Arc::new(AtomicBool::new(true)),
        })
    }
    pub(crate) fn deadline(&self) -> Instant {
        Instant::now() + self.budget
    }
    pub(crate) fn close(&self) {
        self.runtime_open.store(false, Ordering::Release);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endpoint {
    Server,
    Client,
}
struct Live {
    open: AtomicBool,
    runtime_open: Option<Arc<AtomicBool>>,
}
impl Live {
    fn is_open(&self) -> bool {
        self.open.load(Ordering::Acquire)
            && self
                .runtime_open
                .as_ref()
                .is_none_or(|r| r.load(Ordering::Acquire))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AttemptState {
    Fresh,
    Server,
    Client,
    Agreed,
    #[cfg(feature = "paired-packages")]
    Configuring,
    #[cfg(feature = "paired-packages")]
    Configured,
    #[cfg(feature = "paired-packages")]
    Transferred,
    Closed,
}
/// Owns the actual unsplit connection, with no mutable connection escape hatch.
/// Terminal I/O, explicit close and scope destruction invalidate queued receipts.
pub struct NegotiationConnection {
    connection: Option<Connection>,
    live: Arc<Live>,
    deadline: Instant,
    state: AttemptState,
}
impl NegotiationConnection {
    pub fn new(connection: Connection, deadline: Instant) -> Result<Self> {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || left > MAX_BUDGET {
            connection.writer.shutdown();
            return Err(Refusal::Budget);
        }
        Ok(Self {
            connection: Some(connection),
            live: Arc::new(Live {
                open: AtomicBool::new(true),
                runtime_open: None,
            }),
            deadline,
            state: AttemptState::Fresh,
        })
    }
    pub(crate) fn for_required(
        connection: Connection,
        required: &RequiredNegotiation,
        deadline: Instant,
    ) -> Result<Self> {
        let mut scope = Self::new(connection, deadline)?;
        if !required.runtime_open.load(Ordering::Acquire) {
            scope.close();
            return Err(Refusal::Closed);
        }
        scope.live = Arc::new(Live {
            open: AtomicBool::new(true),
            runtime_open: Some(required.runtime_open.clone()),
        });
        Ok(scope)
    }
    /// Retained actors keep the actual failed acquisition, even on invalid budget
    /// or a runtime that closed before registration completed.
    #[cfg(feature = "paired-packages")]
    #[allow(clippy::result_large_err)]
    pub(crate) fn for_retained(
        connection: Connection,
        required: &RequiredNegotiation,
        deadline: Instant,
    ) -> std::result::Result<Self, (Self, Refusal)> {
        let mut scope = Self {
            connection: Some(connection),
            live: Arc::new(Live {
                open: AtomicBool::new(true),
                runtime_open: Some(required.runtime_open.clone()),
            }),
            deadline,
            state: AttemptState::Fresh,
        };
        let left = deadline.saturating_duration_since(Instant::now());
        let reason = if left.is_zero() || left > MAX_BUDGET {
            Some(Refusal::Budget)
        } else if !scope.live.is_open() {
            Some(Refusal::Closed)
        } else {
            None
        };
        if let Some(reason) = reason {
            scope.close();
            Err((scope, reason))
        } else {
            Ok(scope)
        }
    }
    #[cfg(feature = "paired-packages")]
    pub(crate) fn is_memory(&self) -> bool {
        self.connection
            .as_ref()
            .expect("original unsplit scope")
            .is_memory()
    }
    fn io<T>(&mut self, result: horizon_protocol::Result<T>) -> Result<T> {
        match result {
            Ok(value) if self.live.is_open() => Ok(value),
            Ok(_) => {
                self.close();
                Err(Refusal::Closed)
            }
            Err(error) => {
                self.close();
                Err(Refusal::Io(error.to_string()))
            }
        }
    }
    fn recv(&mut self) -> Result<(i32, Vec<u8>)> {
        if !self.live.is_open() {
            self.close();
            return Err(Refusal::Closed);
        }
        let result = self
            .connection
            .as_mut()
            .expect("unsplit scope")
            .recv_until(self.deadline);
        self.io(result)
    }
    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        if !self.live.is_open() {
            self.close();
            return Err(Refusal::Closed);
        }
        let result = self
            .connection
            .as_mut()
            .expect("unsplit scope")
            .send_until(self.deadline, bytes);
        self.io(result)
    }
    pub fn close(&mut self) {
        self.state = AttemptState::Closed;
        self.live.open.store(false, Ordering::Release);
        if let Some(connection) = &self.connection {
            connection.writer.shutdown();
        }
    }
    fn refused<T>(&mut self, result: Result<T>) -> Result<T> {
        if result.is_err() {
            self.close();
        }
        result
    }
    /// The bounded startup refusal uses the SAME attempt deadline.
    pub fn refuse_configuration(&mut self) -> Result<()> {
        if self.state != AttemptState::Agreed {
            self.close();
            return Err(Refusal::Order);
        }
        let result = self.send(
            &login::Clientbound::Disconnect(login::Disconnect {
                reason_json: format!("\"{CONFIGURATION_DISABLED}\""),
            })
            .encode(),
        );
        self.close();
        result
    }
    /// After client agreement only the terminal login Disconnect is accepted.
    pub fn observe_configuration_refusal(&mut self) -> Result<()> {
        if self.state != AttemptState::Agreed {
            self.close();
            return Err(Refusal::Order);
        }
        let result = (|| {
            let (id, payload) = self.recv()?;
            match login::Clientbound::decode(id, &payload).map_err(|_| Refusal::Order)? {
                login::Clientbound::Disconnect(p)
                    if p.reason_json == format!("\"{CONFIGURATION_DISABLED}\"") =>
                {
                    Ok(())
                }
                _ => Err(Refusal::Order),
            }
        })();
        self.close();
        result
    }
}
impl Drop for NegotiationConnection {
    fn drop(&mut self) {
        #[cfg(feature = "paired-packages")]
        if self.state == AttemptState::Transferred {
            return;
        }
        self.close();
    }
}

/// Opaque endpoint-local agreement. It cannot create a player or grant world access.
/// No Clone, public constructor, serialization or disconnected success value.
pub struct AgreementReceipt {
    role: Endpoint,
    expectation: Arc<NegotiationExpectation>,
    uuid: u128,
    session: [u8; 32],
    transcript: [u8; 32],
    live: Arc<Live>,
}
impl AgreementReceipt {
    pub fn is_live(
        &self,
        scope: &NegotiationConnection,
        role: Endpoint,
        expectation: &Arc<NegotiationExpectation>,
        uuid: u128,
    ) -> bool {
        self.role == role
            && self.uuid == uuid
            && Arc::ptr_eq(&self.expectation, expectation)
            && Arc::ptr_eq(&self.live, &scope.live)
            && self.live.is_open()
            && scope.state == AttemptState::Agreed
            && Instant::now() < scope.deadline
    }
    /// One-shot validation for future configuration consumers; returns only data.
    pub fn consume(
        self,
        scope: &NegotiationConnection,
        role: Endpoint,
        expectation: &Arc<NegotiationExpectation>,
        uuid: u128,
    ) -> Result<([u8; 32], [u8; 32])> {
        if !self.is_live(scope, role, expectation, uuid) {
            return Err(Refusal::Binding);
        }
        Ok((self.session, self.transcript))
    }
}

/// Dormant strict-configuration result. This owns the ORIGINAL unsplit scope;
/// it grants neither player admission nor a Session/Hub publication capability.
#[cfg(feature = "paired-packages")]
pub struct ConfiguredEndpoint {
    scope: NegotiationConnection,
    role: Endpoint,
    expectation: Arc<NegotiationExpectation>,
    profile: login::GameProfile,
    information: horizon_protocol::packets::configuration::ClientInformation,
    registries: horizon_protocol::registry::Dynamic,
    session: [u8; 32],
    transcript: [u8; 32],
}
#[cfg(feature = "paired-packages")]
impl ConfiguredEndpoint {
    pub fn is_live(
        &self,
        role: Endpoint,
        expectation: &Arc<NegotiationExpectation>,
        uuid: u128,
    ) -> bool {
        self.role == role
            && self.profile.uuid == uuid
            && Arc::ptr_eq(&self.expectation, expectation)
            && self.scope.state == AttemptState::Configured
            && self.scope.live.is_open()
            && Instant::now() < self.scope.deadline
    }
    pub fn profile(&self) -> &login::GameProfile {
        &self.profile
    }
    pub fn information(&self) -> &horizon_protocol::packets::configuration::ClientInformation {
        &self.information
    }
    pub fn registries(&self) -> &horizon_protocol::registry::Dynamic {
        &self.registries
    }
    /// Inspectable transcript data is not an admission receipt.
    pub fn transcript(&self) -> ([u8; 32], [u8; 32]) {
        (self.session, self.transcript)
    }
    pub fn close(&mut self) {
        self.scope.close();
    }

    /// Only an actual registered retained actor can request this crate-private
    /// transfer. The caller must install the owner before publishing the envelope.
    #[allow(clippy::result_large_err)]
    pub(crate) fn transfer(
        mut self,
        role: Endpoint,
        required: &RequiredNegotiation,
        counters: &Arc<Counters>,
        uuid: u128,
    ) -> std::result::Result<ConfiguredTransfer, (Self, Refusal)> {
        if !self.is_live(role, &required.expectation, uuid)
            || !self
                .scope
                .live
                .runtime_open
                .as_ref()
                .is_some_and(|runtime| Arc::ptr_eq(runtime, &required.runtime_open))
            || !self
                .scope
                .connection
                .as_ref()
                .is_some_and(|connection| Arc::ptr_eq(&connection.reader.counters, counters))
        {
            return Err((self, Refusal::Binding));
        }
        let connection = self
            .scope
            .connection
            .take()
            .expect("checked original scope");
        self.scope.state = AttemptState::Transferred;
        let life = Arc::new(ConfiguredLife {
            live: self.scope.live.clone(),
            expectation: self.expectation,
            runtime: required.runtime_open.clone(),
            counters: counters.clone(),
            role,
            uuid,
            deadline: self.scope.deadline,
            consumed: AtomicBool::new(false),
        });
        let owner = ConfiguredOwner {
            life: life.clone(),
            shutdown: connection.shutdown_handle(),
        };
        let envelope = ConfiguredEnvelope {
            life,
            payload: Some(ConfiguredMetadata {
                profile: self.profile,
                information: self.information,
                registries: Arc::new(self.registries),
                session: self.session,
                transcript: self.transcript,
            }),
        };
        Ok(ConfiguredTransfer {
            connection,
            owner,
            envelope,
        })
    }
}

#[cfg(feature = "paired-packages")]
struct ConfiguredLife {
    live: Arc<Live>,
    expectation: Arc<NegotiationExpectation>,
    runtime: Arc<AtomicBool>,
    counters: Arc<Counters>,
    role: Endpoint,
    uuid: u128,
    deadline: Instant,
    consumed: AtomicBool,
}
#[cfg(feature = "paired-packages")]
impl ConfiguredLife {
    fn pending(
        &self,
        role: Endpoint,
        required: &RequiredNegotiation,
        counters: &Arc<Counters>,
        uuid: u128,
    ) -> bool {
        self.role == role
            && self.uuid == uuid
            && Arc::ptr_eq(&self.expectation, &required.expectation)
            && Arc::ptr_eq(&self.runtime, &required.runtime_open)
            && Arc::ptr_eq(&self.counters, counters)
            && self.live.is_open()
            && Instant::now() < self.deadline
            && !self.consumed.load(Ordering::Acquire)
    }
}
#[cfg(feature = "paired-packages")]
pub(crate) struct ConfiguredTransfer {
    pub connection: Connection,
    pub owner: ConfiguredOwner,
    pub envelope: ConfiguredEnvelope,
}
/// Opaque queued metadata; only its original Session/Hub can consume it.
#[cfg(feature = "paired-packages")]
pub struct ConfiguredEnvelope {
    life: Arc<ConfiguredLife>,
    payload: Option<ConfiguredMetadata>,
}
#[cfg(feature = "paired-packages")]
pub(crate) struct ConfiguredOwner {
    life: Arc<ConfiguredLife>,
    shutdown: ConnectionShutdown,
}
#[cfg(feature = "paired-packages")]
impl ConfiguredOwner {
    pub(crate) fn invalidate(&self) {
        self.life.live.open.store(false, Ordering::Release);
    }
    pub(crate) fn validates(
        &self,
        envelope: &ConfiguredEnvelope,
        role: Endpoint,
        required: &RequiredNegotiation,
        counters: &Arc<Counters>,
        uuid: u128,
    ) -> bool {
        Arc::ptr_eq(&self.life, &envelope.life)
            && envelope.payload.is_some()
            && self.life.pending(role, required, counters, uuid)
    }
    pub(crate) fn consume(
        &self,
        envelope: &mut ConfiguredEnvelope,
        role: Endpoint,
        required: &RequiredNegotiation,
        counters: &Arc<Counters>,
        uuid: u128,
    ) -> Result<ConfiguredAdmission> {
        if !self.validates(envelope, role, required, counters, uuid)
            || self
                .life
                .consumed
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return Err(Refusal::Binding);
        }
        Ok(ConfiguredAdmission {
            life: self.life.clone(),
            metadata: envelope.payload.take().expect("validated one-use metadata"),
        })
    }
    pub(crate) fn established(&self) -> bool {
        self.life.consumed.load(Ordering::Acquire) && self.life.live.is_open()
    }
}
#[cfg(feature = "paired-packages")]
impl Drop for ConfiguredOwner {
    fn drop(&mut self) {
        self.invalidate();
        let _ = self.shutdown.shutdown();
    }
}
#[cfg(feature = "paired-packages")]
struct ConfiguredMetadata {
    profile: login::GameProfile,
    information: horizon_protocol::packets::configuration::ClientInformation,
    registries: Arc<horizon_protocol::registry::Dynamic>,
    session: [u8; 32],
    transcript: [u8; 32],
}
/// Successful actual-owner consumption. Live/runtime still govern play; the
/// configuration deadline no longer governs this established admission.
#[cfg(feature = "paired-packages")]
pub struct ConfiguredAdmission {
    life: Arc<ConfiguredLife>,
    metadata: ConfiguredMetadata,
}
#[cfg(feature = "paired-packages")]
impl ConfiguredAdmission {
    pub fn is_live(&self) -> bool {
        self.life.live.is_open()
    }
    pub fn profile(&self) -> &login::GameProfile {
        &self.metadata.profile
    }
    pub fn information(&self) -> &horizon_protocol::packets::configuration::ClientInformation {
        &self.metadata.information
    }
    pub fn registries(&self) -> &Arc<horizon_protocol::registry::Dynamic> {
        &self.metadata.registries
    }
    pub fn transcript(&self) -> ([u8; 32], [u8; 32]) {
        (self.metadata.session, self.metadata.transcript)
    }
}

/// Expected failure retains the exact closed original scope, not a replacement
/// connection. Dropping or explicitly closing it uses the existing semantics.
#[cfg(feature = "paired-packages")]
pub struct ConfigurationFailure {
    scope: NegotiationConnection,
    pub reason: Refusal,
}
#[cfg(feature = "paired-packages")]
impl ConfigurationFailure {
    pub fn into_scope(self) -> NegotiationConnection {
        self.scope
    }
}

#[cfg(feature = "paired-packages")]
mod configuration {
    use super::*;
    use horizon_protocol::{
        buf::{Reader, Writer},
        packets::{Empty, configuration as cfg},
        registry::Dynamic,
    };
    // Covers bounded native frame/decompression/encode/copy overlaps and small
    // profile/prefix objects. Variable Tag/container/Dynamic storage is charged
    // separately by capture_for_configuration before any native projection.
    const BUFFERS: usize =
        8 * horizon_protocol::frame::MAX_UNCOMPRESSED + 4 * horizon_protocol::frame::MAX_FRAME;

    fn invalid(stage: &'static str) -> Refusal {
        Refusal::Syntax(stage)
    }
    fn valid_name(name: &str) -> bool {
        !name.is_empty()
            && name.len() <= 16
            && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
    }
    fn check_profile(profile: &login::GameProfile) -> Result<()> {
        if !valid_name(&profile.name)
            || profile.properties.len() > 16
            || profile.properties.iter().any(|p| {
                p.name.len() > 64
                    || p.value.len() > MAX_PAYLOAD
                    || p.signature.as_ref().is_some_and(|s| s.len() > 1024)
            })
        {
            return Err(invalid("configuration profile"));
        }
        Ok(())
    }
    fn information(info: &cfg::ClientInformation) -> Result<()> {
        if info.language.is_empty()
            || info.language.len() > 16
            || !(2..=32).contains(&info.view_distance)
            || !(0..=2).contains(&info.chat_visibility)
            || !(0..=1).contains(&info.main_hand)
            || info.model_customisation & 0x80 != 0
            || !(0..=2).contains(&info.particle_status)
        {
            return Err(invalid("configuration client information"));
        }
        Ok(())
    }
    fn brand(value: &str) -> cfg::CustomPayload {
        let mut data = Writer::new();
        data.string(value);
        cfg::CustomPayload {
            channel: "minecraft:brand".into(),
            data: data.data,
        }
    }
    fn packs(empty: bool) -> cfg::SelectKnownPacks {
        cfg::SelectKnownPacks {
            packs: if empty {
                vec![]
            } else {
                vec![cfg::KnownPack {
                    namespace: super::super::data::CORE_PACK.0.into(),
                    id: super::super::data::CORE_PACK.1.into(),
                    version: super::super::data::CORE_PACK.2.into(),
                }]
            },
        }
    }
    fn exact(scope: &mut NegotiationConnection, body: &[u8]) -> Result<()> {
        let mut expected = Reader::new(body);
        let id = expected
            .varint()
            .map_err(|_| invalid("local configuration packet"))?;
        let (actual_id, actual) = scope.recv()?;
        if actual_id != id || actual != body[expected.position()..] {
            return Err(invalid("configuration native body/order"));
        }
        Ok(())
    }
    fn prefix(scope: &mut NegotiationConnection) -> Result<(i32, Vec<u8>)> {
        let packet = scope.recv()?;
        if packet.1.len() > MAX_PAYLOAD {
            return Err(Refusal::Budget);
        }
        Ok(packet)
    }
    fn prepare(
        scope: &NegotiationConnection,
        receipt: &AgreementReceipt,
        role: Endpoint,
        expected: &Arc<NegotiationExpectation>,
        uuid: u128,
    ) -> Result<()> {
        if !receipt.is_live(scope, role, expected, uuid) {
            return Err(Refusal::Binding);
        }
        let snapshot = RegistrySnapshot::capture_for_configuration(
            &expected.sync,
            expected.blocks,
            BUFFERS,
            32,
        )
        .map_err(|_| Refusal::Budget)?;
        if expected.contract.claims().registries != &snapshot {
            return Err(Refusal::Binding);
        }
        if Instant::now() >= scope.deadline {
            return Err(Refusal::Budget);
        }
        Ok(())
    }

    impl NegotiationConnection {
        /// A separate fixed paired route; existing login-only refusal is intact.
        /// Authentication precedes this call. No ordinary caller selects it yet.
        #[allow(clippy::result_large_err)] // Refusal keeps the original unsplit scope without another allocation.
        pub fn configure_server(
            mut self,
            receipt: AgreementReceipt,
            expected: Arc<NegotiationExpectation>,
            native_profile: login::GameProfile,
            compression: Option<i32>,
        ) -> std::result::Result<ConfiguredEndpoint, ConfigurationFailure> {
            let result = (|| {
                prepare(
                    &self,
                    &receipt,
                    Endpoint::Server,
                    &expected,
                    native_profile.uuid,
                )?;
                check_profile(&native_profile)?;
                if compression.is_some_and(|threshold| threshold < 0)
                    || (self.connection.as_ref().expect("unsplit scope").is_memory()
                        && compression.is_some())
                {
                    return Err(Refusal::Unsupported);
                }
                let finished = login::Clientbound::LoginFinished(login::LoginFinished {
                    profile: native_profile.clone(),
                    session: native_profile.uuid,
                })
                .encode();
                if finished.len() > MAX_PAYLOAD {
                    return Err(Refusal::Budget);
                }
                let (session, transcript) =
                    receipt.consume(&self, Endpoint::Server, &expected, native_profile.uuid)?;
                self.state = AttemptState::Configuring;
                if let Some(threshold) = compression {
                    self.send(
                        &login::Clientbound::Compression(login::Compression { threshold }).encode(),
                    )?;
                    self.connection
                        .as_mut()
                        .expect("unsplit scope")
                        .set_compression(threshold);
                }
                self.send(&finished)?;
                exact(
                    &mut self,
                    &login::Serverbound::LoginAcknowledged(Empty).encode(),
                )?;
                self.send(&cfg::Clientbound::CustomPayload(brand("vanilla")).encode())?;
                self.send(
                    &cfg::Clientbound::UpdateEnabledFeatures(cfg::UpdateEnabledFeatures {
                        features: expected.sync.features.clone(),
                    })
                    .encode(),
                )?;
                self.send(&cfg::Clientbound::SelectKnownPacks(packs(false)).encode())?;
                let (id, payload) = prefix(&mut self)?;
                let info = match cfg::Serverbound::decode(id, &payload)
                    .map_err(|_| invalid("configuration client information"))?
                {
                    cfg::Serverbound::ClientInformation(info) => info,
                    _ => return Err(invalid("configuration client information order")),
                };
                information(&info)?;
                exact(
                    &mut self,
                    &cfg::Serverbound::CustomPayload(brand("horizon")).encode(),
                )?;
                exact(
                    &mut self,
                    &cfg::Serverbound::SelectKnownPacks(packs(true)).encode(),
                )?;
                self.send(
                    &cfg::Clientbound::CustomPayload(cfg::CustomPayload {
                        channel: horizon_protocol::quiet::CHANNEL.into(),
                        data: vec![],
                    })
                    .encode(),
                )?;
                let mut dynamic = Dynamic::default();
                for packet in expected.sync.registry_packets(false) {
                    let native = cfg::Clientbound::RegistryData(packet);
                    self.send(&native.encode())?;
                    let cfg::Clientbound::RegistryData(packet) = native else {
                        unreachable!()
                    };
                    dynamic.add(packet);
                }
                self.send(&cfg::Clientbound::UpdateTags(expected.sync.tags.clone()).encode())?;
                self.send(&cfg::Clientbound::FinishConfiguration(Empty).encode())?;
                exact(
                    &mut self,
                    &cfg::Serverbound::FinishConfiguration(Empty).encode(),
                )?;
                Ok((info, dynamic, session, transcript))
            })();
            match result {
                Ok((information, registries, session, transcript))
                    if self.live.is_open() && Instant::now() < self.deadline =>
                {
                    self.state = AttemptState::Configured;
                    Ok(ConfiguredEndpoint {
                        scope: self,
                        role: Endpoint::Server,
                        expectation: expected,
                        profile: native_profile,
                        information,
                        registries,
                        session,
                        transcript,
                    })
                }
                Ok(_) => {
                    let reason = if self.live.is_open() {
                        Refusal::Budget
                    } else {
                        Refusal::Closed
                    };
                    self.close();
                    Err(ConfigurationFailure {
                        scope: self,
                        reason,
                    })
                }
                Err(reason) => {
                    self.close();
                    Err(ConfigurationFailure {
                        scope: self,
                        reason,
                    })
                }
            }
        }
        #[allow(clippy::result_large_err)] // Refusal keeps the original unsplit scope without another allocation.
        pub fn configure_client(
            mut self,
            receipt: AgreementReceipt,
            expected: Arc<NegotiationExpectation>,
            uuid: u128,
            name: &str,
            info: cfg::ClientInformation,
        ) -> std::result::Result<ConfiguredEndpoint, ConfigurationFailure> {
            let result = (|| {
                prepare(&self, &receipt, Endpoint::Client, &expected, uuid)?;
                if !valid_name(name) {
                    return Err(invalid("configuration profile name"));
                }
                information(&info)?;
                let (session, transcript) =
                    receipt.consume(&self, Endpoint::Client, &expected, uuid)?;
                self.state = AttemptState::Configuring;
                let (mut id, mut payload) = prefix(&mut self)?;
                let mut packet = login::Clientbound::decode(id, &payload)
                    .map_err(|_| invalid("configuration login profile"))?;
                if let login::Clientbound::Compression(compression) = packet {
                    if self.connection.as_ref().expect("unsplit scope").is_memory()
                        || compression.threshold < 0
                    {
                        return Err(Refusal::Unsupported);
                    }
                    self.connection
                        .as_mut()
                        .expect("unsplit scope")
                        .set_compression(compression.threshold);
                    (id, payload) = prefix(&mut self)?;
                    packet = login::Clientbound::decode(id, &payload)
                        .map_err(|_| invalid("configuration login profile"))?;
                }
                let profile = match packet {
                    login::Clientbound::LoginFinished(finished)
                        if finished.profile.uuid == uuid
                            && finished.profile.name == name
                            && finished.session == uuid =>
                    {
                        finished.profile
                    }
                    _ => return Err(invalid("configuration native profile/order")),
                };
                check_profile(&profile)?;
                self.send(&login::Serverbound::LoginAcknowledged(Empty).encode())?;
                exact(
                    &mut self,
                    &cfg::Clientbound::CustomPayload(brand("vanilla")).encode(),
                )?;
                exact(
                    &mut self,
                    &cfg::Clientbound::UpdateEnabledFeatures(cfg::UpdateEnabledFeatures {
                        features: expected.sync.features.clone(),
                    })
                    .encode(),
                )?;
                exact(
                    &mut self,
                    &cfg::Clientbound::SelectKnownPacks(packs(false)).encode(),
                )?;
                self.send(&cfg::Serverbound::ClientInformation(info.clone()).encode())?;
                self.send(&cfg::Serverbound::CustomPayload(brand("horizon")).encode())?;
                self.send(&cfg::Serverbound::SelectKnownPacks(packs(true)).encode())?;
                exact(
                    &mut self,
                    &cfg::Clientbound::CustomPayload(cfg::CustomPayload {
                        channel: horizon_protocol::quiet::CHANNEL.into(),
                        data: vec![],
                    })
                    .encode(),
                )?;
                let mut dynamic = Dynamic::default();
                for packet in expected.sync.registry_packets(false) {
                    let native = cfg::Clientbound::RegistryData(packet);
                    exact(&mut self, &native.encode())?;
                    let cfg::Clientbound::RegistryData(packet) = native else {
                        unreachable!()
                    };
                    dynamic.add(packet);
                }
                exact(
                    &mut self,
                    &cfg::Clientbound::UpdateTags(expected.sync.tags.clone()).encode(),
                )?;
                exact(
                    &mut self,
                    &cfg::Clientbound::FinishConfiguration(Empty).encode(),
                )?;
                self.send(&cfg::Serverbound::FinishConfiguration(Empty).encode())?;
                Ok((profile, dynamic, session, transcript))
            })();
            match result {
                Ok((profile, registries, session, transcript))
                    if self.live.is_open() && Instant::now() < self.deadline =>
                {
                    self.state = AttemptState::Configured;
                    Ok(ConfiguredEndpoint {
                        scope: self,
                        role: Endpoint::Client,
                        expectation: expected,
                        profile,
                        information: info,
                        registries,
                        session,
                        transcript,
                    })
                }
                Ok(_) => {
                    let reason = if self.live.is_open() {
                        Refusal::Budget
                    } else {
                        Refusal::Closed
                    };
                    self.close();
                    Err(ConfigurationFailure {
                        scope: self,
                        reason,
                    })
                }
                Err(reason) => {
                    self.close();
                    Err(ConfigurationFailure {
                        scope: self,
                        reason,
                    })
                }
            }
        }
    }
}

struct Challenge {
    uuid: u128,
    server: [u8; 32],
    q1: i32,
    contract: WireContract,
    bytes: Vec<u8>,
}
struct Answer {
    uuid: u128,
    server: [u8; 32],
    client: [u8; 32],
    contract: WireContract,
    bytes: Vec<u8>,
    transcript: [u8; 32],
}
struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}
impl<'a> Cursor<'a> {
    fn new(data: &'a [u8], phase: u8) -> Result<Self> {
        if data.len() > MAX_PAYLOAD {
            return Err(Refusal::Syntax("payload limit"));
        }
        let mut r = Self { data, pos: 0 };
        if r.take(8)? != MAGIC || r.array::<2>()? != [0, 1] || r.take(1)? != [phase] {
            return Err(Refusal::Syntax("magic/version/phase"));
        }
        Ok(r)
    }
    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(count)
            .ok_or(Refusal::Syntax("length overflow"))?;
        let bytes = self
            .data
            .get(self.pos..end)
            .ok_or(Refusal::Syntax("truncated"))?;
        self.pos = end;
        Ok(bytes)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.take(N)?
            .try_into()
            .map_err(|_| Refusal::Syntax("truncated"))
    }
    fn contract(&mut self) -> Result<(WireContract, Vec<u8>)> {
        let count = u16::from_be_bytes(self.array()?) as usize;
        let bytes = self.take(count)?;
        let contract = WireContract::decode(bytes).map_err(|_| Refusal::Syntax("wire contract"))?;
        if contract
            .encode()
            .map_err(|_| Refusal::Syntax("wire contract"))?
            != bytes
        {
            return Err(Refusal::Syntax("noncanonical contract"));
        }
        Ok((contract, bytes.to_vec()))
    }
    fn end(self) -> Result<()> {
        if self.pos == self.data.len() {
            Ok(())
        } else {
            Err(Refusal::Syntax("trailing"))
        }
    }
}
fn prefix(phase: u8) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    out.extend([0, 1, phase]);
    out
}
fn put_contract(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    out.extend_from_slice(bytes);
}
fn challenge(bytes: &[u8]) -> Result<Challenge> {
    let mut r = Cursor::new(bytes, 1)?;
    let uuid = u128::from_be_bytes(r.array()?);
    let server = r.array()?;
    let q1 = i32::from_be_bytes(r.array()?);
    if q1 < 0 {
        return Err(Refusal::Binding);
    }
    let (contract, bytes) = r.contract()?;
    r.end()?;
    Ok(Challenge {
        uuid,
        server,
        q1,
        contract,
        bytes,
    })
}
fn answer(bytes: &[u8]) -> Result<Answer> {
    let mut r = Cursor::new(bytes, 2)?;
    let uuid = u128::from_be_bytes(r.array()?);
    let server = r.array()?;
    let client = r.array()?;
    let (contract, bytes) = r.contract()?;
    let transcript = r.array()?;
    r.end()?;
    Ok(Answer {
        uuid,
        server,
        client,
        contract,
        bytes,
        transcript,
    })
}
fn confirmation(bytes: &[u8], phase: u8) -> Result<[u8; 32]> {
    let mut r = Cursor::new(bytes, phase)?;
    let hash = r.array()?;
    r.end()?;
    Ok(hash)
}
fn confirm(phase: u8, transcript: &[u8; 32]) -> Vec<u8> {
    let mut out = prefix(phase);
    out.extend(transcript);
    out
}
fn next_transaction(q0: i32) -> i32 {
    if q0 == i32::MAX { 0 } else { q0 + 1 }
}
fn hashes(
    server: &[u8; 32],
    client: &[u8; 32],
    uuid: u128,
    q0: i32,
    q1: i32,
    server_contract: &[u8],
    client_contract: &[u8],
) -> ([u8; 32], [u8; 32]) {
    let mut hash = Sha256::new();
    hash.update(b"horizon-login-session-1\0");
    hash.update(server);
    hash.update(client);
    hash.update(uuid.to_be_bytes());
    hash.update(q0.to_be_bytes());
    hash.update(q1.to_be_bytes());
    let session: [u8; 32] = hash.finalize().into();
    let mut hash = Sha256::new();
    hash.update(b"horizon-login-transcript-1\0");
    hash.update(session);
    hash.update((server_contract.len() as u16).to_be_bytes());
    hash.update(server_contract);
    hash.update((client_contract.len() as u16).to_be_bytes());
    hash.update(client_contract);
    (session, hash.finalize().into())
}
fn query(q: i32, data: Vec<u8>) -> Vec<u8> {
    login::Clientbound::CustomQuery(login::CustomQuery {
        transaction: q,
        channel: CHANNEL.into(),
        data,
    })
    .encode()
}
fn reply(q: i32, data: Vec<u8>) -> Vec<u8> {
    login::Serverbound::CustomQueryAnswer(login::CustomQueryAnswer {
        transaction: q,
        payload: Some(data),
    })
    .encode()
}
fn receive_answer(scope: &mut NegotiationConnection, transaction: i32) -> Result<Vec<u8>> {
    let (id, data) = scope.recv()?;
    match login::Serverbound::decode(id, &data).map_err(|_| Refusal::Order)? {
        login::Serverbound::CustomQueryAnswer(a) if a.transaction == transaction => {
            a.payload.ok_or(Refusal::Unsupported)
        }
        _ => Err(Refusal::Order),
    }
}
fn receive_query(scope: &mut NegotiationConnection) -> Result<login::CustomQuery> {
    let (id, data) = scope.recv()?;
    match login::Clientbound::decode(id, &data).map_err(|_| Refusal::Order)? {
        login::Clientbound::CustomQuery(q) if q.channel == CHANNEL => Ok(q),
        _ => Err(Refusal::Order),
    }
}

pub fn negotiate_server(
    scope: &mut NegotiationConnection,
    expectation: Arc<NegotiationExpectation>,
    uuid: u128,
) -> Result<AgreementReceipt> {
    if scope.state != AttemptState::Fresh {
        scope.close();
        return Err(Refusal::Order);
    }
    scope.state = AttemptState::Server;
    let result = (|| {
        let mut nonce = [0; 32];
        let mut transaction = [0; 4];
        random_bytes(&mut nonce).map_err(|_| Refusal::Entropy)?;
        random_bytes(&mut transaction).map_err(|_| Refusal::Entropy)?;
        let q0 = i32::from_be_bytes(transaction) & i32::MAX;
        let q1 = next_transaction(q0);
        let mut data = prefix(1);
        data.extend(uuid.to_be_bytes());
        data.extend(nonce);
        data.extend(q1.to_be_bytes());
        put_contract(&mut data, &expectation.canonical);
        scope.send(&query(q0, data))?;
        let peer = answer(&receive_answer(scope, q0)?)?;
        if peer.uuid != uuid || peer.server != nonce {
            return Err(Refusal::Binding);
        }
        expectation
            .contract
            .compare(&peer.contract)
            .map_err(Refusal::Mismatch)?;
        let (session, transcript) = hashes(
            &nonce,
            &peer.client,
            uuid,
            q0,
            q1,
            &expectation.canonical,
            &peer.bytes,
        );
        if peer.transcript != transcript {
            return Err(Refusal::Binding);
        }
        scope.send(&query(q1, confirm(3, &transcript)))?;
        if confirmation(&receive_answer(scope, q1)?, 4)? != transcript {
            return Err(Refusal::Binding);
        }
        scope.state = AttemptState::Agreed;
        Ok(AgreementReceipt {
            role: Endpoint::Server,
            expectation,
            uuid,
            session,
            transcript,
            live: scope.live.clone(),
        })
    })();
    scope.refused(result)
}

/// The caller may already have decoded the first query while handling online
/// authentication. Its deadline began BEFORE waiting for that first query.
pub(crate) fn negotiate_client_from_query(
    scope: &mut NegotiationConnection,
    expectation: Arc<NegotiationExpectation>,
    uuid: u128,
    first: login::CustomQuery,
) -> Result<AgreementReceipt> {
    let result = prepare_client_confirmation(scope, expectation, uuid, first)
        .and_then(|pending| finish_client_confirmation(scope, pending));
    scope.refused(result)
}
/// Private state between verified phase3 and the final native write. It is bound
/// to this actual connection and cannot be reused on a retry or become a receipt.
struct ClientConfirmation {
    expectation: Arc<NegotiationExpectation>,
    uuid: u128,
    q1: i32,
    session: [u8; 32],
    transcript: [u8; 32],
    live: Arc<Live>,
}
fn prepare_client_confirmation(
    scope: &mut NegotiationConnection,
    expectation: Arc<NegotiationExpectation>,
    uuid: u128,
    first: login::CustomQuery,
) -> Result<ClientConfirmation> {
    if scope.state != AttemptState::Fresh {
        scope.close();
        return Err(Refusal::Order);
    }
    scope.state = AttemptState::Client;
    let result = (|| {
        if first.channel != CHANNEL || first.transaction < 0 {
            return Err(Refusal::Order);
        }
        let peer = challenge(&first.data)?;
        if peer.uuid != uuid || peer.q1 != next_transaction(first.transaction) {
            return Err(Refusal::Binding);
        }
        expectation
            .contract
            .compare(&peer.contract)
            .map_err(Refusal::Mismatch)?;
        let mut nonce = [0; 32];
        random_bytes(&mut nonce).map_err(|_| Refusal::Entropy)?;
        let (session, transcript) = hashes(
            &peer.server,
            &nonce,
            uuid,
            first.transaction,
            peer.q1,
            &peer.bytes,
            &expectation.canonical,
        );
        let mut data = prefix(2);
        data.extend(uuid.to_be_bytes());
        data.extend(peer.server);
        data.extend(nonce);
        put_contract(&mut data, &expectation.canonical);
        data.extend(transcript);
        scope.send(&reply(first.transaction, data))?;
        let final_query = receive_query(scope)?;
        if final_query.transaction != peer.q1 || confirmation(&final_query.data, 3)? != transcript {
            return Err(Refusal::Binding);
        }
        Ok(ClientConfirmation {
            expectation,
            uuid,
            q1: peer.q1,
            session,
            transcript,
            live: scope.live.clone(),
        })
    })();
    scope.refused(result)
}
fn finish_client_confirmation(
    scope: &mut NegotiationConnection,
    pending: ClientConfirmation,
) -> Result<AgreementReceipt> {
    let result = (|| {
        if scope.state != AttemptState::Client
            || !Arc::ptr_eq(&scope.live, &pending.live)
            || !scope.live.is_open()
        {
            return Err(Refusal::Binding);
        }
        scope.send(&reply(pending.q1, confirm(4, &pending.transcript)))?;
        scope.state = AttemptState::Agreed;
        Ok(AgreementReceipt {
            role: Endpoint::Client,
            expectation: pending.expectation,
            uuid: pending.uuid,
            session: pending.session,
            transcript: pending.transcript,
            live: pending.live,
        })
    })();
    scope.refused(result)
}
pub fn negotiate_client(
    scope: &mut NegotiationConnection,
    expectation: Arc<NegotiationExpectation>,
    uuid: u128,
) -> Result<AgreementReceipt> {
    if scope.state != AttemptState::Fresh {
        scope.close();
        return Err(Refusal::Order);
    }
    let first = match receive_query(scope) {
        Ok(q) => q,
        Err(e) => {
            scope.close();
            return Err(e);
        }
    };
    negotiate_client_from_query(scope, expectation, uuid, first)
}

#[cfg(test)]
#[path = "login_negotiation_tests.rs"]
mod tests;

#[cfg(all(test, feature = "paired-packages"))]
pub(crate) fn retained_test_expectation(sync: Arc<SyncData>) -> Arc<NegotiationExpectation> {
    tests::capture_actual(sync)
}
