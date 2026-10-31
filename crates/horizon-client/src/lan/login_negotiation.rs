//! Two-query login compatibility confirmation. Agreement is data, never admission.
#[cfg(feature = "paired-packages")]
use super::data::RegistrySnapshot;
use super::{
    data::SyncData,
    peer_contract::{ContractMismatch, WireContract},
};
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
    Closed,
}
/// Owns the actual unsplit connection, with no mutable connection escape hatch.
/// Terminal I/O, explicit close and scope destruction invalidate queued receipts.
pub struct NegotiationConnection {
    connection: Connection,
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
            connection,
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
        let result = self.connection.recv_until(self.deadline);
        self.io(result)
    }
    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        if !self.live.is_open() {
            self.close();
            return Err(Refusal::Closed);
        }
        let result = self.connection.send_until(self.deadline, bytes);
        self.io(result)
    }
    pub fn close(&mut self) {
        self.state = AttemptState::Closed;
        self.live.open.store(false, Ordering::Release);
        self.connection.writer.shutdown();
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
