#![cfg(feature = "paired-packages")]
use horizon_mods::{Capability, Manifest, Side, paired::*};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

const EXAMPLE: &str = r#"
format = 1
host-contract = "paired-preview-1"
[content]
id = "counter_demo"
name = "Counter demonstration"
version = "0.0.1"
api = "0.2"
side = "both"
capabilities = ["registries"]
namespaces = ["diagnostic_assets"]
[client]
entry = "client.wasm"
api = "0.1"
capabilities = ["hud"]
[server]
entry = "server.wasm"
api = "0.3"
capabilities = ["world"]
"#;

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "horizon-paired-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn files(&self) {
        fs::write(self.0.join(DESCRIPTOR), EXAMPLE).unwrap();
        // Deliberately just bytes: preflight is not Wasm type/link validation.
        fs::write(self.0.join("client.wasm"), b"client bytes").unwrap();
        fs::write(self.0.join("server.wasm"), b"server bytes").unwrap();
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        assert_eq!(self.0.parent(), Some(std::env::temp_dir().as_path()));
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn one_owner_and_separate_side_plans() {
    let bundle = Bundle::parse(EXAMPLE).unwrap();
    assert_eq!(
        bundle
            .content_owner()
            .owned_namespaces()
            .collect::<Vec<_>>(),
        ["counter_demo", "diagnostic_assets"]
    );
    for (role, expected) in [(Role::Client, Side::Client), (Role::Server, Side::Server)] {
        let plan = bundle.plan(role);
        assert!(std::ptr::eq(plan.content_owner, bundle.content_owner()));
        assert_eq!(plan.executables.len(), 1);
        assert_eq!(plan.executables[0].manifest().side, expected);
        assert_eq!(plan.executables[0].manifest().id, "counter_demo");
        assert!(
            !plan.executables[0]
                .manifest()
                .capabilities
                .contains(&Capability::Registries)
        );
    }
    let plan = bundle.plan(Role::InFrame);
    assert_eq!(
        plan.executables
            .iter()
            .map(|e| e.entry())
            .collect::<Vec<_>>(),
        ["client.wasm", "server.wasm"]
    );
    assert_eq!(
        plan.executables[0].manifest().capabilities,
        [Capability::Hud]
    );
    assert_eq!(
        plan.executables[1].manifest().capabilities,
        [Capability::World]
    );
}

#[test]
fn refuses_missing_halves_unknown_contracts_and_ambiguous_content() {
    let client_start = EXAMPLE.find("[client]").unwrap();
    let server_start = EXAMPLE.find("[server]").unwrap();
    let no_client = format!("{}{}", &EXAMPLE[..client_start], &EXAMPLE[server_start..]);
    assert!(Bundle::parse(&no_client).is_err());
    assert!(Bundle::parse(&EXAMPLE[..server_start]).is_err());
    for text in [
        EXAMPLE.replace("format = 1", "format = 2"),
        EXAMPLE.replace(CONTRACT, "unknown"),
        EXAMPLE.replace("side = \"both\"", "side = \"server\""),
        EXAMPLE.replace("capabilities = [\"registries\"]", "capabilities = []"),
        EXAMPLE.replace("[client]", "[client]\nunknown = true"),
        EXAMPLE.replace("diagnostic_assets", "minecraft"),
    ] {
        assert!(Bundle::parse(&text).is_err(), "{text}");
    }
}

#[test]
fn reuses_legacy_capability_and_api_validation_per_side() {
    for text in [
        EXAMPLE.replace("capabilities = [\"hud\"]", "capabilities = [\"world\"]"),
        EXAMPLE.replace("capabilities = [\"world\"]", "capabilities = [\"hud\"]"),
        EXAMPLE.replace(
            "capabilities = [\"world\"]",
            "capabilities = [\"registries\"]",
        ),
        EXAMPLE.replace("api = \"0.3\"", "api = \"0.1\""),
        EXAMPLE.replace("api = \"0.1\"", "api = \"0.4\""),
        EXAMPLE.replace("capabilities = [\"hud\"]", "capabilities = [\"render\"]"),
    ] {
        assert!(Bundle::parse(&text).is_err(), "{text}");
    }
    let allowed = EXAMPLE.replace(
        "capabilities = [\"hud\"]",
        "capabilities = [\"render\"]\nexperimental = [\"render\"]",
    );
    let bundle = Bundle::parse(&allowed).unwrap();
    let plan = bundle.plan(Role::Client);
    assert!(
        plan.executables[0]
            .manifest()
            .check_imports(["horizon:mods/world@0.1.0"])
            .is_err()
    );
    assert!(
        plan.executables[0]
            .manifest()
            .check_imports(["horizon:mods/render@0.1.0"])
            .is_ok()
    );
}

#[test]
fn entries_are_fixed_plain_names() {
    for bad in [
        "../client.wasm",
        "/client.wasm",
        "nested/client.wasm",
        "server.wasm",
        "",
    ] {
        assert!(Bundle::parse(&EXAMPLE.replace("client.wasm", bad)).is_err());
    }
    assert!(Bundle::parse(&EXAMPLE.replace("server.wasm", "client.wasm")).is_err());
}

#[test]
fn reserved_primary_ids_cannot_claim_vanilla_or_engine_content() {
    for reserved in ["minecraft", "c", "horizon"] {
        assert!(Bundle::parse(&EXAMPLE.replace("counter_demo", reserved)).is_err());
    }
}

#[test]
fn nonempty_content_stays_with_the_single_owner_and_its_aliases() {
    let scratch = Scratch::new();
    scratch.files();
    for namespace in ["counter_demo", "diagnostic_assets"] {
        let lang = scratch.0.join("assets").join(namespace).join("lang");
        fs::create_dir_all(&lang).unwrap();
        fs::write(lang.join("en_us.json"), b"{}").unwrap();
    }
    let prepared = PreparedBundle::read_folder(&scratch.0).unwrap();
    assert_eq!(prepared.content().files.len(), 2);
    assert_eq!(
        prepared.content().namespaces,
        ["counter_demo", "diagnostic_assets"]
    );
    let foreign = scratch.0.join("assets/unowned/lang");
    fs::create_dir_all(&foreign).unwrap();
    fs::write(foreign.join("en_us.json"), b"{}").unwrap();
    assert!(PreparedBundle::read_folder(&scratch.0).is_err());
}

#[test]
fn namespaces_collide_across_paired_and_legacy_owners() {
    let bundle = Bundle::parse(EXAMPLE).unwrap();
    let other = Bundle::parse(&EXAMPLE.replace("counter_demo", "other_demo")).unwrap();
    let error = check_owners([bundle.content_owner(), other.content_owner()]).unwrap_err();
    assert!(error.contains("diagnostic_assets"));
    let legacy = Manifest::parse("id='legacy'\nname='Legacy'\nversion='1'\napi='0.2'\ncapabilities=['registries']\nnamespaces=['diagnostic_assets']").unwrap();
    assert!(check_owners([bundle.content_owner(), &legacy]).is_err());
    assert!(check_owners([bundle.content_owner(), bundle.content_owner()]).is_err());
    assert!(check_owners([bundle.content_owner()]).is_ok());
}

#[test]
fn folder_requires_both_files_before_producing_any_plan() {
    let scratch = Scratch::new();
    scratch.files();
    let prepared = PreparedBundle::read_folder(&scratch.0).unwrap();
    assert_eq!(prepared.code(Side::Client).unwrap(), b"client bytes");
    assert_eq!(prepared.code(Side::Server).unwrap(), b"server bytes");
    assert!(prepared.code(Side::Both).is_err());
    assert_eq!(prepared.bundle().plan(Role::Server).executables.len(), 1);
    assert!(prepared.content().files.is_empty());
    fs::remove_file(scratch.0.join("server.wasm")).unwrap();
    assert!(PreparedBundle::read_folder(&scratch.0).is_err());
    fs::write(scratch.0.join("server.wasm"), []).unwrap();
    assert!(PreparedBundle::read_folder(&scratch.0).is_err());
}

#[test]
fn file_limits_and_legacy_fail_closed() {
    let scratch = Scratch::new();
    scratch.files();
    assert!(horizon_mods::find(&scratch.0).is_empty());
    assert!(Manifest::parse(EXAMPLE).is_err());
    let file = fs::OpenOptions::new()
        .write(true)
        .open(scratch.0.join("client.wasm"))
        .unwrap();
    file.set_len((32 << 20) + 1).unwrap();
    assert!(PreparedBundle::read_folder(&scratch.0).is_err());
    fs::write(scratch.0.join(DESCRIPTOR), vec![b' '; (64 << 10) + 1]).unwrap();
    assert!(PreparedBundle::read_folder(&scratch.0).is_err());
}

#[cfg(unix)]
#[test]
fn executable_symlinks_are_not_plain_package_files() {
    let scratch = Scratch::new();
    scratch.files();
    fs::remove_file(scratch.0.join("client.wasm")).unwrap();
    std::os::unix::fs::symlink("server.wasm", scratch.0.join("client.wasm")).unwrap();
    assert!(PreparedBundle::read_folder(&scratch.0).is_err());
}

#[test]
fn captured_bytes_and_declarations_survive_edits_and_source_removal() {
    let scratch = Scratch::new();
    scratch.files();
    let declaration = "data/counter_demo/horizon/block/counter.json";
    let resource = "assets/counter_demo/lang/en_us.json";
    for (name, bytes) in [
        (declaration, br#"{"template":"minecraft:stone"}"#.as_slice()),
        (resource, b"original"),
    ] {
        let path = scratch.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    let prepared = PreparedBundle::read_folder(&scratch.0).unwrap();
    let identity = *prepared.identity();
    assert_eq!(prepared.content().snapshot_identity(), Some(&identity));
    fs::write(
        scratch.0.join(declaration),
        br#"{"template":"minecraft:dirt"}"#,
    )
    .unwrap();
    fs::write(scratch.0.join(resource), b"changed").unwrap();
    fs::remove_dir_all(scratch.0.join("assets")).unwrap();
    fs::remove_dir_all(scratch.0.join("data")).unwrap();
    fs::remove_file(scratch.0.join(DESCRIPTOR)).unwrap();
    fs::remove_file(scratch.0.join("client.wasm")).unwrap();
    fs::remove_file(scratch.0.join("server.wasm")).unwrap();
    assert_eq!(prepared.content().file(resource).unwrap(), b"original");
    assert_eq!(prepared.content().blocks[0].template, "minecraft:stone");
    assert_eq!(
        prepared.content().file(declaration).unwrap(),
        br#"{"template":"minecraft:stone"}"#
    );
    assert!(
        prepared
            .content()
            .file("assets/counter_demo/missing")
            .is_err()
    );
    assert_eq!(prepared.descriptor(), EXAMPLE.as_bytes());
    assert_eq!(prepared.code(Side::Client).unwrap(), b"client bytes");
    assert_eq!(prepared.code(Side::Server).unwrap(), b"server bytes");
    assert_eq!(*prepared.identity(), identity);
}

#[test]
fn identity_framing_matches_independent_sha256_vector() {
    let scratch = Scratch::new();
    scratch.files();
    let prepared = PreparedBundle::read_folder(&scratch.0).unwrap();
    let hex: String = prepared
        .identity()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    // Python hashlib/struct, explicit scheme/count and the three fixed files;
    // the expected value does not call the Rust digest implementation.
    assert_eq!(
        hex,
        "d35af60c0e5babd95a8bb3f10fa8ec03a4bb60c1c19d0c78488791f36f5d9bdd"
    );
}

#[test]
fn identity_covers_descriptor_both_halves_declarations_and_resource_paths() {
    let scratch = Scratch::new();
    scratch.files();
    let declaration = "data/counter_demo/horizon/block/counter.json";
    let resource = "assets/counter_demo/lang/en_us.json";
    let originals: [(&str, &[u8]); 5] = [
        (DESCRIPTOR, EXAMPLE.as_bytes()),
        ("client.wasm", b"client bytes"),
        ("server.wasm", b"server bytes"),
        (declaration, br#"{"template":"minecraft:stone"}"#),
        (resource, b"abc"),
    ];
    for (name, bytes) in originals {
        let path = scratch.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    let original = *PreparedBundle::read_folder(&scratch.0).unwrap().identity();
    for (name, bytes) in [
        (
            DESCRIPTOR,
            format!("{EXAMPLE}\n# descriptor bytes matter\n").into_bytes(),
        ),
        ("client.wasm", b"other client".to_vec()),
        ("server.wasm", b"other server".to_vec()),
        (declaration, br#"{"template":"minecraft:dirt"}"#.to_vec()),
        (resource, b"xyz".to_vec()),
    ] {
        fs::write(scratch.0.join(name), bytes).unwrap();
        assert_ne!(
            *PreparedBundle::read_folder(&scratch.0).unwrap().identity(),
            original,
            "{name}"
        );
        fs::write(
            scratch.0.join(name),
            originals.iter().find(|(n, _)| *n == name).unwrap().1,
        )
        .unwrap();
    }
    fs::rename(
        scratch.0.join(resource),
        scratch.0.join("assets/counter_demo/lang/other.json"),
    )
    .unwrap();
    assert_ne!(
        *PreparedBundle::read_folder(&scratch.0).unwrap().identity(),
        original
    );
    let other = Scratch::new();
    for (name, bytes) in originals.into_iter().rev() {
        let path = other.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    assert_eq!(
        *PreparedBundle::read_folder(&other.0).unwrap().identity(),
        original
    );
}

#[test]
fn captured_content_reuses_fences_declaration_validation_and_file_limits() {
    for (name, bytes) in [
        ("assets/foreign/lang/en_us.json", b"{}".as_slice()),
        ("assets/counter_demo/INVALID/bad", b"{}"),
        ("data/counter_demo/horizon/block/counter.json", b"not json"),
    ] {
        let scratch = Scratch::new();
        scratch.files();
        let path = scratch.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
        assert!(PreparedBundle::read_folder(&scratch.0).is_err());
    }
    let scratch = Scratch::new();
    scratch.files();
    let path = scratch.0.join("assets/counter_demo/large");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::File::create(path)
        .unwrap()
        .set_len(horizon_mods::content::MAX_FILE + 1)
        .unwrap();
    assert!(PreparedBundle::read_folder(&scratch.0).is_err());
}

#[cfg(unix)]
#[test]
fn captured_content_refuses_file_directory_and_package_symlinks() {
    use std::os::unix::fs::symlink;
    let scratch = Scratch::new();
    scratch.files();
    let outside = Scratch::new();
    fs::write(outside.0.join("payload"), b"secret").unwrap();
    let assets = scratch.0.join("assets/counter_demo");
    fs::create_dir_all(&assets).unwrap();
    symlink(outside.0.join("payload"), assets.join("link")).unwrap();
    assert!(PreparedBundle::read_folder(&scratch.0).is_err());
    fs::remove_file(assets.join("link")).unwrap();
    symlink(&outside.0, assets.join("directory")).unwrap();
    assert!(PreparedBundle::read_folder(&scratch.0).is_err());
    fs::remove_file(assets.join("directory")).unwrap();
    fs::remove_dir_all(scratch.0.join("assets")).unwrap();
    symlink(&outside.0, scratch.0.join("assets")).unwrap();
    assert!(PreparedBundle::read_folder(&scratch.0).is_err());
    symlink(&scratch.0, outside.0.join("package")).unwrap();
    assert!(PreparedBundle::read_folder(&outside.0.join("package")).is_err());
}
