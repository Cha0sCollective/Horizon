#![cfg(all(feature = "paired-packages", feature = "runtime", feature = "pack"))]
use horizon_mods::{
    Mods, Side,
    paired::{DESCRIPTOR, PreparedBundle},
};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

const DESCR: &str = r#"
format=1
host-contract='paired-preview-1'
[content]
id='paired_types'
name='Paired types'
version='0'
api='0.2'
side='both'
capabilities=['registries']
[client]
entry='client.wasm'
api='0.1'
capabilities=['game-read','chat','hud','storage','command','ui','render','input','recipes']
experimental=['ui','render','input','recipes']
[server]
entry='server.wasm'
api='0.3'
capabilities=['storage','players','world']
"#;

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        assert_eq!(self.0.parent(), Some(std::env::temp_dir().as_path()));
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn prepared(client: &[u8], server: &[u8], descriptor: &str) -> (Scratch, PreparedBundle) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = Scratch(std::env::temp_dir().join(format!(
        "horizon-paired-types-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )));
    fs::create_dir(&dir.0).unwrap();
    fs::write(dir.0.join(DESCRIPTOR), descriptor).unwrap();
    fs::write(dir.0.join("client.wasm"), client).unwrap();
    fs::write(dir.0.join("server.wasm"), server).unwrap();
    let bundle = PreparedBundle::read_folder(&dir.0).unwrap();
    (dir, bundle)
}
fn dummy(world: &str, future: bool) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../horizon-mod-sdk/wit/horizon.wit");
    let mut wit = fs::read_to_string(path).unwrap();
    if future {
        wit = wit.replace(
            "    set-minimap: func(minimap: option<minimap>);",
            "    set-minimap: func(minimap: option<minimap>);\n    set-sky: func(on: bool);",
        );
    }
    encode_dummy(world, &wit)
}
fn encode_dummy(world: &str, wit: &str) -> Vec<u8> {
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str("horizon.wit", wit).unwrap();
    let world = resolve.select_world(&[package], Some(world)).unwrap();
    let mut module =
        wit_component::dummy_module(&resolve, world, wit_parser::ManglingAndAbi::Standard32);
    wit_component::embed_component_metadata(
        &mut module,
        &resolve,
        world,
        wit_component::StringEncoding::UTF8,
    )
    .unwrap();
    horizon_mods::manifest::componentize(&module).unwrap()
}

#[test]
fn both_worlds_typecheck_without_calling_unreachable_dummy_functions() {
    let (_dir, bundle) = prepared(
        &dummy("horizon-mod", false),
        &dummy("horizon-server-mod", false),
        DESCR,
    );
    let mut mods = Mods::default();
    let checked = mods.preflight_pair(&bundle).unwrap();
    assert!(std::ptr::eq(checked.prepared(), &bundle));
    assert!(checked.component(Side::Client).is_ok());
    assert!(checked.component(Side::Server).is_ok());
    assert!(checked.component(Side::Both).is_err());
    assert!(mods.entries().is_empty());
    assert!(mods.drain().is_empty());
}

#[test]
fn matching_export_names_with_wrong_function_types_are_refused() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../horizon-mod-sdk/wit/horizon.wit");
    let wit = fs::read_to_string(path).unwrap();
    let client = dummy("horizon-mod", false);
    let server = dummy("horizon-server-mod", false);
    let mut mods = Mods::default();
    for (interface, world, count) in [
        ("events", "horizon-mod", 5),
        ("server-events", "horizon-server-mod", 8),
    ] {
        let start = wit.find(&format!("interface {interface} {{")).unwrap();
        let end = start + wit[start..].find("\n}\n").unwrap();
        let callbacks: Vec<_> = wit[start..end]
            .lines()
            .filter(|line| line.contains(": func("))
            .collect();
        assert_eq!(
            callbacks.len(),
            count,
            "update callback coverage for WIT changes"
        );
        for line in callbacks {
            let name = line.trim().split(':').next().unwrap();
            let changed = wit[start..end].replacen(
                line,
                &format!("    {name}: func(unexpected: f64) -> u32;"),
                1,
            );
            let wrong = format!("{}{}{}", &wit[..start], changed, &wit[end..]);
            let code = encode_dummy(world, &wrong);
            let (_dir, bundle) = if interface == "events" {
                prepared(&code, &server, DESCR)
            } else {
                prepared(&client, &code, DESCR)
            };
            match mods.preflight_pair(&bundle) {
                Err(e) => assert!(e.contains(name), "unexpected {interface}.{name} error: {e}"),
                Ok(_) => panic!("wrong {interface}.{name} signature was accepted"),
            }
        }
    }
    assert!(mods.entries().is_empty());
    assert!(mods.drain().is_empty());
}

#[test]
fn malformed_core_or_exportless_halves_fail_before_a_checked_pair() {
    let client = dummy("horizon-mod", false);
    let server = dummy("horizon-server-mod", false);
    for invalid in [
        b"bad".as_slice(),
        b"\0asm\x01\0\0\0".as_slice(),
        b"\0asm\x0d\0\x01\0".as_slice(),
    ] {
        let (_dir, bundle) = prepared(invalid, &server, DESCR);
        let mut mods = Mods::default();
        assert!(matches!(mods.preflight_pair(&bundle), Err(e) if e.starts_with("client.wasm:")));
        assert!(mods.entries().is_empty());
        let (_dir, bundle) = prepared(&client, invalid, DESCR);
        assert!(matches!(mods.preflight_pair(&bundle), Err(e) if e.starts_with("server.wasm:")));
        assert!(mods.drain().is_empty());
    }
}

#[test]
fn wrong_worlds_undeclared_imports_and_newer_functions_are_refused() {
    let client = dummy("horizon-mod", false);
    let server = dummy("horizon-server-mod", false);
    let mut mods = Mods::default();
    let (_dir, swapped) = prepared(&server, &client, DESCR);
    assert!(mods.preflight_pair(&swapped).is_err());
    let limited = DESCR.replace(
        "capabilities=['storage','players','world']",
        "capabilities=[]",
    );
    let (_dir, missing) = prepared(&client, &server, &limited);
    assert!(
        matches!(mods.preflight_pair(&missing), Err(e) if e.contains("missing from horizon-mod.toml"))
    );
    let (_dir, future) = prepared(&dummy("horizon-ui-mod", true), &server, DESCR);
    assert!(
        matches!(mods.preflight_pair(&future), Err(e) if e.contains("newer Horizon") && e.contains("render.set-sky"))
    );
    assert!(mods.entries().is_empty());
    assert!(mods.drain().is_empty());
}
