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
