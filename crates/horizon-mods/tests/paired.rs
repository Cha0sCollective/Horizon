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
