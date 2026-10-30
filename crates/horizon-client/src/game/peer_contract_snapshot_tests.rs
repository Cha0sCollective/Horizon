//! Defining data-only controls. Resource cases consume an existing prebuilt Rust
//! pair; no guest compiler, evaluator, subprocess, or process environment mutation.
use super::*;
use horizon_client::lan::data::RegistrySnapshot;
#[cfg(all(feature = "paired-packages", feature = "mods"))]
use horizon_client::lan::data::SyncData;
#[cfg(all(feature = "paired-packages", feature = "mods"))]
use horizon_world::registry::BlockRegistry;

fn registries() -> RegistrySnapshot {
    RegistrySnapshot {
        builtin: [1; 32],
        blocks: [2; 32],
        synchronized: [3; 32],
        tags: [4; 32],
        features: [5; 32],
    }
}
fn snapshot() -> PeerSnapshot {
    PeerSnapshot {
        domain: "horizon-peer-snapshot-1/package",
        protocol: horizon_protocol::PROTOCOL_VERSION,
        game: horizon_protocol::GAME_VERSION.into(),
        build: "sha256:test".into(),
        package_scheme: "paired-folder-sha256-1".into(),
        package: ("example".into(), "captured-content".into()),
        schema: ("example:counter".into(), 1),
        registries: registries(),
    }
}
fn selected_pins() -> Packages {
    Packages {
        #[cfg(feature = "paired-packages")]
        catalog: None,
        build: "test-executable".into(),
        identities: BTreeMap::from([("counter".into(), "test-package".into())]),
    }
}

#[test]
fn comparison_reports_specific_components_without_granting_network_authority() {
    let a = snapshot();
    assert_eq!(a.compare(&a.clone()), Ok(()));
    let controls: Vec<(&str, PeerSnapshot)> = vec![
        ("protocol/game version", {
            let mut s = a.clone();
            s.protocol += 1;
            s
        }),
        ("exact executable", {
            let mut s = a.clone();
            s.build.push('x');
            s
        }),
        ("captured package/content", {
            let mut s = a.clone();
            s.package.1.push('x');
            s
        }),
        ("machine save schema", {
            let mut s = a.clone();
            s.schema.1 += 1;
            s
        }),
        ("builtin registry", {
            let mut s = a.clone();
            s.registries.builtin[0] ^= 1;
            s
        }),
        ("runtime block states/defaults", {
            let mut s = a.clone();
            s.registries.blocks[0] ^= 1;
            s
        }),
        ("synchronized registry data", {
            let mut s = a.clone();
            s.registries.synchronized[0] ^= 1;
            s
        }),
        ("configuration tags", {
            let mut s = a.clone();
            s.registries.tags[0] ^= 1;
            s
        }),
        ("enabled features", {
            let mut s = a.clone();
            s.registries.features[0] ^= 1;
            s
        }),
    ];
    for (reason, b) in controls {
        assert_eq!(a.compare(&b), Err(reason));
        assert_eq!(b.compare(&a), Err(reason));
    }
    Packages::default().external_network().unwrap();
    assert!(selected_pins().external_network().is_err());
    assert_eq!(a, snapshot());
}

#[test]
fn local_wire_projection_requires_exact_snapshot_scheme_and_captured_hex() {
    use horizon_client::lan::peer_contract::{ContractMismatch, WireContract};
    // Synthetic projection inputs exercise spelling/field mapping, not local
    // executable capture or a successful frozen-input constructor.
    let mut local = snapshot();
    local.build = format!("sha256:{}", "01".repeat(32));
    local.package.1 = "23".repeat(32);
    let wire = local.to_wire_contract().unwrap();
    assert_eq!(wire.claims().build, [1; 32]);
    assert_eq!(wire.claims().package, [0x23; 32]);
    assert_eq!(wire.claims().registries, &local.registries);
    let decoded = WireContract::decode(&wire.encode().unwrap()).unwrap();
    assert_eq!(wire.compare(&decoded), Ok(()));
    assert!(selected_pins().external_network().is_err());
    for build in [
        "sha256:test".to_owned(),
        "01".repeat(32),
        format!("sha256:{}", "AA".repeat(32)),
        format!("sha256:{}", "g0".repeat(32)),
    ] {
        let mut changed = local.clone();
        changed.build = build;
        assert!(changed.to_wire_contract().is_err());
    }
    for digest in [
        "f".repeat(63),
        "FF".repeat(32),
        "gg".repeat(32),
        "é".repeat(32),
    ] {
        let mut changed = local.clone();
        changed.package.1 = digest;
        assert!(changed.to_wire_contract().is_err());
    }
    let mut changed = local.clone();
    changed.domain = "future-snapshot";
    assert!(changed.to_wire_contract().is_err());
    let mut changed = local.clone();
    changed.package_scheme.push('2');
    assert!(changed.to_wire_contract().is_err());
    let mut changed = local.clone();
    changed.schema.1 = 2;
    assert!(changed.to_wire_contract().is_err());
    let mut changed = local;
    changed.registries.features[0] ^= 1;
    assert_eq!(
        wire.compare(&changed.to_wire_contract().unwrap()),
        Err(ContractMismatch::Features)
    );
}

#[cfg(all(feature = "paired-packages", feature = "mods"))]
mod resource {
    use super::*;
    use horizon_client::mod_content::{ContentMod, resource_root};
    use sha2::{Digest, Sha256};

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "horizon-peer-snapshot-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            assert_eq!(self.0.parent(), Some(std::env::temp_dir().as_path()));
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn tree(root: &Path) -> BTreeMap<PathBuf, [u8; 32]> {
        fn visit(root: &Path, path: &Path, out: &mut BTreeMap<PathBuf, [u8; 32]>) {
            let mut entries: Vec<_> = fs::read_dir(path)
                .unwrap()
                .map(|e| e.unwrap().path())
                .collect();
            entries.sort();
            for entry in entries {
                let meta = fs::symlink_metadata(&entry).unwrap();
                assert!(
                    !meta.file_type().is_symlink(),
                    "prebuilt pair fixture must contain regular inputs"
                );
                if meta.is_dir() {
                    visit(root, &entry, out);
                } else {
                    assert!(meta.is_file());
                    out.insert(
                        entry.strip_prefix(root).unwrap().into(),
                        Sha256::digest(fs::read(entry).unwrap()).into(),
                    );
                }
            }
        }
        let mut out = BTreeMap::new();
        visit(root, root, &mut out);
        out
    }
    fn copy_tree(source: &Path, target: &Path) {
        fs::create_dir(target).unwrap();
        for relative in tree(source).keys() {
            let out = target.join(relative);
            fs::create_dir_all(out.parent().unwrap()).unwrap();
            fs::copy(source.join(relative), out).unwrap();
        }
    }
    fn materialize(packages: &Packages, base: &Path, cache: &Path) -> PathBuf {
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
    fn load(root: &Path) -> (SyncData, BlockRegistry) {
        (
            SyncData::load(root).unwrap(),
            BlockRegistry::from_facts_json(fs::File::open(root.join("facts.json")).unwrap())
                .unwrap(),
        )
    }
    fn captured(path: &Path) -> Packages {
        // Existing typed preflight may execute metadata exports. This case does
        // not activate a runtime or invoke any counter evaluator.
        Packages::read(&BTreeSet::new(), &[path.into()]).unwrap()
    }

    #[test]
    #[ignore = "isolated pinned HORIZON_TEST_RESOURCES and prebuilt HORIZON_COUNTER_PAIR_FIXTURE"]
    fn actual_captured_pair_materialized_tables_and_frozen_snapshot_controls() {
        let base = PathBuf::from(
            std::env::var_os("HORIZON_TEST_RESOURCES").expect("pinned resources required"),
        );
        let pair = PathBuf::from(
            std::env::var_os("HORIZON_COUNTER_PAIR_FIXTURE").expect("prebuilt Rust pair required"),
        );
        let before_pair = tree(&pair);
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
        let before_sources = [
            "lan/data.rs",
            "game/mod_packages.rs",
            "game/peer_contract_snapshot_tests.rs",
        ]
        .map(|p| fs::read(source.join(p)).unwrap());
        let before_base =
            ["facts.json", "registries.json"].map(|p| fs::read(base.join(p)).unwrap());
        let scratch = Scratch::new();
        let a = captured(&pair);
        let b = captured(&pair);
        assert!(!Arc::ptr_eq(
            a.catalog.as_ref().unwrap(),
            b.catalog.as_ref().unwrap()
        ));
        let root_a = materialize(&a, &base, &scratch.0.join("cache-a"));
        let root_b = materialize(&b, &base, &scratch.0.join("cache-b"));
        let (sync_a, blocks_a) = load(&root_a);
        let (mut sync_b, blocks_b) = load(&root_b);
        let tables_before =
            ["facts.json", "registries.json"].map(|p| fs::read(root_a.join(p)).unwrap());
        let first = a.peer_snapshot(&sync_a, &blocks_a).unwrap().unwrap();
        let second = b.peer_snapshot(&sync_b, &blocks_b).unwrap().unwrap();
        assert_eq!(first.compare(&second), Ok(()));
        assert_eq!(first.schema.1, 1);
        assert_eq!(
            first.schema.0,
            a.catalog()
                .unwrap()
                .prepared()
                .values()
                .next()
                .unwrap()
                .content()
                .machine_types()
                .types()[0]
                .id()
        );
        assert!(
            Packages::default()
                .peer_snapshot(&sync_a, &blocks_a)
                .unwrap()
                .is_none()
        );
        assert!(a.external_network().is_err() && b.external_network().is_err());
        let unmarked = scratch.0.join("unmarked-world");
        fs::create_dir(&unmarked).unwrap();
        fs::write(unmarked.join("level.dat"), b"untouched world control").unwrap();
        assert_eq!(first.compare(&second), Ok(()));
        assert!(a.check_world(&unmarked).is_err() && b.record_world(&unmarked).is_err());
        assert_eq!(
            fs::read(unmarked.join("level.dat")).unwrap(),
            b"untouched world control"
        );
        assert!(!unmarked.join(WORLD_PACKAGES).exists());
        let pinned = scratch.0.join("pinned-world");
        fs::create_dir(&pinned).unwrap();
        a.record_world(&pinned).unwrap();
        let pin_before = fs::read(pinned.join(WORLD_PACKAGES)).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&pin_before).unwrap()["format"],
            1
        );
        assert!(Packages::default().check_world(&pinned).is_err());
        b.check_world(&pinned).unwrap();
        assert_eq!(fs::read(pinned.join(WORLD_PACKAGES)).unwrap(), pin_before);
        sync_b.features.push("example:snapshot_control".into());
        let changed = b.peer_snapshot(&sync_b, &blocks_b).unwrap().unwrap();
        assert_eq!(first.compare(&changed), Err("enabled features"));
        assert_eq!(
            first, second,
            "old owned value survives source-table change"
        );
        let mut missing = b.clone();
        missing.catalog = None;
        assert!(missing.peer_snapshot(&sync_a, &blocks_a).is_err());
        let mut multiple = b.clone();
        multiple.identities.insert("extra".into(), "0".repeat(64));
        assert!(multiple.peer_snapshot(&sync_a, &blocks_a).is_err());
        let legacy = scratch.0.join("extra-legacy");
        fs::create_dir(&legacy).unwrap();
        fs::write(
            legacy.join("horizon-mod.toml"),
            "id='snapshot_extra'\nname='Snapshot extra'\nversion='1'\napi='0.1'\ncapabilities=[]",
        )
        .unwrap();
        fs::write(legacy.join("mod.wasm"), b"unexecuted legacy control").unwrap();
        let extra = Packages::read(&BTreeSet::new(), &[pair.clone(), legacy]).unwrap();
        assert!(
            extra
                .peer_snapshot(&sync_a, &blocks_a)
                .unwrap_err()
                .contains("extra/legacy/unknown")
        );
        let mut changed_build = b.clone();
        changed_build
            .build
            .replace_range(7..8, if &b.build[7..8] == "0" { "1" } else { "0" });
        assert_eq!(
            first.compare(
                &changed_build
                    .peer_snapshot(&sync_a, &blocks_a)
                    .unwrap()
                    .unwrap()
            ),
            Err("exact executable")
        );
        let altered = scratch.0.join("altered-pair");
        copy_tree(&pair, &altered);
        let prepared = a.catalog().unwrap().prepared().values().next().unwrap();
        let machine_file = prepared
            .content()
            .files
            .iter()
            .find(|p| p.contains("/horizon/machine/"))
            .unwrap();
        // A whitespace-only real content edit keeps semantics/current schema but
        // changes the actual captured content identity under the same package.
        let mut bytes = fs::read(altered.join(machine_file)).unwrap();
        bytes.push(b'\n');
        fs::write(altered.join(machine_file), bytes).unwrap();
        let changed_content = captured(&altered);
        assert_eq!(
            first.compare(
                &changed_content
                    .peer_snapshot(&sync_a, &blocks_a)
                    .unwrap()
                    .unwrap()
            ),
            Err("captured package/content")
        );
        let mut bytes = fs::read(altered.join(machine_file)).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["save-schema"], 1);
        let mut json = json;
        json["save-schema"] = serde_json::json!(2);
        bytes = serde_json::to_vec(&json).unwrap();
        fs::write(altered.join(machine_file), bytes).unwrap();
        assert!(Packages::read(&BTreeSet::new(), &[altered]).is_err());
        assert_eq!(tree(&pair), before_pair);
        assert_eq!(
            ["facts.json", "registries.json"].map(|p| fs::read(base.join(p)).unwrap()),
            before_base
        );
        assert_eq!(
            ["facts.json", "registries.json"].map(|p| fs::read(root_a.join(p)).unwrap()),
            tables_before
        );
        assert_eq!(
            [
                "lan/data.rs",
                "game/mod_packages.rs",
                "game/peer_contract_snapshot_tests.rs"
            ]
            .map(|p| fs::read(source.join(p)).unwrap()),
            before_sources
        );
    }

    #[test]
    #[ignore = "isolated pinned HORIZON_TEST_RESOURCES and prebuilt HORIZON_COUNTER_PAIR_FIXTURE"]
    fn actual_frozen_input_owners_wire_data_and_mutation_controls() {
        use horizon_client::lan::peer_contract::{ContractMismatch, WireContract};
        use horizon_world::{nbt::Tag, registry::BlockStateId};
        let base = PathBuf::from(
            std::env::var_os("HORIZON_TEST_RESOURCES").expect("pinned resources required"),
        );
        let pair = PathBuf::from(
            std::env::var_os("HORIZON_COUNTER_PAIR_FIXTURE").expect("prebuilt Rust pair required"),
        );
        let before_pair = tree(&pair);
        let before_base =
            ["facts.json", "registries.json"].map(|p| fs::read(base.join(p)).unwrap());
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
        let paths = [
            "lan/peer_contract.rs",
            "lan/mod.rs",
            "game/mod_packages.rs",
            "game/peer_contract_snapshot_tests.rs",
            "lan/data.rs",
        ];
        let before_sources = paths.map(|p| fs::read(source.join(p)).unwrap());
        let scratch = Scratch::new();
        // Change only a scratch copy later; the provenance fixture stays read-only.
        let local_pair = scratch.0.join("captured-pair");
        copy_tree(&pair, &local_pair);
        let a = captured(&local_pair);
        let b = captured(&local_pair);
        assert!(!Arc::ptr_eq(
            a.catalog.as_ref().unwrap(),
            b.catalog.as_ref().unwrap()
        ));
        let root_a = materialize(&a, &base, &scratch.0.join("cache-a"));
        let root_b = materialize(&b, &base, &scratch.0.join("cache-b"));
        let tables_before =
            ["facts.json", "registries.json"].map(|p| fs::read(root_a.join(p)).unwrap());
        let (sync_a, blocks_a) = load(&root_a);
        let (sync_b, blocks_b) = load(&root_b);
        // The production registry is immutable/static too. Leak only these
        // finite test registries so the exact lifetime contract is exercised.
        let blocks_a: &'static BlockRegistry = Box::leak(Box::new(blocks_a));
        let blocks_b: &'static BlockRegistry = Box::leak(Box::new(blocks_b));
        let sync_a = Arc::new(sync_a);
        let sync_b = Arc::new(sync_b);
        let first = a
            .freeze_peer_inputs(sync_a.clone(), blocks_a)
            .unwrap()
            .unwrap();
        let second = b
            .freeze_peer_inputs(sync_b.clone(), blocks_b)
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(&first.sync, &sync_a));
        assert!(Arc::ptr_eq(
            first.packages.catalog.as_ref().unwrap(),
            a.catalog.as_ref().unwrap()
        ));
        assert!(std::ptr::eq(first.sync(), sync_a.as_ref()));
        assert!(std::ptr::eq(first.blocks(), blocks_a));
        assert!(!Arc::ptr_eq(&first.sync, &second.sync));
        let encoded = first.wire_contract().encode().unwrap();
        assert_eq!(encoded, second.wire_contract().encode().unwrap());
        assert_eq!(first.snapshot.compare(&second.snapshot), Ok(()));
        let decoded = WireContract::decode(&encoded).unwrap();
        assert_eq!(first.wire_contract().compare(&decoded), Ok(()));
        assert_eq!(decoded.claims().machine_id, first.snapshot.schema.0);
        assert_eq!(
            decoded.claims().package,
            *first
                .packages
                .catalog()
                .unwrap()
                .prepared()
                .values()
                .next()
                .unwrap()
                .identity()
        );
        // Independent measurement of the actual current test executable; this
        // digest is never an argument to freeze_peer_inputs.
        let mut executable = fs::File::open(std::env::current_exe().unwrap()).unwrap();
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 128 << 10];
        loop {
            let count = executable.read(&mut buffer).unwrap();
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
        }
        let actual_build: [u8; 32] = hasher.finalize().into();
        assert_eq!(decoded.claims().build, actual_build);
        assert_eq!(
            captured_hex_digest(first.packages.build.strip_prefix("sha256:").unwrap()).unwrap(),
            actual_build
        );
        let catalog_weak = Arc::downgrade(a.catalog.as_ref().unwrap());
        let sync_weak = Arc::downgrade(&sync_a);
        let mut shared = sync_a.clone();
        assert!(Arc::get_mut(&mut shared).is_none());
        drop(shared);
        drop(sync_a);
        drop(a);
        assert!(Arc::ptr_eq(&sync_weak.upgrade().unwrap(), &first.sync));
        assert!(Arc::ptr_eq(
            &catalog_weak.upgrade().unwrap(),
            first.packages.catalog.as_ref().unwrap()
        ));
        assert_eq!(first.wire_contract().encode().unwrap(), encoded);
        assert!(std::ptr::eq(first.blocks(), blocks_a));

        // An independently loaded mutable owner can change. It cannot replace
        // the exact Arc already held by either frozen value.
        let mut controls = Arc::new(SyncData::load(&root_b).unwrap());
        Arc::get_mut(&mut controls)
            .unwrap()
            .features
            .push("example:wire_control".into());
        let changed = b
            .freeze_peer_inputs(controls.clone(), blocks_b)
            .unwrap()
            .unwrap();
        assert_eq!(
            first.wire_contract().compare(changed.wire_contract()),
            Err(ContractMismatch::Features)
        );
        drop(changed);
        Arc::get_mut(&mut controls).unwrap().features.pop();
        let (registry_index, tag_index) = controls
            .tags
            .registries
            .iter()
            .enumerate()
            .find_map(|(r, registry)| {
                registry
                    .tags
                    .iter()
                    .position(|t| !t.ids.is_empty())
                    .map(|t| (r, t))
            })
            .unwrap();
        let removed = Arc::get_mut(&mut controls).unwrap().tags.registries[registry_index].tags
            [tag_index]
            .ids
            .pop()
            .unwrap();
        let changed = b
            .freeze_peer_inputs(controls.clone(), blocks_b)
            .unwrap()
            .unwrap();
        assert_eq!(
            first.wire_contract().compare(changed.wire_contract()),
            Err(ContractMismatch::Tags)
        );
        drop(changed);
        Arc::get_mut(&mut controls).unwrap().tags.registries[registry_index].tags[tag_index]
            .ids
            .push(removed);

        let ids = Arc::get_mut(&mut controls)
            .unwrap()
            .builtin
            .registries
            .get_mut("minecraft:item")
            .unwrap();
        let names = [ids.names[0].clone(), ids.names[1].clone()];
        ids.names.swap(0, 1);
        ids.ids.insert(names[0].clone(), 1);
        ids.ids.insert(names[1].clone(), 0);
        let changed = b
            .freeze_peer_inputs(controls.clone(), blocks_b)
            .unwrap()
            .unwrap();
        assert_eq!(
            first.wire_contract().compare(changed.wire_contract()),
            Err(ContractMismatch::Builtin)
        );
        drop(changed);
        let ids = Arc::get_mut(&mut controls)
            .unwrap()
            .builtin
            .registries
            .get_mut("minecraft:item")
            .unwrap();
        ids.names.swap(0, 1);
        ids.ids.insert(names[0].clone(), 0);
        ids.ids.insert(names[1].clone(), 1);

        let dynamic = Arc::get_mut(&mut controls)
            .unwrap()
            .dynamic
            .registries
            .get_mut("minecraft:worldgen/biome")
            .unwrap();
        dynamic.swap(0, 1);
        assert!(b.freeze_peer_inputs(controls.clone(), blocks_b).is_err());
        Arc::get_mut(&mut controls)
            .unwrap()
            .dynamic
            .registries
            .get_mut("minecraft:worldgen/biome")
            .unwrap()
            .swap(0, 1);
        Arc::get_mut(&mut controls)
            .unwrap()
            .dynamic
            .registries
            .get_mut("minecraft:worldgen/biome")
            .unwrap()[0]
            .1 = Some(Tag::Int(123));
        assert!(b.freeze_peer_inputs(controls.clone(), blocks_b).is_err());
        Arc::get_mut(&mut controls)
            .unwrap()
            .dynamic
            .registries
            .get_mut("minecraft:worldgen/biome")
            .unwrap()[0]
            .1 = None;
        let restored = b
            .freeze_peer_inputs(controls.clone(), blocks_b)
            .unwrap()
            .unwrap();
        assert_eq!(
            first.wire_contract().compare(restored.wire_contract()),
            Ok(())
        );
        drop(restored);

        // Change exactly one actual default while preserving states and IDs.
        let (name, old, replacement) = (0..blocks_b.len())
            .find_map(|index| {
                let id = BlockStateId(index as u32);
                let state = blocks_b.state(id).unwrap();
                let default = blocks_b.default_state(&state.name).unwrap();
                (default != id).then(|| (state.name.clone(), default, id))
            })
            .expect("actual resource block with multiple states");
        let changed_blocks: &'static BlockRegistry = {
            let mut facts: serde_json::Value =
                serde_json::from_slice(&fs::read(root_b.join("facts.json")).unwrap()).unwrap();
            assert_eq!(facts["defaults"][&name], serde_json::json!(old.0));
            facts["defaults"][&name] = serde_json::json!(replacement.0);
            let bytes = serde_json::to_vec(&facts).unwrap();
            Box::leak(Box::new(
                BlockRegistry::from_facts_json(bytes.as_slice()).unwrap(),
            ))
        };
        let changed = b
            .freeze_peer_inputs(controls.clone(), changed_blocks)
            .unwrap()
            .unwrap();
        assert_eq!(
            first.wire_contract().compare(changed.wire_contract()),
            Err(ContractMismatch::Blocks)
        );
        drop(changed);

        let machine_file = b
            .catalog()
            .unwrap()
            .prepared()
            .values()
            .next()
            .unwrap()
            .content()
            .files
            .iter()
            .find(|p| p.contains("/horizon/machine/"))
            .unwrap()
            .clone();
        let mut bytes = fs::read(local_pair.join(&machine_file)).unwrap();
        bytes.push(b'\n');
        fs::write(local_pair.join(&machine_file), bytes).unwrap();
        let recaptured = captured(&local_pair);
        let changed = recaptured
            .freeze_peer_inputs(controls.clone(), blocks_b)
            .unwrap()
            .unwrap();
        assert_eq!(
            first.wire_contract().compare(changed.wire_contract()),
            Err(ContractMismatch::PackageContent)
        );
        drop(changed);
        assert_eq!(first.wire_contract().encode().unwrap(), encoded);
        assert_eq!(second.wire_contract().encode().unwrap(), encoded);

        // Explicitly synthetic build-field control, not a second executable.
        let mut synthetic_build = b.clone();
        synthetic_build
            .build
            .replace_range(7..8, if &b.build[7..8] == "0" { "1" } else { "0" });
        let changed = synthetic_build
            .freeze_peer_inputs(controls.clone(), blocks_b)
            .unwrap()
            .unwrap();
        assert_eq!(
            first.wire_contract().compare(changed.wire_contract()),
            Err(ContractMismatch::Executable)
        );
        drop(changed);
        let mut missing = b.clone();
        missing.catalog = None;
        assert!(
            missing
                .freeze_peer_inputs(controls.clone(), blocks_b)
                .is_err()
        );
        let mut multiple = b.clone();
        multiple.identities.insert("extra".into(), "0".repeat(64));
        assert!(
            multiple
                .freeze_peer_inputs(controls.clone(), blocks_b)
                .is_err()
        );
        let legacy = scratch.0.join("wire-extra-legacy");
        fs::create_dir(&legacy).unwrap();
        fs::write(
            legacy.join("horizon-mod.toml"),
            "id='wire_extra'\nname='Wire extra'\nversion='1'\napi='0.1'\ncapabilities=[]",
        )
        .unwrap();
        fs::write(legacy.join("mod.wasm"), b"unexecuted legacy control").unwrap();
        let extra = Packages::read(&BTreeSet::new(), &[local_pair, legacy]).unwrap();
        assert!(
            extra
                .freeze_peer_inputs(controls.clone(), blocks_b)
                .is_err()
        );
        Arc::get_mut(&mut controls)
            .unwrap()
            .features
            .push("minecraft:vanilla".into());
        assert!(b.freeze_peer_inputs(controls.clone(), blocks_b).is_err());
        assert!(
            Packages::default()
                .freeze_peer_inputs(controls, blocks_b)
                .unwrap()
                .is_none(),
            "no-pair capture does not inspect even inconsistent registry inputs"
        );

        // Decoded equality never changes the old external or world-pin guards.
        assert_eq!(first.wire_contract().compare(&decoded), Ok(()));
        assert!(
            first.packages.external_network().is_err()
                && second.packages.external_network().is_err()
        );
        let unmarked = scratch.0.join("unmarked-wire-world");
        fs::create_dir(&unmarked).unwrap();
        fs::write(unmarked.join("level.dat"), b"untouched").unwrap();
        assert!(first.packages.check_world(&unmarked).is_err());
        assert!(first.packages.record_world(&unmarked).is_err());
        assert_eq!(fs::read(unmarked.join("level.dat")).unwrap(), b"untouched");
        assert!(!unmarked.join(WORLD_PACKAGES).exists());
        let pinned = scratch.0.join("pinned-wire-world");
        fs::create_dir(&pinned).unwrap();
        first.packages.record_world(&pinned).unwrap();
        let pin_before = fs::read(pinned.join(WORLD_PACKAGES)).unwrap();
        assert!(Packages::default().check_world(&pinned).is_err());
        second.packages.check_world(&pinned).unwrap();
        assert_eq!(fs::read(pinned.join(WORLD_PACKAGES)).unwrap(), pin_before);
        drop(b);
        drop(sync_b);
        assert_eq!(second.wire_contract().encode().unwrap(), encoded);
        drop(first);
        assert!(catalog_weak.upgrade().is_none());
        assert!(sync_weak.upgrade().is_none());
        assert_eq!(tree(&pair), before_pair);
        assert_eq!(
            ["facts.json", "registries.json"].map(|p| fs::read(base.join(p)).unwrap()),
            before_base
        );
        assert_eq!(
            ["facts.json", "registries.json"].map(|p| fs::read(root_a.join(p)).unwrap()),
            tables_before
        );
        assert_eq!(
            paths.map(|p| fs::read(source.join(p)).unwrap()),
            before_sources
        );
    }
}
