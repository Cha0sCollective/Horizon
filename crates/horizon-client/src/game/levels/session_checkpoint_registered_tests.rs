//! Genuine captured materialization and actual Game controls. Run each resource
//! case in an isolated process; no compiler, environment mutation or fake IDs.
use super::*;
use crate::game::{Options, PackResources, Progress, Resources, load_resources};
use horizon_client::{
    mod_content::{ContentMod, resource_root},
    settings::Settings,
};
use horizon_world::registry::{BlockRegistry, BlockStateId};

struct Materialized {
    scratch: PathBuf,
    base: PathBuf,
    pair: PathBuf,
    pair_before: BTreeMap<PathBuf, [u8; 32]>,
    base_before: BTreeMap<PathBuf, [u8; 32]>,
    root: PathBuf,
    resources: Arc<Resources>,
    pack: PackResources,
    packages: crate::game::mod_packages::Packages,
    type_id: String,
    alias: String,
}
fn input_files(root: &Path) -> BTreeMap<PathBuf, [u8; 32]> {
    let mut out = BTreeMap::new();
    for name in [
        "facts.json",
        "registries.json",
        "block-behaviours.json",
        "gameplay-facts.json",
        "items.json",
        "model-states.json",
        "render-definitions.json",
        "occlusion.nbt",
        "cached-collision.nbt",
    ] {
        out.insert(
            name.into(),
            Sha256::digest(fs::read(root.join(name)).unwrap()).into(),
        );
    }
    out
}
fn package_files(root: &Path) -> BTreeMap<PathBuf, [u8; 32]> {
    fn visit(root: &Path, at: &Path, out: &mut BTreeMap<PathBuf, [u8; 32]>) {
        for entry in fs::read_dir(at).unwrap() {
            let path = entry.unwrap().path();
            let meta = fs::symlink_metadata(&path).unwrap();
            assert!(!meta.file_type().is_symlink());
            if meta.is_dir() {
                visit(root, &path, out);
            } else {
                assert!(meta.is_file());
                out.insert(
                    path.strip_prefix(root).unwrap().into(),
                    Sha256::digest(fs::read(&path).unwrap()).into(),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    visit(root, root, &mut out);
    out
}
fn materialize(
    packages: &crate::game::mod_packages::Packages,
    base: &Path,
    cache: &Path,
) -> PathBuf {
    let content = packages
        .catalog()
        .unwrap()
        .prepared()
        .values()
        .map(|p| ContentMod {
            id: p.bundle().content_owner().id.clone(),
            content: p.content().clone(),
        })
        .collect::<Vec<_>>();
    resource_root(base, &content, cache).unwrap()
}
impl Materialized {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let scratch = std::env::temp_dir().join(format!(
            "horizon-registered-binding-{}-{}-{label}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&scratch).unwrap();
        let base = PathBuf::from(
            std::env::var_os("HORIZON_TEST_RESOURCES").expect("pinned base resources required"),
        );
        let pair = PathBuf::from(
            std::env::var_os("HORIZON_COUNTER_PAIR_FIXTURE")
                .expect("prebuilt genuine schema1 pair required"),
        );
        let pair_before = package_files(&pair);
        let base_before = input_files(&base);
        let packages = crate::game::mod_packages::Packages::read(
            &BTreeSet::new(),
            std::slice::from_ref(&pair),
        )
        .unwrap();
        let selected = packages
            .catalog()
            .unwrap()
            .prepared()
            .values()
            .flat_map(|p| {
                p.content().blocks.iter().filter_map(|b| {
                    b.machine_type()
                        .map(|t| (t.to_owned(), b.id.clone(), b.item))
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(
            selected.len(),
            1,
            "fixture has exactly one captured machine binding"
        );
        assert!(selected[0].2, "genuine block item required");
        let root = materialize(&packages, &base, &scratch.join("materialized"));
        assert_ne!(root, base);
        let loaded = load_resources(
            root.clone(),
            &base,
            &Settings::default(),
            Instant::now(),
            false,
            &Progress::new(),
            &mut |_, _| {},
        )
        .unwrap();
        assert_eq!(loaded.root, root, "actual loader must not fall back");
        assert!(loaded.pack_error.is_none());
        assert!(loaded.rest_loader.is_none());
        let fixture = Self {
            scratch,
            base,
            pair,
            pair_before,
            base_before,
            root,
            resources: loaded.resources,
            pack: loaded.pack,
            packages,
            type_id: selected[0].0.clone(),
            alias: selected[0].1.clone(),
        };
        fixture.check_physical_resources();
        fixture
    }
    fn binding(&self) -> NativeBinding {
        let p = self
            .packages
            .catalog()
            .unwrap()
            .prepared()
            .values()
            .find(|p| p.content().machine_types().get(&self.type_id).is_some())
            .unwrap();
        NativeBinding::from_registered_prepared(p, &self.type_id, self.pack.assets.registry)
            .unwrap()
    }
    fn check_physical_resources(&self) {
        let binding = self.binding();
        let registry = self.pack.assets.registry;
        assert_eq!(binding.profile(), NativeBindingProfile::RegisteredCube);
        assert_eq!(binding.physical_descriptor().name, self.alias);
        assert!(binding.physical_descriptor().properties.is_empty());
        assert_eq!(
            registry.default_state(&self.alias),
            Some(binding.state_id())
        );
        assert_eq!(
            registry.id(binding.physical_descriptor()),
            Some(binding.state_id())
        );
        let iron = registry.default_state("minecraft:iron_block").unwrap();
        assert_ne!(binding.state_id(), iron);
        let base =
            BlockRegistry::from_facts_json(File::open(self.base.join("facts.json")).unwrap())
                .unwrap();
        for id in 0..base.len() {
            assert_eq!(
                registry.state(BlockStateId(id as u32)),
                base.state(BlockStateId(id as u32)),
                "vanilla ID {id}"
            );
        }
        let collision = self.resources.collision();
        assert!(std::ptr::eq(collision.registry(), registry));
        for state in [iron, binding.state_id()] {
            let shape = collision
                .cached_shapes()
                .cached_shape(state)
                .unwrap()
                .unwrap();
            assert_eq!(shape.discrete_bounds(), ([0, 0, 0], [1, 1, 1]));
            for axis in horizon_core::geometry::Axis::ALL {
                assert_eq!(shape.coordinates(axis), &[0.0, 1.0]);
            }
            assert!(shape.is_full(0, 0, 0));
        }
        let visibility = horizon_client::models::load_visibility(&self.root, registry).unwrap();
        for direction in horizon_core::geometry::Direction::ALL {
            assert_eq!(
                visibility
                    .occlusion_face(binding.state_id(), direction)
                    .unwrap()
                    .0,
                visibility.occlusion_face(iron, direction).unwrap().0
            );
            assert!(
                !visibility
                    .occlusion_face(binding.state_id(), direction)
                    .unwrap()
                    .1
                    .is_empty()
            );
        }
        let models = self
            .pack
            .render
            .model_catalog
            .collect(
                binding.state_id(),
                horizon_core::position::BlockPos::new(2, 120, 2),
            )
            .unwrap();
        assert!(actual_cube_model_source(
            &self.root,
            &self.pack,
            binding.state_id()
        ));
        assert_eq!(models.len(), 1);
        let (variant, model) = models[0];
        assert_ne!(variant.model, "minecraft:builtin/missing");
        let (namespace, path) = variant.model.split_once(':').unwrap();
        assert!(
            self.root
                .join(format!("assets/{namespace}/models/{path}.json"))
                .is_file()
        );
        assert!(model.cube.is_some());
        assert_eq!(model.faces.len(), 6);
        assert!(model.particle.is_some());
        for face in &model.faces {
            assert!(
                face.quad
                    .positions
                    .iter()
                    .flatten()
                    .all(|v| *v == 0.0 || *v == 1.0)
            );
            let name = self
                .pack
                .render
                .sprite_layers
                .iter()
                .find_map(|(name, layer)| (*layer == face.layer).then_some(name))
                .unwrap();
            assert!(
                !name.contains("missing"),
                "nonfallback texture required: {name}"
            );
        }
        assert!(self.resources.items().get(&self.alias).is_some());
        let item = self
            .resources
            .behaviours()
            .item_binding(&self.alias)
            .unwrap();
        assert_eq!(item.kind, horizon_world::level::ItemKind::Block);
        assert_eq!(
            item.block,
            self.resources.behaviours().block(binding.state_id())
        );
    }
    fn options(&self, role: ExecutionRole, root: &Path) -> Options {
        Options {
            resources: root.to_owned(),
            distance: 2,
            distance_override: Some(2),
            seed: 17,
            creative: false,
            test_terrain: true,
            frames: None,
            world_frames: None,
            screenshot_after: None,
            timeout: WAIT,
            report: None,
            world: None,
            look: None,
            time: None,
            weather: None,
            dimension: None,
            no_pause: false,
            control: None,
            spikes: None,
            offscreen: false,
            window_size: [1280, 720],
            server: None,
            player_name: Some("RegisteredFixture".into()),
            dedicated: Some(Vec::new()),
            benchmark: None,
            dev_mods: Vec::new(),
            mod_packages: self.packages.clone(),
            server_thread: false,
            execution_role: role,
        }
    }
    fn fresh(&self, owner: bool) -> Game<'_> {
        let role = if owner {
            ExecutionRole::IntegratedServer
        } else {
            ExecutionRole::DedicatedServer
        };
        let settings = Settings {
            render_distance: 2,
            simulation_distance: 2,
            ..Settings::default()
        };
        fresh_with_resources(
            &self.resources,
            self.pack.clone(),
            self.options(role, &self.root),
            settings,
        )
    }
    fn session(&self, label: &str, owner: bool) -> Session<'_> {
        session_with_game(
            self.fresh(owner),
            self.scratch.join(format!("world-{label}")),
            label,
            2,
            owner,
            true,
        )
    }
    fn recover<'a>(
        &'a self,
        source: &Session<'_>,
        label: &str,
        owner: bool,
    ) -> (
        Game<'a>,
        RegisteredRecoveryReceipt,
        Vec<mpsc::Receiver<Out>>,
    ) {
        let mut candidate = self.fresh(owner);
        let (host, receivers) = make_host(&mut candidate, owner, [10, 20], false);
        let (game, receipt) = candidate
            .recover_registered_session_checkpoint(request(source, self.scratch.join(label)), host)
            .unwrap();
        (game, receipt, receivers)
    }
}
impl Drop for Materialized {
    fn drop(&mut self) {
        assert_eq!(
            package_files(&self.pair),
            self.pair_before,
            "captured source inputs changed"
        );
        assert_eq!(
            input_files(&self.base),
            self.base_before,
            "shared base tables changed"
        );
        if !std::thread::panicking() {
            assert_eq!(self.scratch.parent(), Some(std::env::temp_dir().as_path()));
            fs::remove_dir_all(&self.scratch).unwrap();
        }
    }
}
fn active(receipt: &RegisteredRecoveryReceipt) -> (RuntimeCounterKey, SlotKey) {
    match &receipt.state {
        RegisteredRecoveryState::Active { counter, slot } => (counter.clone(), slot.clone()),
        RegisteredRecoveryState::Dormant => panic!("expected real active allocation"),
    }
}
fn actual_cube_model_source(root: &Path, pack: &PackResources, state: BlockStateId) -> bool {
    let Ok(models) = pack
        .render
        .model_catalog
        .collect(state, horizon_core::position::BlockPos::new(2, 120, 2))
    else {
        return false;
    };
    models.len() == 1
        && models.iter().all(|(variant, model)| {
            variant.model != "minecraft:builtin/missing"
                && variant.model.split_once(':').is_some_and(|(ns, path)| {
                    root.join(format!("assets/{ns}/models/{path}.json"))
                        .is_file()
                })
                && model.cube.is_some()
                && model.faces.len() == 6
                && model.faces.iter().all(|face| {
                    pack.render
                        .sprite_layers
                        .iter()
                        .any(|(name, layer)| *layer == face.layer && !name.contains("missing"))
                })
        })
}
fn assert_image_profile(s: &Session<'_>, epoch: u64, dormant: bool) {
    let image = published(s, epoch);
    let verified = verify_image(
        &s.game,
        &image,
        epoch,
        s.counter.placement().world,
        &s.target,
        &s.type_id,
    )
    .unwrap();
    assert_eq!(verified.manifest.format, 1);
    assert_eq!(verified.manifest.kind, KIND);
    assert_eq!(verified.counter.format, 2);
    assert_eq!(
        verified.counter.kind,
        "native-registered-counter-checkpoint"
    );
    assert_eq!(verified.counter.binding.template, "minecraft:iron_block");
    assert_eq!(
        verified.slot.binding_profile(),
        NativeBindingProfile::RegisteredCube
    );
    assert_eq!(
        matches!(verified.counter.placement, Placement::Dormant),
        dormant
    );
    assert_eq!(
        matches!(
            verified.slot.view().allocation,
            SlotFrameAllocation::Dormant
        ),
        dormant
    );
}

#[test]
#[ignore = "isolated actual resources and prebuilt pair: real materialized registries, CPU geometry and frozen inputs"]
fn registered_actual_materialization_and_frozen_registry_identity() {
    let f = Materialized::new("materialization");
    let independent =
        crate::game::mod_packages::Packages::read(&BTreeSet::new(), std::slice::from_ref(&f.pair))
            .unwrap();
    let second = materialize(&independent, &f.base, &f.scratch.join("second-capture"));
    let loaded = load_resources(
        second.clone(),
        &f.base,
        &Settings::default(),
        Instant::now(),
        false,
        &Progress::new(),
        &mut |_, _| {},
    )
    .unwrap();
    assert_eq!(loaded.root, second);
    assert!(loaded.pack_error.is_none());
    let a = f
        .packages
        .freeze_peer_inputs(
            Arc::new(horizon_client::lan::data::SyncData::load(&f.root).unwrap()),
            f.pack.assets.registry,
        )
        .unwrap()
        .unwrap();
    let b = independent
        .freeze_peer_inputs(
            Arc::new(horizon_client::lan::data::SyncData::load(&second).unwrap()),
            loaded.pack.assets.registry,
        )
        .unwrap()
        .unwrap();
    assert_eq!(a.wire_contract().compare(b.wire_contract()), Ok(()));
    assert_eq!(
        a.blocks().default_state(&f.alias),
        b.blocks().default_state(&f.alias)
    );
    let p = f
        .packages
        .catalog()
        .unwrap()
        .prepared()
        .values()
        .find(|p| p.content().machine_types().get(&f.type_id).is_some())
        .unwrap();
    let legacy = NativeBinding::from_prepared(p, &f.type_id, f.pack.assets.registry).unwrap();
    assert_eq!(legacy.profile(), NativeBindingProfile::LegacyIron);
    assert_ne!(legacy.state_id(), f.binding().state_id());
}

#[test]
#[ignore = "isolated actual resources and prebuilt pair: registered physical alias native save/recovery/recapture in both roles"]
fn registered_actual_active_native_roundtrip_both_roles() {
    let f = Materialized::new("active");
    for owner in [true, false] {
        let label = if owner { "integrated" } else { "dedicated" };
        let mut s = f.session(label, owner);
        let before = s
            .game
            .world
            .counter_checkpoint_frame(s.game.world.game_time)
            .unwrap();
        assert_eq!(before.observed_state.name, f.alias);
        assert!(
            s.game.checkpoint_session(&s.archive, 1, until()).is_err(),
            "legacy adapter must not accept registered frames"
        );
        s.game
            .checkpoint_registered_session(&s.archive, 1, until())
            .unwrap();
        assert_image_profile(&s, 1, false);
        let bytes = tree(&published(&s, 1), &[]).unwrap();
        let mut legacy_candidate = f.fresh(owner);
        let (legacy_host, _legacy_receivers) =
            make_host(&mut legacy_candidate, owner, [10, 20], false);
        let legacy_destination = f.scratch.join(format!("legacy-refused-{label}"));
        assert!(
            legacy_candidate
                .recover_session_checkpoint(request(&s, legacy_destination.clone()), legacy_host)
                .is_err()
        );
        assert!(!legacy_destination.exists());
        let (mut game, receipt, _receivers) = f.recover(&s, &format!("recovery-{label}"), owner);
        let (counter, slot) = active(&receipt);
        assert_eq!(receipt.epoch, 1);
        assert_ne!(counter, s.counter);
        assert_ne!(slot, s.slot);
        assert_eq!(
            game.world
                .counter_checkpoint_frame(game.world.game_time)
                .unwrap(),
            before
        );
        assert_eq!(total(&game, &slot), 14);
        assert!(game.world.read_persistent_counter(&s.counter).is_err());
        assert!(game.world.read_world_slot(&s.slot).is_err());
        game.checkpoint_registered_session(&s.archive, 2, until())
            .unwrap();
        let (third, third_receipt, _third_receivers) =
            f.recover(&s, &format!("third-{label}"), owner);
        assert_eq!(third_receipt.epoch, 2);
        assert_eq!(total(&third, &active(&third_receipt).1), 14);
        assert_eq!(tree(&published(&s, 1), &[]).unwrap(), bytes);
    }
    let mut legacy = session_with_game(
        f.fresh(true),
        f.scratch.join("legacy-same-registry"),
        "legacy-same-registry",
        2,
        true,
        false,
    );
    legacy
        .game
        .checkpoint_session(&legacy.archive, 1, until())
        .unwrap();
    let mut candidate = f.fresh(true);
    let (host, _receivers) = make_host(&mut candidate, true, [10, 20], false);
    let destination = f.scratch.join("registered-refuses-legacy");
    assert!(
        candidate
            .recover_registered_session_checkpoint(request(&legacy, destination.clone()), host)
            .is_err()
    );
    assert!(!destination.exists());
}

fn delivered<'a>(f: &'a Materialized, label: &str, owner: bool, replacement: &str) -> Session<'a> {
    let mut s = f.session(label, owner);
    let slot = s.slot.clone();
    transfer(&mut s.game, &slot, 10, Direction::IntoPlayer, 7);
    s.game.lan.as_mut().unwrap().hub.guests[0].inventory[1] = Some(session_items_rich());
    let mut host = s.game.lan.take().unwrap();
    let result = s
        .game
        .as_guest(&mut host, 10, |g| {
            g.transfer_world_slot_inventory_cell(&slot, 1, Direction::IntoMachine, 7)
        })
        .unwrap()
        .unwrap();
    assert!(matches!(result, TransferOutcome::Committed(row) if row.moved == 7));
    s.game.lan = Some(host);
    assert_eq!(session_items_total(&s.game, &slot), 28);
    let state = s
        .game
        .pack
        .assets
        .registry
        .default_state(replacement)
        .unwrap();
    s.game
        .world
        .set(POSITION[0], POSITION[1], POSITION[2], state)
        .unwrap();
    assert!(
        s.game
            .checkpoint_registered_session(&s.archive, 1, until())
            .is_err()
    );
    let original = s
        .game
        .world
        .world_slot_snapshot()
        .unwrap()
        .pending
        .unwrap()
        .allocation
        .slot
        .stack()
        .unwrap()
        .clone();
    assert!(matches!(
        s.game.deliver_pending_persistent_world_slot(&slot),
        Ok(crate::game::slot_retirement::Delivery::Delivered { .. })
    ));
    assert!(
        s.game
            .world
            .reconcile_persistent_world_slot()
            .unwrap()
            .is_none()
    );
    assert!(s.game.world.persistent_counter_key().unwrap().is_none());
    assert_eq!(s.game.dropped.len(), 1);
    retirement_raw_equal(&s.game.dropped[0].stack, &original);
    s.game.dropped[0].health = 4;
    s.game.dropped[0].age = 201;
    s.game.dropped[0].pickup_delay = 17;
    s.game.dropped[0].velocity = Vec3::new(0.125, -0.25, 0.0);
    s
}
fn all_roots_and_nested(game: &Game<'_>) -> u64 {
    let slot = game.world.world_slot_snapshot().unwrap();
    let machine = slot
        .active
        .as_ref()
        .or_else(|| slot.pending.as_ref().map(|p| &p.allocation))
        .and_then(|a| a.slot.stack())
        .map_or(0, session_items_stack_count);
    machine
        + game
            .dropped
            .iter()
            .map(|e| session_items_stack_count(&e.stack))
            .sum::<u64>()
        + game
            .lan
            .as_ref()
            .unwrap()
            .hub
            .guests
            .iter()
            .flat_map(|g| g.inventory.iter().flatten())
            .map(session_items_stack_count)
            .sum::<u64>()
}

#[test]
#[ignore = "isolated actual resources and prebuilt pair: registered dormant air/iron native recovery keeps genuine rich D item and history"]
fn registered_actual_dormant_retirement_and_readmission_both_roles() {
    let f = Materialized::new("dormant");
    for owner in [true, false] {
        for replacement in ["minecraft:air", "minecraft:iron_block"] {
            let label = format!("{owner}-{}", replacement.split_once(':').unwrap().1);
            let mut s = delivered(&f, &label, owner, replacement);
            assert_eq!(all_roots_and_nested(&s.game), 28);
            s.game
                .checkpoint_registered_session(&s.archive, 1, until())
                .unwrap();
            assert_image_profile(&s, 1, true);
            let projection = s.game.project_session_checkpoint_items().unwrap();
            let counter_frame = s
                .game
                .world
                .counter_checkpoint_frame(s.game.world.game_time)
                .unwrap();
            let metadata = s.game.world.persistent_world_slot_metadata().unwrap();
            assert_eq!(projection.rows.len(), 1);
            assert!(
                tree(&published(&s, 1), &[])
                    .unwrap()
                    .files
                    .iter()
                    .any(|m| m.path.contains("/entities/") && m.path.ends_with(".mca"))
            );
            let (mut game, receipt, _receivers) = f.recover(&s, &format!("recover-{label}"), owner);
            assert!(matches!(receipt.state, RegisteredRecoveryState::Dormant));
            assert_eq!(
                game.world
                    .counter_checkpoint_frame(game.world.game_time)
                    .unwrap(),
                counter_frame
            );
            assert_eq!(
                game.world.persistent_world_slot_metadata().unwrap(),
                metadata
            );
            assert_eq!(game.project_session_checkpoint_items().unwrap(), projection);
            assert_eq!(all_roots_and_nested(&game), 28);
            retirement_raw_equal(&game.dropped[0].stack, &s.game.dropped[0].stack);
            game.checkpoint_registered_session(&s.archive, 2, until())
                .unwrap();
            let (mut third, receipt2, _third_receivers) =
                f.recover(&s, &format!("third-{label}"), owner);
            assert!(matches!(receipt2.state, RegisteredRecoveryState::Dormant));
            third
                .world
                .set(
                    POSITION[0],
                    POSITION[1],
                    POSITION[2],
                    f.binding().state_id(),
                )
                .unwrap();
            let fresh_slot = third
                .world
                .reconcile_persistent_world_slot()
                .unwrap()
                .unwrap();
            let fresh_counter = third.world.persistent_counter_key().unwrap().unwrap();
            assert!(third.world.read_world_slot(&s.slot).is_err());
            assert!(third.world.read_persistent_counter(&s.counter).is_err());
            assert!(fresh_counter.placement().generation > s.counter.placement().generation);
            assert!(fresh_slot.incarnation() > s.slot.incarnation());
            assert!(
                third
                    .world
                    .read_world_slot(&fresh_slot)
                    .unwrap()
                    .slot
                    .stack()
                    .is_none()
            );
            assert_eq!(
                third.project_session_checkpoint_items().unwrap(),
                projection
            );
            third
                .checkpoint_registered_session(&s.archive, 3, until())
                .unwrap();
            let (fourth, receipt3, _fourth_receivers) =
                f.recover(&s, &format!("fourth-{label}"), owner);
            assert_eq!(all_roots_and_nested(&fourth), 28);
            assert_eq!(
                fourth.project_session_checkpoint_items().unwrap(),
                projection
            );
            assert!(matches!(
                receipt3.state,
                RegisteredRecoveryState::Active { .. }
            ));
        }
    }
}

#[test]
#[ignore = "isolated actual resources and prebuilt pair: unknown/mixed kinds and native alias palette refuse before tolerant opening"]
fn registered_actual_complete_image_profile_and_palette_refusals() {
    let f = Materialized::new("refusals");
    let mut s = f.session("refusals", true);
    s.game
        .checkpoint_registered_session(&s.archive, 1, until())
        .unwrap();
    let image = published(&s, 1);
    let inventory = tree(&image, &[]).unwrap();
    for mode in 0..9 {
        let archive = f.scratch.join(format!("bad-archive-{mode}"));
        fs::create_dir(&archive).unwrap();
        let bad = archive.join("epoch-00000000000000000002");
        copy_tree(&image, &bad, &inventory, until()).unwrap();
        if mode < 6 {
            let mut counter: Value =
                serde_json::from_slice(&fs::read(bad.join(COUNTER)).unwrap()).unwrap();
            match mode {
                0 => counter["kind"] = "native-registered-counter-checkpoint-vnext".into(),
                1 => counter["kind"] = "native-counter-checkpoint".into(),
                2 => counter["package_identity"] = "ab".repeat(32).into(),
                3 => counter["observed_state"]["name"] = "minecraft:iron_block".into(),
                4 => counter["binding"]["properties"] = serde_json::json!({"axis":"x"}),
                _ => counter["save_schema"] = 2.into(),
            }
            fs::write(bad.join(COUNTER), serde_json::to_vec(&counter).unwrap()).unwrap();
        } else if mode < 8 {
            let mut tag = raw_nbt(&fs::read(bad.join(SLOT)).unwrap()).unwrap();
            *tag.tag.get_mut("kind").unwrap() = Tag::String(
                if mode == 6 {
                    "native-slot-checkpoint"
                } else {
                    "native-registered-slot-checkpoint-vnext"
                }
                .into(),
            );
            fs::write(bad.join(SLOT), nbt_bytes(&tag.tag).unwrap()).unwrap();
        } else {
            let native = bad.join("world/dimensions/minecraft/overworld/region/r.0.0.mca");
            let mut region = RegionFile::open_writable(&native, false).unwrap();
            let at = ChunkPos::new(0, 0);
            let record = region.read(at).unwrap().unwrap();
            let mut named = raw_nbt(
                &record
                    .compression
                    .decode(&record.compressed, STRUCTURED_LIMIT)
                    .unwrap(),
            )
            .unwrap();
            fn rename(tag: &mut Tag, alias: &str) {
                match tag {
                    Tag::String(s) if s.to_string().ok().as_deref() == Some(alias) => {
                        *s = "missing:unregistered_alias".into()
                    }
                    Tag::List(rows) => {
                        for row in rows {
                            rename(row, alias);
                        }
                    }
                    Tag::Compound(rows) => {
                        for row in rows.values_mut() {
                            rename(row, alias);
                        }
                    }
                    _ => {}
                }
            }
            rename(&mut named.tag, &f.alias);
            let record = horizon_world::region::ChunkRecord::from_nbt(
                &named,
                Compression::Zlib,
                0,
                limits(),
            )
            .unwrap();
            region.write(at, &record).unwrap();
        }
        reseal(&bad, 2);
        let mut candidate = f.fresh(true);
        let (host, _receivers) = make_host(&mut candidate, true, [10, 20], false);
        let destination = f.scratch.join(format!("refused-{mode}"));
        let mut recovery = request(&s, destination.clone());
        recovery.archive_root = archive;
        assert!(
            candidate
                .recover_registered_session_checkpoint(recovery, host)
                .is_err(),
            "mode {mode}"
        );
        assert!(
            !destination.exists(),
            "refuse before native copy/open mode {mode}"
        );
        assert_eq!(tree(&image, &[]).unwrap(), inventory);
    }
}

#[test]
#[ignore = "isolated actual resources and prebuilt pair: Copied/Promoted retry retains registered dormant D ownership"]
fn registered_actual_copy_and_promotion_retry_keep_dropped_identity() {
    let f = Materialized::new("retry");
    let mut s = delivered(&f, "retry", true, "minecraft:air");
    s.game
        .checkpoint_registered_session(&s.archive, 1, until())
        .unwrap();
    let projection = s.game.project_session_checkpoint_items().unwrap();
    let first = tree(&published(&s, 1), &[]).unwrap();
    for (epoch, fault, publication) in [
        (2, CheckpointTestFault::Copied, Publication::NotPublished),
        (4, CheckpointTestFault::Promoted, Publication::Uncertain),
    ] {
        let error = s
            .game
            .checkpoint_registered_session_with_test_fault(&s.archive, epoch, until(), fault)
            .unwrap_err();
        assert_eq!(error.publication, publication);
        assert_eq!(error.item_identities_assigned, 0);
        assert_eq!(
            s.game.project_session_checkpoint_items().unwrap(),
            projection
        );
        assert_eq!(tree(&published(&s, 1), &[]).unwrap(), first);
        s.game
            .checkpoint_registered_session(&s.archive, epoch + 1, until())
            .unwrap();
        let (game, receipt, _receivers) = f.recover(&s, &format!("retry-{epoch}"), true);
        assert_eq!(receipt.epoch, epoch + 1);
        assert!(matches!(receipt.state, RegisteredRecoveryState::Dormant));
        assert_eq!(game.project_session_checkpoint_items().unwrap(), projection);
        assert_eq!(all_roots_and_nested(&game), 28);
    }
}

#[cfg(all(feature = "native-writer-test-control", debug_assertions))]
#[test]
#[ignore = "isolated actual resources and prebuilt pair: actual native writer deadline and registered retry"]
fn registered_actual_native_writer_refusal_and_retry_keep_original_item() {
    let f = Materialized::new("writer");
    let mut s = delivered(&f, "writer", true, "minecraft:air");
    let original = s.game.dropped[0].stack.clone();
    let mut gate = s
        .game
        .save
        .as_ref()
        .unwrap()
        .arm_native_writer_test_control(horizon_client::save::NativeWriterTestAction::Block)
        .unwrap();
    let error = s
        .game
        .checkpoint_registered_session(&s.archive, 1, Instant::now() + Duration::from_secs(10))
        .unwrap_err();
    gate.wait_entered(WAIT).unwrap();
    assert_eq!(
        (error.publication, error.item_identities_assigned),
        (Publication::NotPublished, 1)
    );
    retirement_raw_equal(&s.game.dropped[0].stack, &original);
    let projection = s.game.project_session_checkpoint_items().unwrap();
    gate.trigger().unwrap();
    drop(gate);
    native_fence(&mut s.game, until());
    s.game
        .checkpoint_registered_session(&s.archive, 2, until())
        .unwrap();
    let (game, receipt, _receivers) = f.recover(&s, "writer-recovery", true);
    assert!(matches!(receipt.state, RegisteredRecoveryState::Dormant));
    assert_eq!(game.project_session_checkpoint_items().unwrap(), projection);
}

fn copy_owned(source: &Path, target: &Path) {
    fs::create_dir(target).unwrap();
    for row in fs::read_dir(source).unwrap() {
        let row = row.unwrap();
        let from = row.path();
        let to = target.join(row.file_name());
        if from.is_dir() {
            copy_owned(&from, &to);
        } else {
            fs::copy(from, to).unwrap();
        }
    }
}

#[test]
#[ignore = "isolated actual resources and prebuilt pair: damaged owned overlay exercises actual loader fallback and refuses registered bootstrap"]
fn registered_actual_loader_fallback_cannot_supply_registered_authority() {
    let f = Materialized::new("fallback");
    let damaged = f.scratch.join("damaged-owned-overlay");
    copy_owned(&f.root, &damaged);
    fs::write(damaged.join("facts.json"), b"invalid owned facts").unwrap();
    let loaded = load_resources(
        damaged,
        &f.base,
        &Settings::default(),
        Instant::now(),
        false,
        &Progress::new(),
        &mut |_, _| {},
    )
    .unwrap();
    assert_eq!(loaded.root, f.base);
    assert!(loaded.pack_error.is_some());
    assert!(
        loaded
            .pack
            .assets
            .registry
            .default_state(&f.alias)
            .is_none()
    );
    let options = f.options(ExecutionRole::IntegratedServer, &f.base);
    let mut game =
        fresh_with_resources(&loaded.resources, loaded.pack, options, Settings::default());
    open_native(&mut game, &f.scratch.join("fallback-world"));
    residency(&mut game, 2, (0, 0));
    let iron = game
        .pack
        .assets
        .registry
        .default_state("minecraft:iron_block")
        .unwrap();
    game.world
        .set(POSITION[0], POSITION[1], POSITION[2], iron)
        .unwrap();
    let before = tree(&game.world_root, &["session.lock"]).unwrap();
    assert!(
        game.bootstrap_registered_host_counter(&f.type_id, POSITION)
            .is_err()
    );
    assert!(game.world.counter_domain_snapshot().is_none());
    assert_eq!(tree(&game.world_root, &["session.lock"]).unwrap(), before);
    // Genuine legacy bootstrap still succeeds, proving registered refusal did
    // not spend the original native grant or replace its bound World.
    game.bootstrap_host_counter(&f.type_id, POSITION).unwrap();
}

#[test]
#[ignore = "isolated actual resources and prebuilt pair: native collision load and actual missing-model fallback fail resource fixture qualification"]
fn registered_actual_collision_and_model_sources_are_not_registry_strings() {
    let f = Materialized::new("resource-refusals");
    let collision = f.scratch.join("bad-owned-collision");
    copy_owned(&f.root, &collision);
    fs::write(
        collision.join("cached-collision.nbt"),
        b"invalid owned collision table",
    )
    .unwrap();
    assert!(
        load_resources(
            collision,
            &f.base,
            &Settings::default(),
            Instant::now(),
            false,
            &Progress::new(),
            &mut |_, _| {}
        )
        .is_err()
    );
    let missing = f.scratch.join("missing-owned-model");
    copy_owned(&f.root, &missing);
    let state = f.binding().state_id();
    let models = f
        .pack
        .render
        .model_catalog
        .collect(state, horizon_core::position::BlockPos::new(2, 120, 2))
        .unwrap();
    let (ns, path) = models[0].0.model.split_once(':').unwrap();
    fs::remove_file(missing.join(format!("assets/{ns}/models/{path}.json"))).unwrap();
    match load_resources(
        missing.clone(),
        &f.base,
        &Settings::default(),
        Instant::now(),
        false,
        &Progress::new(),
        &mut |_, _| {},
    ) {
        Err(_) => {}
        Ok(loaded) => {
            let registered = loaded.pack.assets.registry.default_state(&f.alias);
            assert!(
                loaded.root != missing
                    || loaded.pack_error.is_some()
                    || registered.is_none_or(|state| !actual_cube_model_source(
                        &missing,
                        &loaded.pack,
                        state
                    ))
            );
        }
    }
    // The genuine-resource fixture refuses either failure before constructing
    // any Game/grant. NativeBinding is a registry check, not model attestation.
}

#[test]
#[ignore = "isolated actual resources and prebuilt pair: actual registered bootstrap refusals preserve native grant and source files"]
fn registered_actual_bootstrap_refusals_leave_real_grant_unused() {
    let f = Materialized::new("bootstrap-refusals");
    for mode in 0..4 {
        let changed = f.scratch.join(format!("changed-captured-source-{mode}"));
        copy_owned(&f.pair, &changed);
        let (ns, path) = if mode == 3 {
            f.type_id.split_once(':').unwrap()
        } else {
            f.alias.split_once(':').unwrap()
        };
        let declaration = changed.join(format!(
            "data/{ns}/horizon/{}/{path}.json",
            if mode == 3 { "machine" } else { "block" }
        ));
        let mut value: Value = serde_json::from_slice(&fs::read(&declaration).unwrap()).unwrap();
        match mode {
            0 => value["template"] = "minecraft:stone".into(),
            1 => value["machine-type"] = "other:counter".into(),
            2 => value["properties"] = serde_json::json!({"light":15}),
            _ => value["save-schema"] = 2.into(),
        }
        fs::write(&declaration, serde_json::to_vec(&value).unwrap()).unwrap();
        if let Ok(packages) =
            crate::game::mod_packages::Packages::read(&BTreeSet::new(), &[changed])
        {
            let prepared = packages
                .catalog()
                .unwrap()
                .prepared()
                .values()
                .find(|p| p.content().machine_types().get(&f.type_id).is_some())
                .unwrap();
            assert!(
                NativeBinding::from_registered_prepared(
                    prepared,
                    &f.type_id,
                    f.pack.assets.registry
                )
                .is_err(),
                "mode {mode}"
            );
        }
    }
    let mut game = f.fresh(true);
    open_native(&mut game, &f.scratch.join("bootstrap-world"));
    residency(&mut game, 2, (0, 0));
    let at = f.binding().state_id();
    game.world
        .set(POSITION[0], POSITION[1], POSITION[2], at)
        .unwrap();
    let source = tree(&game.world_root, &["session.lock"]).unwrap();
    for (type_id, position) in [
        ("missing:counter", POSITION),
        (f.type_id.as_str(), [2, -65, 2]),
        (f.type_id.as_str(), [4096, 120, 4096]),
    ] {
        assert!(
            game.bootstrap_registered_host_counter(type_id, position)
                .is_err()
        );
        assert!(game.world.counter_domain_snapshot().is_none());
        assert!(game.world.world_slot_snapshot().is_none());
        assert_eq!(tree(&game.world_root, &["session.lock"]).unwrap(), source);
    }
    let iron = game
        .pack
        .assets
        .registry
        .default_state("minecraft:iron_block")
        .unwrap();
    game.world
        .set(POSITION[0], POSITION[1], POSITION[2], iron)
        .unwrap();
    assert!(
        game.bootstrap_registered_host_counter(&f.type_id, POSITION)
            .is_err()
    );
    assert!(game.world.counter_domain_snapshot().is_none());
    game.world
        .set(POSITION[0], POSITION[1], POSITION[2], at)
        .unwrap();
    let key = game
        .bootstrap_registered_host_counter(&f.type_id, POSITION)
        .unwrap();
    assert_eq!(
        game.world.read_persistent_counter(&key).unwrap().counter(),
        0
    );
    assert!(
        game.bootstrap_registered_host_counter(&f.type_id, POSITION)
            .is_err()
    );
}

#[cfg(feature = "mods")]
#[path = "../persistent_guest_counter_tests.rs"]
mod persistent_guest;

#[cfg(feature = "mods")]
mod placement_tests {
    use super::*;
    use horizon_client::world::counter_restore::GuestRuntimeState;

    fn use_alias(game: &mut Game<'_>, position: [i32; 3], alias: &str) -> (u32, i32) {
        let mut host = game.lan.take().unwrap();
        let result = game
            .as_guest(&mut host, 10, |game| {
                game.selected = 0;
                let before = game.stats.get("used", alias);
                let hit = crate::game::Hit {
                    pos: [position[0], position[1] - 1, position[2]],
                    face: [0, 1, 0],
                    point: [
                        f64::from(position[0]) + 0.5,
                        f64::from(position[1]),
                        f64::from(position[2]) + 0.5,
                    ],
                };
                game.use_hand(Some(&hit)).unwrap();
                (
                    game.inventory[0].as_ref().unwrap().count,
                    game.stats.get("used", alias) - before,
                )
            })
            .unwrap();
        game.lan = Some(host);
        result
    }

    #[test]
    #[ignore = "isolated real resources/captured pair: native BlockItem, original grant, actual Guest callback and retirement both server roles"]
    fn registered_guest_actual_placement_refusal_callback_custody_and_reload_both_roles() {
        let f = Materialized::new("placement");
        for owner in [true, false] {
            let mut game = f.fresh(owner);
            open_native(
                &mut game,
                &f.scratch.join(format!("placement-world-{owner}")),
            );
            residency(&mut game, 2, (0, 0));
            let state = f.binding().state_id();
            let stone = game
                .pack
                .assets
                .registry
                .default_state("minecraft:stone")
                .unwrap();
            assert_eq!(
                game.world.block(POSITION[0], POSITION[1], POSITION[2]),
                game.world.air
            );
            game.arm_registered_guest_placement(&f.type_id).unwrap();
            game.arm_registered_guest_placement(&f.type_id).unwrap();
            assert!(game.world.counter_domain_snapshot().is_none());
            assert!(game.world.world_slot_snapshot().is_none());
            game.world
                .set(POSITION[0], POSITION[1] - 1, POSITION[2], stone)
                .unwrap();
            let second = [POSITION[0] + 2, POSITION[1], POSITION[2]];
            game.world
                .set(second[0], second[1] - 1, second[2], stone)
                .unwrap();
            let (mut host, _receivers) = make_host(&mut game, owner, [10, 20], true);
            let actor = &mut host.hub.guests[0];
            actor.position = [2.5, 120.0, 0.5];
            actor.motion.as_mut().unwrap().position = Vec3::new(2.5, 120.0, 0.5);
            actor.motion.as_mut().unwrap().velocity = Vec3::new(0.0, 0.0, 0.0);
            actor.game_mode = if owner { 0 } else { 1 };
            actor.inventory[0] = Some(ItemStack::new(f.alias.as_str(), 3));
            actor.inventory[1] = Some(session_items_rich());
            game.lan = Some(host);

            // The original native session is kept alive, not dropped. Missing
            // host context must fail through actual BlockItem/set_raw without
            // successful usage statistics, held consumption or an unowned alias.
            let session = game.session_lock.take().unwrap();
            assert_eq!(use_alias(&mut game, POSITION, &f.alias), (3, 0));
            assert_eq!(
                game.world.block(POSITION[0], POSITION[1], POSITION[2]),
                game.world.air
            );
            assert!(game.world.counter_domain_snapshot().is_none());
            assert!(game.world.world_slot_snapshot().is_none());
            game.session_lock = Some(session);
            let retained_count = if owner { 2 } else { 3 };
            assert_eq!(
                use_alias(&mut game, POSITION, &f.alias),
                (retained_count, 1)
            );
            assert_eq!(
                game.world.block(POSITION[0], POSITION[1], POSITION[2]),
                state
            );
            let GuestRuntimeState::ReadyActive { counter, slot } =
                game.world.guest_runtime_state().unwrap()
            else {
                panic!("actual paired Guest placement required");
            };
            assert_eq!(
                game.world
                    .read_persistent_counter(&counter)
                    .unwrap()
                    .counter(),
                0
            );
            assert!(
                game.world
                    .read_world_slot(&slot)
                    .unwrap()
                    .slot
                    .stack()
                    .is_none()
            );
            let before = game.world.counter_trial_snapshot().unwrap();
            assert_eq!(use_alias(&mut game, second, &f.alias), (retained_count, 0));
            assert_eq!(
                game.world.block(second[0], second[1], second[2]),
                game.world.air
            );
            assert_eq!(game.world.counter_trial_snapshot().unwrap(), before);
            assert_eq!(game.world.counter_domain_snapshot().unwrap().high_water, 1);
            assert_eq!(
                game.world
                    .persistent_world_slot_metadata()
                    .unwrap()
                    .high_water,
                1
            );

            game.tickets.update(Some(Sources {
                players: vec![(0, 0)],
                forced: Vec::new(),
                view_distance: 2,
                simulation_distance: 2,
            }));
            let calls = crate::game::counter_control::guest_evaluation_calls();
            game.world.game_time += 1;
            game.tick_counter_trial(true);
            assert_eq!(
                game.world
                    .read_persistent_counter(&counter)
                    .unwrap()
                    .counter(),
                1
            );
            assert_eq!(
                crate::game::counter_control::guest_evaluation_calls(),
                calls + 1
            );
            game.tick_counter_trial(true);
            assert_eq!(
                crate::game::counter_control::guest_evaluation_calls(),
                calls + 1
            );

            // Explicit existing H06d cell transfer is fixture setup, not an
            // ordinary authorized ingress/admission or new client-wire claim.
            let mut host = game.lan.take().unwrap();
            let transferred = game
                .as_guest(&mut host, 10, |game| {
                    game.transfer_world_slot_inventory_cell(&slot, 1, Direction::IntoMachine, 3)
                })
                .unwrap()
                .unwrap();
            assert!(matches!(transferred, TransferOutcome::Committed(row) if row.moved==3));
            game.lan = Some(host);
            let original = game
                .world
                .read_world_slot(&slot)
                .unwrap()
                .slot
                .stack()
                .unwrap()
                .clone();
            let mut expected = session_items_rich();
            expected.count = 3;
            retirement_raw_equal(&original, &expected);
            // This is the native resident mutation seam. Ordinary destruction
            // and its physical block drops remain separate native gameplay.
            game.world
                .set(POSITION[0], POSITION[1], POSITION[2], game.world.air)
                .unwrap();
            assert!(game.world.world_slot_snapshot().unwrap().pending.is_some());
            assert_eq!(
                use_alias(&mut game, POSITION, &f.alias),
                (retained_count, 0)
            );
            let retired = game
                .world
                .counter_trial_snapshot()
                .unwrap()
                .last_retirement();
            assert!(
                game.world
                    .counter_trial_snapshot()
                    .unwrap()
                    .record()
                    .is_none()
            );
            let old_drops = game.dropped.len();
            let calls = crate::game::counter_control::guest_evaluation_calls();
            game.service_registered_guest_lifecycle().unwrap(); // explicit paused custody service
            assert_eq!(
                crate::game::counter_control::guest_evaluation_calls(),
                calls
            );
            assert_eq!(game.dropped.len(), old_drops + 1);
            retirement_raw_equal(&game.dropped.last().unwrap().stack, &expected);
            let next_entity = game.next_entity;
            game.service_registered_guest_lifecycle().unwrap();
            assert_eq!(game.next_entity, next_entity);
            assert_eq!(game.dropped.len(), old_drops + 1);
            assert!(game.world.world_slot_snapshot().unwrap().pending.is_none());
            assert_eq!(
                use_alias(&mut game, POSITION, &f.alias),
                (if owner { 1 } else { 3 }, 1)
            );
            let GuestRuntimeState::ReadyActive {
                counter: next_counter,
                slot: next_slot,
            } = game.world.guest_runtime_state().unwrap()
            else {
                panic!("actual paired readmission required");
            };
            assert_ne!(counter, next_counter);
            assert_ne!(slot, next_slot);
            assert_eq!(game.world.counter_domain_snapshot().unwrap().high_water, 2);
            assert_eq!(
                game.world
                    .persistent_world_slot_metadata()
                    .unwrap()
                    .high_water,
                2
            );
            assert_eq!(
                game.world
                    .counter_trial_snapshot()
                    .unwrap()
                    .last_retirement(),
                retired
            );
            assert!(game.world.read_persistent_counter(&counter).is_err());
            assert!(game.world.read_world_slot(&slot).is_err());

            let before = game.world.read_persistent_counter(&next_counter).unwrap();
            residency(&mut game, 0, (80, 80));
            game.world.game_time += 10;
            game.tick_counter_trial(true);
            let unloaded = game
                .world
                .counter_trial_snapshot()
                .unwrap()
                .record()
                .unwrap();
            assert_eq!(unloaded.counter(), before.counter());
            assert!(!unloaded.resident());
            residency(&mut game, 2, (0, 0));
            game.tick_counter_trial(true);
            assert_eq!(
                game.world
                    .read_persistent_counter(&next_counter)
                    .unwrap()
                    .counter(),
                before.counter() + 1
            );
            assert_eq!(game.world.counter_domain_snapshot().unwrap().high_water, 2);
            assert_eq!(
                game.world
                    .persistent_world_slot_metadata()
                    .unwrap()
                    .high_water,
                2
            );
            retirement_raw_equal(&game.dropped.last().unwrap().stack, &expected);
            // Matching native reload and existing coherent Guest publication use
            // this actual placed allocation, original participants and D item.
            game.save_game_with(false).unwrap();
            game.session_fence(until()).unwrap();
            let archive = f.scratch.join(format!("placement-archive-{owner}"));
            fs::create_dir(&archive).unwrap();
            game.checkpoint_registered_guest_session(&archive, 1, until())
                .unwrap();
            expected.count = 4;
            retirement_raw_equal(
                game.lan.as_ref().unwrap().hub.guests[0].inventory[1]
                    .as_ref()
                    .unwrap(),
                &expected,
            );
            eprintln!("REGISTERED_PLACEMENT_NATIVE_ROLE_PASS={owner}");
        }
    }
}

#[cfg(all(
    feature = "mods",
    feature = "native-writer-test-control",
    debug_assertions
))]
mod local_tests {
    use super::*;
    use crate::game::{
        GameCpuResources, InitialGameWorld, UiResources,
        paired_close::{CloseOwner, CloseProfile, CloseStatus},
        world_loading::{OrdinaryPreparation, PreparationCloseStatus, RetainedEntryIdentity},
    };

    fn local_game<'a>(
        f: &'a Materialized,
        startup: &Arc<crate::game::mod_packages::FrozenHostInputs>,
        tree: &Arc<horizon_client::advancements::Tree>,
        entry: &RetainedEntryIdentity,
        request: Option<WorldRequest>,
    ) -> Box<Game<'a>> {
        let mut options = Options::read_from(vec![
            "--resources".into(),
            f.root.to_string_lossy().into_owned(),
            "--in-frame".into(),
            "--test-terrain".into(),
            "--creative".into(),
            "--name".into(),
            "LocalFixture".into(),
        ])
        .unwrap()
        .unwrap()
        .options;
        options.mod_packages = f.packages.clone();
        assert!(options.execution_role.is_client() && options.dedicated.is_none());
        let mut settings = Settings {
            render_distance: 2,
            simulation_distance: 2,
            ..Default::default()
        };
        settings.horizon.lod_distance = 0;
        settings.controller.enabled = false;
        let activated = startup
            .activate(ExecutionRole::Client, false, false, &settings)
            .unwrap_or_else(|refused| {
                panic!("actual LOCAL runtime activation: {}", refused.reason)
            });
        let ordinary = request.is_some();
        let mut cpu = GameCpuResources::load(&f.root).unwrap();
        crate::game::prepare_ordinary_creative(
            &mut cpu,
            &f.resources,
            &f.packages,
            &f.pack,
            startup,
        )
        .unwrap();
        let initial = if let Some(request) = request {
            let preparation = OrdinaryPreparation::prepare(
                request,
                &f.resources,
                &f.pack,
                &options,
                &settings,
                entry,
                startup.clone(),
            );
            let prepared = preparation
                .into_prepared(entry)
                .unwrap_or_else(|_| panic!("actual LOCAL preparation refused"));
            InitialGameWorld::Ordinary(Box::new(prepared))
        } else {
            InitialGameWorld::OrdinaryRecoverySeed(startup.clone())
        };
        let mut game = Box::new(
            Game::new_with_world(
                &f.resources,
                f.pack.clone(),
                UiResources::load(&f.root, &f.base).unwrap(),
                options,
                settings,
                horizon_client::audio::Audio::silent(&f.root),
                Some(cpu),
                initial,
            )
            .unwrap(),
        );
        game.adopt_activated_startup(activated)
            .unwrap_or_else(|refused| panic!("original LOCAL runtime transfer: {}", refused.1));
        game.adv.tree = tree.clone();
        if ordinary {
            game.title = None;
            game.screen = crate::game::Screen::Playing;
            game.restore_ordinary_native_player();
            game.load_world_state();
            game.load_stats();
            game.load_advancements();
        }
        game
    }
    fn close_local<'a>(
        game: Box<Game<'a>>,
        archive: &Path,
        retry: bool,
    ) -> crate::game::paired_close::OrdinaryClosedWorld {
        let mut owner = CloseOwner::begin(
            game,
            CloseProfile::CoherentSession {
                archive: archive.to_owned(),
            },
            until(),
        );
        let receipt = if retry {
            let (control, receipt) =
                CoherentImageTestControl::new(CoherentImageTestAction::FailCopied);
            owner.local_image_control_for_test(control);
            Some(receipt)
        } else {
            None
        };
        let mut retried = false;
        loop {
            match owner.poll() {
                CloseStatus::Pending(_) => std::thread::yield_now(),
                CloseStatus::NativeReady => break,
                CloseStatus::Blocked(error) => {
                    assert!(
                        retry && !retried && receipt.as_ref().unwrap().entered(),
                        "unexpected LOCAL close refusal: {error}"
                    );
                    assert!(epochs(archive).unwrap().is_empty());
                    owner.retry(until()).unwrap();
                    retried = true;
                }
            }
        }
        assert_eq!(retried, retry);
        assert_eq!(owner.observe().participants, 1);
        assert_eq!(
            owner.local_cached_level_for_test().motion,
            [0.125, -0.25, 0.5]
        );
        assert!(!owner.local_cached_level_for_test().on_ground);
        assert_eq!(owner.local_cached_level_for_test().food_exhaustion, 2.25);
        assert_eq!(owner.local_cached_level_for_test().food_tick_timer, 7);
        assert_eq!(owner.local_cached_level_for_test().rotation, [91.0, -17.0]);
        let ready = owner
            .try_native_ready()
            .unwrap_or_else(|_| panic!("actual LOCAL ready transfer refused"));
        let mut disposed = ready
            .try_dispose_dormant()
            .unwrap_or_else(|(_, reason)| panic!("actual LOCAL completed disposal: {reason}"));
        disposed
            .take_ordinary_world()
            .expect("actual consuming LOCAL completion")
    }

    fn owned_runtime(
        f: &Materialized,
    ) -> (
        Arc<crate::game::mod_packages::FrozenHostInputs>,
        Arc<horizon_client::advancements::Tree>,
    ) {
        let startup = Arc::new(
            f.packages
                .freeze_peer_inputs(
                    Arc::new(horizon_client::lan::data::SyncData::load(&f.root).unwrap()),
                    f.pack.assets.registry,
                )
                .unwrap()
                .unwrap(),
        );
        let tree = Arc::new(horizon_client::advancements::Tree::load(
            &f.root.join("data"),
        ));
        assert!(!tree.list.is_empty());
        (startup, tree)
    }
    fn owned_player(game: &mut Game<'_>, f: &Materialized) {
        residency(game, 2, (0, 0));
        let end = until();
        while game.player.is_none() {
            update_native(game);
            game.spawn_player().unwrap();
            assert!(Instant::now() < end, "actual owned player spawn deadline");
            std::thread::yield_now();
        }
        residency(game, 2, (0, 0));
        game.load_stats();
        game.load_advancements();
        let body = game.player.as_mut().unwrap();
        body.position = Vec3::new(2.5, 120.0, 0.5);
        body.velocity = Vec3::new(0.125, -0.25, 0.5);
        body.ground.on_ground = false;
        body.health.food.exhaustion = 2.25;
        body.health.food.timer = 7;
        body.may_fly = true;
        body.invulnerable = true;
        body.flying = true;
        game.yaw = 91.0;
        game.pitch = -17.0;
        game.sync_player_body();
        game.inventory = vec![None; 36];
        game.inventory[0] = Some(ItemStack::new(f.alias.as_str(), 3));
        game.inventory[1] = Some(session_items_rich());
        game.ender_items[4] = Some(session_items_rich());
        game.stats
            .values
            .entry("minecraft:custom".into())
            .or_default()
            .insert("minecraft:jump".into(), 7);
        let advancement = game
            .adv
            .tree
            .list
            .iter()
            .find(|entry| !entry.criteria.is_empty())
            .unwrap();
        assert!(game.adv.progress.grant(
            &advancement.id,
            &advancement.criteria[0].name,
            "2026-01-01 00:00:00 +0000"
        ));
        assert!(!game.ui.rules.bool("spawn_mobs"));
    }
    fn owned_place(game: &mut Game<'_>, f: &Materialized) -> (RuntimeCounterKey, SlotKey) {
        game.arm_registered_guest_placement(&f.type_id).unwrap();
        let stone = game
            .pack
            .assets
            .registry
            .default_state("minecraft:stone")
            .unwrap();
        game.world
            .set(POSITION[0], POSITION[1] - 1, POSITION[2], stone)
            .unwrap();
        let hit = crate::game::Hit {
            pos: [POSITION[0], POSITION[1] - 1, POSITION[2]],
            face: [0, 1, 0],
            point: [2.5, 120.0, 2.5],
        };
        game.use_hand(Some(&hit)).unwrap();
        let GuestRuntimeState::ReadyActive { counter, slot } =
            game.world.guest_runtime_state().unwrap()
        else {
            panic!("actual owned first placement absent")
        };
        assert!(
            game.session_lock
                .as_mut()
                .unwrap()
                .counter_bootstrap()
                .is_some()
        );
        assert!(
            game.bootstrap_registered_host_counter(&f.type_id, [4, 120, 2])
                .is_err(),
            "second target cannot mint another initial allocation"
        );
        game.world.set(4, 119, 2, stone).unwrap();
        let second = crate::game::Hit {
            pos: [4, 119, 2],
            face: [0, 1, 0],
            point: [4.5, 120.0, 2.5],
        };
        game.use_hand(Some(&second)).unwrap();
        assert_eq!(
            game.world.block(4, 120, 2),
            game.world.air,
            "actual second click cannot allocate"
        );
        assert_eq!(game.world.counter_domain_snapshot().unwrap().high_water, 1);
        let binding = crate::game::world_loading::owned_guest_binding(
            &game.options.mod_packages,
            game.pack.assets.registry,
        )
        .unwrap();
        assert!(
            game.session_lock
                .as_ref()
                .unwrap()
                .validate_owned_unplaced(&game.world, &binding)
                .is_err(),
            "spent unused admission is stale after the actual first placement"
        );
        game.tickets.update(Some(Sources {
            players: vec![(0, 0)],
            forced: Vec::new(),
            view_distance: 2,
            simulation_distance: 2,
        }));
        game.world.game_time += 1;
        game.tick_counter_trial(true);
        assert_eq!(
            game.world
                .read_persistent_counter(&counter)
                .unwrap()
                .counter(),
            1
        );
        assert!(
            matches!(game.transfer_world_slot_inventory_cell(&slot, 1, Direction::IntoMachine, 3).unwrap(), TransferOutcome::Committed(row) if row.moved==3)
        );
        (counter, slot)
    }
    fn owned_native_close(game: Box<Game<'_>>) {
        let mut owner = CloseOwner::begin(game, CloseProfile::NativeWorld, until());
        loop {
            match owner.poll() {
                CloseStatus::Pending(_) => std::thread::yield_now(),
                CloseStatus::NativeReady => break,
                CloseStatus::Blocked(error) => panic!("owned native close: {error}"),
            }
        }
        let ready = owner
            .try_native_ready()
            .unwrap_or_else(|_| panic!("owned native ready absent"));
        let _disposed = ready
            .try_dispose_dormant()
            .unwrap_or_else(|(_, error)| panic!("owned native disposal: {error}"));
    }
    fn owned_refusal_close(work: &mut OrdinaryRecoveryWork<'_>) {
        work.begin_close(until());
        loop {
            match work.poll_close() {
                PreparationCloseStatus::Pending => std::thread::yield_now(),
                PreparationCloseStatus::Complete => break,
                PreparationCloseStatus::Blocked(error) => {
                    panic!("owned original refused preparation close: {error}")
                }
            }
        }
        assert!(work.game_mut().save.is_none() && work.game_mut().session_lock.is_none());
    }
    fn owned_reopen<'a>(
        f: &'a Materialized,
        startup: &Arc<crate::game::mod_packages::FrozenHostInputs>,
        tree: &Arc<horizon_client::advancements::Tree>,
        root: &Path,
        suffix: &str,
    ) -> Box<Game<'a>> {
        let entry = RetainedEntryIdentity {
            token: 401,
            owner: Arc::new(()),
        };
        let namespace =
            crate::game::world_loading::host_identity::session_authority::AuthorityNamespace::new(
                &root.join("authority"),
            )
            .unwrap();
        let request = OwnedRecoveryRequest {
            namespace,
            anchor: root.join("world"),
            destination: root.join("destinations").join(suffix),
            deadline: until(),
        };
        let mut owner = OrdinaryRecoveryWork::prepare_owned(
            local_game(f, startup, tree, &entry, None),
            entry,
            request,
        );
        assert!(
            owner.original_error().is_none(),
            "owned original native recovery: {:?}",
            owner.original_error()
        );
        assert!(owner.game_mut().native_close_authority.is_none());
        let game = owner
            .try_activate()
            .unwrap()
            .expect("actual LOCAL owned recovery needs no fake network participant");
        assert!(game.native_close_owner_authorized() && game.local_checkpoint_owner());
        assert_eq!(
            game.local_continuation_archive.as_deref(),
            Some(owned_observed_archive(root).as_path())
        );
        game
    }
    fn owned_observed_archive(root: &Path) -> PathBuf {
        // Expected-result observation ONLY. OwnedRecoveryRequest never consumes
        // a decoded archive path or UUID; its real owner acquires/rechecks OWN.
        let identity: Value = json(
            &fs::read(root.join("world/horizon-world-identity.json")).unwrap(),
            1024,
        )
        .unwrap();
        let world = identity["world_uuid"].as_str().unwrap();
        let own: Value = json(
            &fs::read(root.join("authority").join(world).join("owned.json")).unwrap(),
            16 << 10,
        )
        .unwrap();
        directory(Path::new(own["archive"].as_str().unwrap())).unwrap()
    }
    fn owned_actual_recovery_refusals(
        f: &Materialized,
        startup: &Arc<crate::game::mod_packages::FrozenHostInputs>,
        tree: &Arc<horizon_client::advancements::Tree>,
        root: &Path,
    ) {
        let identity: Value = json(
            &fs::read(root.join("world/horizon-world-identity.json")).unwrap(),
            1024,
        )
        .unwrap();
        let record = root
            .join("authority")
            .join(identity["world_uuid"].as_str().unwrap())
            .join("owned.json");
        let original = fs::read(&record).unwrap();
        let native_before = super::super::tree(&root.join("world"), &["session.lock"]).unwrap();
        let controls = root.join("negative-inputs");
        fs::create_dir(&controls).unwrap();
        for kind in ["strict-association", "profile", "native-uuid", "pin"] {
            let mut bad: Value = json(&original, 16 << 10).unwrap();
            match kind {
                "strict-association" => {
                    bad["unexpected"] = true.into();
                }
                "profile" => {
                    bad["profile"] = serde_json::to_value(crate::game::world_loading::host_identity::session_authority::OwnedProfile::Dedicated).unwrap();
                }
                "native-uuid" => {
                    bad["native_player"][0] =
                        (bad["native_player"][0].as_i64().unwrap() ^ 1).into();
                }
                "pin" => {
                    bad["package_pin_sha256"] = "00".repeat(32).into();
                }
                _ => unreachable!(),
            }
            let bytes = serde_json::to_vec_pretty(&bad).unwrap();
            fs::write(controls.join(format!("owned-{kind}.json")), &bytes).unwrap();
            fs::write(&record, &bytes).unwrap();
            let entry = RetainedEntryIdentity {
                token: 405,
                owner: Arc::new(()),
            };
            let destination = root.join("destinations").join(format!("refused-{kind}"));
            let mut refused = OrdinaryRecoveryWork::prepare_owned(local_game(f,startup,tree,&entry,None),entry,OwnedRecoveryRequest {
                namespace: crate::game::world_loading::host_identity::session_authority::AuthorityNamespace::new(&root.join("authority")).unwrap(),
                anchor:root.join("world"), destination:destination.clone(), deadline:until(),
            });
            assert!(
                refused.original_error().is_some(),
                "actual owned {kind} recovery must refuse"
            );
            eprintln!(
                "OWNED_ACTUAL_RECOVERY_REFUSAL {kind}: {}",
                refused.original_error().unwrap()
            );
            owned_refusal_close(&mut refused);
            assert!(
                !destination.exists(),
                "refusal must precede copied native construction"
            );
            assert_eq!(
                super::super::tree(&root.join("world"), &["session.lock"]).unwrap(),
                native_before
            );
            fs::write(&record, &original).unwrap();
        }
    }
    #[test]
    #[ignore = "isolated native resources and exact same executable: two serial real processes, complete Unplaced/Claimed owned restart"]
    fn owned_durable_processes_restore_unplaced_and_claimed() {
        let root =
            std::env::temp_dir().join(format!("horizon-owned-restart-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        eprintln!("OWNED_NATIVE_EVIDENCE_ROOT={}", root.display());
        let exe = std::env::current_exe().unwrap();
        let digest = Sha256::digest(fs::read(&exe).unwrap());
        for phase in ["A", "B"] {
            let home = root.join(format!("process-{phase}"));
            fs::create_dir(&home).unwrap();
            let temp = home.join("temp");
            fs::create_dir(&temp).unwrap();
            let mut child = std::process::Command::new(&exe)
                .args(["--exact", "game::levels::session_checkpoint::tests::registered::local_tests::owned_durable_process_child", "--ignored", "--nocapture", "--test-threads=1"])
                .env("HORIZON_OWN_PHASE", phase).env("HORIZON_OWN_ROOT", &root)
                .env("HOME", &home).env("USERPROFILE", &home)
                .env("XDG_CONFIG_HOME", &home).env("XDG_DATA_HOME", &home).env("XDG_CACHE_HOME", &home)
                .env("TMPDIR", &temp).env("TMP", &temp).env("TEMP", &temp)
                .current_dir(&home).spawn().unwrap();
            let end = Instant::now() + Duration::from_secs(600);
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if Instant::now() >= end {
                    child.kill().unwrap();
                    let _ = child.wait();
                    panic!("owned child {phase} deadline; preserved {}", root.display());
                }
                std::thread::sleep(Duration::from_millis(20));
            };
            assert!(
                status.success(),
                "owned child {phase} failed; preserved {}",
                root.display()
            );
            assert_eq!(
                Sha256::digest(fs::read(&exe).unwrap()),
                digest,
                "same pinned child executable"
            );
        }
        let a: Value = json(&fs::read(root.join("A.json")).unwrap(), STRUCTURED_LIMIT).unwrap();
        let b: Value = json(&fs::read(root.join("B.json")).unwrap(), STRUCTURED_LIMIT).unwrap();
        assert_ne!(a["pid"], b["pid"]);
        assert_eq!(a["executable"], b["executable"]);
        assert_eq!(b["completed"], true);
        eprintln!("OWNED_TWO_PROCESS_UNPLACED_CLAIMED_SAME_ARCHIVE_PASS");
    }
    #[test]
    #[ignore = "child only; invoked by the exact two-process owned fixture"]
    fn owned_durable_process_child() {
        use crate::game::world_loading::host_identity::session_authority::{
            AuthorityNamespace, OwnedProfile,
        };
        let phase = std::env::var("HORIZON_OWN_PHASE").expect("explicit child phase");
        assert!(matches!(phase.as_str(), "A" | "B"));
        let root = PathBuf::from(
            std::env::var_os("HORIZON_OWN_ROOT").expect("explicit owned fixture root"),
        );
        let f = Materialized::new(&format!("own-{phase}"));
        let (startup, tree) = owned_runtime(&f);
        let mut created: BTreeMap<bool, Box<Game<'_>>> = BTreeMap::new();
        if phase == "A" {
            for claimed in [false, true] {
                let case = root.join(if claimed { "claimed" } else { "unplaced" });
                fs::create_dir(&case).unwrap();
                for name in ["authority", "archive", "destinations"] {
                    fs::create_dir(case.join(name)).unwrap();
                }
            }
            let shared_archive_parent = root.join("unplaced/archive");
            // BOTH actual constructors succeed before either publishes. The
            // constructor UUID, never caller-selected emptiness, separates them.
            for claimed in [false, true] {
                let case = root.join(if claimed { "claimed" } else { "unplaced" });
                let namespace = AuthorityNamespace::new(&case.join("authority")).unwrap();
                if claimed {
                    let first: &Game<'_> = created.get(&false).unwrap();
                    let first_archive = first
                        .session_lock
                        .as_ref()
                        .unwrap()
                        .owned()
                        .unwrap()
                        .archive();
                    let before = super::super::tree(first_archive, &[]).unwrap();
                    assert!(
                        namespace
                            .new_owned_route(first_archive, &case.join("destinations"))
                            .is_err(),
                        "another UUID archive cannot become a configured parent"
                    );
                    let valid = namespace
                        .new_owned_route(&shared_archive_parent, &case.join("destinations"))
                        .unwrap();
                    assert!(
                        valid
                            .check_new_root(&first_archive.join("nested-native-world"))
                            .is_err(),
                        "native root cannot poison another UUID archive"
                    );
                    assert!(!case.join("world").exists());
                    assert_eq!(super::super::tree(first_archive, &[]).unwrap(), before);
                    eprintln!("OWNED_NESTED_PARENT_NATIVE_ROOT_REFUSED_BEFORE_EFFECTS_PASS");
                }
                let mut request = WorldRequest::new(
                    "unused",
                    Some("Owned restart fixture"),
                    Some(17),
                    true,
                    None,
                )
                .unwrap()
                .with_identity_intent(IdentityIntent::CreateNewIdentity);
                request.save_root = case.join("world");
                request = request
                    .with_new_owned_route(
                        namespace
                            .new_owned_route(&shared_archive_parent, &case.join("destinations"))
                            .unwrap(),
                    )
                    .unwrap();
                let entry = RetainedEntryIdentity {
                    token: if claimed { 400 } else { 399 },
                    owner: Arc::new(()),
                };
                let game = local_game(&f, &startup, &tree, &entry, Some(request));
                let owned = game.session_lock.as_ref().unwrap().owned().unwrap();
                assert_eq!(
                    owned.archive().file_name().unwrap().to_str().unwrap(),
                    owned.world().to_string()
                );
                created.insert(claimed, game);
            }
            assert_ne!(
                created[&false]
                    .session_lock
                    .as_ref()
                    .unwrap()
                    .owned()
                    .unwrap()
                    .archive(),
                created[&true]
                    .session_lock
                    .as_ref()
                    .unwrap()
                    .owned()
                    .unwrap()
                    .archive()
            );
            eprintln!("OWNED_TWO_NEW_CONSTRUCTORS_SHARED_PARENT_DISTINCT_UUID_ARCHIVES_PASS");
        }
        for claimed in [false, true] {
            let case = root.join(if claimed { "claimed" } else { "unplaced" });
            let archive = if phase == "A" {
                created[&claimed]
                    .session_lock
                    .as_ref()
                    .unwrap()
                    .owned()
                    .unwrap()
                    .archive()
                    .to_owned()
            } else {
                owned_observed_archive(&case)
            };
            if phase == "A" {
                let namespace = AuthorityNamespace::new(&case.join("authority")).unwrap();
                let mut game = created.remove(&claimed).unwrap();
                owned_player(&mut game, &f);
                let owned = game.session_lock.as_ref().unwrap().owned().unwrap().clone();
                assert!(
                    owned
                        .check_profile(&OwnedProfile::Dedicated, owned.native_player())
                        .is_err()
                );
                let mut wrong = owned.native_player();
                wrong[0] ^= 1;
                assert!(owned.check_profile(&OwnedProfile::Local, wrong).is_err());
                assert!(
                    namespace
                        .acquire_owned_recovery(
                            &case.join("world"),
                            &case.join("destinations/contended")
                        )
                        .is_err()
                );
                assert!(!case.join("destinations/contended").exists());
                if claimed {
                    let _keys = owned_place(&mut game, &f);
                } else {
                    assert!(game.prepare_coherent_image_context(&archive).is_ok());
                    game.world.set(4, 120, 2, f.binding().state_id()).unwrap();
                    assert!(
                        game.prepare_coherent_image_context(&archive).is_err(),
                        "any resident alias without history refuses"
                    );
                    game.world.set(4, 120, 2, game.world.air).unwrap();
                    game.arm_registered_guest_placement(&f.type_id).unwrap();
                }
                let _closed = close_local(game, &archive, true);
                let (epoch, selected) = epochs(&archive).unwrap().pop().unwrap();
                let pin_path = case.join("world").join(PIN);
                let pin = fs::read(&pin_path).unwrap();
                fs::write(&pin_path, b"{}").unwrap();
                assert!(owned.check_pin(&case.join("world")).is_err());
                fs::write(&pin_path, &pin).unwrap();
                assert!(owned.check_pin(&case.join("world")).is_ok());
                if !claimed {
                    let parked = archive.join(format!(".pending-{epoch:020}"));
                    fs::rename(&selected, &parked).unwrap();
                    let before =
                        super::super::tree(&case.join("world"), &["session.lock"]).unwrap();
                    let entry = RetainedEntryIdentity {
                        token: 404,
                        owner: Arc::new(()),
                    };
                    let mut refused = OrdinaryRecoveryWork::prepare_owned(
                        local_game(&f, &startup, &tree, &entry, None),
                        entry,
                        OwnedRecoveryRequest {
                            namespace: namespace.clone(),
                            anchor: case.join("world"),
                            destination: case.join("destinations/zero"),
                            deadline: until(),
                        },
                    );
                    assert!(
                        refused
                            .original_error()
                            .unwrap()
                            .contains("no complete owned epoch")
                    );
                    owned_refusal_close(&mut refused);
                    assert!(!case.join("destinations/zero").exists());
                    assert_eq!(
                        super::super::tree(&case.join("world"), &["session.lock"]).unwrap(),
                        before
                    );
                    fs::rename(&parked, &selected).unwrap();
                } else {
                    // Genuine newer working native state, but no newer complete
                    // coherent image. An authored interrupted candidate is kept;
                    // the existing FailCopied/Retry case above covers real I/O.
                    let mut newer = owned_reopen(&f, &startup, &tree, &case, "A-newer");
                    newer.inventory[1].as_mut().unwrap().count = 2;
                    newer.world.game_time += 1;
                    newer.tick_counter_trial(true);
                    owned_native_close(newer);
                    let pending = archive.join(format!(
                        ".pending-{:020}",
                        next_session_checkpoint_epoch(&archive).unwrap()
                    ));
                    fs::create_dir(&pending).unwrap();
                    fs::write(
                        pending.join("interrupted-native-clock.txt"),
                        b"newer native working state has no complete manifest",
                    )
                    .unwrap();
                }
                eprintln!("OWNED_A_COMPLETE claimed={claimed} epoch={epoch}");
            } else {
                if !claimed {
                    owned_actual_recovery_refusals(&f, &startup, &tree, &case);
                }
                let before = epochs(&archive).unwrap();
                let (old_epoch, old_root) = before.last().unwrap();
                let old_counter = if claimed {
                    Some(fs::read(old_root.join(COUNTER)).unwrap())
                } else {
                    None
                };
                let old_slot = if claimed {
                    Some(fs::read(old_root.join(SLOT)).unwrap())
                } else {
                    None
                };
                let pending_before = fs::read_dir(&archive)
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .filter(|path| {
                        path.file_name()
                            .unwrap()
                            .to_string_lossy()
                            .starts_with(".pending-")
                    })
                    .map(|path| {
                        let inventory = super::super::tree(&path, &[]).unwrap();
                        (path, inventory)
                    })
                    .collect::<Vec<_>>();
                let mut game = owned_reopen(&f, &startup, &tree, &case, "B-first");
                assert_eq!(game.stats.get("custom", "minecraft:jump"), 7);
                let (counter, slot) = if claimed {
                    assert!(
                        game.session_lock
                            .as_mut()
                            .unwrap()
                            .counter_bootstrap()
                            .is_none(),
                        "claimed image has no unused grant"
                    );
                    assert_eq!(
                        SessionCounter::from_world(&game.world)
                            .unwrap()
                            .encode()
                            .unwrap(),
                        old_counter.unwrap()
                    );
                    assert_eq!(
                        SessionCounter::from_world(&game.world)
                            .unwrap()
                            .slot_bytes(&game.world, game.res.items())
                            .unwrap(),
                        old_slot.unwrap()
                    );
                    let GuestRuntimeState::ReadyActive { counter, slot } =
                        game.world.guest_runtime_state().unwrap()
                    else {
                        panic!("claimed pair absent")
                    };
                    assert_eq!(
                        game.world
                            .read_persistent_counter(&counter)
                            .unwrap()
                            .counter(),
                        1
                    );
                    assert_eq!(
                        game.inventory[1].as_ref().unwrap().count,
                        4,
                        "restore complete older item owner, never newer working count2"
                    );
                    (counter, slot)
                } else {
                    assert!(game.world.counter_domain_snapshot().is_none());
                    assert_eq!(game.inventory[1].as_ref().unwrap().count, 7);
                    let binding = crate::game::world_loading::owned_guest_binding(
                        &game.options.mod_packages,
                        game.pack.assets.registry,
                    )
                    .unwrap();
                    let world = *game
                        .session_lock
                        .as_ref()
                        .unwrap()
                        .expected_world()
                        .unwrap();
                    let wrong_world = horizon_client::world::World::new(game.world.air, 0);
                    assert!(
                        game.session_lock
                            .as_ref()
                            .unwrap()
                            .validate_owned_unplaced(&wrong_world, &binding)
                            .is_err()
                    );
                    assert!(
                        game.world
                            .restore_verified_unplaced_admission(
                                &binding,
                                world,
                                Dim::Overworld.id(),
                                game.world.game_time
                            )
                            .is_err(),
                        "duplicate recovery cannot replace the actual live incarnation admission"
                    );
                    game.session_lock
                        .as_ref()
                        .unwrap()
                        .validate_owned_unplaced(&game.world, &binding)
                        .unwrap();
                    owned_place(&mut game, &f)
                };
                let mut rich = session_items_rich();
                rich.count = 3;
                retirement_raw_equal(
                    game.world
                        .read_world_slot(&slot)
                        .unwrap()
                        .slot
                        .stack()
                        .unwrap(),
                    &rich,
                );
                rich.count = 4;
                retirement_raw_equal(game.inventory[1].as_ref().unwrap(), &rich);
                rich.count = 7;
                retirement_raw_equal(game.ender_items[4].as_ref().unwrap(), &rich);
                let heads = (
                    game.world.counter_domain_snapshot().unwrap().high_water,
                    game.world
                        .persistent_world_slot_metadata()
                        .unwrap()
                        .high_water,
                );
                let _closed = close_local(game, &archive, false);
                let latest = epochs(&archive).unwrap();
                assert_eq!(latest.len(), before.len() + 1);
                assert!(latest.last().unwrap().0 > *old_epoch);
                for (path, inventory) in pending_before {
                    assert_eq!(
                        super::super::tree(&path, &[]).unwrap(),
                        inventory,
                        "preserve original failed/interrupted predecessor"
                    );
                }
                let mut successor = owned_reopen(&f, &startup, &tree, &case, "B-successor");
                assert!(
                    successor
                        .session_lock
                        .as_mut()
                        .unwrap()
                        .counter_bootstrap()
                        .is_none()
                );
                assert!(successor.world.read_persistent_counter(&counter).is_err());
                assert!(successor.world.read_world_slot(&slot).is_err());
                assert_eq!(
                    (
                        successor
                            .world
                            .counter_domain_snapshot()
                            .unwrap()
                            .high_water,
                        successor
                            .world
                            .persistent_world_slot_metadata()
                            .unwrap()
                            .high_water
                    ),
                    heads
                );
                owned_native_close(successor);
                eprintln!(
                    "OWNED_B_SAME_ARCHIVE_SUCCESSOR claimed={claimed} epoch={}",
                    latest.last().unwrap().0
                );
            }
        }
        assert_eq!(
            package_files(&f.pair),
            f.pair_before,
            "captured source inputs changed"
        );
        assert_eq!(
            input_files(&f.base),
            f.base_before,
            "shared base tables changed"
        );
        let exe = std::env::current_exe().unwrap();
        fs::write(root.join(format!("{phase}.json")), serde_json::to_vec_pretty(&serde_json::json!({"pid":std::process::id(),"executable":format!("{:x}",Sha256::digest(fs::read(exe).unwrap())),"completed":true})).unwrap()).unwrap();
        eprintln!("OWNED_CHILD_{phase}_PASS");
        std::mem::forget(f);
    }

    fn ordinary_local_stone_use_control(game: &mut Game<'_>, x: i32) {
        // Actual loaded native support and a legal current player ray. Exercise
        // use_item, whose ordinary ownership preflight the old test bypassed.
        let old_position = game.player.as_ref().unwrap().position;
        let old_look = (game.yaw, game.pitch);
        let old_selected = game.selected;
        let old_stack = game.inventory[2].take();
        let stone = game
            .pack
            .assets
            .registry
            .default_state("minecraft:stone")
            .unwrap();
        game.world.set(x, 119, 4, stone).unwrap();
        assert_eq!(game.world.block(x, 120, 4), game.world.air);
        game.player.as_mut().unwrap().position = Vec3::new(f64::from(x) + 0.5, 120.0, 2.5);
        game.yaw = 0.0;
        game.pitch = 45.0;
        game.sync_player_body();
        game.inventory[2] = Some(ItemStack::new("minecraft:stone", 1));
        game.selected = 2;
        let hit = game.pick().expect("actual LOCAL legal stone ray");
        assert_eq!(hit.pos, [x, 119, 4]);
        assert_eq!(hit.face, [0, 1, 0]);
        assert!(game.ordinary_stone_use_admitted());
        // Preserve the SAME original constructor while proving the unpublished
        // interval cannot borrow the cloneable local-source observation alone.
        let original = game.native_close_authority.take().unwrap();
        assert!(!game.ordinary_stone_use_admitted());
        game.native_close_authority = Some(original);
        let body = game.player.take().unwrap();
        assert!(!game.ordinary_stone_use_admitted());
        game.player = Some(body);
        game.player.as_mut().unwrap().server_copy = true;
        assert!(!game.ordinary_stone_use_admitted());
        game.player.as_mut().unwrap().server_copy = false;
        game.restore_player_state = true;
        assert!(!game.ordinary_stone_use_admitted());
        game.restore_player_state = false;
        game.retained_entry_closing = true;
        assert!(!game.ordinary_stone_use_admitted());
        game.retained_entry_closing = false;
        assert!(game.ordinary_stone_use_admitted());
        let used = game.stats.get("used", "minecraft:stone");
        assert!(
            game.use_item().unwrap().is_some(),
            "normal use_item must reach native placement"
        );
        assert_eq!(game.world.block(x, 120, 4), stone);
        assert_eq!(game.world.block(x, 119, 4), stone);
        assert_eq!(game.stats.get("used", "minecraft:stone"), used + 1);
        assert_eq!(
            game.inventory[2].as_ref().unwrap().count,
            1,
            "actual Creative count stays unchanged"
        );
        game.inventory[2] = old_stack;
        game.selected = old_selected;
        game.player.as_mut().unwrap().position = old_position;
        (game.yaw, game.pitch) = old_look;
        game.sync_player_body();
        eprintln!(
            "ORDINARY_LOCAL_NORMAL_USE_ITEM_STONE_PASS x={x} used={}",
            used + 1
        );
    }

    fn ordinary_local_pending_cancel_retry(
        f: &Materialized,
        startup: &Arc<crate::game::mod_packages::FrozenHostInputs>,
        tree: &Arc<horizon_client::advancements::Tree>,
        case: &Path,
    ) {
        use std::sync::atomic::Ordering;
        let anchor = case.join("world");
        let before = super::super::tree(&anchor, &["session.lock"]).unwrap();
        let mut request = WorldRequest::new("unused", None, None, true, None)
            .unwrap()
            .with_identity_intent(IdentityIntent::RequireExistingIdentity);
        request.save_root = anchor.clone();
        let selected = request.into_ordinary_local(case, None, until()).unwrap();
        let crate::game::retained_server::OrdinaryRequest::RecoverOwned(request) = selected.request
        else {
            panic!("actual pending LOCAL request absent");
        };
        let namespace = request.namespace.clone();
        let destination = request.destination.clone();
        let contender = destination.parent().unwrap().join("pending-contended");
        let entry = RetainedEntryIdentity {
            token: 500,
            owner: Arc::new(()),
        };
        let pause = Arc::new(LocalOwnedWorkerPause::default());
        let mut owner =
            LocalOwnedRecovery::start(local_game(f, startup, tree, &entry, None), entry, *request);
        owner.pause = Some(pause.clone());
        while !pause.entered.load(Ordering::Acquire) {
            if let Err(error) = owner.poll_activate() {
                pause.released.store(true, Ordering::Release);
                owner.begin_close(until());
                loop {
                    match owner.poll_close() {
                        PreparationCloseStatus::Pending => std::thread::yield_now(),
                        PreparationCloseStatus::Complete => break,
                        PreparationCloseStatus::Blocked(reason) => {
                            panic!("pending LOCAL cleanup {reason}; original {error}")
                        }
                    }
                }
                panic!("actual LOCAL preparation hold not reached: {error}");
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let held = namespace
            .acquire_owned_recovery(&anchor, &contender)
            .is_err();
        owner.begin_close(Instant::now() + Duration::from_millis(1));
        let started = Instant::now();
        let pending = (0..3).all(|_| owner.poll_close() == PreparationCloseStatus::Pending);
        let poll_time = started.elapsed();
        std::thread::sleep(Duration::from_millis(5));
        pause.released.store(true, Ordering::Release);
        let blocked = loop {
            match owner.poll_close() {
                PreparationCloseStatus::Pending => std::thread::yield_now(),
                PreparationCloseStatus::Blocked(reason) => break Some(reason),
                PreparationCloseStatus::Complete => break None,
            }
        };
        if blocked.is_some() {
            owner.retry_close(until()).unwrap();
            loop {
                match owner.poll_close() {
                    PreparationCloseStatus::Pending => std::thread::yield_now(),
                    PreparationCloseStatus::Complete => break,
                    PreparationCloseStatus::Blocked(reason) => {
                        panic!("same LOCAL preparation Retry: {reason}")
                    }
                }
            }
        }
        assert!(held && pending && blocked.is_some());
        assert!(
            poll_time < Duration::from_millis(200),
            "pending native result must not join/wait on UI"
        );
        assert!(owner.core.work.game.as_ref().unwrap().save.is_none());
        assert!(owner.core.work.game.as_ref().unwrap().streaming.is_vacant());
        assert!(owner.core.lease.is_none() && !owner.core.work.armed);
        assert!(!contender.exists());
        assert_eq!(
            super::super::tree(&anchor, &["session.lock"]).unwrap(),
            before
        );
        let released = namespace
            .acquire_owned_recovery(&anchor, &contender)
            .unwrap();
        let archive = released.owned().unwrap().archive().to_owned();
        assert_eq!(
            epochs(&archive).unwrap().len(),
            1,
            "cancelled unpublished recovery cannot publish"
        );
        drop(released);
        eprintln!(
            "ORDINARY_LOCAL_PENDING_NATIVE_CANCEL_RETRY_PASS poll_us={}",
            poll_time.as_micros()
        );
    }

    #[test]
    #[ignore = "isolated ordinary LOCAL OWN routing in two fresh processes of the same executable"]
    fn ordinary_local_owned_two_processes() {
        let root =
            std::env::temp_dir().join(format!("horizon-ordinary-local-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        eprintln!("ORDINARY_LOCAL_NATIVE_EVIDENCE_ROOT={}", root.display());
        let exe = std::env::current_exe().unwrap();
        let digest = Sha256::digest(fs::read(&exe).unwrap());
        for phase in ["A", "B"] {
            let home = root.join(format!("process-{phase}"));
            fs::create_dir(&home).unwrap();
            let temp = home.join("temp");
            fs::create_dir(&temp).unwrap();
            let mut child = std::process::Command::new(&exe)
                .args(["--exact", "game::levels::session_checkpoint::tests::registered::local_tests::ordinary_local_owned_child",
                    "--ignored", "--nocapture", "--test-threads=1"])
                .env("HORIZON_LOCAL_OWN_PHASE", phase).env("HORIZON_LOCAL_OWN_ROOT", &root)
                .env("HOME", &home).env("USERPROFILE", &home)
                .env("XDG_CONFIG_HOME", &home).env("XDG_DATA_HOME", &home).env("XDG_CACHE_HOME", &home)
                .env("TMPDIR", &temp).env("TMP", &temp).env("TEMP", &temp)
                .current_dir(&home).spawn().unwrap();
            let end = Instant::now() + Duration::from_secs(420);
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if Instant::now() >= end {
                    child.kill().unwrap();
                    let _ = child.wait();
                    panic!(
                        "ordinary LOCAL child {phase} deadline; preserved {}",
                        root.display()
                    );
                }
                std::thread::sleep(Duration::from_millis(20));
            };
            assert!(
                status.success(),
                "ordinary LOCAL child {phase}; preserved {}",
                root.display()
            );
            assert_eq!(Sha256::digest(fs::read(&exe).unwrap()), digest);
        }
        let a: Value = json(&fs::read(root.join("A.json")).unwrap(), STRUCTURED_LIMIT).unwrap();
        let b: Value = json(&fs::read(root.join("B.json")).unwrap(), STRUCTURED_LIMIT).unwrap();
        assert_ne!(a["pid"], b["pid"]);
        assert_eq!(a["executable"], b["executable"]);
        assert_eq!(b["completed"], true);
        eprintln!("ORDINARY_LOCAL_FRESH_PROCESSES_UNPLACED_CLAIMED_PASS");
    }
    #[test]
    #[ignore = "child only; invoked by exact ordinary LOCAL owned fixture"]
    fn ordinary_local_owned_child() {
        use crate::game::retained_server::OrdinaryRequest;
        let phase = std::env::var("HORIZON_LOCAL_OWN_PHASE").expect("explicit LOCAL child phase");
        assert!(matches!(phase.as_str(), "A" | "B"));
        let root =
            PathBuf::from(std::env::var_os("HORIZON_LOCAL_OWN_ROOT").expect("explicit LOCAL root"));
        let f = Materialized::new(&format!("ordinary-local-{phase}"));
        let (startup, tree) = owned_runtime(&f);
        for claimed in [false, true] {
            let case = root.join(if claimed { "claimed" } else { "unplaced" });
            let anchor = case.join("world");
            let entry = RetainedEntryIdentity {
                token: if claimed { 502 } else { 501 },
                owner: Arc::new(()),
            };
            if phase == "A" {
                fs::create_dir(&case).unwrap();
                let mut request =
                    WorldRequest::new("unused", Some("Ordinary LOCAL OWN"), Some(17), true, None)
                        .unwrap()
                        .with_identity_intent(IdentityIntent::CreateNewIdentity);
                request.save_root = anchor.clone();
                let selected = request.into_ordinary_local(&case, None, until()).unwrap();
                assert_eq!(selected.anchor, anchor);
                let OrdinaryRequest::Native(request) = selected.request else {
                    panic!("genuine NEW native route absent");
                };
                let mut game = local_game(&f, &startup, &tree, &entry, Some(request));
                owned_player(&mut game, &f);
                ordinary_local_stone_use_control(&mut game, 4);
                let owned = game.session_lock.as_ref().unwrap().owned().unwrap().clone();
                assert_eq!(
                    owned.archive().parent().unwrap(),
                    case.join("backups/horizon-owned/archives")
                );
                assert!(epochs(owned.archive()).unwrap().is_empty());
                if claimed {
                    let _ = owned_place(&mut game, &f);
                }
                let _closed = close_local(game, owned.archive(), false);
                assert_eq!(epochs(owned.archive()).unwrap().len(), 1);
                let record = case
                    .join("backups/horizon-owned/authority")
                    .join(owned.world().to_string())
                    .join("owned.json");
                fs::write(case.join("owned-before.json"), fs::read(record).unwrap()).unwrap();
            } else {
                if !claimed {
                    ordinary_local_pending_cancel_retry(&f, &startup, &tree, &case);
                }
                let mut request = WorldRequest::new("unused", None, None, true, None)
                    .unwrap()
                    .with_identity_intent(IdentityIntent::RequireExistingIdentity);
                request.save_root = anchor.clone();
                let selected = request.into_ordinary_local(&case, None, until()).unwrap();
                assert_eq!(selected.anchor, anchor);
                let OrdinaryRequest::RecoverOwned(request) = selected.request else {
                    panic!("original-anchor OWN route absent");
                };
                let destination = request.destination.clone();
                let seed = local_game(&f, &startup, &tree, &entry, None);
                assert!(
                    seed.renderer.is_none() && seed.save.is_none() && seed.streaming.is_vacant()
                );
                let mut owner = LocalOwnedRecovery::start(seed, entry, *request);
                let mut polls = 0;
                let mut game = loop {
                    polls += 1;
                    match owner.poll_activate() {
                        Ok(Some(game)) => break game,
                        Ok(None) => std::thread::sleep(Duration::from_millis(1)),
                        Err(error) => {
                            owner.begin_close(until());
                            loop {
                                match owner.poll_close() {
                                    PreparationCloseStatus::Pending => std::thread::yield_now(),
                                    PreparationCloseStatus::Complete => break,
                                    PreparationCloseStatus::Blocked(reason) => panic!(
                                        "LOCAL failed recovery close: {reason}; original {error}"
                                    ),
                                }
                            }
                            panic!("ordinary LOCAL recovery refused: {error}");
                        }
                    }
                };
                assert!(polls > 1 && owner.take_construction().is_some());
                assert!(game.native_close_owner_authorized() && game.local_checkpoint_owner());
                assert_eq!(game.world_root, destination);
                assert_ne!(game.world_root, anchor);
                let owned = game.session_lock.as_ref().unwrap().owned().unwrap().clone();
                assert_eq!(
                    game.local_continuation_archive.as_deref(),
                    Some(owned.archive())
                );
                assert_eq!(game.stats.get("custom", "minecraft:jump"), 7);
                let body = game.player.as_ref().unwrap();
                assert!(!body.server_copy && body.flying);
                assert_eq!(body.velocity, Vec3::new(0.125, -0.25, 0.5));
                assert_eq!([body.yaw, body.pitch], [91.0, -17.0]);
                assert_eq!(body.health.food.exhaustion, 2.25);
                assert!(!game.ui.rules.bool("spawn_mobs"));
                assert!(game.lan.is_none() && game.retained_server.is_none());
                ordinary_local_stone_use_control(&mut game, 6);
                if claimed {
                    let GuestRuntimeState::ReadyActive { counter, slot } =
                        game.world.guest_runtime_state().unwrap()
                    else {
                        panic!("actual restored Guest pair absent");
                    };
                    assert_eq!(
                        game.world
                            .read_persistent_counter(&counter)
                            .unwrap()
                            .counter(),
                        1
                    );
                    let mut rich = session_items_rich();
                    rich.count = 3;
                    retirement_raw_equal(
                        game.world
                            .read_world_slot(&slot)
                            .unwrap()
                            .slot
                            .stack()
                            .unwrap(),
                        &rich,
                    );
                    rich.count = 4;
                    retirement_raw_equal(game.inventory[1].as_ref().unwrap(), &rich);
                } else {
                    let binding = crate::game::world_loading::owned_guest_binding(
                        &game.options.mod_packages,
                        game.pack.assets.registry,
                    )
                    .unwrap();
                    game.session_lock
                        .as_ref()
                        .unwrap()
                        .validate_owned_unplaced(&game.world, &binding)
                        .unwrap();
                    assert!(game.world.counter_domain_snapshot().is_none());
                    assert_eq!(game.inventory[1].as_ref().unwrap().count, 7);
                }
                let record = case
                    .join("backups/horizon-owned/authority")
                    .join(owned.world().to_string())
                    .join("owned.json");
                assert_eq!(
                    fs::read(&record).unwrap(),
                    fs::read(case.join("owned-before.json")).unwrap()
                );
                let before = epochs(owned.archive()).unwrap();
                let _closed = close_local(game, owned.archive(), false);
                let after = epochs(owned.archive()).unwrap();
                assert_eq!(after.len(), before.len() + 1);
                assert!(after.last().unwrap().0 > before.last().unwrap().0);
                assert_eq!(
                    fs::read(&record).unwrap(),
                    fs::read(case.join("owned-before.json")).unwrap()
                );
                eprintln!(
                    "ORDINARY_LOCAL_B_RESTORED claimed={claimed} polls={polls} epoch={}",
                    after.last().unwrap().0
                );
            }
        }
        let executable = format!(
            "{:x}",
            Sha256::digest(fs::read(std::env::current_exe().unwrap()).unwrap())
        );
        fs::write(
            root.join(format!("{phase}.json")),
            serde_json::to_vec(&serde_json::json!({
                "phase":phase,"pid":std::process::id(),"executable":executable,"completed":true
            }))
            .unwrap(),
        )
        .unwrap();
        eprintln!("ORDINARY_LOCAL_CHILD_{phase}_PASS");
        std::mem::forget(f);
    }

    /// Aim the SAME native player at existing generated terrain; never write a
    /// support or fabricate a body/hit to make the ordinary placement succeed.
    fn ordinary_local_counter_aim(game: &mut Game<'_>, x: i32, z: i32) -> [i32; 3] {
        let y = (-63..319)
            .rev()
            .find(|y| game.validate_ordinary_stone_target([x, *y, z], 1).is_ok())
            .expect("loaded generated native support");
        game.player.as_mut().unwrap().position =
            Vec3::new(f64::from(x) + 0.5, f64::from(y) + 1.0, f64::from(z) - 1.5);
        game.yaw = 0.0;
        game.pitch = 45.0;
        game.selected = 0;
        game.sync_player_body();
        let hit = game.pick().expect("actual native terrain ray");
        assert_eq!(hit.face, [0, 1, 0]);
        game.validate_ordinary_stone_target(hit.pos, 1).unwrap();
        hit.pos
    }

    #[test]
    #[ignore = "child only; selected by the ordinary empty-counter three-phase parent"]
    fn ordinary_empty_counter_local_child() {
        use crate::game::retained_server::OrdinaryRequest;
        use horizon_client::menu::Input;
        let phase = std::env::var("HORIZON_EMPTY_COUNTER_PHASE").expect("explicit counter phase");
        assert!(matches!(phase.as_str(), "A" | "B" | "C"));
        let root = PathBuf::from(std::env::var_os("HORIZON_EMPTY_COUNTER_ROOT").unwrap());
        let case = root.join("local");
        if phase == "A" {
            fs::create_dir(&case).unwrap();
        }
        let f = Materialized::new(&format!("empty-local-{phase}"));
        let (startup, tree) = owned_runtime(&f);
        let entry = RetainedEntryIdentity {
            token: 601,
            owner: Arc::new(()),
        };
        let mut request = WorldRequest::new(
            "unused",
            (phase == "A").then_some("Ordinary empty counter"),
            (phase == "A").then_some(17),
            true,
            None,
        )
        .unwrap()
        .with_identity_intent(if phase == "A" {
            IdentityIntent::CreateNewIdentity
        } else {
            IdentityIntent::RequireExistingIdentity
        });
        request.save_root = case.join("world");
        let selection = request.into_ordinary_local(&case, None, until()).unwrap();
        let mut game = match selection.request {
            OrdinaryRequest::Native(request) if phase == "A" => {
                let mut game = local_game(&f, &startup, &tree, &entry, Some(request));
                owned_player(&mut game, &f);
                game.inventory[0] = None;
                game
            }
            OrdinaryRequest::RecoverOwned(request) if phase != "A" => {
                let seed = local_game(&f, &startup, &tree, &entry, None);
                let mut work = LocalOwnedRecovery::start(seed, entry, *request);
                let deadline = until();
                loop {
                    match work.poll_activate().unwrap() {
                        Some(game) => {
                            assert!(work.take_construction().is_some());
                            break game;
                        }
                        None => {
                            assert!(Instant::now() < deadline);
                            std::thread::sleep(Duration::from_millis(1));
                        }
                    }
                }
            }
            _ => panic!("ordinary original route differs"),
        };
        // The same non-rendering loading steps used before ordinary native
        // frames. The cold residency fixture cannot own a recovered world.
        let clock = game.world.game_time;
        let ready_deadline = until();
        while !game.streaming.expect_existing().is_lit(0, 0) {
            game.update_world();
            game.poll_ordinary_native_reads().unwrap();
            assert!(
                game.error.is_none() && Instant::now() < ready_deadline,
                "ordinary recovered native column did not become ready"
            );
            std::thread::yield_now();
        }
        assert_eq!(
            game.world.game_time, clock,
            "loading cannot advance the counter clock"
        );
        let binding =
            crate::game::world_loading::owned_guest_binding(&f.packages, f.pack.assets.registry)
                .unwrap();
        let observe = |game: &Game<'_>| match game.world.guest_runtime_state() {
            Ok(GuestRuntimeState::ReadyActive { counter, slot }) => {
                serde_json::json!({"world":counter.placement().world.to_string(),
                    "position":counter.placement().position,"generation":counter.placement().generation,
                    "value":game.world.read_persistent_counter(&counter).unwrap().counter(),"slot_incarnation":slot.incarnation(),
                    "slot_empty":game.world.read_world_slot(&slot).unwrap().slot.stack().is_none()})
            }
            _ => serde_json::Value::Null,
        };
        assert!(
            !game.world.has_registered_guest_placement_policy(),
            "recovery restores committed state before ordinary arming"
        );
        assert!(!game.ordinary_counter_use_ready());
        let domain_before = game.world.counter_domain_snapshot();
        let committed_before = observe(&game);
        let original = game.session_lock.take().unwrap();
        assert!(
            !game.arm_ordinary_owned_counter().unwrap(),
            "no actual OWN cannot arm"
        );
        game.session_lock = Some(original);
        if phase != "A" {
            assert!(
                game.world.chunks.contains_key(&(0, 0)),
                "verified recovery already owns the complete native support column"
            );
            ordinary_local_counter_aim(&mut game, 6, 6);
            assert!(
                !game.ordinary_stone_use_admitted(),
                "counter input waits for policy"
            );
            assert!(game.use_item().unwrap().is_none());
        }
        assert!(game.arm_ordinary_owned_counter().unwrap());
        assert!(
            game.arm_ordinary_owned_counter().unwrap(),
            "same binding arming is idempotent"
        );
        assert!(game.world.has_registered_guest_placement_policy());
        assert_eq!(
            game.world.counter_domain_snapshot(),
            domain_before,
            "arming cannot allocate history"
        );
        assert_eq!(
            observe(&game),
            committed_before,
            "arming cannot reset committed state or mint another slot"
        );
        if phase == "A" {
            assert!(game.world.counter_domain_snapshot().is_none());
            game.session_lock
                .as_ref()
                .unwrap()
                .validate_owned_unplaced(&game.world, &binding)
                .unwrap();
            let original = game.inventory.clone();
            game.screen = crate::game::Screen::Creative;
            game.creative_screen.selected = game
                .creative_screen
                .tabs
                .iter()
                .position(|tab| !tab.search && !tab.inventory)
                .unwrap();
            game.creative_screen.scroll = 1.0;
            let items = game.creative_screen.items(&game.pack.names);
            let index = items
                .iter()
                .position(|stack| stack.item.as_ref() == f.alias)
                .unwrap();
            let first = game.creative_screen.first_row(&game.pack.names);
            let display = crate::game::Creative::DISPLAY + index - first * 9;
            assert!(
                (crate::game::Creative::DISPLAY..crate::game::Creative::DISPLAY + 45)
                    .contains(&display)
            );
            assert!(
                !game.menu_click(Some(display), 0, Input::Pickup),
                "counter carried pickup closed"
            );
            assert!(
                game.menu_click(Some(display), 0, Input::Swap),
                "existing native hotbar selection"
            );
            assert!(game.carried.is_none());
            assert!(game.ordinary_counter_stack(game.inventory[0].as_ref().unwrap()));
            assert_eq!(game.inventory[1..], original[1..]);
            assert!(
                !game.menu_click(Some(0), 0, Input::Pickup),
                "counter pickup stays closed"
            );
            game.screen = crate::game::Screen::Playing;
            assert!(game.world.counter_domain_snapshot().is_none());
        } else {
            let previous = if phase == "B" { "A" } else { "B" };
            let expected: serde_json::Value = serde_json::from_slice(
                &fs::read(case.join(format!("{previous}-counter.json"))).unwrap(),
            )
            .unwrap();
            assert_eq!(
                observe(&game),
                expected,
                "coherent committed value/identity restored"
            );
            assert_eq!(game.inventory[0].as_ref().unwrap().count, 64);
            assert_eq!(game.inventory[1].as_ref().unwrap().count, 7);
            assert_eq!(game.stats.get("custom", "minecraft:jump"), 7);
            assert!(game.carried.is_none());
            if phase == "B" {
                let support = ordinary_local_counter_aim(&mut game, 2, 2);
                let original = game.native_close_authority.take().unwrap();
                assert!(!game.arm_ordinary_owned_counter().unwrap());
                assert!(!game.ordinary_stone_use_admitted());
                game.native_close_authority = Some(original);
                assert!(game.ordinary_stone_use_admitted());
                assert!(
                    game.use_item().unwrap().is_some(),
                    "normal native BlockItem ingress"
                );
                assert_eq!(
                    game.world.block(support[0], support[1] + 1, support[2]),
                    binding.state_id()
                );
                assert_eq!(observe(&game)["value"], 0);
            }
            let initial = observe(&game)["value"].as_u64().unwrap();
            for _ in 0..3 {
                game.tick().unwrap();
            }
            assert_eq!(
                observe(&game)["value"],
                initial + 3,
                "actual Game ticks run captured Wasm"
            );
            assert_eq!(observe(&game)["slot_empty"], true);
            if phase == "C" {
                let active = observe(&game);
                let support = ordinary_local_counter_aim(&mut game, 4, 4);
                game.use_delay = 0;
                game.use_item().unwrap();
                assert_eq!(
                    game.world.block(support[0], support[1] + 1, support[2]),
                    game.world.air
                );
                assert_eq!(observe(&game), active);
                assert_eq!(game.world.counter_domain_snapshot().unwrap().high_water, 1);
                assert_eq!(game.inventory[0].as_ref().unwrap().count, 64);
            }
            // Restore the original native fixture pose, never a counter/domain value.
            let body = game.player.as_mut().unwrap();
            body.position = Vec3::new(2.5, 120.0, 0.5);
            body.velocity = Vec3::new(0.125, -0.25, 0.5);
            body.ground.on_ground = false;
            body.health.food.exhaustion = 2.25;
            body.health.food.timer = 7;
            body.flying = true;
            game.yaw = 91.0;
            game.pitch = -17.0;
            game.sync_player_body();
        }
        let observed = observe(&game);
        let owned = game.session_lock.as_ref().unwrap().owned().unwrap().clone();
        let record = case
            .join("backups/horizon-owned/authority")
            .join(owned.world().to_string())
            .join("owned.json");
        let bytes = fs::read(&record).unwrap();
        if phase == "A" {
            fs::write(case.join("OWN-original.json"), &bytes).unwrap();
        } else {
            assert_eq!(bytes, fs::read(case.join("OWN-original.json")).unwrap());
        }
        close_local(game, owned.archive(), false);
        assert_eq!(
            epochs(owned.archive()).unwrap().len(),
            match phase.as_str() {
                "A" => 1,
                "B" => 2,
                _ => 3,
            }
        );
        assert_eq!(fs::read(record).unwrap(), bytes);
        fs::write(
            case.join(format!("{phase}-counter.json")),
            serde_json::to_vec_pretty(&observed).unwrap(),
        )
        .unwrap();
        fs::write(root.join(format!("local-{phase}.json")),serde_json::to_vec(&serde_json::json!({"completed":true,"pid":std::process::id(),
            "executable":format!("{:x}",Sha256::digest(fs::read(std::env::current_exe().unwrap()).unwrap()))})).unwrap()).unwrap();
        eprintln!("ORDINARY_EMPTY_COUNTER_LOCAL_{phase}_PASS");
        std::mem::forget(f);
    }

    #[test]
    #[ignore = "isolated captured pair and native resources: actual ordinary LOCAL body, coherent Retry, unpublished recovery and cancellation"]
    fn ordinary_local_actual_body_coherent_retry_recovery_and_cancel() {
        let f = Materialized::new("local-coherent");
        let startup = Arc::new(
            f.packages
                .freeze_peer_inputs(
                    Arc::new(horizon_client::lan::data::SyncData::load(&f.root).unwrap()),
                    f.pack.assets.registry,
                )
                .unwrap()
                .unwrap(),
        );
        let tree = Arc::new(horizon_client::advancements::Tree::load(
            &f.root.join("data"),
        ));
        assert!(!tree.list.is_empty());
        for cancel in [true, false] {
            let authority = f.scratch.join(format!("local-authority-{cancel}"));
            let archive = f.scratch.join(format!("local-archive-{cancel}"));
            let continuation = f.scratch.join(format!("local-continuation-{cancel}"));
            for root in [&authority, &archive, &continuation] {
                fs::create_dir(root).unwrap();
            }
            let namespace = crate::game::world_loading::host_identity::session_authority::AuthorityNamespace::new(&authority).unwrap();
            let root = f.scratch.join(format!("local-world-{cancel}"));
            let entry = RetainedEntryIdentity {
                token: if cancel { 1 } else { 2 },
                owner: Arc::new(()),
            };
            let mut request = WorldRequest::new(
                "unused",
                Some("Local coherent fixture"),
                Some(17),
                true,
                None,
            )
            .unwrap()
            .with_identity_intent(IdentityIntent::CreateNewIdentity)
            .with_session_authority(namespace.clone());
            request.save_root = root.clone();
            let mut game = local_game(&f, &startup, &tree, &entry, Some(request));
            assert!(game.local_checkpoint_owner() && game.native_close_owner_authorized());
            assert!(!game.ui.rules.bool("spawn_mobs"));
            eprintln!("LOCAL_ORIGINAL_CREATED={cancel}");
            residency(&mut game, 2, (0, 0));
            let deadline = until();
            while game.player.is_none() {
                update_native(&mut game);
                game.spawn_player().unwrap();
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
            assert!(!game.restore_player_state);
            residency(&mut game, 2, (0, 0));
            eprintln!("LOCAL_ACTUAL_PLAYER_SPAWNED={cancel}");
            game.load_stats();
            game.load_advancements();
            let body = game.player.as_mut().unwrap();
            body.position = Vec3::new(2.5, 120.0, 0.5);
            body.velocity = Vec3::new(0.125, -0.25, 0.5);
            body.ground.on_ground = false;
            body.health.food.food = 14;
            body.health.food.saturation = 3.25;
            body.health.food.exhaustion = 2.25;
            body.health.food.timer = 7;
            body.internal_set_absorption(1.25).unwrap();
            body.fire_ticks = 23;
            body.air = 245;
            body.may_fly = true;
            body.invulnerable = true;
            body.spectator = false;
            body.flying = true;
            game.yaw = 23.0;
            game.pitch = 11.0;
            game.sync_player_body();
            game.sculk.tracker_loaded = true;
            game.sculk.tracker.ticks_since_last_warning = 123;
            game.inventory = vec![None; 36];
            game.inventory[0] = Some(ItemStack::new(f.alias.as_str(), 3));
            game.inventory[1] = Some(session_items_rich());
            game.ender_items[4] = Some(session_items_rich());
            game.stats
                .values
                .entry("minecraft:custom".into())
                .or_default()
                .insert("minecraft:jump".into(), 7);
            assert_eq!(game.stats.get("custom", "minecraft:jump"), 7);
            let advancement = tree
                .list
                .iter()
                .find(|advancement| !advancement.criteria.is_empty())
                .unwrap();
            let criterion = &advancement.criteria[0].name;
            assert!(game.adv.progress.grant(
                &advancement.id,
                criterion,
                "2026-01-01 00:00:00 +0000"
            ));
            game.arm_registered_guest_placement(&f.type_id).unwrap();
            let stone = game
                .pack
                .assets
                .registry
                .default_state("minecraft:stone")
                .unwrap();
            game.world
                .set(POSITION[0], POSITION[1] - 1, POSITION[2], stone)
                .unwrap();
            let hit = crate::game::Hit {
                pos: [POSITION[0], POSITION[1] - 1, POSITION[2]],
                face: [0, 1, 0],
                point: [2.5, 120.0, 2.5],
            };
            game.use_hand(Some(&hit)).unwrap();
            assert_eq!(
                game.world.block(POSITION[0], POSITION[1], POSITION[2]),
                f.binding().state_id()
            );
            let horizon_client::world::counter_restore::GuestRuntimeState::ReadyActive {
                counter,
                slot,
            } = game.world.guest_runtime_state().unwrap()
            else {
                panic!("actual LOCAL Guest pair absent")
            };
            game.tickets.update(Some(Sources {
                players: vec![(0, 0)],
                forced: Vec::new(),
                view_distance: 2,
                simulation_distance: 2,
            }));
            let calls = crate::game::counter_control::guest_evaluation_calls();
            game.world.game_time += 1;
            game.tick_counter_trial(true);
            assert_eq!(
                crate::game::counter_control::guest_evaluation_calls(),
                calls + 1
            );
            assert_eq!(
                game.world
                    .read_persistent_counter(&counter)
                    .unwrap()
                    .counter(),
                1
            );
            assert!(
                matches!(game.transfer_world_slot_inventory_cell(&slot, 1, Direction::IntoMachine, 3).unwrap(), TransferOutcome::Committed(row) if row.moved==3)
            );
            let mut rich = session_items_rich();
            rich.count = 3;
            retirement_raw_equal(
                game.world
                    .read_world_slot(&slot)
                    .unwrap()
                    .slot
                    .stack()
                    .unwrap(),
                &rich,
            );
            let roster = game.observe_coherent_participants().unwrap();
            let local_rows = game.project_coherent_participants(&roster).unwrap();
            assert_eq!(local_rows.len(), 1);
            assert!(
                crate::game::lan::validate_local_session_player_tag(&local_rows[0].player).is_ok()
            );
            assert!(
                crate::game::lan::validate_session_player_tag(&local_rows[0].player).is_err(),
                "network row policy must remain strict about the local tracker"
            );
            game.adv.loaded = false;
            assert!(game.prepare_coherent_image_context(&archive).is_err());
            game.adv.loaded = true;
            game.stats.loaded = false;
            assert!(game.prepare_coherent_image_context(&archive).is_err());
            game.stats.loaded = true;
            let actual_uuid = game.level.player_uuid;
            let mut wrong_uuid = actual_uuid.unwrap();
            wrong_uuid[0] ^= 1;
            game.level.player_uuid = Some(wrong_uuid);
            assert!(game.prepare_coherent_image_context(&archive).is_err());
            game.level.player_uuid = actual_uuid;
            let untouched_archive = super::super::tree(&archive, &[]).unwrap();
            assert!(
                game.checkpoint_registered_guest_session(&archive, 1, until())
                    .is_err()
            );
            assert_eq!(
                super::super::tree(&archive, &[]).unwrap(),
                untouched_archive
            );
            game.carried = Some(ItemStack::new("minecraft:stone", 1));
            assert!(game.prepare_coherent_image_context(&archive).is_err());
            game.carried = None;
            let actual_source = game.local_checkpoint_source.take().unwrap();
            assert!(game.prepare_coherent_image_context(&archive).is_err());
            game.local_checkpoint_source = Some(actual_source);
            let source =
                OrdinaryRecoverySource::capture(&game, namespace, &archive, &continuation).unwrap();
            // A raw expected UUID cannot enter LOCAL recovery or acquire a save.
            let mut raw = WorldRequest::new("unused", None, None, false, None)
                .unwrap()
                .with_expected_checkpoint_world(source.world);
            raw.save_root = f.scratch.join(format!("raw-local-{cancel}"));
            let mut refused = OrdinaryPreparation::prepare(
                raw,
                &game.res,
                &game.pack,
                &game.options,
                &game.settings,
                &entry,
                startup.clone(),
            );
            assert!(
                refused
                    .original_error()
                    .unwrap()
                    .contains("local-player adapter")
            );
            refused.begin_close(until());
            loop {
                match refused.poll_close() {
                    PreparationCloseStatus::Pending => std::thread::yield_now(),
                    PreparationCloseStatus::Complete => break,
                    PreparationCloseStatus::Blocked(error) => {
                        panic!("raw LOCAL refusal close: {error}")
                    }
                }
            }
            let _closed = refused
                .finish_close()
                .unwrap_or_else(|_| panic!("raw LOCAL refusal completion absent"));
            game.yaw = 91.0;
            game.pitch = -17.0;
            assert!(
                game.project_coherent_participants(&roster).is_err(),
                "last mouse input differs until actual close sync"
            );
            let closed = close_local(game, &archive, !cancel);
            eprintln!("LOCAL_COHERENT_CLOSED={cancel}");
            let native = horizon_client::save::Level::read(&root).unwrap();
            assert_eq!(native.motion, [0.125, -0.25, 0.5]);
            assert_eq!(native.rotation, [91.0, -17.0]);
            let world_before = super::super::tree(&root, &["session.lock"]).unwrap();
            let request = source
                .into_request(
                    closed,
                    f.scratch.join(format!("local-recovered-{cancel}")),
                    until(),
                )
                .unwrap_or_else(|_| panic!("actual LOCAL source completion refused"));
            let candidate = local_game(&f, &startup, &tree, &entry, None);
            assert!(
                candidate.fresh_session_candidate().is_err(),
                "generic LOCAL candidate must remain closed"
            );
            let mut recovery = OrdinaryRecoveryWork::prepare(candidate, entry, request);
            assert!(
                recovery.original_error().is_none(),
                "LOCAL native recovery: {:?}",
                recovery.original_error()
            );
            assert!(recovery.game_mut().native_close_authority.is_none());
            assert!(recovery.game_mut().local_checkpoint_owner());
            eprintln!("LOCAL_NATIVE_RECOVERY_PREPARED={cancel}");
            assert!(
                recovery.game_mut().lan.is_none() && recovery.game_mut().retained_server.is_none()
            );
            if cancel {
                recovery.fail("authored cancellation after LOCAL native acquisition".into());
                recovery.begin_close(until());
                loop {
                    match recovery.poll_close() {
                        PreparationCloseStatus::Pending => std::thread::yield_now(),
                        PreparationCloseStatus::Complete => break,
                        PreparationCloseStatus::Blocked(error) => {
                            panic!("original LOCAL cancellation close: {error}")
                        }
                    }
                }
                let candidate = recovery.game_mut();
                assert!(
                    candidate.save.is_none()
                        && candidate.streaming.is_vacant()
                        && candidate.session_lock.is_none()
                );
                assert!(
                    candidate.native_close_authority.is_none()
                        && candidate.local_checkpoint_source.is_none()
                );
                assert_eq!(
                    super::super::tree(&root, &["session.lock"]).unwrap(),
                    world_before
                );
                eprintln!("LOCAL_ORIGINAL_CANCEL_PASS");
            } else {
                let game = recovery
                    .try_activate()
                    .unwrap()
                    .expect("actual LOCAL activation without fake Hub");
                assert!(game.native_close_owner_authorized() && game.local_checkpoint_owner());
                assert_eq!(
                    game.local_continuation_archive.as_deref(),
                    Some(continuation.as_path())
                );
                let body = game.player.as_ref().unwrap();
                assert!(!body.server_copy);
                assert_eq!(body.velocity, Vec3::new(0.125, -0.25, 0.5));
                assert!(!body.ground.on_ground);
                assert_eq!([body.yaw, body.pitch], [91.0, -17.0]);
                assert_eq!(body.health.food.exhaustion, 2.25);
                assert_eq!(body.health.food.timer, 7);
                assert_eq!(body.fire_ticks, 23);
                assert_eq!(body.air, 245);
                assert_eq!(body.health.absorption(), 1.25);
                assert_eq!(game.stats.get("custom", "minecraft:jump"), 7);
                assert_eq!(game.sculk.tracker.ticks_since_last_warning, 123);
                assert!(game.adv.progress.has(&advancement.id, criterion));
                let horizon_client::world::counter_restore::GuestRuntimeState::ReadyActive {
                    counter,
                    slot,
                } = game.world.guest_runtime_state().unwrap()
                else {
                    panic!("restored original pair absent")
                };
                assert_eq!(
                    game.world
                        .read_persistent_counter(&counter)
                        .unwrap()
                        .counter(),
                    1
                );
                retirement_raw_equal(
                    game.world
                        .read_world_slot(&slot)
                        .unwrap()
                        .slot
                        .stack()
                        .unwrap(),
                    &rich,
                );
                rich.count = 4;
                retirement_raw_equal(game.inventory[1].as_ref().unwrap(), &rich);
                rich.count = 7;
                retirement_raw_equal(game.ender_items[4].as_ref().unwrap(), &rich);
                let _next_closed = close_local(game, &continuation, false);
                assert_eq!(epochs(&continuation).unwrap().len(), 1);
                eprintln!("LOCAL_ACTUAL_BODY_RETRY_RECOVERY_CONTINUATION_PASS");
            }
        }
        // Retain original controlled failed publication and native worlds. Do not
        // traverse/copy materialized resource links to preserve them.
        eprintln!("LOCAL_NATIVE_EVIDENCE_ROOT={}", f.scratch.display());
        std::mem::forget(f);
    }
}
