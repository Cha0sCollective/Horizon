//! Genuine preparation/lock/worker ownership. No ordinary entry or disposal.
use super::*;
use crate::game::integrated::transition_ownership_tests::Fixture;
use horizon_client::save::SessionLock;
use std::{
    fs,
    path::Path,
    sync::mpsc,
    time::{Duration, Instant},
};

const WAIT: Duration = Duration::from_secs(120);

fn request(root: &Path) -> WorldRequest {
    let mut request = WorldRequest::new(
        "retained",
        Some("Retained native preparation"),
        Some(17),
        false,
        None,
    )
    .unwrap()
    .with_identity_intent(IdentityIntent::CreateNewIdentity);
    request.save_root = root.to_owned();
    request
}

fn start(
    game: &Game<'_>,
    request: WorldRequest,
    control: Option<RetainedPrepareControl>,
) -> RetainedWorldLoader {
    let mut options = game.options.clone();
    options.test_terrain = true;
    RetainedWorldLoader::start_inner(
        request,
        game.res.clone(),
        game.pack.clone(),
        options,
        game.settings.clone(),
        control,
    )
    .unwrap()
}

fn controlled(
    stage: RetainedPrepareStage,
    fail: bool,
) -> (RetainedPrepareControl, mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (entered, observe) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    (
        RetainedPrepareControl {
            stage,
            entered,
            release: wait,
            fail,
            #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
            streaming: None,
            #[cfg(all(feature = "native-writer-test-control", debug_assertions))]
            streaming_threads: None,
        },
        observe,
        release,
    )
}

fn harvest(loader: &mut RetainedWorldLoader) -> PreparedOutcome {
    // Controls are released before entering this helper. It never relinquishes
    // an unfinished native handle on a test timeout/assertion unwind.
    loop {
        if let Some(outcome) = loader.poll_retained_outcome() {
            return outcome.unwrap_or_else(|error| panic!("{error}"));
        }
        std::thread::yield_now();
    }
}

fn close(game: &Game<'_>, mut outcome: PreparedOutcome) -> PreparationClosed {
    outcome.begin_close(Instant::now() + WAIT);
    loop {
        match outcome.poll_close(game) {
            PreparationCloseStatus::Complete => break,
            PreparationCloseStatus::Pending => std::thread::yield_now(),
            PreparationCloseStatus::Blocked(error) => {
                // Retain the owner, including late actual handles, while an
                // expired healthy attempt is explicitly retried.
                if outcome.retry_close(Instant::now() + WAIT).is_err() {
                    eprintln!("retained preparation remains blocked: {error}");
                    std::thread::yield_now();
                }
            }
        }
    }
    match outcome.finish_close() {
        Ok(closed) => closed,
        Err(_) => unreachable!("terminal owner refused finish"),
    }
}

#[cfg(all(feature = "native-writer-test-control", debug_assertions))]
fn one_native_column(game: &Game<'_>, outcome: &mut PreparedOutcome, loaded: bool) -> (bool, bool) {
    use horizon_client::streaming::NativeWorkOrigin;
    let streaming = outcome.owner_mut().streaming.as_mut().unwrap();
    let mut gate = streaming
        .arm_retained_native_work_test_control(if loaded {
            NativeWorkOrigin::Generate
        } else {
            NativeWorkOrigin::Finish
        })
        .unwrap();
    let submitted = streaming.submit_retained_native_test_column(0, 0).is_ok();
    let mut finished_submitted = loaded;
    if !loaded {
        loop {
            match streaming.poll_retained_native_test_finish() {
                Ok(true) => {
                    finished_submitted = true;
                    break;
                }
                Ok(false) => std::thread::yield_now(),
                Err(_) => break,
            }
        }
    }
    let until = Instant::now() + WAIT;
    while !gate.entered() && Instant::now() < until {
        std::thread::yield_now();
    }
    let entered = gate.entered();
    outcome.begin_close(Instant::now() + WAIT);
    let mut pending = true;
    for _ in 0..16 {
        pending &= matches!(outcome.poll_close(game), PreparationCloseStatus::Pending);
    }
    gate.release();
    drop(gate);
    (submitted && finished_submitted && entered, pending)
}

fn native_item() -> horizon_world::nbt::Tag {
    use horizon_world::nbt::Tag;
    Tag::compound([
        ("id", Tag::String("minecraft:item".into())),
        ("UUID", Tag::IntArray(vec![123, 456, 789, 1011])),
        (
            "Pos",
            Tag::List(vec![
                Tag::Double(2.5f64.to_bits()),
                Tag::Double(120.5f64.to_bits()),
                Tag::Double(2.5f64.to_bits()),
            ]),
        ),
        (
            "Motion",
            Tag::List(vec![Tag::Double(0), Tag::Double(0), Tag::Double(0)]),
        ),
        ("Rotation", Tag::List(vec![Tag::Float(0), Tag::Float(0)])),
        ("OnGround", Tag::Byte(1)),
        ("fall_distance", Tag::Double(0)),
        ("Fire", Tag::Short(-7)),
        ("Air", Tag::Short(300)),
        ("Invulnerable", Tag::Byte(0)),
        ("PortalCooldown", Tag::Int(0)),
        (
            "Item",
            Tag::compound([
                ("id", Tag::String("minecraft:stone".into())),
                ("count", Tag::Int(7)),
            ]),
        ),
        ("Health", Tag::Short(4)),
        ("Age", Tag::Short(201)),
        ("PickupDelay", Tag::Short(17)),
        ("Thrower", Tag::IntArray(vec![1, 2, 3, 4])),
        ("Owner", Tag::IntArray(vec![5, 6, 7, 8])),
    ])
}

#[cfg(all(feature = "native-writer-test-control", debug_assertions))]
#[test]
#[ignore = "owned prepared resources: actual held native Finish, saved column/items and original loaded-record close"]
fn retained_prepare_native_late_full_column_and_saved_items_close_without_activation() {
    let fixture = Fixture::new("retained-full-column");
    let game = fixture.game();
    let root = game
        .world_root
        .parent()
        .unwrap()
        .join("retained-full-column");
    let mut loader = start(&game, request(&root), None);
    let mut outcome = harvest(&mut loader);
    let (fresh_held, fresh_pending) = one_native_column(&game, &mut outcome, false);
    let fresh_closed = close(&game, outcome);
    // Authored supported native input is written through the genuine storage
    // service only after the first retained producer/writer owner is closed.
    let dimension = horizon_client::save::dimension_dir(&root, Dim::Overworld.id());
    let authored = native_item();
    let saved = SaveDir::new(dimension.clone());
    saved
        .save_entities(vec![((0, 0), vec![authored.clone()])])
        .unwrap();
    drop(saved);
    let column_path = dimension.join("region/r.0.0.mca");
    let entity_path = dimension.join("entities/r.0.0.mca");
    let column_before = fs::read(&column_path).unwrap();
    let entities_before = fs::read(&entity_path).unwrap();
    let mut reopen = request(&root).with_identity_intent(IdentityIntent::RequireExistingIdentity);
    reopen.seed = None;
    let mut loader = start(&game, reopen, None);
    let mut outcome = harvest(&mut loader);
    let save = outcome.owner().save.as_ref().unwrap().clone();
    let original_world = outcome.owner().world.as_deref().unwrap() as *const World as usize;
    let (loaded_held, loaded_pending) = one_native_column(&game, &mut outcome, true);
    let mut stored_identities = false;
    loop {
        stored_identities |= outcome
            .owner()
            .accepted_item_uuids
            .contains(&[123, 456, 789, 1011]);
        match outcome.poll_close(&game) {
            PreparationCloseStatus::Complete => break,
            PreparationCloseStatus::Pending => std::thread::yield_now(),
            PreparationCloseStatus::Blocked(_) => {
                let _ = outcome.retry_close(Instant::now() + WAIT);
            }
        }
    }
    let no_activation = outcome
        .owner()
        .world
        .as_deref()
        .is_some_and(|world| world.chunks.is_empty() && world.loaded_entities.is_empty());
    let same_world =
        outcome.owner().world.as_deref().unwrap() as *const World as usize == original_world;
    let same_writer = outcome
        .owner()
        .writer_closed
        .as_ref()
        .is_some_and(|closed| Arc::ptr_eq(closed.save(), &save));
    let loaded_closed = outcome.finish_close().ok().unwrap();
    assert!(
        fresh_held
            && fresh_pending
            && loaded_held
            && loaded_pending
            && stored_identities
            && no_activation
            && same_world
            && same_writer
    );
    assert!(fresh_closed.original_error.is_none() && loaded_closed.original_error.is_none());
    assert_eq!(fs::read(&column_path).unwrap(), column_before);
    assert_eq!(fs::read(&entity_path).unwrap(), entities_before);
    assert!(SessionLock::acquire(&root).is_ok());
}

fn captured_load(game: &Game<'_>, save: Arc<SaveDir>) -> horizon_client::save::RetainedNativeLoad {
    use horizon_client::streaming::ChunkGenerator;
    let generator = Persistent {
        inner: Arc::new(TestTerrain::new(game.pack.assets.registry, game.res.biomes()).unwrap()),
        save,
        registry: game.pack.assets.clone(),
        biomes: game.res.biomes().clone(),
    };
    let handle = std::thread::spawn(move || generator.retained_load(0, 0));
    while !handle.is_finished() {
        std::thread::yield_now();
    }
    handle.join().unwrap()
}

fn captured_adapter_refuses(
    game: &Game<'_>,
    outcome: &PreparedOutcome,
    chunk: &Arc<horizon_client::chunk::Chunk>,
    evidence: &horizon_client::save::RetainedLoadedRecords,
) -> bool {
    let owner = outcome.owner();
    loop {
        match validate_retained_column(
            game,
            &owner.request.save_root,
            owner.level.as_ref(),
            owner.world.as_deref(),
            owner.session.as_ref(),
            owner.save.as_ref().unwrap(),
            owner.native_checkpoint_origin.as_ref(),
            &owner.accepted_item_uuids,
            chunk,
            Some(evidence),
        ) {
            Ok(None) => std::thread::yield_now(),
            Ok(Some(_)) => return false,
            Err(_) => return true,
        }
    }
}

#[cfg(all(feature = "native-writer-test-control", debug_assertions))]
#[test]
#[ignore = "owned prepared resources: actual pending native Chunk/FinishedEntities/Delete custody and strict adapter"]
fn retained_prepare_original_pending_native_records_survive_adapter_and_writer_close() {
    use horizon_client::{
        save::{NativeWriterTestAction, RetainedPendingKind, RetainedRecordView},
        streaming::ChunkGenerator,
    };
    let fixture = Fixture::new("retained-pending-records");
    let game = fixture.game();
    for with_items in [true, false] {
        let root = game
            .world_root
            .parent()
            .unwrap()
            .join(format!("retained-pending-{with_items}"));
        let mut loader = start(&game, request(&root), None);
        let outcome = harvest(&mut loader);
        let save = outcome.owner().save.as_ref().unwrap().clone();
        let mut control = save
            .arm_native_writer_test_control(NativeWriterTestAction::Block)
            .unwrap();
        let generator = Persistent {
            inner: Arc::new(
                TestTerrain::new(game.pack.assets.registry, game.res.biomes()).unwrap(),
            ),
            save: save.clone(),
            registry: game.pack.assets.clone(),
            biomes: game.res.biomes().clone(),
        };
        let handle = std::thread::spawn(move || {
            generator.worker();
            let proto = generator.proto(&mut (), 0, 0);
            let mut chunk = generator.finish(proto);
            if with_items {
                chunk.entities.push(native_item());
            }
            generator.save_finished(&Arc::new(chunk)).unwrap();
        });
        while !handle.is_finished() {
            std::thread::yield_now();
        }
        let generated = handle.join().is_ok();
        let entered = control.wait_entered(WAIT).is_ok();
        let deleted = with_items || save.save_entities_later(vec![((0, 0), Vec::new())]).is_ok();
        let loaded = captured_load(&game, save.clone());
        let (chunk, evidence) = match Box::new(loaded).into_parts() {
            Ok(parts) => parts,
            Err(refused) => {
                let message = refused.error().map(str::to_owned);
                let _ = control.trigger();
                drop(control);
                close(&game, outcome);
                drop(refused);
                panic!("actual pending native load failed: {message:?}");
            }
        };
        let chunk = Arc::new(chunk);
        let pending_column = matches!(
            evidence.column(),
            RetainedRecordView::Pending {
                kind: RetainedPendingKind::Chunk,
                chunk: Some(_),
                ..
            }
        );
        let pending_entities = if with_items {
            matches!(
                evidence.entities(),
                RetainedRecordView::Pending {
                    kind: RetainedPendingKind::FinishedEntities,
                    chunk: Some(_),
                    ..
                }
            )
        } else {
            matches!(
                evidence.entities(),
                RetainedRecordView::Pending {
                    kind: RetainedPendingKind::Delete,
                    ..
                }
            )
        };
        let accepted = !captured_adapter_refuses(&game, &outcome, &chunk, &evidence);
        let _ = save.save_entities_later(vec![((50, 50), Vec::new())]);
        let unrelated_current = loop {
            match evidence.poll_current() {
                Ok(Some(current)) => break current,
                Ok(None) => std::thread::yield_now(),
                Err(_) => break false,
            }
        };
        let original_items = chunk.entities.clone();
        let _ = control.trigger();
        drop(control);
        let closed = close(&game, outcome);
        let still_original = chunk.entities == original_items;
        drop(evidence);
        assert!(
            generated
                && entered
                && deleted
                && pending_column
                && pending_entities
                && accepted
                && unrelated_current
                && still_original
        );
        assert!(closed.original_error.is_none());
        assert_eq!(
            original_items,
            if with_items {
                vec![native_item()]
            } else {
                Vec::new()
            }
        );
        assert!(SessionLock::acquire(&root).is_ok());
    }
}

#[test]
#[ignore = "owned prepared resources: strict adapter rejects original malformed items and newer relevant storage receipts"]
fn retained_prepare_original_loaded_adapter_refuses_payload_and_sequence_ambiguity() {
    use horizon_client::streaming::ChunkGenerator;
    let fixture = Fixture::new("retained-adapter-refusals");
    let game = fixture.game();
    let root = game
        .world_root
        .parent()
        .unwrap()
        .join("retained-adapter-refusals");
    let mut loader = start(&game, request(&root), None);
    let outcome = harvest(&mut loader);
    let save = outcome.owner().save.as_ref().unwrap().clone();
    let inner = Arc::new(TestTerrain::new(game.pack.assets.registry, game.res.biomes()).unwrap());
    let generator = Persistent {
        inner,
        save: save.clone(),
        registry: game.pack.assets.clone(),
        biomes: game.res.biomes().clone(),
    };
    let handle = std::thread::spawn(move || {
        generator.worker();
        let proto = generator.proto(&mut (), 0, 0);
        let chunk = Arc::new(generator.finish(proto));
        generator.save_finished(&chunk).unwrap();
    });
    while !handle.is_finished() {
        std::thread::yield_now();
    }
    handle.join().unwrap();
    let mut invalid = native_item();
    if let horizon_world::nbt::Tag::Compound(values) = &mut invalid {
        values.insert("unsupported".into(), horizon_world::nbt::Tag::Byte(1));
    }
    save.save_entities_later(vec![((0, 0), vec![invalid])])
        .unwrap();
    let loaded = captured_load(&game, save.clone());
    let (chunk, evidence) = match Box::new(loaded).into_parts() {
        Ok(parts) => parts,
        Err(refused) => {
            let message = refused.error().map(str::to_owned);
            close(&game, outcome);
            drop(refused);
            panic!("actual native load failed before strict adapter: {message:?}");
        }
    };
    let chunk = Arc::new(chunk);
    let refused = captured_adapter_refuses(&game, &outcome, &chunk, &evidence);
    save.save_entities_later(vec![((0, 0), vec![native_item()])])
        .unwrap();
    let newer_refused = captured_adapter_refuses(&game, &outcome, &chunk, &evidence);
    let unchanged_payload =
        chunk.entities.len() == 1 && chunk.entities[0].get("unsupported").is_some();
    let closed = close(&game, outcome);
    // Keep the original refused evidence alive through the same writer close.
    drop(evidence);
    assert!(refused && newer_refused && unchanged_payload && closed.original_error.is_none());
    assert!(SessionLock::acquire(&root).is_ok());
}

#[cfg(all(feature = "native-writer-test-control", debug_assertions))]
fn stored_native_input(game: &Game<'_>, root: &Path) {
    use horizon_client::streaming::ChunkGenerator;
    let mut loader = start(game, request(root), None);
    let outcome = harvest(&mut loader);
    let generator = Persistent {
        inner: Arc::new(TestTerrain::new(game.pack.assets.registry, game.res.biomes()).unwrap()),
        save: outcome.owner().save.as_ref().unwrap().clone(),
        registry: game.pack.assets.clone(),
        biomes: game.res.biomes().clone(),
    };
    let handle = std::thread::spawn(move || {
        generator.worker();
        let proto = generator.proto(&mut (), 0, 0);
        let mut chunk = generator.finish(proto);
        chunk.entities.push(native_item());
        generator.save_finished(&Arc::new(chunk))
    });
    while !handle.is_finished() {
        std::thread::yield_now();
    }
    let generated = handle.join().ok().is_some_and(|result| result.is_ok());
    let closed = close(game, outcome);
    assert!(generated && closed.original_error.is_none() && closed.read_refusals.is_empty());
}

#[cfg(all(feature = "native-writer-test-control", debug_assertions))]
#[test]
#[ignore = "owned prepared resources: actual malformed/proto/unsupported/orphan read failures close without regeneration or rewrite"]
fn retained_prepare_refused_native_input_has_distinct_failed_cleanup_and_preserves_original_storage()
 {
    use horizon_client::streaming::NativeWorkOrigin;
    use horizon_core::position::ChunkPos;
    use horizon_world::{
        compression::Compression,
        nbt::{self, Limits, Tag},
        region::{ChunkRecord, RegionFile},
    };
    let fixture = Fixture::new("retained-refused-input");
    let game = fixture.game();
    for mode in [
        "proto",
        "wrong-column-coordinate",
        "malformed-nbt",
        "malformed-compression",
        "raw-header",
        "unknown-item",
        "orphan-sidecar",
    ] {
        let root = game
            .world_root
            .parent()
            .unwrap()
            .join(format!("retained-refused-{mode}"));
        stored_native_input(&game, &root);
        let dimension = horizon_client::save::dimension_dir(&root, Dim::Overworld.id());
        let column_path = dimension.join("region/r.0.0.mca");
        let entity_path = dimension.join("entities/r.0.0.mca");
        match mode {
            "proto" | "wrong-column-coordinate" | "malformed-nbt" | "malformed-compression" => {
                let mut region = RegionFile::open_writable(&column_path, false).unwrap();
                let at = ChunkPos::new(0, 0);
                let original = region.read(at).unwrap().unwrap();
                let (compression, compressed) = match mode {
                    "proto" | "wrong-column-coordinate" => {
                        let raw = original
                            .compression
                            .decode(&original.compressed, 1 << 20)
                            .unwrap();
                        let mut named = nbt::read_named(&raw, Limits::default()).unwrap().0;
                        let Tag::Compound(values) = &mut named.tag else {
                            panic!("native fixture column root");
                        };
                        if mode == "wrong-column-coordinate" {
                            values.insert("xPos".into(), Tag::Int(1));
                        } else {
                            values.insert("Status".into(), Tag::String("minecraft:noise".into()));
                        }
                        (
                            Compression::Zlib,
                            Compression::Zlib
                                .encode(&nbt::write_named(&named, Limits::default()).unwrap())
                                .unwrap(),
                        )
                    }
                    "malformed-nbt" => (
                        Compression::Zlib,
                        Compression::Zlib.encode(&[0x0a, 0]).unwrap(),
                    ),
                    _ => (Compression::Zlib, vec![0xff, 0]),
                };
                region
                    .write(
                        at,
                        &ChunkRecord {
                            compression,
                            compressed,
                            timestamp: original.timestamp,
                        },
                    )
                    .unwrap();
            }
            "raw-header" => {
                let mut original = fs::read(&column_path).unwrap();
                original[..4].copy_from_slice(&0x0000_0101u32.to_be_bytes());
                fs::write(&column_path, original).unwrap();
            }
            "unknown-item" => {
                let mut item = native_item();
                let Tag::Compound(values) = &mut item else {
                    unreachable!();
                };
                values.insert("unsupported".into(), Tag::Byte(1));
                let save = SaveDir::new(dimension.clone());
                save.save_entities(vec![((0, 0), vec![item])]).unwrap();
            }
            "orphan-sidecar" => fs::remove_file(&column_path).unwrap(),
            _ => unreachable!(),
        }
        let column_before = fs::read(&column_path).ok();
        let entities_before = fs::read(&entity_path).ok();
        let identity_before = fs::read(root.join(host_identity::IDENTITY_FILE)).unwrap();
        let mut loader = start(
            &game,
            request(&root).with_identity_intent(IdentityIntent::RequireExistingIdentity),
            None,
        );
        let mut outcome = harvest(&mut loader);
        let (held, pending) = one_native_column(&game, &mut outcome, true);
        loop {
            match outcome.poll_close(&game) {
                PreparationCloseStatus::Complete => break,
                PreparationCloseStatus::Pending => std::thread::yield_now(),
                PreparationCloseStatus::Blocked(_) => {
                    let _ = outcome.retry_close(Instant::now() + WAIT);
                }
            }
        }
        let no_activation = outcome
            .owner()
            .world
            .as_deref()
            .is_some_and(|world| world.chunks.is_empty() && world.loaded_entities.is_empty());
        let observation = outcome
            .owner()
            .streaming
            .as_ref()
            .unwrap()
            .observe_native_work();
        let no_finish = observation
            .origins
            .iter()
            .find(|(origin, _)| *origin == NativeWorkOrigin::Finish)
            .unwrap()
            .1
            .submitted
            == 0;
        let closed = outcome.finish_close().ok().unwrap();
        assert!(held && pending && no_activation && no_finish);
        assert!(
            closed.original_error.is_none() && !closed.read_refusals.is_empty(),
            "{mode} did not retain distinct failed-cleanup refusal"
        );
        if mode == "wrong-column-coordinate" {
            assert!(
                closed
                    .read_refusals
                    .iter()
                    .any(|error| error.contains("coordinate"))
            );
        }
        assert_eq!(
            fs::read(&column_path).ok(),
            column_before,
            "{mode} rewrote original column"
        );
        assert_eq!(
            fs::read(&entity_path).ok(),
            entities_before,
            "{mode} rewrote original sidecar"
        );
        assert_eq!(
            fs::read(root.join(host_identity::IDENTITY_FILE)).unwrap(),
            identity_before
        );
        assert!(SessionLock::acquire(&root).is_ok());
    }
}

#[test]
#[ignore = "owned prepared resources: native retained Ready, checked dormant transfer, producer/writer close"]
fn retained_prepare_ready_dormant_adoption_and_same_allocation_close() {
    let fixture = Fixture::new("retained-ready");
    let mut game = fixture.game();
    let root = game.world_root.parent().unwrap().join("retained-ready");
    game.options.test_terrain = true;
    let token = game.begin_retained_preparation(request(&root)).unwrap();
    let outcome = loop {
        if let Some(outcome) = game.poll_retained_prepare_outcome() {
            break outcome.unwrap();
        }
        std::thread::yield_now();
    };
    let original_owner = outcome.owner() as *const PrepareOwner as usize;
    let original_world = outcome.owner().world.as_deref().unwrap() as *const World as usize;
    let save = outcome.owner().save.as_ref().unwrap().clone();
    let locked = SessionLock::acquire(&root).is_err();
    let refused_next_while_harvested = game
        .begin_retained_preparation(request(&root.with_extension("next")))
        .is_err();
    let ready = match outcome {
        PreparedOutcome::Ready(ready) => ready,
        failed => {
            let closed = close(&game, failed);
            panic!(
                "unexpected failed native prepare: {:?}",
                closed.original_error
            );
        }
    };
    let transfer = game.adopt_retained_prepared(token, ready);
    let transfer_ok = transfer.is_ok();
    let mut outcome = match transfer {
        Ok(()) => game.retained_preparation.take().unwrap(),
        Err((ready, _)) => PreparedOutcome::Ready(ready),
    };
    let same_owner = outcome.owner() as *const PrepareOwner as usize == original_owner;
    let same_world =
        outcome.owner().world.as_deref().unwrap() as *const World as usize == original_world;
    outcome.request_cancel();
    let closed = close(&game, outcome);
    let closed_cancelled = closed.cancelled;
    let closed_error = closed.original_error.clone();
    let refused_next_before_ack = game
        .begin_retained_preparation(request(&root.with_extension("next")))
        .is_err();
    let acknowledged = game.acknowledge_retained_preparation_closed(closed).is_ok();
    let reacquired = SessionLock::acquire(&root);
    assert!(
        locked
            && transfer_ok
            && same_owner
            && same_world
            && refused_next_while_harvested
            && refused_next_before_ack
            && acknowledged
    );
    assert!(closed_cancelled && closed_error.is_none());
    assert!(reacquired.is_ok());
    assert!(save.root.ends_with("dimensions/minecraft/overworld"));
}

#[test]
#[ignore = "owned worldgen bundle: genuine flat and Vanilla initialization, no-job retained close"]
fn retained_prepare_flat_and_vanilla_native_factories_close_without_activation() {
    let fixture = Fixture::new("retained-native-factories");
    let game = fixture.game();
    for flat in [true, false] {
        let root = game.world_root.parent().unwrap().join(if flat {
            "retained-flat"
        } else {
            "retained-vanilla"
        });
        let mut request = request(&root);
        request.create = Some(horizon_client::title::WorldOptions {
            flat,
            ..Default::default()
        });
        let mut options = game.options.clone();
        options.test_terrain = false;
        let mut loader = RetainedWorldLoader::start(
            request,
            game.res.clone(),
            game.pack.clone(),
            options,
            game.settings.clone(),
        )
        .unwrap();
        let mut outcome = harvest(&mut loader);
        let healthy = matches!(&outcome, PreparedOutcome::Ready(_));
        let world_empty = outcome
            .owner()
            .world
            .as_ref()
            .is_some_and(|world| world.chunks.is_empty());
        let locator = outcome.owner().locator.is_some();
        let submitted = outcome
            .owner()
            .streaming
            .as_ref()
            .map(|streaming| streaming.observe_native_work());
        outcome.request_cancel();
        let closed = close(&game, outcome);
        assert!(
            healthy && world_empty,
            "native factory preparation failed: {:?}",
            closed.original_error
        );
        assert_eq!(locator, !flat);
        assert!(
            submitted
                .unwrap()
                .origins
                .iter()
                .all(|(_, counts)| counts.submitted == 0)
        );
        assert!(closed.cancelled && closed.original_error.is_none());
        assert!(SessionLock::acquire(&root).is_ok());
    }
}

#[test]
#[ignore = "owned prepared resources: real initialization handle held at every retained acquisition seam"]
fn retained_prepare_cancel_and_late_failure_preserve_all_partial_owners() {
    let fixture = Fixture::new("retained-partials");
    let game = fixture.game();
    for (index, stage) in [
        RetainedPrepareStage::BeforeAcquire,
        RetainedPrepareStage::NativeAcquired,
        RetainedPrepareStage::SaveAcquired,
        RetainedPrepareStage::StreamingAcquired,
    ]
    .into_iter()
    .enumerate()
    {
        for fail in [false, true] {
            let root = game
                .world_root
                .parent()
                .unwrap()
                .join(format!("retained-partial-{index}-{fail}"));
            let (control, entered, release) = controlled(stage, fail);
            let mut loader = start(&game, request(&root), Some(control));
            let entered = entered.recv_timeout(WAIT).is_ok();
            loader.request_cancel();
            loader.request_cancel();
            let started = Instant::now();
            let mut pending = true;
            for _ in 0..32 {
                pending &= loader.poll_retained_outcome().is_none();
            }
            let elapsed = started.elapsed();
            let locked_while_held = stage == RetainedPrepareStage::BeforeAcquire
                || SessionLock::acquire(&root).is_err();
            let _ = release.send(());
            let outcome = harvest(&mut loader);
            let harvested_once = loader.poll_retained_outcome().is_none();
            let cancelled = outcome.owner().cancelled;
            let raw_retained = stage != RetainedPrepareStage::NativeAcquired
                || !fail
                || outcome
                    .owner()
                    .native_acquisition
                    .as_ref()
                    .is_some_and(|lock| lock.acquired());
            let save_retained = !matches!(
                stage,
                RetainedPrepareStage::SaveAcquired | RetainedPrepareStage::StreamingAcquired
            ) || outcome.owner().save.is_some();
            let stream_retained = stage != RetainedPrepareStage::StreamingAcquired
                || outcome.owner().streaming.is_some();
            let original_error = outcome.owner().original_error.clone();
            let closed = close(&game, outcome);
            let reacquired = SessionLock::acquire(&root).is_ok();
            assert!(entered && pending && elapsed < Duration::from_secs(1));
            assert!(
                locked_while_held
                    && cancelled
                    && raw_retained
                    && save_retained
                    && stream_retained
                    && harvested_once
                    && reacquired
            );
            assert_eq!(closed.original_error, original_error);
            assert_eq!(closed.cancelled, cancelled);
            assert!(
                original_error.is_some() == (fail || stage == RetainedPrepareStage::BeforeAcquire)
            );
        }
    }
}

#[test]
#[ignore = "owned prepared resources: actual exclusive identity create failure keeps native and authority custody"]
fn retained_prepare_identity_create_error_and_stale_adoption_keep_original_owners() {
    let fixture = Fixture::new("retained-identity-error");
    let mut game = fixture.game();
    let base = game.world_root.parent().unwrap();
    let root = base.join("retained-identity-error");
    let namespace_root = base.join("retained-authority");
    fs::create_dir(&namespace_root).unwrap();
    let namespace =
        host_identity::session_authority::AuthorityNamespace::new(&namespace_root).unwrap();
    let (control, entered, release) = controlled(RetainedPrepareStage::IdentityFinish, false);
    let mut loader = start(
        &game,
        request(&root).with_session_authority(namespace),
        Some(control),
    );
    let entered = entered.recv_timeout(WAIT).is_ok();
    let collided = fs::write(
        root.join(host_identity::IDENTITY_FILE),
        b"retained exclusive-create collision",
    )
    .is_ok();
    let _ = release.send(());
    let failed = harvest(&mut loader);
    let original = failed.owner().original_error.clone();
    let native_retained = failed
        .owner()
        .native_acquisition
        .as_ref()
        .is_some_and(|lock| lock.acquired());
    let authority_retained = failed.owner().authority.is_some();
    let closed = close(&game, failed);
    assert!(entered && collided && native_retained && authority_retained);
    assert_eq!(original, closed.original_error);
    assert!(
        closed
            .original_error
            .unwrap()
            .contains("exclusive identity create")
    );

    let root = base.join("retained-stale");
    let mut loader = start(&game, request(&root), None);
    let outcome = harvest(&mut loader);
    let original_owner = outcome.owner() as *const PrepareOwner as usize;
    let ready = match outcome {
        PreparedOutcome::Ready(ready) => ready,
        failed => {
            let closed = close(&game, failed);
            panic!(
                "unexpected failed native prepare: {:?}",
                closed.original_error
            );
        }
    };
    let token = game.retained_entry_token;
    game.invalidate_retained_entry().unwrap();
    let refused = game.adopt_retained_prepared(token, ready);
    let (outcome, same_owner, refused) = match refused {
        Err((ready, _)) => {
            let same = &*ready.owner as *const PrepareOwner as usize == original_owner;
            (PreparedOutcome::Ready(ready), same, true)
        }
        Ok(()) => (game.retained_preparation.take().unwrap(), false, false),
    };
    close(&game, outcome);
    assert!(same_owner && refused);
}

#[test]
fn retained_resource_tail_finished_only_error_and_panic_harvest() {
    for panic in [false, true] {
        let (release, wait) = mpsc::channel();
        let loader = WorldRestLoader {
            progress: Progress::with(&["retained resource tail"]),
            handle: std::thread::spawn(move || {
                let _ = wait.recv();
                if panic {
                    panic!("controlled retained resource tail panic");
                }
                Err("controlled retained resource tail error".to_owned())
            }),
        };
        let (mut loader, pending) = match loader.try_harvest() {
            Err(loader) => (Some(loader), true),
            Ok(_) => (None, false),
        };
        let _ = release.send(());
        let result = loop {
            let Some(current) = loader.take() else {
                break Ok(());
            };
            match current.try_harvest() {
                Ok(result) => break result,
                Err(current) => loader = Some(current),
            }
            std::thread::yield_now();
        };
        assert!(pending);
        assert_eq!(
            result.unwrap_err(),
            if panic {
                "world resource loader panicked"
            } else {
                "controlled retained resource tail error"
            }
        );
    }
}

#[cfg(all(feature = "native-writer-test-control", debug_assertions))]
#[test]
#[ignore = "owned prepared resources: actual partial native lock header retained by outer preparation guard"]
fn retained_prepare_partial_native_header_closes_without_reacquisition_or_replay() {
    use horizon_client::save::{RetainedLockTestFault, RetainedSessionLock};
    let fixture = Fixture::new("retained-partial-header");
    let game = fixture.game();
    let root = game
        .world_root
        .parent()
        .unwrap()
        .join("retained-partial-header");
    let mut owner = PrepareOwner::new(request(&root));
    owner.native_acquisition = Some(RetainedSessionLock::with_test_fault(
        &root,
        RetainedLockTestFault::PartialHeader,
    ));
    let error = owner
        .native_acquisition
        .as_mut()
        .unwrap()
        .acquire()
        .unwrap_err()
        .to_string();
    owner.original_error = Some(error.clone());
    let actual_handle = owner.native_acquisition.as_ref().unwrap().test_handle();
    let held = SessionLock::acquire(&root).is_err();
    let closed = close(&game, PreparedOutcome::Failed(PrepareFailure { owner }));
    assert!(held && actual_handle.is_some());
    assert_eq!(closed.original_error.as_deref(), Some(error.as_str()));
    assert!(SessionLock::acquire(&root).is_ok());
    assert!(!root.join(host_identity::IDENTITY_FILE).exists());
}

#[cfg(all(feature = "native-writer-test-control", debug_assertions))]
#[test]
#[ignore = "owned prepared resources: actual held native writer, deadline retention and explicit same-owner Retry"]
fn retained_prepare_writer_deadline_keeps_identity_and_locks_until_actual_join() {
    use horizon_client::save::NativeWriterTestAction;
    let fixture = Fixture::new("retained-writer-deadline");
    let game = fixture.game();
    let root = game
        .world_root
        .parent()
        .unwrap()
        .join("retained-writer-deadline");
    let mut loader = start(&game, request(&root), None);
    let mut outcome = harvest(&mut loader);
    let owner_address = outcome.owner() as *const PrepareOwner as usize;
    let save = outcome.owner().save.as_ref().unwrap().clone();
    let identity = fs::read(root.join(host_identity::IDENTITY_FILE)).unwrap();
    let mut control = save
        .arm_native_writer_test_control(NativeWriterTestAction::Block)
        .unwrap();
    let accepted = save
        .save_entities_later(vec![((31, 31), Vec::new())])
        .is_ok();
    let entered = control.wait_entered(WAIT).is_ok();
    let deadline = Instant::now() + Duration::from_secs(1);
    outcome.begin_close(deadline);
    let mut saw_writer = false;
    let mut saw_blocked = false;
    let started = Instant::now();
    while Instant::now() < deadline + Duration::from_millis(20) {
        saw_writer |= outcome.owner().writer.is_some();
        saw_blocked |= matches!(
            outcome.poll_close(&game),
            PreparationCloseStatus::Blocked(_)
        );
        std::thread::yield_now();
    }
    let elapsed = started.elapsed();
    let retained_lock = SessionLock::acquire(&root).is_err();
    let same_owner = outcome.owner() as *const PrepareOwner as usize == owner_address;
    let same_save = Arc::ptr_eq(&save, outcome.owner().save.as_ref().unwrap());
    let _ = control.trigger();
    drop(control);
    // Harvest actual completion while preserving the latched Blocked attempt;
    // Retry may not substitute an unfinished previous writer operation.
    loop {
        if matches!(outcome.poll_close(&game), PreparationCloseStatus::Complete) {
            break;
        }
        if outcome.retry_close(Instant::now() + WAIT).is_ok() {
            break;
        }
        std::thread::yield_now();
    }
    let closed = close(&game, outcome);
    assert!(
        accepted
            && entered
            && saw_writer
            && saw_blocked
            && retained_lock
            && same_owner
            && same_save
    );
    assert!(elapsed < Duration::from_secs(2));
    assert!(closed.original_error.is_none());
    assert_eq!(
        fs::read(root.join(host_identity::IDENTITY_FILE)).unwrap(),
        identity
    );
    assert!(SessionLock::acquire(&root).is_ok());
}

#[cfg(all(feature = "native-writer-test-control", debug_assertions))]
#[test]
#[ignore = "owned prepared resources: actual first/later native workers and later spawn failure retained through outer prepare"]
fn retained_prepare_partial_streaming_start_holds_real_workers_until_retry_and_join() {
    use horizon_client::streaming::RetainedStreamingStartTestControl;
    let fixture = Fixture::new("retained-worker-start");
    let game = fixture.game();
    for worker in [0, 1] {
        for fail_spawn in [None, Some(2)] {
            let root = game
                .world_root
                .parent()
                .unwrap()
                .join(format!("retained-worker-{worker}-{}", fail_spawn.is_some()));
            let (mut control, entered, release) =
                controlled(RetainedPrepareStage::BeforeAcquire, false);
            let (streaming_control, mut receipt) =
                RetainedStreamingStartTestControl::new(Some(worker), fail_spawn);
            control.streaming = Some(streaming_control);
            control.streaming_threads = Some(3);
            let mut loader = start(&game, request(&root), Some(control));
            let outer_entered = entered.recv_timeout(WAIT).is_ok();
            let _ = release.send(());
            let until = Instant::now() + WAIT;
            while !receipt.entered() && Instant::now() < until {
                std::thread::yield_now();
            }
            let native_entered = receipt.entered();
            loader.request_cancel();
            let mut outcome = harvest(&mut loader);
            let original_error = outcome.owner().original_error.clone();
            let no_world_on_failure = fail_spawn.is_none() || outcome.owner().world.is_none();
            let retained_stream = outcome.owner().streaming.is_some();
            let deadline = Instant::now() + Duration::from_millis(10);
            outcome.begin_close(deadline);
            let mut blocked = false;
            while Instant::now() < deadline + Duration::from_millis(5) {
                blocked |= matches!(
                    outcome.poll_close(&game),
                    PreparationCloseStatus::Blocked(_)
                );
                std::thread::yield_now();
            }
            let locked = SessionLock::acquire(&root).is_err();
            receipt.release();
            drop(receipt);
            let closed = close(&game, outcome);
            assert!(
                outer_entered
                    && native_entered
                    && blocked
                    && locked
                    && no_world_on_failure
                    && retained_stream
            );
            assert_eq!(closed.original_error, original_error);
            assert_eq!(closed.original_error.is_some(), fail_spawn.is_some());
            assert!(closed.cancelled);
            assert!(SessionLock::acquire(&root).is_ok());
        }
    }
}

#[test]
#[ignore = "separate direct prebuilt process: marked expected fatal initialization-handle Drop; never run in ordinary battery"]
fn retained_prepare_unfinished_initialization_handle_drop_fatal() {
    use std::io::Write;
    let fixture = Fixture::new("retained-fatal-init-handle");
    let game = fixture.game();
    let root = game
        .world_root
        .parent()
        .unwrap()
        .join("retained-fatal-init-handle");
    let (control, entered, release) = controlled(RetainedPrepareStage::NativeAcquired, false);
    let mut loader = start(&game, request(&root), Some(control));
    let entered = entered.recv_timeout(WAIT).is_ok();
    let unfinished = loader
        .handle
        .as_ref()
        .is_some_and(|handle| !handle.is_finished());
    let pending = loader.poll_retained_outcome().is_none();
    let excluded = SessionLock::acquire(&root).is_err();
    if !(entered && unfinished && pending && excluded) {
        let _ = release.send(());
        let outcome = harvest(&mut loader);
        close(&game, outcome);
        panic!("real unfinished retained initialization handle was not armed");
    }
    let mut output = std::io::stdout().lock();
    if writeln!(output, "HORIZON_RETAINED_INITIALIZATION_HANDLE_DROP_ARMED")
        .and_then(|_| output.flush())
        .is_err()
    {
        std::process::exit(91);
    }
    drop(output);
    drop(loader);
    // If this Drop returns, do not allow another lexical owner's destructor to
    // supply an unrelated abort that could masquerade as the expected guard.
    std::process::exit(91);
}

#[test]
#[ignore = "separate direct prebuilt process: marked expected fatal harvested Ready owner Drop; never run in ordinary battery"]
fn retained_prepare_harvested_ready_owner_drop_fatal() {
    use std::io::Write;
    let fixture = Fixture::new("retained-fatal-ready-owner");
    let game = fixture.game();
    let root = game
        .world_root
        .parent()
        .unwrap()
        .join("retained-fatal-ready-owner");
    let mut loader = start(&game, request(&root), None);
    let outcome = harvest(&mut loader);
    let ready = matches!(&outcome, PreparedOutcome::Ready(_));
    let armed = outcome.owner().armed
        && outcome.owner().streaming.is_some()
        && outcome.owner().world.is_some()
        && outcome.owner().session.is_some();
    let harvested = loader.handle.is_none();
    let excluded = SessionLock::acquire(&root).is_err();
    if !(ready && armed && harvested && excluded) {
        close(&game, outcome);
        panic!("actual harvested Ready preparation owner was not armed");
    }
    let mut output = std::io::stdout().lock();
    if writeln!(output, "HORIZON_RETAINED_PREPARATION_OWNER_DROP_ARMED")
        .and_then(|_| output.flush())
        .is_err()
    {
        std::process::exit(91);
    }
    drop(output);
    drop(outcome);
    std::process::exit(91);
}

#[test]
#[ignore = "owned prepared resources: actual native writer IO error, path repair and explicit same-owner Game-close Retry"]
fn retained_prepare_writer_io_error_repairs_original_native_payload_without_prepare_replay() {
    use horizon_client::{
        save::{RetainedPendingKind, RetainedRecordView},
        streaming::ChunkGenerator,
    };
    let fixture = Fixture::new("retained-writer-io-repair");
    let mut game = fixture.game();
    game.options.test_terrain = true;
    let root = game
        .world_root
        .parent()
        .unwrap()
        .join("retained-writer-io-repair");
    let entry = game.begin_retained_preparation(request(&root)).unwrap();
    let mut outcome = loop {
        if let Some(outcome) = game.poll_retained_prepare_outcome() {
            break outcome.unwrap();
        }
        std::thread::yield_now();
    };
    let owner_address = outcome.owner() as *const PrepareOwner as usize;
    let world_address = outcome.owner().world.as_deref().unwrap() as *const World as usize;
    let save = outcome.owner().save.as_ref().unwrap().clone();
    let identity_before = fs::read(root.join(host_identity::IDENTITY_FILE)).unwrap();
    let metadata_before = fs::read(root.join("level.dat")).unwrap();
    let clocks_before = outcome
        .owner()
        .world
        .as_deref()
        .map(|world| (world.game_time, world.day_time));
    let obstruction = save.root.join("entities/r.0.0.mca");
    // A directory at the exact native region-file path causes a real isolated
    // filesystem write error. It changes no global permissions or disk capacity.
    fs::create_dir_all(&obstruction).unwrap();
    let generator = Persistent {
        inner: Arc::new(TestTerrain::new(game.pack.assets.registry, game.res.biomes()).unwrap()),
        save: save.clone(),
        registry: game.pack.assets.clone(),
        biomes: game.res.biomes().clone(),
    };
    let handle = std::thread::spawn(move || {
        generator.worker();
        let proto = generator.proto(&mut (), 0, 0);
        let mut column = generator.finish(proto);
        column.entities.push(native_item());
        let column = Arc::new(column);
        let accepted = generator.save_finished(&column).is_ok();
        (column, accepted)
    });
    while !handle.is_finished() {
        std::thread::yield_now();
    }
    let (original_column, accepted) = handle.join().unwrap();
    outcome.begin_close(Instant::now() + WAIT);
    let blocked_error = loop {
        match outcome.poll_close(&game) {
            PreparationCloseStatus::Blocked(error) => break error,
            PreparationCloseStatus::Pending => std::thread::yield_now(),
            PreparationCloseStatus::Complete => break String::new(),
        }
    };
    let locked_while_failed = SessionLock::acquire(&root).is_err();
    let entry_still_owned = game
        .retained_entry_owner
        .as_ref()
        .is_some_and(|owner| owner.token == entry);
    let same_owner = outcome.owner() as *const PrepareOwner as usize == owner_address;
    let same_save = Arc::ptr_eq(outcome.owner().save.as_ref().unwrap(), &save);
    let failure_history = if let Some(writer) = outcome.owner().writer.as_ref() {
        loop {
            match writer.failure_count() {
                Ok(Some(count)) => break count,
                Ok(None) => std::thread::yield_now(),
                Err(_) => break 0,
            }
        }
    } else {
        0
    };
    let captured = captured_load(&game, save.clone());
    let original_payload_retained = captured.evidence().is_some_and(|evidence| {
        matches!(evidence.entities(), RetainedRecordView::Pending {
            kind: RetainedPendingKind::FinishedEntities, chunk: Some(column), ..
        } if Arc::ptr_eq(column, &original_column) && column.entities == original_column.entities && column.entities.len() == 1)
    });
    let repair = fs::remove_dir(&obstruction).is_ok();
    let retried = loop {
        let _ = outcome.poll_close(&game);
        if outcome.retry_close(Instant::now() + WAIT).is_ok() {
            break true;
        }
        if !outcome.owner().armed {
            break false;
        }
        std::thread::yield_now();
    };
    loop {
        match outcome.poll_close(&game) {
            PreparationCloseStatus::Complete => break,
            PreparationCloseStatus::Pending => std::thread::yield_now(),
            PreparationCloseStatus::Blocked(_) => {
                let _ = outcome.retry_close(Instant::now() + WAIT);
                std::thread::yield_now();
            }
        }
    }
    let same_world =
        outcome.owner().world.as_deref().unwrap() as *const World as usize == world_address;
    let same_clocks = outcome
        .owner()
        .world
        .as_deref()
        .map(|world| (world.game_time, world.day_time))
        == clocks_before;
    let joined_same_writer = outcome
        .owner()
        .writer_closed
        .as_ref()
        .is_some_and(|closed| Arc::ptr_eq(closed.save(), &save));
    let no_generation_replay = outcome
        .owner()
        .streaming
        .as_ref()
        .unwrap()
        .observe_native_work()
        .origins
        .iter()
        .all(|(_, counts)| counts.submitted == 0);
    let evidence_current = match (captured.evidence(), outcome.owner().writer_closed.as_ref()) {
        (Some(evidence), Some(closed)) => loop {
            match evidence.poll_closed_current(closed) {
                Ok(Some(current)) => break current,
                Ok(None) => std::thread::yield_now(),
                Err(_) => break false,
            }
        },
        _ => false,
    };
    let closed = outcome.finish_close().ok().unwrap();
    drop(captured);
    let healthy_cleanup = closed.original_error.is_none() && closed.read_refusals.is_empty();
    let acknowledged = game.acknowledge_retained_preparation_closed(closed).is_ok();
    let reread = SaveDir::new(save.root.clone()).load_entities(0, 0);
    assert!(
        accepted
            && !blocked_error.is_empty()
            && locked_while_failed
            && entry_still_owned
            && same_owner
            && same_save
            && failure_history > 0
    );
    assert!(
        original_payload_retained
            && repair
            && retried
            && same_world
            && same_clocks
            && joined_same_writer
            && no_generation_replay
            && evidence_current
    );
    assert!(healthy_cleanup && acknowledged);
    assert_eq!(reread, original_column.entities);
    assert_eq!(
        fs::read(root.join(host_identity::IDENTITY_FILE)).unwrap(),
        identity_before
    );
    assert_eq!(fs::read(root.join("level.dat")).unwrap(), metadata_before);
    assert!(SessionLock::acquire(&root).is_ok());
}
