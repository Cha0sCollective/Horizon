//! Actual original Hub/Session transports. The runner must supply pinned resources.
use super::*;
use crate::{
    chunk::{Chunk, MIN_Y, SECTIONS},
    net::{Event, RetainedClose, RetainedSession, RetainedSettings, RetainedTarget, SendAdmission},
};
use horizon_protocol::{packets::recipes::RecipeBookChangeSettings, registry::ceil_log2};
use horizon_world::registry::BlockStateId;
use std::time::Instant;

fn construction() -> (
    Arc<SyncData>,
    ChunkEncoder,
    HubConfig,
    RetainedOfflineConfig,
) {
    let sync = super::super::super::retained_test_sync();
    let biomes: Vec<String> = sync.dynamic.registries["minecraft:worldgen/biome"]
        .iter()
        .map(|(name, _)| name.clone())
        .collect();
    let encoder = ChunkEncoder::new(
        [
            (Arc::<str>::from("minecraft:air"), false),
            (Arc::from("minecraft:stone"), false),
        ],
        &sync.builtin,
        &biomes,
        &biomes,
    );
    let config = HubConfig {
        default_game_mode: 1,
        guest_commands: false,
        force_game_mode: false,
        hardcore: false,
        view_distance: 2,
        simulation_distance: 2,
        dimension: "minecraft:overworld".into(),
        seed: 1234,
        max_players: 2,
        dedicated: true,
        integrated: false,
    };
    let offline = RetainedOfflineConfig {
        bind: Some(std::net::Ipv4Addr::LOCALHOST.into()),
        port: 0,
        motd: "original retained Hub fixture".into(),
        max_players: 2,
        compression_threshold: -1,
        enable_status: true,
        hide_online_players: false,
        announce: false,
    };
    (sync, encoder, config, offline)
}
struct Fixture {
    hub: Option<Hub>,
    clients: Vec<RetainedSession>,
    world: World,
    next: u32,
}
impl Fixture {
    fn new() -> Self {
        let (sync, encoder, config, offline) = construction();
        let hub = Hub::new_retained(sync, encoder, config, offline)
            .unwrap_or_else(|e| panic!("{}", e.reason));
        Self::from_hub(hub)
    }
    fn from_hub(hub: Hub) -> Self {
        let mut world = World::new(BlockStateId(0), 0);
        for cz in -4..=4 {
            for cx in -4..=4 {
                let mut chunk = Chunk::empty(cx, cz);
                for z in 0..16 {
                    for x in 0..16 {
                        chunk.set(x, MIN_Y + 4, z, 1);
                    }
                }
                chunk.lit = true;
                world.chunks.insert((cx, cz), Arc::new(chunk));
            }
        }
        Self {
            hub: Some(hub),
            clients: Vec::new(),
            world,
            next: 100,
        }
    }
    fn hub(&mut self) -> &mut Hub {
        self.hub.as_mut().unwrap()
    }
    fn connect(&mut self, tcp: bool, name: &str) {
        let target = if tcp {
            let port = self
                .hub()
                .retained_listen()
                .expect("real loopback listener");
            RetainedTarget::Address(format!("127.0.0.1:{port}"))
        } else {
            RetainedTarget::Memory(
                self.hub()
                    .retained_connect_local()
                    .expect("real memory pipe"),
            )
        };
        let client = RetainedSession::new_offline(
            target,
            RetainedSettings::offline(name.into(), 2, Arc::default()),
        )
        .unwrap_or_else(|e| panic!("{}", e.reason));
        self.clients.push(client);
        self.clients
            .last_mut()
            .unwrap()
            .start()
            .expect("start original client actors");
    }
    fn turn(&mut self, place: bool, level: bool) {
        let border = crate::worldborder::WorldBorder::default();
        let mut ctx = TickCtx {
            world: &mut self.world,
            host: None,
            entities: &[],
            next_entity: &mut self.next,
            spawn: [8.5, f64::from(MIN_Y + 5), 8.5],
            difficulty: 2,
            difficulty_locked: false,
            clock_rate: 1.0,
            tick_rate: 20.0,
            tick_frozen: false,
            tick_steps: (0, 0),
            collision: None,
            border: &border,
        };
        let hub = self.hub.as_mut().unwrap();
        let events = hub.poll_retained(&mut ctx, 16);
        if place {
            for event in events {
                if let HostEvent::Joined(conn) = event {
                    hub.place_new_player(conn, &mut ctx);
                }
            }
        }
        if level {
            hub.tick_level(&mut ctx);
            hub.end_tick();
        }
    }
    fn until(&mut self, mut check: impl FnMut(&mut Self) -> bool) -> bool {
        let end = Instant::now() + Duration::from_secs(8);
        while Instant::now() < end {
            if check(self) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        check(self)
    }
    fn close(&mut self) -> ClosedHub {
        super::super::super::conn::retained_test_release_writer();
        let hub = self.hub.as_mut().unwrap();
        hub.begin_retained_close(Duration::from_secs(10));
        if let Some(view) = hub.poll_retained_close()
            && view.phase == RetainedHubPhase::Blocked
        {
            hub.retry_retained_close(view.attempt, Duration::from_secs(10));
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        for client in &mut self.clients {
            client.begin_close(deadline);
        }
        loop {
            let ready = self
                .hub
                .as_mut()
                .unwrap()
                .poll_retained_close()
                .unwrap()
                .phase
                == RetainedHubPhase::Ready;
            let clients = self
                .clients
                .iter_mut()
                .all(|client| client.poll_close() == RetainedClose::Quiescent);
            if ready && clients {
                break;
            }
            if Instant::now() >= deadline {
                eprintln!("fatal: original Hub fixture cleanup unresolved");
                std::process::abort();
            }
            std::thread::yield_now();
        }
        for client in self.clients.drain(..) {
            if client.try_closed().is_err() {
                std::process::abort();
            }
        }
        match self.hub.take().unwrap().try_retained_closed() {
            Ok(closed) => closed,
            Err(_) => std::process::abort(),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if self.hub.is_some() {
            self.close();
        }
    }
}

type Expectation = super::super::super::login_negotiation::NegotiationExpectation;
fn paired_fixture(compression: bool) -> (Fixture, Arc<Expectation>) {
    paired_fixture_budget(compression, Duration::from_secs(12))
}
fn paired_fixture_budget(compression: bool, budget: Duration) -> (Fixture, Arc<Expectation>) {
    let (sync, encoder, config, mut offline) = construction();
    offline.compression_threshold = if compression { 64 } else { -1 };
    let expected = super::super::super::login_negotiation::retained_test_expectation(sync.clone());
    let required =
        super::super::super::login_negotiation::RequiredNegotiation::new(expected.clone(), budget)
            .unwrap();
    let hub = Hub::new_retained_paired(sync, encoder, config, offline, required)
        .unwrap_or_else(|e| panic!("{}", e.inputs.reason));
    (Fixture::from_hub(hub), expected)
}
fn paired_connect(
    f: &mut Fixture,
    expected: &Arc<Expectation>,
    tcp: bool,
    name: &str,
    budget: Duration,
) -> RetainedSession {
    let target = if tcp {
        let port = match &f.hub().lan.kind {
            Transport::Retained(owner) => owner.port,
            _ => unreachable!(),
        };
        let port = if port == 0 {
            f.hub().retained_listen().unwrap()
        } else {
            port
        };
        RetainedTarget::Address(format!("127.0.0.1:{port}"))
    } else {
        RetainedTarget::Memory(f.hub().retained_connect_local().unwrap())
    };
    let required =
        super::super::super::login_negotiation::RequiredNegotiation::new(expected.clone(), budget)
            .unwrap();
    RetainedSession::new_offline_paired(
        target,
        RetainedSettings::offline(name.into(), 2, Arc::default()),
        required,
    )
    .unwrap_or_else(|e| panic!("{}", e.reason))
}
fn original_lan(f: &Fixture) -> &RetainedLan {
    match &f.hub.as_ref().unwrap().lan.kind {
        Transport::Retained(owner) => owner.lan.as_ref().unwrap(),
        _ => unreachable!(),
    }
}
fn take_paired(
    f: &mut Fixture,
    index: usize,
) -> super::super::super::login_negotiation::ConfiguredEnvelope {
    let mut envelope = None;
    assert!(
        f.until(|f| {
            for event in f.clients[index].poll_events(64) {
                match event {
                    Event::PairedJoined(original) => {
                        assert!(envelope.is_none());
                        envelope = Some(original);
                    }
                    Event::Joined { .. } => panic!("paired actor published generic authority"),
                    Event::Disconnected { reason, .. } => {
                        panic!("premature paired disconnect: {reason}")
                    }
                    _ => {}
                }
            }
            envelope.is_some()
        }),
        "actual paired client publication"
    );
    envelope.unwrap()
}
fn take_server_paired(f: &mut Fixture) -> Box<super::super::super::PairedJoined> {
    let mut paired = None;
    assert!(f.until(|f| {
        for event in original_lan(f).poll_events(16) {
            match event {
                NetEvent::PairedJoined(original) => {
                    if paired.is_none() {
                        paired = Some(original);
                    } else if let Transport::Retained(owner) = &mut f.hub().lan.kind {
                        owner.pending.push_back(NetEvent::PairedJoined(original));
                    }
                }
                _ => panic!("unexpected event before paired admission"),
            }
        }
        paired.is_some()
    }));
    paired.unwrap()
}

#[test]
#[ignore = "pinned pair/resources; actual Memory and compressed TCP Session/Hub actors"]
fn paired_retained_actual_two_clients_consume_and_native_play() {
    use horizon_protocol::packets::Empty;
    for tcp in [false, true] {
        let (mut f, expected) = paired_fixture_budget(tcp, Duration::from_secs(6));
        let (mut other, _) = paired_fixture(false);
        let mut ordinary = Fixture::new();
        for name in ["ConfiguredOne", "ConfiguredTwo"] {
            let mut client = paired_connect(&mut f, &expected, tcp, name, Duration::from_secs(6));
            client.start().unwrap();
            f.clients.push(client);
        }
        let mut e0 = take_paired(&mut f, 0);
        let mut e1 = take_paired(&mut f, 1);
        assert!(
            f.hub().guests.is_empty(),
            "native actors cannot create a Guest"
        );
        assert!(matches!(
            f.clients[0].send(S::PlayerLoaded(Empty)),
            SendAdmission::Refused { .. }
        ));
        assert!(
            f.clients[1].consume_paired(&mut e0).is_err(),
            "wrong actual Session refuses"
        );
        let a0 = f.clients[0].consume_paired(&mut e0).unwrap();
        let a1 = f.clients[1].consume_paired(&mut e1).unwrap();
        assert!(
            f.clients[0].consume_paired(&mut e0).is_err(),
            "one-use client consume"
        );
        assert!(a0.is_live() && a1.is_live());
        assert_eq!(a0.registries().registries, a1.registries().registries);
        // Test the actual first server envelope against a foreign real registry.
        let mut server = take_server_paired(&mut f);
        assert!(!original_lan(&other).validate_paired(&server));
        assert!(original_lan(&other).consume_paired(&mut server).is_err());
        assert!(!original_lan(&ordinary).validate_paired(&server));
        assert!(original_lan(&ordinary).consume_paired(&mut server).is_err());
        if let Transport::Retained(owner) = &mut ordinary.hub().lan.kind {
            owner.pending.push_back(NetEvent::PairedJoined(server));
        }
        ordinary.turn(false, false);
        assert!(ordinary.hub().guests.is_empty());
        assert_eq!(
            ordinary.hub().retained_close_view().unwrap().phase,
            RetainedHubPhase::Frozen
        );
        let server = match &mut ordinary.hub().lan.kind {
            Transport::Retained(owner) => {
                assert!(
                    matches!(owner.pending.front(), Some(NetEvent::PairedJoined(_))),
                    "ordinary refusal retains original at queue head"
                );
                match owner.pending.pop_front().unwrap() {
                    NetEvent::PairedJoined(server) => server,
                    _ => unreachable!(),
                }
            }
            _ => unreachable!(),
        };
        assert!(
            original_lan(&f).validate_paired(&server),
            "foreign refusal keeps original envelope intact"
        );
        if let Transport::Retained(owner) = &mut f.hub().lan.kind {
            owner.pending.push_front(NetEvent::PairedJoined(server));
        }
        other.close();
        assert!(f.until(|f| {
            f.turn(true, false);
            f.hub().guests.len() == 2
        }));
        let mut native_login = [false; 2];
        assert!(
            f.until(|f| {
                for (index, client) in f.clients.iter().enumerate() {
                    for event in client.poll_events(64) {
                        match event {
                            Event::Packet(packet) if matches!(*packet, C::Login(_)) => {
                                native_login[index] = true;
                            }
                            Event::Joined { .. } => panic!("generic authority in paired play"),
                            Event::Disconnected { reason, .. } => {
                                panic!("native play disconnected: {reason}")
                            }
                            _ => {}
                        }
                    }
                }
                native_login.iter().all(|received| *received)
            }),
            "actual native play Login on both moved connections"
        );
        let identity = f.hub().retained_identity().unwrap();
        for guest in &f.hub.as_ref().unwrap().guests {
            assert!(identity.matches_configured_guest(
                f.hub.as_ref().unwrap(),
                guest.conn,
                guest.uuid
            ));
        }
        // The client budget has expired; established actors must still carry
        // actual play packets, without renewing configuration authority.
        std::thread::sleep(Duration::from_secs(6));
        assert!(a0.is_live() && a1.is_live());
        for guest in &f.hub.as_ref().unwrap().guests {
            assert!(identity.matches_configured_guest(
                f.hub.as_ref().unwrap(),
                guest.conn,
                guest.uuid
            ));
        }
        for client in &f.clients {
            assert!(matches!(
                client.send(S::PlayerLoaded(Empty)),
                SendAdmission::Admitted { .. }
            ));
        }
        assert!(f.until(|f| {
            f.turn(false, false);
            f.hub()
                .guests
                .iter()
                .all(|g| g.inbox.iter().any(|p| matches!(p, S::PlayerLoaded(_))))
        }));
        let closed = f.close();
        assert!(!a0.is_live() && !a1.is_live());
        assert_eq!(
            closed.hub.guests.len(),
            2,
            "checked close retains admitted participants"
        );
        assert!(
            closed
                .hub
                .retained_close_view()
                .unwrap()
                .transport
                .attempts
                .iter()
                .all(|a| !a.outer_pending && !a.writer_pending && a.released)
        );
        ordinary.close();
    }
}

#[test]
#[ignore = "pinned pair/resources; actual expiry and close races"]
fn paired_retained_pending_expiry_and_original_close_refuse() {
    for publication in [false, true] {
        let (mut f, expected) = paired_fixture(false);
        let mut client = paired_connect(
            &mut f,
            &expected,
            false,
            "ClosePending",
            Duration::from_secs(5),
        );
        client.retained_test_paired_gate(publication);
        client.start().unwrap();
        f.clients.push(client);
        assert!(f.until(|f| f.clients[0].retained_test_gate_reached()));
        f.clients[0].begin_close(Instant::now() + Duration::from_secs(5));
        f.clients[0].retained_test_gate_release();
        assert!(f.until(|f| f.clients[0].poll_close() == RetainedClose::Quiescent));
        assert!(f.clients[0].retained_test_configured_custody());
        for event in f.clients[0].poll_events(64) {
            if let Event::PairedJoined(mut envelope) = event {
                assert!(f.clients[0].consume_paired(&mut envelope).is_err());
            }
        }
        assert!(f.hub().guests.is_empty());
        f.close();
    }
    for close in [false, true] {
        let (mut f, expected) = paired_fixture_budget(
            false,
            if close {
                Duration::from_secs(12)
            } else {
                Duration::from_secs(2)
            },
        );
        let mut client = paired_connect(
            &mut f,
            &expected,
            false,
            "QueuedPending",
            Duration::from_secs(2),
        );
        client.start().unwrap();
        f.clients.push(client);
        let mut envelope = take_paired(&mut f, 0);
        let mut server = take_server_paired(&mut f);
        if close {
            original_lan(&f).fence();
            assert!(f.until(|f| !f.clients[0].observe().terminals.is_empty()));
        } else {
            std::thread::sleep(Duration::from_secs(2));
        }
        assert!(f.clients[0].consume_paired(&mut envelope).is_err());
        assert!(original_lan(&f).consume_paired(&mut server).is_err());
        assert!(
            server.joined().is_some(),
            "refusal retains original native metadata"
        );
        assert!(f.hub().guests.is_empty());
        f.close();
    }
    // A real paired attempt's metadata cannot be downgraded to generic Joined,
    // even with the correct original conn/UUID/sender and retained reservation.
    let (mut f, expected) = paired_fixture(false);
    let mut client = paired_connect(
        &mut f,
        &expected,
        false,
        "GenericDowngrade",
        Duration::from_secs(6),
    );
    client.start().unwrap();
    f.clients.push(client);
    let _client_envelope = take_paired(&mut f, 0);
    let mut server = take_server_paired(&mut f);
    let joined = server
        .joined
        .take()
        .expect("actual configured native metadata");
    let conn = joined.conn;
    if let Transport::Retained(owner) = &mut f.hub().lan.kind {
        owner.pending.push_back(NetEvent::Joined(joined));
    }
    f.turn(false, false);
    assert!(f.hub().guests.is_empty());
    assert_eq!(
        f.hub().retained_close_view().unwrap().phase,
        RetainedHubPhase::Frozen
    );
    if let Transport::Retained(owner) = &f.hub().lan.kind {
        assert!(
            matches!(owner.pending.front(), Some(NetEvent::Joined(joined)) if joined.conn == conn)
        );
    }
    f.close();
}

#[test]
#[ignore = "pinned pair/resources; native writer failure and actual duplicate profiles"]
fn paired_retained_writer_failures_and_duplicate_custody() {
    use horizon_protocol::packets::Empty;
    for panic in [false, true] {
        let (mut f, expected) = paired_fixture(false);
        let mut client = paired_connect(
            &mut f,
            &expected,
            false,
            "WriterFailure",
            Duration::from_secs(5),
        );
        client.retained_test_writer_fault(panic);
        client.start().unwrap();
        f.clients.push(client);
        let mut admission = None;
        if panic {
            let mut envelope = take_paired(&mut f, 0);
            admission = Some(f.clients[0].consume_paired(&mut envelope).unwrap());
            assert!(matches!(
                f.clients[0].send(S::PlayerLoaded(Empty)),
                SendAdmission::Admitted { .. }
            ));
        }
        assert!(f.until(|f| !f.clients[0].observe().terminals.is_empty()));
        let observation = f.clients[0].observe();
        assert!(observation.terminals.iter().any(|terminal| if panic {
            matches!(terminal.outcome, crate::net::RetainedOutcome::Panic)
        } else {
            matches!(terminal.outcome, crate::net::RetainedOutcome::Spawn(_))
        }));
        if let Some(admission) = admission {
            assert!(!admission.is_live());
        } else {
            assert!(f.clients[0].retained_test_configured_custody());
        }
        assert!(f.hub().guests.is_empty());
        f.close();
    }
    let (mut f, expected) = paired_fixture(true);
    for _ in 0..2 {
        let mut client = paired_connect(
            &mut f,
            &expected,
            true,
            "SameConfigured",
            Duration::from_secs(6),
        );
        client.start().unwrap();
        f.clients.push(client);
    }
    assert!(f.until(|f| {
        original_lan(f).harvest();
        let o = original_lan(f).observe();
        o.attempts.len() == 2
            && o.attempts.iter().filter(|a| a.uuid.is_some()).count() == 1
            && o.terminals.iter().any(|t| t.conn.is_some())
    }));
    assert_eq!(original_lan(&f).online(), 1);
    assert!(f.until(|f| {
        f.turn(false, false);
        f.hub().guests.len() == 1
    }));
    let conn = f.hub().guests[0].conn;
    let uuid = f.hub().guests[0].uuid;
    assert!(
        f.hub()
            .retained_identity()
            .unwrap()
            .matches_configured_guest(f.hub.as_ref().unwrap(), conn, uuid)
    );
    assert_eq!(
        original_lan(&f).online(),
        1,
        "refused duplicate cannot release genuine reservation"
    );
    super::super::super::conn::retained_test_pause_writer(conn);
    f.hub().guests[0].send(&C::KeepAlive(
        horizon_protocol::packets::configuration::KeepAlive { id: 123 },
    ));
    assert!(f.until(|_| super::super::super::conn::retained_test_writer_entered()));
    original_lan(&f).fence();
    assert!(f.until(|f| {
        original_lan(f).harvest();
        original_lan(f)
            .observe()
            .attempts
            .iter()
            .any(|a| a.conn == conn && !a.outer_pending && a.writer_pending)
    }));
    let observation = original_lan(&f).observe();
    assert!(
        observation
            .attempts
            .iter()
            .any(|a| a.conn == conn && !a.released)
    );
    assert_eq!(
        original_lan(&f).online(),
        1,
        "outer join alone cannot release original UUID"
    );
    super::super::super::conn::retained_test_release_writer();
    let closed = f.close();
    assert_eq!(closed.hub.guests.len(), 1);
    assert!(
        closed
            .hub
            .retained_close_view()
            .unwrap()
            .transport
            .attempts
            .iter()
            .all(|a| !a.outer_pending && !a.writer_pending)
    );
}

#[test]
#[ignore = "pinned pair/resources; actual server publication close and writer failures"]
fn paired_retained_server_publication_and_writer_failure_custody() {
    for case in 0..3 {
        let (mut f, expected) = paired_fixture(false);
        let reached = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        if case == 0 {
            original_lan(&f).retained_test_paired_publication(reached.clone(), release.clone());
        } else {
            super::super::super::conn::retained_test_writer_fault(1, case == 2);
        }
        let mut client = paired_connect(
            &mut f,
            &expected,
            false,
            "ServerFailure",
            Duration::from_secs(6),
        );
        client.start().unwrap();
        f.clients.push(client);
        if case == 0 {
            reached.wait();
            let mut envelope = take_paired(&mut f, 0);
            original_lan(&f).fence();
            release.wait();
            assert!(f.until(|f| !f.clients[0].observe().terminals.is_empty()));
            assert!(f.clients[0].consume_paired(&mut envelope).is_err());
            let mut server = take_server_paired(&mut f);
            assert!(original_lan(&f).consume_paired(&mut server).is_err());
            assert!(server.joined().is_some());
        } else if case == 1 {
            assert!(f.until(|f| {
                original_lan(f).harvest();
                original_lan(f)
                    .observe()
                    .terminals
                    .iter()
                    .any(|t| matches!(t.outcome, super::super::super::RetainedOutcome::Spawn(_)))
            }));
            assert!(
                original_lan(&f).retained_test_configured_custody(),
                "server spawn refusal retains original envelope"
            );
        } else {
            let mut envelope = take_paired(&mut f, 0);
            let admission = f.clients[0].consume_paired(&mut envelope).unwrap();
            assert!(f.until(|f| {
                f.turn(true, false);
                original_lan(f).harvest();
                original_lan(f)
                    .observe()
                    .terminals
                    .iter()
                    .any(|t| matches!(t.outcome, super::super::super::RetainedOutcome::Panic))
            }));
            assert!(f.until(|_| !admission.is_live()));
            assert_eq!(
                f.hub().guests.len(),
                1,
                "terminal after consume preserves admitted Guest"
            );
        }
        super::super::super::conn::retained_test_writer_fault(0, false);
        f.close();
    }
}

#[test]
#[ignore = "requires exact prepared resources and serial native actors"]
fn retained_hub_real_memory_and_tcp_terrain_and_play() {
    for tcp in [false, true] {
        let mut fixture = Fixture::new();
        fixture.connect(tcp, if tcp { "HubTcp" } else { "HubMemory" });
        let mut joined = false;
        let mut login = false;
        let mut terrain = false;
        let mut settings_sent = false;
        let advanced = fixture.until(|fixture| {
            fixture.turn(true, true);
            for event in fixture.clients[0].poll_events(64) {
                match event {
                    Event::Joined { name, .. } => {
                        assert_eq!(name, if tcp { "HubTcp" } else { "HubMemory" });
                        joined = true;
                    }
                    Event::Packet(packet) => match *packet {
                        C::Login(packet) => {
                            assert!(joined);
                            assert_eq!(packet.spawn.dimension, "minecraft:overworld");
                            login = true;
                        }
                        C::LevelChunkWithLight(packet) => {
                            assert!(login, "actual Login must precede terrain");
                            let biome_bits = ceil_log2(
                                fixture.hub().sync.dynamic.len("minecraft:worldgen/biome"),
                            );
                            let sections = packet
                                .chunk
                                .sections(SECTIONS, 15, biome_bits)
                                .expect("real native encoded sections");
                            assert_eq!(sections[0].non_empty, 256);
                            assert_eq!(sections[0].states.get(4 * 256 + 8 * 16 + 8), 1);
                            assert_eq!(sections[0].states.get(5 * 256 + 8 * 16 + 8), 0);
                            terrain = true;
                        }
                        _ => {}
                    },
                    Event::DecodeError(error) => panic!("native decode failed: {error}"),
                    Event::Disconnected { reason, .. } => panic!("unexpected disconnect: {reason}"),
                    _ => {}
                }
            }
            if login && !settings_sent {
                assert!(matches!(
                    fixture.clients[0].send(S::RecipeBookChangeSettings(
                        RecipeBookChangeSettings {
                            book_type: 0,
                            open: true,
                            filtering: true
                        }
                    )),
                    SendAdmission::Admitted { .. }
                ));
                settings_sent = true;
            }
            terrain
                && fixture
                    .hub()
                    .guests
                    .first()
                    .is_some_and(|g| g.recipe_book.open[0] && g.recipe_book.filtering[0])
        });
        assert!(
            advanced && joined && login && terrain,
            "genuine protocol and original inbox progress required"
        );
        assert!(fixture.hub().lan.legacy().is_none());
        assert!(
            fixture
                .hub()
                .retained_close_view()
                .unwrap()
                .failure
                .is_none()
        );
        let guest = &fixture.hub().guests[0] as *const Guest;
        let closed = fixture.close();
        assert_eq!(&closed.hub.guests[0] as *const Guest, guest);
        let view = closed.view();
        assert!(view.transport.quiescent);
        assert!(
            view.transport
                .attempts
                .iter()
                .all(|a| a.released && !a.outer_pending && !a.writer_pending)
        );
        assert_eq!(view.guests, 1);
        assert_eq!(view.attempt, 1);
    }
}

#[test]
#[ignore = "requires exact prepared resources and serial native actors"]
fn retained_hub_refused_join_and_departure_keep_original_inputs() {
    let (sync, encoder, mut config, offline) = construction();
    config.view_distance = 0;
    let block_ids = encoder.block_ids.as_ptr();
    let refusal = match Hub::new_retained(sync.clone(), encoder, config, offline) {
        Err(r) => r,
        Ok(_) => panic!("invalid constructor admitted"),
    };
    assert!(Arc::ptr_eq(&sync, &refusal.sync));
    assert_eq!(refusal.encoder.block_ids.as_ptr(), block_ids);
    for corrupt in [true, false] {
        let mut fixture = Fixture::new();
        fixture.connect(false, "HubRefusal");
        if corrupt {
            assert!(fixture.until(|f| {
                let Transport::Retained(owner) = &mut f.hub().lan.kind else {
                    unreachable!()
                };
                if owner.pending.is_empty() {
                    owner
                        .pending
                        .extend(owner.lan.as_ref().unwrap().poll_events(1));
                }
                matches!(owner.pending.front(), Some(NetEvent::Joined(_)))
            }));
            let Transport::Retained(owner) = &mut fixture.hub().lan.kind else {
                unreachable!()
            };
            let NetEvent::Joined(joined) = owner.pending.front_mut().unwrap() else {
                unreachable!()
            };
            joined.uuid ^= 1; // A refused input is never a positive admission witness.
            let original = &**joined as *const super::super::super::Joined;
            fixture.turn(true, true);
            assert_eq!(fixture.next, 100);
            assert!(fixture.hub().guests.is_empty());
            let Transport::Retained(owner) = &fixture.hub().lan.kind else {
                unreachable!()
            };
            let NetEvent::Joined(joined) = owner.pending.front().unwrap() else {
                unreachable!()
            };
            assert_eq!(&**joined as *const super::super::super::Joined, original);
            assert_eq!(owner.phase, RetainedHubPhase::Frozen);
        } else {
            assert!(fixture.until(|f| {
                f.turn(true, false);
                !f.hub().guests.is_empty()
            }));
            let guest = &fixture.hub().guests[0] as *const Guest;
            fixture.clients[0].begin_close(Instant::now() + Duration::from_secs(5));
            assert!(fixture.until(|f| {
                f.turn(false, true);
                f.hub().retained_close_view().unwrap().phase == RetainedHubPhase::Frozen
            }));
            assert_eq!(&fixture.hub().guests[0] as *const Guest, guest);
            assert_eq!(fixture.hub().retained_close_view().unwrap().guests, 1);
        }
        let before = fixture.hub().retained_close_view().unwrap();
        let closed = fixture.close();
        assert_eq!(closed.view().guests, before.guests);
        assert_eq!(closed.view().pending_events, before.pending_events);
        assert!(closed.view().failure.is_some());
    }
}

#[test]
#[ignore = "requires exact prepared resources and serial native actors"]
fn retained_hub_blocked_close_keeps_guests_and_first_failure() {
    let mut fixture = Fixture::new();
    fixture.connect(false, "HubBlocked");
    assert!(fixture.until(|f| {
        f.turn(true, false);
        !f.hub().guests.is_empty()
    }));
    let guest = &fixture.hub().guests[0] as *const Guest;
    let conn = fixture.hub().guests[0].conn;
    let sender = fixture.hub().guests[0].sender.clone();
    super::super::super::conn::retained_test_pause_writer(conn);
    sender.send(
        C::SystemChat(play::SystemChat {
            content: horizon_protocol::text::literal("held original actor"),
            overlay: false,
        })
        .encode(),
    );
    assert!(fixture.until(|_| super::super::super::conn::retained_test_writer_entered()));
    assert!(matches!(
        fixture.clients[0].send(S::RecipeBookChangeSettings(RecipeBookChangeSettings {
            book_type: 0,
            open: true,
            filtering: true
        })),
        SendAdmission::Admitted { .. }
    ));
    assert!(fixture.until(|f| {
        f.turn(false, false);
        !f.hub().guests[0].inbox.is_empty()
    }));
    let inbox = fixture.hub().guests[0].inbox.len();
    sender.send(vec![
        0xa5;
        super::super::super::conn::retained_test_outbound_limit(
        ) + 1
    ]);
    assert!(!fixture.hub().retained_turn_open());
    let first = fixture
        .hub()
        .retained_close_view()
        .unwrap()
        .failure
        .unwrap();
    assert!(fixture.hub().begin_retained_close(Duration::ZERO));
    assert_eq!(
        fixture.hub().poll_retained_close().unwrap().phase,
        RetainedHubPhase::Blocked
    );
    assert!(!fixture.hub().begin_retained_close(Duration::from_secs(120)));
    assert!(
        !fixture
            .hub()
            .retry_retained_close(0, Duration::from_secs(5))
    );
    sender.send(vec![0x19, 0x31]);
    fixture.turn(true, true);
    assert_eq!(fixture.hub().guests[0].inbox.len(), inbox);
    assert_eq!(&fixture.hub().guests[0] as *const Guest, guest);
    assert!(fixture.hub().retained_connect_local().is_err());
    assert!(
        fixture
            .hub()
            .retry_retained_close(1, Duration::from_secs(5))
    );
    assert!(
        !fixture
            .hub()
            .retry_retained_close(1, Duration::from_secs(5))
    );
    super::super::super::conn::retained_test_release_writer();
    let closed = fixture.close();
    let view = closed.view();
    assert_eq!(view.attempt, 2);
    assert_eq!(view.failure.as_deref(), Some(first.as_str()));
    assert_eq!(view.inbox_packets, inbox);
    assert_eq!(&closed.hub.guests[0] as *const Guest, guest);
    let out = view.transport.attempts[0].outbound.as_ref().unwrap();
    assert_eq!(out.refused_bodies, 2);
    assert_eq!(out.first_refusal, Some("capacity"));
    assert!(view.transport.quiescent);
}
