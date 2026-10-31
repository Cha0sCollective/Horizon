use super::super::data::RegistrySnapshot;
use super::*;

fn wire() -> WireContract {
    use super::super::peer_contract::WireClaims;
    WireContract::from_claims(WireClaims {
        protocol: 777,
        game: "26.3",
        build: [1; 32],
        package_id: "example",
        package: [2; 32],
        machine_id: "example:counter",
        schema: 1,
        registries: &RegistrySnapshot {
            builtin: [3; 32],
            blocks: [4; 32],
            synchronized: [5; 32],
            tags: [6; 32],
            features: [7; 32],
        },
    })
    .unwrap()
}
fn literal_challenge() -> Vec<u8> {
    // Independent literal framing: magic/version/phase, UUID, nonce, q1,
    // then a separately checked wire contract. No exchange encoder is called.
    let mut bytes = b"HZPLOGIN\x00\x01\x01".to_vec();
    bytes.extend([0x12; 16]);
    bytes.extend([0x34; 32]);
    bytes.extend([0, 0, 0, 9]);
    let contract = wire().encode().unwrap();
    bytes.extend((contract.len() as u16).to_be_bytes());
    bytes.extend(contract);
    bytes
}
#[test]
fn exact_codec_boundaries_all_truncations_versions_phases_lengths_and_trailing() {
    let bytes = literal_challenge();
    let parsed = challenge(&bytes).unwrap();
    assert_eq!(parsed.uuid, u128::from_be_bytes([0x12; 16]));
    assert_eq!(parsed.server, [0x34; 32]);
    assert_eq!(parsed.q1, 9);
    assert_eq!(parsed.contract, wire());
    for length in 0..bytes.len() {
        assert!(challenge(&bytes[..length]).is_err(), "truncation {length}");
    }
    for index in [0, 8, 9, 10, 59, 63, 64] {
        let mut bad = bytes.clone();
        bad[index] ^= 0x80;
        assert!(challenge(&bad).is_err(), "field {index}");
    }
    let mut bad = bytes.clone();
    bad.push(0);
    assert!(challenge(&bad).is_err());
    let mut malformed = wire().encode().unwrap();
    let schema = malformed.len() - 164;
    malformed[schema + 3] = 2;
    let mut bad = bytes[..63].to_vec();
    bad.extend((malformed.len() as u16).to_be_bytes());
    bad.extend(malformed);
    assert!(matches!(
        challenge(&bad),
        Err(Refusal::Syntax("wire contract"))
    ));
    assert!(challenge(&vec![0; MAX_PAYLOAD + 1]).is_err());
    for phase in [3, 4] {
        let mut literal = b"HZPLOGIN\x00\x01".to_vec();
        literal.push(phase);
        literal.extend([0x55; 32]);
        assert_eq!(confirmation(&literal, phase).unwrap(), [0x55; 32]);
        for length in 0..literal.len() {
            assert!(confirmation(&literal[..length], phase).is_err());
        }
        assert!(confirmation(&literal, 7 - phase).is_err());
        literal.push(0);
        assert!(confirmation(&literal, phase).is_err());
    }
    let mut literal = b"HZPLOGIN\x00\x01\x02".to_vec();
    literal.extend([0x12; 16]);
    literal.extend([0x34; 32]);
    literal.extend([0x56; 32]);
    let contract = wire().encode().unwrap();
    literal.extend((contract.len() as u16).to_be_bytes());
    literal.extend(contract);
    literal.extend([0x78; 32]);
    let parsed = answer(&literal).unwrap();
    assert_eq!(parsed.client, [0x56; 32]);
    assert_eq!(parsed.transcript, [0x78; 32]);
    for length in 0..literal.len() {
        assert!(answer(&literal[..length]).is_err());
    }
    literal.push(0);
    assert!(answer(&literal).is_err());
    assert_eq!(next_transaction(i32::MAX), 0);
    assert_eq!(next_transaction(0), 1);
}

#[test]
fn transcript_binds_each_nonce_profile_transaction_and_both_contracts() {
    let baseline = hashes(
        &[0x11; 32],
        &[0x22; 32],
        0x00112233445566778899aabbccddeeff,
        0x01020304,
        0x01020305,
        &[0, 1, 2],
        &[3, 4],
    );
    // A separately assembled preimage establishes field order and endianness.
    let mut session = b"horizon-login-session-1\0".to_vec();
    session.extend([0x11; 32]);
    session.extend([0x22; 32]);
    session.extend([
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff,
    ]);
    session.extend([1, 2, 3, 4, 1, 2, 3, 5]);
    let session: [u8; 32] = Sha256::digest(session).into();
    let mut transcript = b"horizon-login-transcript-1\0".to_vec();
    transcript.extend(session);
    transcript.extend([0, 3, 0, 1, 2, 0, 2, 3, 4]);
    let transcript: [u8; 32] = Sha256::digest(transcript).into();
    assert_eq!(baseline, (session, transcript));
    // Independently computed .NET SHA256 literals, using the explicit preimages.
    assert_eq!(
        session.map(|b| format!("{b:02x}")).concat(),
        "52c600b001fa5212d54139e2dae3a45c53474906832aa2a56643e14b36eabebb"
    );
    assert_eq!(
        transcript.map(|b| format!("{b:02x}")).concat(),
        "07196bf7db3994dd772f4c8d54fedec3f7f8bf6aef83e27e5c4d562da362bf01"
    );
    for changed in [
        hashes(
            &[0x10; 32],
            &[0x22; 32],
            0x00112233445566778899aabbccddeeff,
            0x01020304,
            0x01020305,
            &[0, 1, 2],
            &[3, 4],
        ),
        hashes(
            &[0x11; 32],
            &[0x23; 32],
            0x00112233445566778899aabbccddeeff,
            0x01020304,
            0x01020305,
            &[0, 1, 2],
            &[3, 4],
        ),
        hashes(
            &[0x11; 32],
            &[0x22; 32],
            0x00112233445566778899aabbccddeefe,
            0x01020304,
            0x01020305,
            &[0, 1, 2],
            &[3, 4],
        ),
        hashes(
            &[0x11; 32],
            &[0x22; 32],
            0x00112233445566778899aabbccddeeff,
            0x01020303,
            0x01020305,
            &[0, 1, 2],
            &[3, 4],
        ),
        hashes(
            &[0x11; 32],
            &[0x22; 32],
            0x00112233445566778899aabbccddeeff,
            0x01020304,
            0x01020306,
            &[0, 1, 2],
            &[3, 4],
        ),
        hashes(
            &[0x11; 32],
            &[0x22; 32],
            0x00112233445566778899aabbccddeeff,
            0x01020304,
            0x01020305,
            &[0, 1, 3],
            &[3, 4],
        ),
        hashes(
            &[0x11; 32],
            &[0x22; 32],
            0x00112233445566778899aabbccddeeff,
            0x01020304,
            0x01020305,
            &[0, 1, 2],
            &[3, 5],
        ),
    ] {
        assert_ne!(changed.1, baseline.1);
    }
}

#[test]
fn invalid_scope_deadline_closes_the_owned_connection() {
    for deadline in [
        Instant::now(),
        Instant::now() + MAX_BUDGET + Duration::from_secs(1),
    ] {
        let (a, b) = horizon_protocol::connection::Pipe::pair();
        let result = NegotiationConnection::new(Connection::local(a), deadline);
        assert!(matches!(result, Err(Refusal::Budget)));
        let mut peer = Connection::local(b);
        assert!(peer.send(&[1]).is_err());
    }
}

#[cfg(feature = "paired-packages")]
mod tcp {
    use super::*;
    use std::{
        io::Read,
        net::{TcpListener, TcpStream},
        path::PathBuf,
        sync::mpsc::channel,
    };

    fn captured() -> Arc<NegotiationExpectation> {
        let resources = PathBuf::from(
            std::env::var_os("HORIZON_TEST_RESOURCES").expect("pinned resources required"),
        );
        let pair = PathBuf::from(
            std::env::var_os("HORIZON_COUNTER_PAIR_FIXTURE").expect("prebuilt Rust pair required"),
        );
        let prepared = Arc::new(horizon_mods::paired::PreparedBundle::read_folder(&pair).unwrap());
        let sync = Arc::new(SyncData::load(&resources).unwrap());
        let blocks = Box::leak(Box::new(
            BlockRegistry::from_facts_json(
                std::fs::File::open(resources.join("facts.json")).unwrap(),
            )
            .unwrap(),
        ));
        let projections = RegistrySnapshot::capture(&sync, blocks, 0, 0).unwrap();
        let mut executable = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
        let mut hash = Sha256::new();
        let mut chunk = [0; 65536];
        loop {
            let n = executable.read(&mut chunk).unwrap();
            if n == 0 {
                break;
            }
            hash.update(&chunk[..n]);
        }
        let machines = prepared.content().machine_types().types();
        assert_eq!(machines.len(), 1);
        let contract = WireContract::from_claims(super::super::super::peer_contract::WireClaims {
            protocol: horizon_protocol::PROTOCOL_VERSION,
            game: horizon_protocol::GAME_VERSION,
            build: hash.finalize().into(),
            package_id: &prepared.bundle().content_owner().id,
            package: *prepared.identity(),
            machine_id: machines[0].id(),
            schema: machines[0].save_schema(),
            registries: &projections,
        })
        .unwrap();
        NegotiationExpectation::from_retained(sync, blocks, prepared, contract).unwrap()
    }
    fn sockets() -> (Connection, Connection) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        (
            Connection::from_stream(server).unwrap(),
            Connection::from_stream(client).unwrap(),
        )
    }
    fn scope(c: Connection) -> NegotiationConnection {
        NegotiationConnection::new(c, Instant::now() + Duration::from_secs(3)).unwrap()
    }
    fn peer_answer(
        q: &login::CustomQuery,
        uuid: u128,
        client: [u8; 32],
        contract: &[u8],
    ) -> Vec<u8> {
        let p = challenge(&q.data).unwrap();
        let (_, transcript) = hashes(
            &p.server,
            &client,
            uuid,
            q.transaction,
            p.q1,
            &p.bytes,
            contract,
        );
        let mut data = prefix(2);
        data.extend(uuid.to_be_bytes());
        data.extend(p.server);
        data.extend(client);
        put_contract(&mut data, contract);
        data.extend(transcript);
        data
    }
    fn request(c: &mut Connection) -> login::CustomQuery {
        let (id, data) = c
            .recv_until(Instant::now() + Duration::from_secs(3))
            .unwrap();
        match login::Clientbound::decode(id, &data).unwrap() {
            login::Clientbound::CustomQuery(q) => q,
            _ => panic!("query required"),
        }
    }
    fn first_query(expected: &NegotiationExpectation) -> Vec<u8> {
        let mut data = prefix(1);
        data.extend(42u128.to_be_bytes());
        data.extend([1; 32]);
        data.extend(8i32.to_be_bytes());
        put_contract(&mut data, &expected.canonical);
        query(7, data)
    }

    #[test]
    #[ignore = "isolated pinned resources and prebuilt Rust pair"]
    fn actual_independent_capture_matching_receipts_live_then_refused_and_invalidated() {
        let a = captured();
        let b = captured();
        let substitute = captured();
        assert!(matches!(
            RequiredNegotiation::new(a.clone(), Duration::ZERO),
            Err(Refusal::Budget)
        ));
        assert!(matches!(
            RequiredNegotiation::new(a.clone(), MAX_BUDGET + Duration::from_nanos(1)),
            Err(Refusal::Budget)
        ));
        assert!(!Arc::ptr_eq(&a, &b));
        assert_eq!(a.contract, b.contract);
        let (server, client) = sockets();
        let server_expected = a.clone();
        let server = std::thread::spawn(move || {
            let mut scope = scope(server);
            let receipt = negotiate_server(&mut scope, server_expected, 42).unwrap();
            (scope, receipt)
        });
        let mut client = scope(client);
        let client_receipt = negotiate_client(&mut client, b.clone(), 42).unwrap();
        let (mut server, server_receipt) = server.join().unwrap();
        assert!(client_receipt.is_live(&client, Endpoint::Client, &b, 42));
        assert!(server_receipt.is_live(&server, Endpoint::Server, &a, 42));
        assert_eq!(server_receipt.session, client_receipt.session);
        assert_eq!(server_receipt.transcript, client_receipt.transcript);
        assert_eq!(
            server
                .connection
                .reader
                .counters
                .sent
                .load(Ordering::Relaxed),
            2
        );
        assert_eq!(
            server
                .connection
                .reader
                .counters
                .received
                .load(Ordering::Relaxed),
            2
        );
        assert_eq!(
            client
                .connection
                .reader
                .counters
                .sent
                .load(Ordering::Relaxed),
            2
        );
        assert_eq!(
            client
                .connection
                .reader
                .counters
                .received
                .load(Ordering::Relaxed),
            2
        );
        assert!(!server_receipt.is_live(&server, Endpoint::Server, &substitute, 42));
        assert!(!server_receipt.is_live(&client, Endpoint::Server, &a, 42));
        assert!(!server_receipt.is_live(&server, Endpoint::Client, &a, 42));
        assert!(!server_receipt.is_live(&server, Endpoint::Server, &a, 43));
        server.refuse_configuration().unwrap();
        client.observe_configuration_refusal().unwrap();
        assert_eq!(
            server
                .connection
                .reader
                .counters
                .sent
                .load(Ordering::Relaxed),
            3
        );
        assert_eq!(
            client
                .connection
                .reader
                .counters
                .received
                .load(Ordering::Relaxed),
            3
        );
        assert!(!server_receipt.is_live(&server, Endpoint::Server, &a, 42));
        assert!(!client_receipt.is_live(&client, Endpoint::Client, &b, 42));
        assert!(
            client_receipt
                .consume(&client, Endpoint::Client, &b, 42)
                .is_err()
        );
        // A receipt may be queued outside its connection scope; dropping that
        // scope invalidates its latch rather than manufacturing live success.
        let latch = server_receipt.live.clone();
        drop(server);
        assert!(!latch.open.load(Ordering::Acquire));
    }

    #[test]
    #[ignore = "isolated pinned resources and prebuilt Rust pair"]
    fn actual_tcp_all_nine_remote_mismatch_classes_are_refused() {
        use super::super::super::peer_contract::{ContractMismatch::*, WireClaims};
        let expected = captured();
        for mismatch in [
            ProtocolGame,
            Executable,
            PackageContent,
            MachineSchema,
            Builtin,
            Blocks,
            Synchronized,
            Tags,
            Features,
        ] {
            let mut claims = expected.contract.claims();
            let mut projections = claims.registries.clone();
            let alternate_machine = format!("{}:other", claims.package_id);
            match mismatch {
                ProtocolGame => claims.protocol += 1,
                Executable => claims.build[0] ^= 1,
                PackageContent => claims.package[0] ^= 1,
                MachineSchema => claims.machine_id = &alternate_machine,
                Builtin => projections.builtin[0] ^= 1,
                Blocks => projections.blocks[0] ^= 1,
                Synchronized => projections.synchronized[0] ^= 1,
                Tags => projections.tags[0] ^= 1,
                Features => projections.features[0] ^= 1,
            }
            let remote = WireContract::from_claims(WireClaims {
                registries: &projections,
                ..claims
            })
            .unwrap()
            .encode()
            .unwrap();
            let (server, mut client) = sockets();
            let expected = expected.clone();
            let server = std::thread::spawn(move || {
                let mut scope = scope(server);
                negotiate_server(&mut scope, expected, 42).err().unwrap()
            });
            let q = request(&mut client);
            let bytes = peer_answer(&q, 42, [8; 32], &remote);
            client
                .send_until(
                    Instant::now() + Duration::from_secs(3),
                    &reply(q.transaction, bytes),
                )
                .unwrap();
            let outcome = server.join().unwrap();
            assert_eq!(outcome, Refusal::Mismatch(mismatch));
            assert!(
                client
                    .recv_until(Instant::now() + Duration::from_secs(1))
                    .is_err()
            );
        }
    }

    #[test]
    #[ignore = "isolated pinned resources and prebuilt Rust pair"]
    fn actual_server_missing_none_wrong_transaction_phase_binding_and_early_ack() {
        let expected = captured();
        for case in 0..9 {
            let (server, mut client) = sockets();
            let server_expected = expected.clone();
            let server = std::thread::spawn(move || {
                let mut s = scope(server);
                negotiate_server(&mut s, server_expected, 42).err().unwrap()
            });
            let q = request(&mut client);
            if case == 0 {
                client.writer.shutdown();
            } else {
                let mut data = peer_answer(&q, 42, [8; 32], &expected.canonical);
                let bytes = match case {
                    1 => login::Serverbound::CustomQueryAnswer(login::CustomQueryAnswer {
                        transaction: q.transaction,
                        payload: None,
                    })
                    .encode(),
                    2 => reply(next_transaction(q.transaction), data),
                    3 => {
                        data[10] = 4;
                        reply(q.transaction, data)
                    }
                    4 => {
                        data[11] ^= 1;
                        reply(q.transaction, data)
                    }
                    5 => {
                        data[27] ^= 1;
                        reply(q.transaction, data)
                    }
                    6 => {
                        *data.last_mut().unwrap() ^= 1;
                        reply(q.transaction, data)
                    }
                    7 => login::Serverbound::LoginAcknowledged(horizon_protocol::packets::Empty)
                        .encode(),
                    _ => {
                        data.push(0);
                        reply(q.transaction, data)
                    }
                };
                client
                    .send_until(Instant::now() + Duration::from_secs(3), &bytes)
                    .unwrap();
            }
            let outcome = server.join().unwrap();
            assert!(matches!(
                outcome,
                Refusal::Io(_)
                    | Refusal::Unsupported
                    | Refusal::Order
                    | Refusal::Syntax(_)
                    | Refusal::Binding
            ));
        }
    }

    #[test]
    #[ignore = "isolated pinned resources and prebuilt Rust pair"]
    fn actual_client_reordered_duplicate_alternate_channel_tampered_and_early_finish() {
        let expected = captured();
        for case in 0..8 {
            let (mut server, client) = sockets();
            let client_expected = expected.clone();
            let client = std::thread::spawn(move || {
                let mut c = scope(client);
                negotiate_client(&mut c, client_expected, 42).err().unwrap()
            });
            let mut data = prefix(1);
            data.extend(42u128.to_be_bytes());
            data.extend([1; 32]);
            data.extend(8i32.to_be_bytes());
            put_contract(&mut data, &expected.canonical);
            if case == 0 {
                data[10] = 3;
            }
            if case == 1 {
                data[11] ^= 1;
            }
            if case == 2 {
                data[62] = 7;
            }
            let mut q = login::CustomQuery {
                transaction: 7,
                channel: CHANNEL.into(),
                data,
            };
            if case == 3 {
                q.channel = "example:other".into();
            }
            let first = login::Clientbound::CustomQuery(q.clone()).encode();
            server
                .send_until(Instant::now() + Duration::from_secs(3), &first)
                .unwrap();
            if case >= 4 {
                let (id, data) = server
                    .recv_until(Instant::now() + Duration::from_secs(3))
                    .unwrap();
                let answer = match login::Serverbound::decode(id, &data).unwrap() {
                    login::Serverbound::CustomQueryAnswer(a) => {
                        answer(&a.payload.unwrap()).unwrap()
                    }
                    _ => panic!("answer required"),
                };
                let bytes = match case {
                    4 => first,
                    5 => query(8, confirm(3, &[0; 32])),
                    6 => {
                        let mut q = login::CustomQuery {
                            transaction: 8,
                            channel: "example:other".into(),
                            data: confirm(3, &answer.transcript),
                        };
                        q.data.push(0);
                        login::Clientbound::CustomQuery(q).encode()
                    }
                    _ => login::Clientbound::LoginFinished(login::LoginFinished {
                        profile: login::GameProfile {
                            uuid: 42,
                            name: "test".into(),
                            properties: Vec::new(),
                        },
                        session: 42,
                    })
                    .encode(),
                };
                server
                    .send_until(Instant::now() + Duration::from_secs(3), &bytes)
                    .unwrap();
            }
            let outcome = client.join().unwrap();
            assert!(matches!(
                outcome,
                Refusal::Order | Refusal::Binding | Refusal::Syntax(_)
            ));
        }
    }

    #[test]
    #[ignore = "isolated pinned resources and prebuilt Rust pair"]
    fn actual_fresh_nonce_retry_rejects_replayed_phase_two() {
        let expected = captured();
        let mut old = None;
        for attempt in 0..2 {
            let (server, mut client) = sockets();
            let server_expected = expected.clone();
            let server = std::thread::spawn(move || {
                let mut s = scope(server);
                negotiate_server(&mut s, server_expected, 42).err().unwrap()
            });
            let q = request(&mut client);
            if attempt == 0 {
                old = Some((
                    challenge(&q.data).unwrap().server,
                    peer_answer(&q, 42, [9; 32], &expected.canonical),
                ));
                client.writer.shutdown();
            } else {
                let (nonce, data) = old.take().unwrap();
                assert_ne!(nonce, challenge(&q.data).unwrap().server);
                client
                    .send_until(
                        Instant::now() + Duration::from_secs(3),
                        &reply(q.transaction, data),
                    )
                    .unwrap();
            }
            let outcome = server.join().unwrap();
            if attempt == 1 {
                assert_eq!(outcome, Refusal::Binding);
            }
        }
        // A new exact connection succeeds after the refused attempt.
        let (server, client) = sockets();
        let server_expected = expected.clone();
        let server = std::thread::spawn(move || {
            let mut s = scope(server);
            let r = negotiate_server(&mut s, server_expected, 42).unwrap();
            (s, r)
        });
        let mut c = scope(client);
        let r = negotiate_client(&mut c, expected.clone(), 42).unwrap();
        let (mut s, _) = server.join().unwrap();
        assert!(r.is_live(&c, Endpoint::Client, &expected, 42));
        s.close();
        assert!(c.observe_configuration_refusal().is_err());
        assert!(!r.is_live(&c, Endpoint::Client, &expected, 42));
    }

    #[test]
    #[ignore = "isolated pinned resources and prebuilt Rust pair"]
    fn actual_new_client_nonce_rejects_replayed_server_confirmation() {
        let expected = captured();
        let mut old = None;
        for attempt in 0..2 {
            let (mut server, client) = sockets();
            let e = expected.clone();
            let client = std::thread::spawn(move || {
                let mut c = scope(client);
                negotiate_client(&mut c, e, 42).err().unwrap()
            });
            server
                .send_until(
                    Instant::now() + Duration::from_secs(3),
                    &first_query(&expected),
                )
                .unwrap();
            let (id, data) = server
                .recv_until(Instant::now() + Duration::from_secs(3))
                .unwrap();
            let a = match login::Serverbound::decode(id, &data).unwrap() {
                login::Serverbound::CustomQueryAnswer(a) => answer(&a.payload.unwrap()).unwrap(),
                _ => panic!("phase2"),
            };
            if attempt == 0 {
                old = Some((a.client, a.transcript));
                server.writer.shutdown();
            } else {
                let (nonce, transcript) = old.take().unwrap();
                assert_ne!(nonce, a.client);
                server
                    .send_until(
                        Instant::now() + Duration::from_secs(3),
                        &query(8, confirm(3, &transcript)),
                    )
                    .unwrap();
            }
            let outcome = client.join().unwrap();
            if attempt == 1 {
                assert_eq!(outcome, Refusal::Binding);
            }
        }
    }

    #[test]
    #[ignore = "isolated pinned resources and prebuilt Rust pair"]
    fn actual_client_phase_four_write_failure_and_server_final_receive_failure_create_no_receipt() {
        let expected = captured();
        let (server, client) = sockets();
        let server_expected = expected.clone();
        let server = std::thread::spawn(move || {
            let mut c = server;
            c.send_until(
                Instant::now() + Duration::from_secs(3),
                &first_query(&server_expected),
            )
            .unwrap();
            let (id, data) = c
                .recv_until(Instant::now() + Duration::from_secs(3))
                .unwrap();
            let a = match login::Serverbound::decode(id, &data).unwrap() {
                login::Serverbound::CustomQueryAnswer(a) => answer(&a.payload.unwrap()).unwrap(),
                _ => panic!("phase2"),
            };
            c.send_until(
                Instant::now() + Duration::from_secs(3),
                &query(8, confirm(3, &a.transcript)),
            )
            .unwrap();
            c.recv_until(Instant::now() + Duration::from_secs(3))
                .is_err()
        });
        let mut c = scope(client);
        let first = receive_query(&mut c).unwrap();
        let pending = prepare_client_confirmation(&mut c, expected.clone(), 42, first).unwrap();
        // This is the actual client socket, after native phase3 verification.
        // The production final-write helper must fail rather than mint receipt.
        c.connection.writer.shutdown();
        let outcome = finish_client_confirmation(&mut c, pending).err().unwrap();
        let terminal = server.join().unwrap();
        assert!(matches!(outcome, Refusal::Io(_)));
        assert!(!c.live.is_open());
        assert!(terminal);

        let (server, mut client) = sockets();
        let server_expected = expected.clone();
        let server = std::thread::spawn(move || {
            let mut s = scope(server);
            negotiate_server(&mut s, server_expected, 42).err().unwrap()
        });
        let q = request(&mut client);
        let data = peer_answer(&q, 42, [9; 32], &expected.canonical);
        client
            .send_until(
                Instant::now() + Duration::from_secs(3),
                &reply(q.transaction, data),
            )
            .unwrap();
        let phase3 = request(&mut client);
        assert_eq!(phase3.transaction, challenge(&q.data).unwrap().q1);
        client.writer.shutdown();
        assert!(matches!(server.join().unwrap(), Refusal::Io(_)));
    }

    #[test]
    #[ignore = "isolated pinned resources and prebuilt Rust pair"]
    fn actual_disconnect_before_and_after_every_transition_closes_and_reaps_both_endpoints() {
        let expected = captured();
        for transition in 0..5 {
            let (server, mut client) = sockets();
            let e = expected.clone();
            let server = std::thread::spawn(move || {
                let mut s = scope(server);
                let result = negotiate_server(&mut s, e, 42);
                (s, result)
            });
            if transition > 0 {
                let q = request(&mut client);
                if transition > 1 {
                    client
                        .send_until(
                            Instant::now() + Duration::from_secs(3),
                            &reply(
                                q.transaction,
                                peer_answer(&q, 42, [9; 32], &expected.canonical),
                            ),
                        )
                        .unwrap();
                    if transition > 2 {
                        let q1 = request(&mut client);
                        if transition > 3 {
                            let transcript = confirmation(&q1.data, 3).unwrap();
                            client
                                .send_until(
                                    Instant::now() + Duration::from_secs(3),
                                    &reply(q1.transaction, confirm(4, &transcript)),
                                )
                                .unwrap();
                        }
                    }
                }
            }
            client.writer.shutdown();
            let (mut server, result) = server.join().unwrap();
            if let Ok(receipt) = result {
                assert_eq!(transition, 4);
                assert!(server.recv().is_err());
                assert!(!receipt.is_live(&server, Endpoint::Server, &expected, 42));
            } else {
                assert!(!server.live.is_open());
            }
        }
        for transition in 0..5 {
            let (mut server, client) = sockets();
            let e = expected.clone();
            let client = std::thread::spawn(move || {
                let mut c = scope(client);
                let result = negotiate_client(&mut c, e, 42);
                (c, result)
            });
            if transition > 0 {
                server
                    .send_until(
                        Instant::now() + Duration::from_secs(3),
                        &first_query(&expected),
                    )
                    .unwrap();
                if transition > 1 {
                    let (id, data) = server
                        .recv_until(Instant::now() + Duration::from_secs(3))
                        .unwrap();
                    let a = match login::Serverbound::decode(id, &data).unwrap() {
                        login::Serverbound::CustomQueryAnswer(a) => {
                            answer(&a.payload.unwrap()).unwrap()
                        }
                        _ => panic!("phase2"),
                    };
                    if transition > 2 {
                        server
                            .send_until(
                                Instant::now() + Duration::from_secs(3),
                                &query(8, confirm(3, &a.transcript)),
                            )
                            .unwrap();
                        if transition > 3 {
                            let (id, data) = server
                                .recv_until(Instant::now() + Duration::from_secs(3))
                                .unwrap();
                            match login::Serverbound::decode(id, &data).unwrap() {
                                login::Serverbound::CustomQueryAnswer(last) => assert_eq!(
                                    confirmation(&last.payload.unwrap(), 4).unwrap(),
                                    a.transcript
                                ),
                                _ => panic!("phase4"),
                            };
                        }
                    }
                }
            }
            server.writer.shutdown();
            let (mut client, result) = client.join().unwrap();
            if let Ok(receipt) = result {
                assert!(transition >= 3);
                assert!(client.observe_configuration_refusal().is_err());
                assert!(!receipt.is_live(&client, Endpoint::Client, &expected, 42));
            } else {
                assert!(!client.live.is_open());
            }
        }
    }

    #[test]
    #[ignore = "isolated pinned resources and prebuilt Rust pair"]
    fn actual_runtime_close_invalidates_queued_receipt_before_socket_scope_ends() {
        let expected = captured();
        let policy = RequiredNegotiation::new(expected.clone(), Duration::from_secs(3)).unwrap();
        let (server, client) = sockets();
        let e = expected.clone();
        let server_policy = policy.clone();
        let server = std::thread::spawn(move || {
            let mut s = NegotiationConnection::for_required(
                server,
                &server_policy,
                server_policy.deadline(),
            )
            .unwrap();
            let r = negotiate_server(&mut s, e, 42).unwrap();
            (s, r)
        });
        let mut client = scope(client);
        let _ = negotiate_client(&mut client, expected.clone(), 42).unwrap();
        let (mut server, receipt) = server.join().unwrap();
        let queued = vec![receipt];
        assert!(queued[0].is_live(&server, Endpoint::Server, &expected, 42));
        policy.close();
        assert!(!queued[0].is_live(&server, Endpoint::Server, &expected, 42));
        assert!(
            queued
                .into_iter()
                .next()
                .unwrap()
                .consume(&server, Endpoint::Server, &expected, 42)
                .is_err()
        );
        assert!(server.refuse_configuration().is_err());
        assert!(client.observe_configuration_refusal().is_err());
    }

    #[test]
    #[ignore = "isolated pinned resources and prebuilt Rust pair"]
    fn actual_agreement_is_one_attempt_per_connection_and_receipt_consumes_once() {
        let expected = captured();
        let (server, client) = sockets();
        let e = expected.clone();
        let server = std::thread::spawn(move || {
            let mut s = scope(server);
            let r = negotiate_server(&mut s, e, 42).unwrap();
            (s, r)
        });
        let mut client = scope(client);
        let receipt = negotiate_client(&mut client, expected.clone(), 42).unwrap();
        let (mut server, server_receipt) = server.join().unwrap();
        let data = receipt
            .consume(&client, Endpoint::Client, &expected, 42)
            .unwrap();
        assert_eq!(data, (server_receipt.session, server_receipt.transcript));
        assert_eq!(
            negotiate_server(&mut server, expected.clone(), 42)
                .err()
                .unwrap(),
            Refusal::Order
        );
        assert!(!server_receipt.is_live(&server, Endpoint::Server, &expected, 42));
        assert!(client.observe_configuration_refusal().is_err());
    }

    #[test]
    #[ignore = "isolated pinned resources and prebuilt Rust pair"]
    fn actual_queued_agreement_expires_with_its_original_total_deadline() {
        let expected = captured();
        let (server, client) = sockets();
        let e = expected.clone();
        let deadline = Instant::now() + Duration::from_millis(500);
        let server = std::thread::spawn(move || {
            let mut s = NegotiationConnection::new(server, deadline).unwrap();
            let r = negotiate_server(&mut s, e, 42).unwrap();
            (s, r)
        });
        let mut client = NegotiationConnection::new(client, deadline).unwrap();
        let receipt = negotiate_client(&mut client, expected.clone(), 42).unwrap();
        let (mut server, server_receipt) = server.join().unwrap();
        assert!(receipt.is_live(&client, Endpoint::Client, &expected, 42));
        std::thread::sleep(
            deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(10),
        );
        assert!(!receipt.is_live(&client, Endpoint::Client, &expected, 42));
        assert!(!server_receipt.is_live(&server, Endpoint::Server, &expected, 42));
        assert!(server.refuse_configuration().is_err());
        assert!(client.observe_configuration_refusal().is_err());
    }

    #[test]
    #[ignore = "isolated pinned resources and prebuilt Rust pair"]
    fn required_server_preserves_duplicate_and_never_publishes_guest_or_registry() {
        use super::super::super::{Lan, NetConfig, conn};
        let expected = captured();
        let mut config = NetConfig::lan(0, "test".into(), 8);
        config.listen = false;
        config.announce = false;
        let mut lan = Lan::start_with_required_negotiation(
            expected.sync.clone(),
            config,
            RequiredNegotiation::new(expected.clone(), Duration::from_secs(2)).unwrap(),
        )
        .unwrap();
        let uuid = horizon_protocol::crypto::offline_uuid("ExactGuest");
        lan.shared
            .online
            .lock()
            .unwrap()
            .push((uuid, "ExistingGuest".into()));
        lan.shared.reserved.lock().unwrap().insert(123);
        for case in 0..8 {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let shared = lan.shared.clone();
            let server = std::thread::spawn(move || {
                let (stream, address) = listener.accept().unwrap();
                conn::serve(stream, address, shared)
            });
            let mut c = Connection::from_stream(TcpStream::connect(address).unwrap()).unwrap();
            c.send(
                &horizon_protocol::packets::handshake::Serverbound::Intention(
                    horizon_protocol::packets::handshake::Intention {
                        protocol: horizon_protocol::PROTOCOL_VERSION,
                        host: "127.0.0.1".into(),
                        port: address.port(),
                        intent: horizon_protocol::packets::handshake::LOGIN,
                    },
                )
                .encode(),
            )
            .unwrap();
            c.send(
                &login::Serverbound::Hello(login::ServerHello {
                    name: "ExactGuest".into(),
                    uuid,
                })
                .encode(),
            )
            .unwrap();
            if case == 0 || case == 7 {
                let mut c = scope(c);
                let receipt = negotiate_client(&mut c, expected.clone(), uuid).unwrap();
                c.observe_configuration_refusal().unwrap();
                assert!(!receipt.is_live(&c, Endpoint::Client, &expected, uuid));
                assert!(server.join().unwrap().is_ok());
            } else {
                if case == 3 {
                    c.writer.shutdown();
                } else {
                    let q = request(&mut c);
                    match case {
                        1 => c
                            .send(
                                &login::Serverbound::CustomQueryAnswer(login::CustomQueryAnswer {
                                    transaction: q.transaction,
                                    payload: None,
                                })
                                .encode(),
                            )
                            .unwrap(),
                        2 => {
                            let claims = expected.contract.claims();
                            let mut changed = claims.registries.clone();
                            changed.features[0] ^= 1;
                            let data = WireContract::from_claims(
                                super::super::super::peer_contract::WireClaims {
                                    registries: &changed,
                                    ..claims
                                },
                            )
                            .unwrap()
                            .encode()
                            .unwrap();
                            c.send(&reply(q.transaction, peer_answer(&q, uuid, [9; 32], &data)))
                                .unwrap();
                        }
                        4 => c
                            .send(
                                &login::Serverbound::LoginAcknowledged(
                                    horizon_protocol::packets::Empty,
                                )
                                .encode(),
                            )
                            .unwrap(),
                        5 => {
                            c.send(&reply(
                                q.transaction,
                                peer_answer(&q, uuid, [9; 32], &expected.canonical),
                            ))
                            .unwrap();
                            request(&mut c);
                            c.writer.shutdown();
                        }
                        _ => {} // A silent unsupported peer holds its actual socket.
                    }
                }
                assert!(server.join().unwrap().is_err());
                let mut terminal = false;
                for _ in 0..2 {
                    match c.recv_until(Instant::now() + Duration::from_secs(1)) {
                        Err(_) => {
                            terminal = true;
                            break;
                        }
                        Ok((id, data)) => assert!(
                            matches!(login::Clientbound::decode(id,&data).unwrap(),login::Clientbound::CustomQuery(q) if q.channel==CHANNEL)
                        ),
                    }
                }
                assert!(terminal, "the actual peer must observe closure");
            }
            assert!(lan.poll().is_empty());
            assert_eq!(
                lan.shared.online.lock().unwrap().as_slice(),
                &[(uuid, "ExistingGuest".into())]
            );
            assert_eq!(
                *lan.shared.reserved.lock().unwrap(),
                std::collections::HashSet::from([123])
            );
            assert_eq!(lan.shared.next.load(Ordering::Relaxed), 1);
        }
        lan.stop();
    }

    #[test]
    #[ignore = "isolated pinned resources and prebuilt Rust pair"]
    fn required_session_silent_unsupported_peer_has_total_first_query_deadline_and_no_join() {
        let expected = captured();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (release, done) = channel();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut c = Connection::from_stream(stream).unwrap();
            c.recv().unwrap();
            c.recv().unwrap();
            let _ = done.recv();
            c.writer.shutdown();
        });
        let start = Instant::now();
        let mut session = crate::net::Session::connect_required_negotiation(
            &address.to_string(),
            "SilentGuest",
            None,
            2,
            Arc::new(horizon_protocol::text::Language::new()),
            RequiredNegotiation::new(expected, Duration::from_millis(100)).unwrap(),
        );
        let mut events = Vec::new();
        while start.elapsed() < Duration::from_secs(2) {
            events.extend(session.poll());
            if events
                .iter()
                .any(|e| matches!(e, crate::net::Event::Disconnected { .. }))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let elapsed = start.elapsed();
        session.disconnect();
        let _ = release.send(());
        server.join().unwrap();
        assert!(elapsed < Duration::from_secs(2));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, crate::net::Event::Disconnected { .. }))
        );
        assert!(elapsed >= Duration::from_millis(50));
        assert!(!events.iter().any(|e| matches!(
            e,
            crate::net::Event::Joined { .. } | crate::net::Event::Reconfigured(_)
        )));
    }

    #[test]
    #[ignore = "isolated pinned resources and prebuilt Rust pair"]
    fn required_session_and_server_agree_then_refuse_with_no_joined_or_duplicate_event() {
        use super::super::super::{Lan, NetConfig, conn};
        let server_expected = captured();
        let client_expected = captured();
        let mut config = NetConfig::lan(0, "test".into(), 8);
        config.listen = false;
        config.announce = false;
        let mut lan = Lan::start_with_required_negotiation(
            server_expected.sync.clone(),
            config,
            RequiredNegotiation::new(server_expected, Duration::from_secs(2)).unwrap(),
        )
        .unwrap();
        let uuid = horizon_protocol::crypto::offline_uuid("ExactGuest");
        lan.shared
            .online
            .lock()
            .unwrap()
            .push((uuid, "ExistingGuest".into()));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shared = lan.shared.clone();
        let server = std::thread::spawn(move || {
            let (stream, address) = listener.accept().unwrap();
            conn::serve(stream, address, shared)
        });
        let mut session = crate::net::Session::connect_required_negotiation(
            &address.to_string(),
            "ExactGuest",
            None,
            2,
            Arc::new(horizon_protocol::text::Language::new()),
            RequiredNegotiation::new(client_expected, Duration::from_secs(2)).unwrap(),
        );
        let start = Instant::now();
        let mut events = Vec::new();
        while start.elapsed() < Duration::from_secs(3) {
            events.extend(session.poll());
            if events
                .iter()
                .any(|e| matches!(e, crate::net::Event::Disconnected { .. }))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        session.disconnect();
        let outcome = server.join().unwrap();
        lan.stop();
        assert!(outcome.is_ok());
        assert!(events.iter().any(|e|matches!(e,crate::net::Event::Disconnected{reason,from_server:true} if reason==CONFIGURATION_DISABLED)));
        assert!(!events.iter().any(|e| matches!(
            e,
            crate::net::Event::Joined { .. } | crate::net::Event::Reconfigured(_)
        )));
        assert!(lan.poll().is_empty());
        assert_eq!(
            lan.shared.online.lock().unwrap().as_slice(),
            &[(uuid, "ExistingGuest".into())]
        );
        assert_eq!(lan.shared.next.load(Ordering::Relaxed), 1);
    }
}
