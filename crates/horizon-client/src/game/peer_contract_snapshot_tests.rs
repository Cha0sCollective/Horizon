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
}
