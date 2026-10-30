//! One accounted used-streamer cell, held by an actual exclusive owner Task.
use super::super::tests::{loaded, source};
use super::*;
use crate::game::{
    integrated::{
        capture_completion_tests::{
            NativeFenceProof, NativeFenceRequest, dispatch_native_write_fence,
            dispatch_slot_capture, dispatch_slot_recapture,
        },
        transition_ownership_tests::Fixture,
    },
    slot_player::TransferOutcome,
};
use horizon_client::{
    chunk::Chunk,
    slot_transfer::Direction,
    streaming::{CapturePristineError, NativeWorkObservation, NativeWorkOrigin},
    world::{counter_restore::DomainSnapshot, slot_restore::SlotMetadataSnapshot},
};
use horizon_core::random::RandomSource;
use horizon_world::nbt::NamedTag;
use std::time::{Duration, Instant};

type Disk = BTreeMap<PathBuf, Option<Vec<u8>>>;
fn disk(root: &Path) -> Disk {
    fn visit(path: &Path, out: &mut Disk) {
        let metadata = fs::symlink_metadata(path).unwrap();
        assert!(!metadata.file_type().is_symlink());
        if metadata.is_dir() {
            out.insert(path.to_owned(), None);
            for entry in fs::read_dir(path).unwrap() {
                visit(&entry.unwrap().path(), out);
            }
        } else {
            assert!(metadata.is_file());
            out.insert(path.to_owned(), Some(fs::read(path).unwrap()));
        }
    }
    let mut out = BTreeMap::new();
    visit(root, &mut out);
    out
}

#[derive(Debug, PartialEq)]
struct Column {
    at: (i32, i32),
    pointer: usize,
    sections: Vec<(usize, Vec<u32>, Vec<u32>, Vec<u8>)>,
    saved_pointer: Option<usize>,
    saved: String,
    encoded: NamedTag,
    flags: (bool, bool, bool),
    entities: Vec<Tag>,
    names: Vec<((i32, i32, i32), String)>,
    postprocess: Vec<[i32; 3]>,
    references: Vec<(Arc<str>, (i32, i32))>,
}
fn column(game: &Game<'_>, chunk: &Arc<Chunk>) -> Column {
    assert!(chunk.block_entities.is_empty() && chunk.structure_starts.is_empty());
    Column {
        at: (chunk.x, chunk.z),
        pointer: Arc::as_ptr(chunk) as usize,
        sections: chunk
            .sections
            .iter()
            .map(|section| {
                (
                    Arc::as_ptr(section) as usize,
                    (0..4096).map(|i| section.get(i)).collect(),
                    (0..64).map(|i| section.biomes.get(i)).collect(),
                    (0..4096).map(|i| section.light(i)).collect(),
                )
            })
            .collect(),
        saved_pointer: chunk
            .saved
            .as_ref()
            .map(|saved| Arc::as_ptr(saved) as usize),
        saved: format!("{:?}", chunk.saved),
        encoded: horizon_client::save::encode(chunk, game.pack.assets.registry, game.res.biomes()),
        flags: (chunk.lit, chunk.light_loaded, chunk.fresh),
        entities: chunk.entities.clone(),
        names: chunk.block_names.clone(),
        postprocess: chunk.postprocess.clone(),
        references: chunk.structure_refs.clone(),
    }
}
#[derive(Debug, PartialEq)]
struct State {
    counter: CounterFrameV2,
    slot: Vec<u8>,
    domain: DomainSnapshot,
    metadata: SlotMetadataSnapshot,
    players: Vec<(Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>)>,
    level_player: Tag,
    clock: (i64, i64, u64, u64, u64, [i32; 4]),
    level_clock: (i64, i64),
    weather: [u32; 9],
    columns: Vec<Column>,
    inhabited: BTreeMap<(i32, i32), i64>,
    modified: BTreeSet<(i32, i32)>,
    dirty: BTreeMap<(i32, i32), u32>,
    dirty_light: BTreeSet<(i32, i32)>,
    light_edits: Vec<((i32, i32, i32), u32, u32)>,
    loaded: Vec<((i32, i32), Vec<Tag>)>,
    block_log: Option<Vec<(i32, i32, i32)>>,
    light_log: Option<BTreeMap<(i32, i32), u32>>,
    watched: Vec<((i32, i32, i32), u32, u32)>,
    events: Vec<horizon_world::level::LevelEvent>,
}
fn state(game: &Game<'_>) -> State {
    game.observe_slot_advance_entity_owners().unwrap();
    assert!(game.world.block_entities.is_empty() && game.world.chest_openers.is_empty());
    assert!(game.world.block_names.is_empty() && game.world.moving_pistons.is_empty());
    assert!(game.world.moving_piston_fallback.is_empty());
    assert!(
        game.pending.shulker.is_empty()
            && game.pending.banner.is_empty()
            && game.pending.names.is_empty()
            && game.pending.pot.is_empty()
            && game.pending.copied.is_empty()
    );
    assert!(
        game.spill.spawns.is_empty() && game.spill.removed.is_empty() && game.spill.orbs.is_empty()
    );
    let (counter, slot) = game.slot_advance_projection().unwrap();
    let mut random = game.world.random.clone();
    let mut columns: Vec<_> = game
        .world
        .chunks
        .values()
        .map(|chunk| column(game, chunk))
        .collect();
    columns.sort_by_key(|row| row.at);
    let weather = game.world.weather;
    State {
        counter,
        slot,
        domain: game.world.counter_domain_snapshot().unwrap(),
        metadata: game.world.persistent_world_slot_metadata().unwrap(),
        players: game
            .slot_player_images()
            .unwrap()
            .into_iter()
            .map(|row| {
                (
                    serde_json::to_vec(&row.descriptor).unwrap(),
                    row.nbt,
                    row.advancements,
                    row.stats,
                )
            })
            .collect(),
        level_player: game.level.player_tag(),
        clock: (
            game.world.game_time,
            game.world.day_time,
            game.ticks,
            game.world.revision,
            game.world.chunks.membership_revision(),
            [
                random.next_int(),
                random.next_int(),
                random.next_int(),
                random.next_int(),
            ],
        ),
        level_clock: (game.level.time, game.level.day_time),
        weather: [
            weather.clear_time as u32,
            weather.rain_time as u32,
            weather.thunder_time as u32,
            u32::from(weather.raining),
            u32::from(weather.thundering),
            weather.rain_level.0.to_bits(),
            weather.rain_level.1.to_bits(),
            weather.thunder_level.0.to_bits(),
            weather.thunder_level.1.to_bits(),
        ],
        columns,
        inhabited: game.world.inhabited.iter().map(|(p, t)| (*p, *t)).collect(),
        modified: game.world.modified.iter().copied().collect(),
        dirty: game.world.dirty_sections.columns().collect(),
        dirty_light: game.world.dirty_light.iter().copied().collect(),
        light_edits: game.world.light_edits.clone(),
        loaded: game.world.loaded_entities.clone(),
        block_log: game.world.block_log.clone(),
        light_log: game
            .world
            .light_log
            .as_ref()
            .map(|log| log.iter().map(|(p, v)| (*p, *v)).collect()),
        watched: game.world.watched_writes.clone(),
        events: game.level_events.clone(),
    }
}
fn conserved(observation: &NativeWorkObservation) {
    assert!(
        !observation.accounting_fault
            && !observation.receipt_mismatch
            && !observation.worker_initialization_fault
            && !observation.worker_exit_fault
            && !observation.result_disconnected
            && !observation.stopped
    );
    for (origin, counts) in observation.origins {
        assert_eq!(
            counts.submitted,
            counts.queued
                + counts.worker_held
                + counts.returned
                + counts.reconciling
                + counts.disposed
                + counts.unresolved,
            "{origin:?}"
        );
        assert_eq!(counts.unresolved, 0);
        assert_eq!(counts.storage_errors, 0);
    }
}

impl Game<'_> {
    pub(in crate::game) fn native_write_fence_fixture_operation(
        &mut self,
        request: CaptureRequest,
    ) -> std::result::Result<NativeFenceProof, String> {
        assert!(self.serving() && self.remote.is_none() && self.player.is_none());
        assert!(self.levels.parked.is_empty());
        assert_eq!(self.levels_to_tick(), vec![Dim::Overworld]);
        assert!(self.options.test_terrain);
        // The source/loaded constructors call make_streaming with test_terrain
        // and no flat settings: the compiled provider is Persistent<TestTerrain>.
        // Its worker/proto methods are in-memory, load reads SaveDir, and only
        // its inspected save callbacks enqueue. No locator, LOD or external
        // producer holds this directory; this is not a plug-in capability.
        assert!(
            horizon_client::save::world_gen_settings(&self.world_root)
                .and_then(|settings| {
                    horizon_client::flatgen::FlatTerrain::from_settings(
                        &settings,
                        self.pack.assets.registry,
                        self.res.biomes(),
                    )
                })
                .is_none()
        );
        assert!(!self.agent.hold_server && !self.clock.frozen() && !self.clock.paused());
        self.streaming.require_pristine_fixture_capture().unwrap();
        let ticks = self.ticks;
        self.tick_server(false).map_err(|error| error.to_string())?;
        assert_eq!(self.ticks, ticks + 1);
        assert_eq!(self.world.game_time, 127);
        let before = self.streaming.observe_native_work();
        conserved(&before);
        assert!(
            before.origins[NativeWorkOrigin::Generate as usize]
                .1
                .submitted
                > 0
        );
        let submitted: Vec<_> = before
            .origins
            .iter()
            .map(|(_, counts)| counts.submitted)
            .collect();
        let save = self.save.clone();
        let mut writer_polls = 0;
        self.streaming
            .native_write_fence_until(
                &mut self.world,
                Instant::now() + Duration::from_secs(30),
                || {
                    writer_polls += 1;
                    save.writes_finished()
                },
            )
            .map_err(|error| error.to_string())?;
        // The borrow has ended, but the actual Task still exclusively owns
        // Game. Reconciliation may have changed World before this baseline.
        assert_eq!(self.ticks, ticks + 1);
        let after = self.streaming.observe_native_work();
        conserved(&after);
        assert_eq!(
            after
                .origins
                .iter()
                .map(|(_, counts)| counts.submitted)
                .collect::<Vec<_>>(),
            submitted
        );
        assert_eq!(
            self.streaming.require_pristine_fixture_capture(),
            Err(CapturePristineError::PreviouslySubmitted)
        );
        let baseline = state(self);
        let source = disk(&self.world_root);
        let archive_root = request.archive_root.clone();
        let archive = disk(&archive_root);
        let error = self
            .capture_advanced_slot_fixture_epoch(request)
            .unwrap_err();
        assert_eq!(error.publication, Publication::NotPublished);
        assert_eq!(state(self), baseline);
        assert_eq!(disk(&self.world_root), source);
        assert_eq!(disk(&archive_root), archive);
        assert_eq!(self.streaming.observe_native_work(), after);
        assert_eq!(
            self.streaming.require_pristine_fixture_capture(),
            Err(CapturePristineError::PreviouslySubmitted)
        );
        Ok(NativeFenceProof {
            writer_polls,
            observation: after,
            refusal: format!("{error:?}"),
        })
    }
}

fn seed(game: &mut Game<'_>, binding: &NativeBinding) -> SlotKey {
    assert!(game.level.world_spawn.is_none());
    game.level.world_spawn = Some([0, 64, 0]);
    let owner = game
        .lan
        .as_mut()
        .unwrap()
        .hub
        .guests
        .iter_mut()
        .find(|guest| guest.uuid == 11)
        .unwrap();
    assert_eq!(owner.spawn, Some([4.5, 65.0, -6.5]));
    owner.inventory[0] = Some(ItemStack::new("minecraft:stone", 10));
    let slot = game
        .world
        .initialize_persistent_world_slot(binding, &target(game.dim).unwrap())
        .unwrap()
        .unwrap();
    let mut host = game.lan.take().unwrap();
    let moved = game
        .as_guest(&mut host, 11, |game| {
            game.transfer_world_slot_inventory_cell(&slot, 0, Direction::IntoMachine, 7)
        })
        .unwrap()
        .unwrap();
    assert!(matches!(moved, TransferOutcome::Committed(result) if result.moved == 7));
    game.lan = Some(host);
    share(game, &slot);
    slot
}
fn share(game: &Game<'_>, slot: &SlotKey) {
    let slot = game.world.read_world_slot(slot).unwrap();
    assert_eq!(slot.slot.revision(), 1);
    assert!(plain_stone(slot.slot.stack(), 7));
    let owner = game
        .lan
        .as_ref()
        .unwrap()
        .hub
        .guests
        .iter()
        .find(|guest| guest.uuid == 11)
        .unwrap();
    assert!(plain_stone(owner.inventory[0].as_ref(), 3));
}

#[test]
#[ignore = "isolated actual Game resources: exclusive Task native fence and refused capture"]
fn native_write_fence_actual_task_preserves_accounted_cell_and_refuses_capture_after_observer_timeout()
 {
    let fixture = Fixture::new("h08nf1-native-write-fence");
    let (original, _body, source_rx, identity, binding, original_counter, _held) = source(&fixture);
    let mut original = Box::new(original);
    let original_slot = seed(&mut original, &binding);
    let parent = original.world_root.parent().unwrap().to_owned();
    let archive_root = parent.join("h08nf1-archive");
    fs::create_dir(&archive_root).unwrap();
    let first = CaptureRequest {
        archive_root,
        epoch: 1,
        identity,
    };
    dispatch_slot_capture(&mut original, first.clone()).unwrap();
    let expected_world = original_counter.placement().world;
    let host = target(original.dim).unwrap();
    let middle_root = parent.join("h08nf1-middle");
    let first_recovery = recover_slot_fixture_epoch(
        &first.archive_root,
        &first.identity,
        &middle_root,
        &binding,
        original.res.items(),
        expected_world,
        &host,
    )
    .unwrap();
    let (middle, middle_rx) = loaded(&fixture, &middle_root);
    let mut middle = Box::new(middle);
    let (middle_counter, middle_slot) = middle
        .install_slot_epoch(
            &binding,
            expected_world,
            &first_recovery.counter,
            first_recovery.slot,
        )
        .unwrap();
    share(&middle, &middle_slot);
    let second = CaptureRequest { epoch: 2, ..first };
    dispatch_slot_recapture(&mut middle, second.clone()).unwrap();
    let working_root = parent.join("h08nf1-working");
    let second_recovery = recover_slot_fixture_epoch(
        &second.archive_root,
        &second.identity,
        &working_root,
        &binding,
        middle.res.items(),
        expected_world,
        &host,
    )
    .unwrap();
    let (game, working_rx) = loaded(&fixture, &working_root);
    let mut game = Box::new(game);
    let (counter, slot) = game
        .install_slot_epoch(
            &binding,
            expected_world,
            &second_recovery.counter,
            second_recovery.slot,
        )
        .unwrap();
    share(&game, &slot);
    assert_ne!(counter.session_nonce(), original_counter.session_nonce());
    assert_ne!(counter.session_nonce(), middle_counter.session_nonce());
    assert_ne!(slot, original_slot);
    assert_ne!(slot, middle_slot);
    let archive = disk(&second.archive_root);
    let original_disk = disk(&original.world_root);
    let middle_disk = disk(&middle.world_root);
    let original_state = original.slot_epoch_projection().unwrap();
    let middle_state = middle.slot_epoch_projection().unwrap();
    let result = dispatch_native_write_fence(
        &mut game,
        NativeFenceRequest {
            capture: CaptureRequest { epoch: 3, ..second },
            observe_timeout: true,
        },
    )
    .unwrap();
    assert!(result.writer_polls > 0);
    conserved(&result.observation);
    let record = game.world.read_persistent_counter(&counter).unwrap();
    assert_eq!((record.counter(), record.last_advanced()), (4, Some(127)));
    share(&game, &slot);
    share(&original, &original_slot);
    share(&middle, &middle_slot);
    assert!(
        game.world
            .read_persistent_counter(&original_counter)
            .is_err()
    );
    assert!(game.world.read_persistent_counter(&middle_counter).is_err());
    assert!(game.world.read_world_slot(&original_slot).is_err());
    assert!(game.world.read_world_slot(&middle_slot).is_err());
    assert_eq!(disk(&parent.join("h08nf1-archive")), archive);
    assert_eq!(disk(&original.world_root), original_disk);
    assert_eq!(disk(&middle.world_root), middle_disk);
    assert_eq!(original.slot_epoch_projection().unwrap(), original_state);
    assert_eq!(middle.slot_epoch_projection().unwrap(), middle_state);
    eprintln!(
        "NATIVE_WRITE_FENCE_TASK writer_polls={} native={:?} refusal={} counter=4 slot=7 player=3",
        result.writer_polls, result.observation, result.refusal
    );
    drop((source_rx, middle_rx, working_rx));
}
