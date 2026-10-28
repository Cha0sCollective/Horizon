#![cfg(all(feature = "paired-packages", feature = "runtime", feature = "pack"))]
//! Actual SDK components exercise the role and storage boundary. Dummy functions
//! are deliberately unreachable to distinguish type preflight from execution.
use horizon_mods::{
    Capability, GameMode, Mods, Output, Runs, ServerPlayer, Snapshot, Status,
    manifest::{STORAGE_DIR, componentize},
    paired::{DESCRIPTOR, PreparedBundle},
};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

const DESCR: &str = r#"
format=1
host-contract='paired-preview-1'
[content]
id='paired_activation'
name='Paired activation'
version='0'
api='0.2'
side='both'
capabilities=['registries']
[client]
entry='client.wasm'
api='0.1'
capabilities=['chat','storage']
[server]
entry='server.wasm'
api='0.3'
capabilities=['storage','players','world']
"#;

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "horizon-paired-activation-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn components() -> &'static (Vec<u8>, Vec<u8>) {
    static CODE: OnceLock<(Vec<u8>, Vec<u8>)> = OnceLock::new();
    CODE.get_or_init(|| {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let target = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("target"))
            .join("paired-guest");
        let status = Command::new(env!("CARGO"))
            .current_dir(&root)
            .args([
                "build",
                "--offline",
                "--release",
                "--target",
                "wasm32-unknown-unknown",
                "-p",
                "horizon-mods-test-guest",
                "-p",
                "welcome",
                "--target-dir",
            ])
            .arg(&target)
            .status()
            .unwrap();
        assert!(status.success());
        let built = target.join("wasm32-unknown-unknown/release");
        let components = (
            componentize(&fs::read(built.join("horizon_mods_test_guest.wasm")).unwrap()).unwrap(),
            componentize(&fs::read(built.join("welcome.wasm")).unwrap()).unwrap(),
        );
        let fixtures = target.join("components");
        fs::create_dir_all(&fixtures).unwrap();
        fs::write(fixtures.join("client.wasm"), &components.0).unwrap();
        fs::write(fixtures.join("server.wasm"), &components.1).unwrap();
        components
    })
}
fn pair(root: &Path, client: &[u8], server: &[u8]) -> (PathBuf, Arc<PreparedBundle>) {
    let path = root.join("package");
    fs::create_dir(&path).unwrap();
    fs::write(path.join(DESCRIPTOR), DESCR).unwrap();
    fs::write(path.join("client.wasm"), client).unwrap();
    fs::write(path.join("server.wasm"), server).unwrap();
    let snapshot = Arc::new(PreparedBundle::read_folder(&path).unwrap());
    (path, snapshot)
}
fn load(role: Runs, path: &Path, pair: Arc<PreparedBundle>) -> Mods {
    Mods::load_for_with_prepared(
        role,
        &[path.to_owned()],
        &BTreeMap::from([(path.to_owned(), pair)]),
        |_| true,
        |_| false,
    )
}
fn player() -> ServerPlayer {
    ServerPlayer {
        id: "00000000-0000-4000-8000-000000000001".into(),
        name: "Alex".into(),
        x: 0.5,
        y: 64.,
        z: 0.5,
        dimension: "minecraft:overworld".into(),
        game_mode: GameMode::Creative,
        health: 20.,
    }
}
fn dummy_server() -> Vec<u8> {
    let wit = Path::new(env!("CARGO_MANIFEST_DIR")).join("../horizon-mod-sdk/wit");
    let mut resolve = wit_parser::Resolve::default();
    let (package, _) = resolve.push_dir(wit).unwrap();
    let world = resolve
        .select_world(&[package], Some("horizon-server-mod"))
        .unwrap();
    let mut module =
        wit_component::dummy_module(&resolve, world, wit_parser::ManglingAndAbi::Standard32);
    wit_component::embed_component_metadata(
        &mut module,
        &resolve,
        world,
        wit_component::StringEncoding::UTF8,
    )
    .unwrap();
    componentize(&module).unwrap()
}

#[test]
fn each_role_uses_only_its_hooks_and_one_package_row() {
    let (client, server) = components();
    for role in [Runs::Client, Runs::Server, Runs::Both] {
        let scratch = Scratch::new();
        let (path, snapshot) = pair(&scratch.0, client, server);
        // The executable originals cannot be used after capture.
        fs::write(path.join("client.wasm"), b"changed").unwrap();
        fs::remove_file(path.join("server.wasm")).unwrap();
        let mut mods = load(role, &path, snapshot);
        assert_eq!(mods.entries().len(), 1);
        assert_eq!(mods.entries()[0].status, Status::Running);
        assert!(
            mods.entries()[0]
                .capabilities()
                .contains(&Capability::Players)
        );
        assert!(
            mods.entries()[0]
                .capabilities()
                .contains(&Capability::World)
        );
        assert_eq!(mods.runs_server(), role != Runs::Client);
        assert_eq!(mods.has_command("hello"), role != Runs::Server);
        assert_eq!(
            mods.server_commands().any(|c| c.name == "online"),
            role != Runs::Client
        );
        assert_eq!(mods.wants_world(), role != Runs::Client);
        mods.tick(1);
        mods.server_tick(1);
        assert_eq!(mods.command("hello roles"), role != Runs::Server);
        if role != Runs::Server {
            assert!(mods.drain().iter().any(
                |o| matches!(o, Output::Chat{text,..} if text == "hello roles after 1 ticks")
            ));
        }
        assert_eq!(
            mods.server_command(&player(), "online"),
            role != Runs::Client
        );
        assert_eq!(mods.entries()[0].status, Status::Running);
    }
}

#[test]
fn in_frame_storage_has_separate_side_directories_and_fault_stops_both() {
    let scratch = Scratch::new();
    let (client, server) = components();
    let (path, snapshot) = pair(&scratch.0, client, server);
    let mut mods = load(Runs::Both, &path, snapshot);
    let player = player();
    mods.set_snapshot(Snapshot {
        players: vec![player.clone()],
        ..Snapshot::default()
    });
    assert!(mods.command("save_key seen client value"));
    mods.player_joined(&player);
    let data = scratch.0.join(STORAGE_DIR).join("paired_activation");
    assert_eq!(fs::read(data.join("client/seen")).unwrap(), b"client value");
    assert_eq!(
        fs::read(data.join("server/seen")).unwrap(),
        player.id.as_bytes()
    );
    mods.drain();
    assert!(mods.command("panic_after_output"));
    assert!(
        mods.drain()
            .iter()
            .all(|o| matches!(o, Output::Stopped { .. }))
    );
    assert!(!mods.is_running());
    assert!(!mods.runs_server());
    assert!(matches!(mods.entries()[0].status, Status::Failed(_)));
    assert!(!mods.server_command(&player, "online"));
}

#[test]
fn invalid_unselected_half_is_rejected_before_client_init() {
    let scratch = Scratch::new();
    let (client, _) = components();
    let (path, snapshot) = pair(&scratch.0, client, b"invalid component");
    let data = scratch.0.join(STORAGE_DIR).join("paired_activation/client");
    fs::create_dir_all(&data).unwrap();
    fs::write(data.join("init_probe"), b"yes").unwrap();
    let mut mods = load(Runs::Client, &path, snapshot);
    assert!(
        matches!(&mods.entries()[0].status, Status::Failed(e) if e.starts_with("server.wasm:"))
    );
    assert!(!mods.is_running());
    assert!(!data.join("init_seen").exists());
    assert!(mods.drain().is_empty());
}

#[test]
fn failed_second_init_discards_first_store_and_buffered_output() {
    let scratch = Scratch::new();
    let (client, _) = components();
    let (path, snapshot) = pair(&scratch.0, client, &dummy_server());
    let data = scratch.0.join(STORAGE_DIR).join("paired_activation/client");
    fs::create_dir_all(&data).unwrap();
    fs::write(data.join("init_probe"), b"yes").unwrap();
    let mut mods = load(Runs::Both, &path, snapshot);
    assert!(matches!(mods.entries()[0].status, Status::Failed(_)));
    assert!(!mods.is_running());
    assert!(mods.drain().is_empty());
    // Existing settings writes are not transactional; this is not an H06 test.
    assert_eq!(
        fs::read(data.join("init_seen")).unwrap(),
        b"client initialized"
    );
    assert!(!mods.command("hello"));
}

#[test]
fn missing_prepared_selection_refuses_execution_and_paired_reload_keeps_snapshot() {
    let scratch = Scratch::new();
    let (client, server) = components();
    let (path, snapshot) = pair(&scratch.0, client, server);
    let rejected = Mods::load_for_with_prepared(
        Runs::Both,
        std::slice::from_ref(&path),
        &BTreeMap::new(),
        |_| true,
        |_| false,
    );
    assert!(
        matches!(&rejected.entries()[0].status, Status::Failed(e) if e.contains("snapshot unavailable"))
    );
    assert!(!rejected.is_running());
    let mut mods = load(Runs::Both, &path, snapshot);
    fs::write(path.join("client.wasm"), b"broken replacement").unwrap();
    assert!(!mods.reload_changed(|_| false));
    assert!(mods.command("hello original"));
    assert!(
        mods.drain()
            .iter()
            .any(|o| matches!(o, Output::Chat{text,..} if text.contains("original")))
    );
}

#[test]
fn disabled_header_displays_both_sides_without_executables_or_snapshot() {
    let scratch = Scratch::new();
    let path = scratch.0.join("package");
    fs::create_dir(&path).unwrap();
    fs::write(path.join(DESCRIPTOR), DESCR).unwrap();
    let mods =
        Mods::load_for_with_prepared(Runs::Both, &[path], &BTreeMap::new(), |_| false, |_| false);
    assert_eq!(mods.entries()[0].status, Status::Off);
    let capabilities = mods.entries()[0].capabilities();
    assert!(capabilities.contains(&Capability::Chat));
    assert!(capabilities.contains(&Capability::Players));
    assert!(capabilities.contains(&Capability::World));
    assert!(!mods.is_running());
}
