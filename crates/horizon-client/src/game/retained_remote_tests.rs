//! Original Game-bin owners. The runner supplies isolated paths and pinned
//! complete resources; these tests never borrow a legacy title Game.
use super::*;
use crate::game::{Game, GameCpuResources, Loaded, Options, Progress, load_resources};
use horizon_client::{settings::Settings, skins};
use horizon_protocol::packets::{Empty, play::Serverbound};
use std::{io::Read, net::TcpListener, path::PathBuf, sync::mpsc, time::Duration};

fn inputs() -> (Loaded, Options, Settings) {
    let root = PathBuf::from(
        std::env::var_os("HORIZON_TEST_RESOURCES").expect("complete pinned resources"),
    );
    let mut settings = Settings {
        render_distance: 2,
        ..Settings::default()
    };
    settings.horizon.lod_distance = 0;
    settings.controller.enabled = false;
    let options = Options::read_from(vec![
        "--resources".into(),
        root.to_string_lossy().into_owned(),
        "--in-frame".into(),
    ])
    .unwrap()
    .unwrap()
    .options;
    let loaded = load_resources(
        root.clone(),
        &root,
        &settings,
        Instant::now(),
        false,
        &Progress::new(),
        &mut |_, _| {},
    )
    .expect("actual synchronous resources");
    assert!(loaded.pack_error.is_none() && loaded.rest_loader.is_none() && loaded.gui.is_some());
    (loaded, options, settings)
}

struct OwnedGame<'a>(Option<Box<Game<'a>>>);
impl Drop for OwnedGame<'_> {
    fn drop(&mut self) {
        let Some(game) = &mut self.0 else { return };
        let deadline = Instant::now() + Duration::from_secs(20);
        game.begin_retained_remote_close(deadline);
        while !game.poll_retained_remote_close().quiescent {
            if Instant::now() >= deadline {
                use std::io::Write;
                let _ = std::io::stderr()
                    .write_all(b"fatal: original remote test cleanup unresolved\n");
                std::process::abort();
            }
            std::thread::yield_now();
        }
        drop(self.0.take());
    }
}

#[test]
#[ignore = "pinned Game-bin resources: direct original remote constructor and helper boundaries"]
fn retained_remote_factory_and_helper_guards() {
    let (loaded, options, settings) = inputs();
    let resources = loaded.resources;
    let cpu = GameCpuResources::load(&loaded.root).unwrap();
    let mut owner = OwnedGame(Some(Box::new(
        Game::new_retained_remote(
            &resources,
            loaded.pack,
            loaded.gui.unwrap(),
            options,
            settings,
            cpu,
            "127.0.0.1:25565".into(),
            "OwnerFixture".into(),
        )
        .unwrap(),
    )));
    let game = owner.0.as_mut().unwrap();
    assert!(game.save.is_none() && game.session_lock.is_none());
    assert!(game.title.is_none() && game.integrated.is_none() && game.lod.is_none());
    assert!(game.streaming.as_existing().is_err());
    let stream = game.streaming.remote().unwrap().try_observe().unwrap();
    assert!(!stream.started && stream.workers == 0 && stream.owned == 0);
    let io = game.remote.as_ref().unwrap().io.fixed().unwrap();
    let session = io.session.as_ref().unwrap().observe();
    assert!(!session.started && !session.reader_registered && !session.writer_registered);
    let decoder = io.decoder.as_ref().unwrap().observe().unwrap();
    assert!(!decoder.spawned && decoder.owned == 0);
    assert!(game.world.chunks.is_empty() && !game.native_close_owner_authorized());
    let url = "https://textures.minecraft.net/texture/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    assert_eq!(
        game.skin_downloads
            .borrow_mut()
            .state(skins::Kind::Skin, url),
        skins::Download::Failed
    );
    assert!(
        !game
            .skin_downloads
            .borrow_mut()
            .again("skins/0123456789abcdef")
    );
    assert!(matches!(
        game.skin_profiles
            .borrow_mut()
            .state(&skins::NameOrId::Name("Someone".into())),
        skins::Resolve::Done(None)
    ));
    assert!(game.skin_downloads.borrow_mut().take_ready().is_empty());
    let settings_before = game.settings.serialize();
    let file_before = game.settings.file.serialize();
    let inventory_before = game.inventory.as_ptr();
    let context_before = game.pack.context.clone();
    let uuid_before = game.level.player_uuid;
    let native_folder = format!("retained-refused-native-{}", std::process::id());
    let native_path = std::env::current_dir()
        .unwrap()
        .join("saves")
        .join(&native_folder);
    assert!(!native_path.exists());
    assert!(
        game.options_control(
            &["options-set", "renderDistance", "17"],
            "options-set renderDistance 17"
        )
        .is_err()
    );
    assert!(game.stage_selected_packs().is_err());
    assert!(
        game.lan_open(crate::game::lan::LanSettings {
            port: 0,
            guest_commands: false,
            force_game_mode: false
        })
        .is_err()
    );
    assert!(
        game.open_world(&native_folder, None, None, false, None)
            .is_err()
    );
    assert_eq!(game.settings.serialize(), settings_before);
    assert_eq!(game.settings.file.serialize(), file_before);
    assert_eq!(game.inventory.as_ptr(), inventory_before);
    assert_eq!(game.level.player_uuid, uuid_before);
    assert!(std::sync::Arc::ptr_eq(&context_before, &game.pack.context));
    assert!(game.world.chunks.is_empty() && game.player.is_none() && game.lan.is_none());
    assert!(game.save.is_none() && game.session_lock.is_none() && !native_path.exists());
    // Existing native provenance queries refuse this actual no-storage Game.
    assert!(
        game.prepare_close_identity_fields_avoiding(&std::collections::HashSet::new())
            .is_err()
    );
    game.retained_remote_request_leave();
    assert!(game.retained_remote_leave_requested());
    assert!(game.retained_remote_failure().is_none());
}

#[test]
#[ignore = "pinned Game-bin resources: exact refused encoding and original prepared closure"]
fn retained_remote_owned_refusal_and_prepared_close() {
    let (loaded, options, settings) = inputs();
    let resources = loaded.resources;
    let cpu = GameCpuResources::load(&loaded.root).unwrap();
    let mut owner = OwnedGame(Some(Box::new(
        Game::new_retained_remote(
            &resources,
            loaded.pack,
            loaded.gui.unwrap(),
            options,
            settings,
            cpu,
            "127.0.0.1:25565".into(),
            "RefusalFixture".into(),
        )
        .unwrap(),
    )));
    let game = owner.0.as_mut().unwrap();
    let address = std::ptr::from_ref(&**game);
    let original = Box::new(horizon_protocol::packets::play::Clientbound::Ping(
        horizon_protocol::packets::configuration::Ping { id: 71 },
    ));
    let original_address = std::ptr::from_ref(&*original);
    game.remote
        .as_mut()
        .unwrap()
        .io
        .fixed_mut()
        .unwrap()
        .pending
        .push_back(Event::Packet(original));
    // Honest controlled input: a typed play Ping before Session reached Play.
    // The actual production dispatch/send returns Phase, retaining BOTH the
    // exact incoming Box and the actual encoded refusal, without replay.
    assert!(game.poll_remote().is_err());
    game.send(Serverbound::ClientTickEnd(Empty));
    {
        let io = game.remote.as_ref().unwrap().io.fixed().unwrap();
        let refused = io.refused.borrow();
        let refused = refused.as_ref().unwrap();
        let (reason, body) = refused.first.as_ref().unwrap();
        assert_eq!(*reason, SendRefusal::Phase);
        assert_eq!(
            *body,
            Serverbound::Pong(horizon_protocol::packets::configuration::Ping { id: 71 }).encode()
        );
        assert_eq!(
            original_address,
            std::ptr::from_ref(&**io.applying.as_ref().unwrap())
        );
        assert_eq!(refused.subsequent.len(), 1);
        assert!(matches!(
            refused.subsequent.front(),
            Some(SendIntent::Play(Serverbound::ClientTickEnd(_)))
        ));
        let observation = io.session.as_ref().unwrap().observe();
        assert_eq!(observation.queued_bodies, 0);
        assert!(!observation.started);
    }
    let first_deadline = Instant::now() + Duration::from_secs(5);
    game.begin_retained_remote_close(first_deadline);
    game.begin_retained_remote_close(first_deadline + Duration::from_secs(20));
    assert_eq!(
        game.remote.as_ref().unwrap().io.fixed().unwrap().deadline,
        Some(first_deadline)
    );
    assert!(!game.retry_retained_remote_close(0, first_deadline));
    let status = game.poll_retained_remote_close();
    assert!(status.quiescent && !status.blocked && status.attempt == 1);
    assert_eq!(address, std::ptr::from_ref(&**game));
    assert!(game.retained_remote_failure().is_some());
    let io = game.remote.as_ref().unwrap().io.fixed().unwrap();
    assert!(io.closed_session.is_some() && io.closed_decoder.is_some());
    assert!(game.streaming.remote().is_none());
    assert!(io.refused.borrow().as_ref().unwrap().first.is_some());
}

#[test]
#[ignore = "pinned Game-bin resources: actual TCP held in Login, original actors and checked close"]
fn retained_remote_tcp_startup_close_keeps_original_owners() {
    partial_login_close(false);
    partial_login_close(true);
}

fn partial_login_close(unexpected_receive: bool) {
    let (loaded, options, settings) = inputs();
    let resources = loaded.resources;
    let cpu = GameCpuResources::load(&loaded.root).unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let (accepted, received) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let mut owner = OwnedGame(Some(Box::new(
        Game::new_retained_remote(
            &resources,
            loaded.pack,
            loaded.gui.unwrap(),
            options,
            settings,
            cpu,
            address,
            "SocketFixture".into(),
        )
        .unwrap(),
    )));
    let peer = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if wait.try_recv().is_ok() {
                        return;
                    }
                    assert!(Instant::now() < deadline, "actual peer accept timeout");
                    std::thread::yield_now();
                }
                Err(error) => panic!("actual peer accept failed: {error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        let mut first = [0u8; 1];
        assert_eq!(stream.read(&mut first).unwrap(), 1);
        // An incomplete VarInt frame remains in the actual native reader.
        std::io::Write::write_all(&mut stream, &[0x80]).unwrap();
        accepted.send(()).unwrap();
        // Deliberately no Login response: actual client receive IO is held.
        let _ = wait.recv_timeout(Duration::from_secs(20));
    });
    let assertions = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let game = owner.0.as_mut().unwrap();
        game.start_retained_remote().unwrap();
        received.recv_timeout(Duration::from_secs(10)).unwrap();
        let io = game.remote.as_ref().unwrap().io.fixed().unwrap();
        let observed_until = Instant::now() + Duration::from_secs(10);
        let observation = loop {
            let observation = io.session.as_ref().unwrap().observe();
            if observation.packets_sent == 2 && observation.bytes_in == 1 {
                break observation;
            }
            assert!(
                Instant::now() < observed_until,
                "original partial Login read missing"
            );
            std::thread::yield_now();
        };
        assert!(
            observation.started
                && observation.acquired
                && observation.reader_registered
                && !observation.writer_registered
        );
        assert!(io.decoder.as_ref().unwrap().observe().unwrap().spawned);
        if unexpected_receive {
            // The real peer ends the partial frame BEFORE the parent stop.
            release.send(()).unwrap();
            let until = Instant::now() + Duration::from_secs(10);
            while game.retained_remote_failure().is_none() {
                assert!(Instant::now() < until, "unexpected native Receive missing");
                std::thread::yield_now();
            }
        } else {
            assert!(game.retained_remote_failure().is_none());
        }
        // A later deliberate leave cannot erase an already observed error.
        game.retained_remote_request_leave();
        let first_deadline = Instant::now();
        game.begin_retained_remote_close(first_deadline);
        assert!(!game.retry_retained_remote_close(0, Instant::now() + Duration::from_secs(5)));
        let retried = game.retry_retained_remote_close(1, Instant::now() + Duration::from_secs(5));
        let expected_attempt = if retried { 2 } else { 1 };
        println!("S1OGRC1_TCP_RETRY_ADMITTED={retried}");
        assert_eq!(
            game.remote
                .as_ref()
                .unwrap()
                .io
                .fixed()
                .unwrap()
                .parent_attempt,
            expected_attempt
        );
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            let status = game.poll_retained_remote_close();
            if status.quiescent {
                assert_eq!(status.attempt, expected_attempt);
                break;
            }
            assert!(
                Instant::now() < until,
                "actual original TCP actors did not close"
            );
            std::thread::yield_now();
        }
        let io = game.remote.as_ref().unwrap().io.fixed().unwrap();
        let closed = io.closed_session.as_ref().unwrap().observe();
        assert!(closed.reader_registered && closed.reader_joined);
        assert!(!closed.writer_registered && !closed.writer_joined);
        assert!(
            io.closed_decoder
                .as_ref()
                .unwrap()
                .observe()
                .unwrap()
                .joined
        );
        assert!(game.save.is_none() && game.streaming.remote().is_none());
        assert!(
            closed
                .terminals
                .iter()
                .any(|t| t.actor == horizon_client::net::RetainedActor::Reader
                    && matches!(t.outcome, horizon_client::net::RetainedOutcome::Receive(_)))
        );
        let diagnostics = io.receive_diagnostics();
        if unexpected_receive {
            assert!(game.retained_remote_failure().is_some());
            assert!(diagnostics.is_empty());
        } else {
            assert!(game.retained_remote_failure().is_none());
            assert_eq!(diagnostics.len(), 1);
            assert!(
                diagnostics[0]
                    .contains("first observed after deliberate stop request; cause unresolved")
            );
            assert!(
                game.poll_retained_remote_close()
                    .detail
                    .contains(&diagnostics[0])
            );
        }
        println!(
            "S1OGRC1_RECEIVE_DIAGNOSTIC unexpected={unexpected_receive} detail={diagnostics:?}"
        );
    }));
    // Cleanup keeps original Game outside the unwind closure, and releases and
    // checks the real peer actor before resuming any assertion failure.
    drop(owner);
    let _ = release.send(());
    peer.join().unwrap();
    if let Err(panic) = assertions {
        std::panic::resume_unwind(panic);
    }
}
