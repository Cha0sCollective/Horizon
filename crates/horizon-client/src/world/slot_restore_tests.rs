//! Native defining-module proofs only. No Game or epoch admission.
use super::*;
use crate::{
    chunk::Chunk,
    slot_transfer::{Direction, ResolvedLimits, SlotProjection},
    world::counter_restore::CounterFrameV2,
    world_slot_trial::{Preparation, RetirementReason},
};
use horizon_mods::paired::PreparedBundle;
use horizon_world::registry::BlockRegistry;
use std::{
    fs,
    sync::{Arc, OnceLock},
};

const POSITION: [i32; 3] = [2, 70, 2];
const TIME: i64 = 12;
fn registered_registry() -> &'static BlockRegistry {
    static REGISTRY: OnceLock<BlockRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| BlockRegistry::from_facts_json(&br#"{"defaults":{"minecraft:air":0,"minecraft:iron_block":1,"minecraft:stone":2,"slot_restore:cube":3},"states":[["air",true,false,"","minecraft:air"],["iron",false,false,"","minecraft:iron_block"],["stone",false,false,"","minecraft:stone"],["alias",false,false,"","slot_restore:cube"]]}"#[..]).unwrap())
}
fn registered_binding() -> NativeBinding {
    NativeBinding::from_registered_prepared(
        &fixture_material().2,
        "slot_restore:type",
        registered_registry(),
    )
    .unwrap()
}

#[test]
fn registered_slot_profiles_are_explicit_and_legacy_binding_cannot_restore_them() {
    let binding = registered_binding();
    let mut world = bare(binding.state_id().0);
    world
        .initialize_counter_domain(&binding, &target())
        .unwrap();
    let key = world
        .initialize_persistent_world_slot(&binding, &target())
        .unwrap()
        .unwrap();
    let counter = world.counter_checkpoint_frame(TIME).unwrap();
    let bytes = world.slot_checkpoint_bytes(fixture().1, TIME).unwrap();
    assert_eq!(counter.kind, "native-registered-counter-checkpoint");
    assert_eq!(counter.binding.template, "minecraft:iron_block");
    assert_eq!(counter.observed_state.name, "slot_restore:cube");
    let decoded = decode_slot_frame(&bytes, fixture().1).unwrap();
    assert_eq!(
        decoded.binding_profile(),
        NativeBindingProfile::RegisteredCube
    );
    assert_eq!(decoded.view().observed_state, "slot_restore:cube");
    let legacy = NativeBinding::from_prepared(
        &fixture_material().2,
        "slot_restore:type",
        registered_registry(),
    )
    .unwrap();
    let mut wrong = bare(binding.state_id().0);
    assert_eq!(
        wrong.restore_counter_frame(&legacy, counter.world_uuid, &target(), &counter),
        Err(super::super::counter_restore::RestoreError::BindingMismatch)
    );
    assert!(wrong.counter_domain_snapshot().is_none());
    let mut fresh = bare(binding.state_id().0);
    fresh
        .restore_counter_frame(&binding, counter.world_uuid, &target(), &counter)
        .unwrap();
    assert!(
        fresh
            .restore_persistent_world_slot(
                &legacy,
                fixture().1,
                counter.world_uuid,
                &target(),
                decoded
            )
            .is_err()
    );
    assert!(fresh.world_slot_snapshot().is_none());
    let restored = fresh
        .restore_persistent_world_slot(
            &binding,
            fixture().1,
            counter.world_uuid,
            &target(),
            decode_slot_frame(&bytes, fixture().1).unwrap(),
        )
        .unwrap()
        .unwrap();
    assert_ne!(key, restored);
    assert_eq!(
        fresh.slot_checkpoint_bytes(fixture().1, TIME).unwrap(),
        bytes
    );
}

#[test]
fn registered_dormant_air_and_iron_preserve_profile_heads_and_original_custody() {
    for replacement in [0, 1] {
        let binding = registered_binding();
        let mut world = bare(binding.state_id().0);
        let old_counter = world
            .initialize_counter_domain(&binding, &target())
            .unwrap();
        let old_slot = world
            .initialize_persistent_world_slot(&binding, &target())
            .unwrap()
            .unwrap();
        world
            .set(
                POSITION[0],
                POSITION[1],
                POSITION[2],
                BlockStateId(replacement),
            )
            .unwrap();
        assert!(world.slot_checkpoint_bytes(fixture().1, TIME).is_err());
        let owned = world
            .take_persistent_world_slot_retirement(&old_slot)
            .unwrap();
        assert!(owned.into_parts().0.into_parts().1.into_stack().is_none());
        assert!(world.reconcile_persistent_world_slot().unwrap().is_none());
        let counter = world.counter_checkpoint_frame(TIME).unwrap();
        let bytes = world.slot_checkpoint_bytes(fixture().1, TIME).unwrap();
        assert_eq!(
            counter.placement,
            super::super::counter_restore::Placement::Dormant
        );
        assert_eq!(
            counter.binding_profile().unwrap(),
            NativeBindingProfile::RegisteredCube
        );
        let mut fresh = bare(replacement);
        assert!(
            fresh
                .restore_counter_frame(&binding, counter.world_uuid, &target(), &counter)
                .unwrap()
                .is_none()
        );
        assert!(
            fresh
                .restore_persistent_world_slot(
                    &binding,
                    fixture().1,
                    counter.world_uuid,
                    &target(),
                    decode_slot_frame(&bytes, fixture().1).unwrap()
                )
                .unwrap()
                .is_none()
        );
        assert_eq!(fresh.counter_checkpoint_frame(TIME).unwrap(), counter);
        assert_eq!(
            fresh.slot_checkpoint_bytes(fixture().1, TIME).unwrap(),
            bytes
        );
        fresh
            .set(POSITION[0], POSITION[1], POSITION[2], binding.state_id())
            .unwrap();
        let new_slot = fresh.reconcile_persistent_world_slot().unwrap().unwrap();
        assert_eq!(new_slot.incarnation(), 2);
        assert!(fresh.read_world_slot(&old_slot).is_err());
        assert!(fresh.read_persistent_counter(&old_counter).is_err());
        assert_eq!(fresh.counter_domain_snapshot().unwrap().high_water, 2);
    }
}

#[test]
fn registered_raw_slot_kind_profile_and_typed_fields_are_closed() {
    // Closed raw records, independent of the production encoder's output.
    let template = Tag::compound([
        ("format", Tag::Int(1)),
        ("kind", text("native-registered-slot-checkpoint")),
        ("world_uuid", text("12345678-1234-4234-8234-123456789abc")),
        ("dimension", text("minecraft:overworld")),
        ("position", Tag::IntArray(POSITION.to_vec())),
        ("package_identity", text(&"ab".repeat(32))),
        ("type_id", text("slot_restore:type")),
        ("save_schema", Tag::Int(1)),
        ("block_id", text("slot_restore:cube")),
        ("template", text("minecraft:iron_block")),
        ("properties", Tag::compound([] as [(&str, Tag); 0])),
        (
            "observed_state",
            Tag::compound([
                ("name", text("minecraft:iron_block")),
                ("properties", Tag::compound([] as [(&str, Tag); 0])),
            ]),
        ),
        ("saved_game_time", Tag::Long(TIME)),
        (
            "machine_high_water",
            Tag::ByteArray(vec![0, 0, 0, 0, 0, 0, 0, 7]),
        ),
        (
            "slot_high_water",
            Tag::ByteArray(vec![0, 0, 0, 0, 0, 0, 0, 3]),
        ),
        ("ownership", Tag::compound([("kind", text("dormant"))])),
    ]);
    assert_eq!(
        decode_slot_frame(&raw(template.clone()), fixture().1)
            .unwrap()
            .binding_profile(),
        NativeBindingProfile::RegisteredCube
    );
    for mode in 0..7 {
        let mut tag = template.clone();
        match mode {
            0 => *tag.get_mut("kind").unwrap() = text("native-registered-slot-checkpoint-vnext"),
            1 => *tag.get_mut("format").unwrap() = Tag::Int(2),
            2 => *tag.get_mut("kind").unwrap() = text("native-slot-checkpoint"),
            3 => *tag.get_mut("saved_game_time").unwrap() = Tag::Int(TIME as i32),
            4 => {
                *tag.get_mut("observed_state")
                    .unwrap()
                    .get_mut("name")
                    .unwrap() = text("slot_restore:cube")
            }
            5 => *tag.get_mut("properties").unwrap() = Tag::compound([("axis", text("x"))]),
            _ => *tag.get_mut("ownership").unwrap() = Tag::compound([("kind", text("pending"))]),
        }
        assert!(
            decode_slot_frame(&raw(tag), fixture().1).is_err(),
            "mode {mode}"
        );
    }
}
fn fixture_material() -> &'static (NativeBinding, Items, PreparedBundle) {
    static FIXTURE: OnceLock<(NativeBinding, Items, PreparedBundle)> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        static REGISTRY:OnceLock<BlockRegistry>=OnceLock::new();
        let registry=REGISTRY.get_or_init(||BlockRegistry::from_facts_json(&br#"{"defaults":{"minecraft:air":0,"minecraft:iron_block":1,"minecraft:stone":2},"states":[["air",true,false,"","minecraft:air"],["iron",false,false,"","minecraft:iron_block"],["stone",false,false,"","minecraft:stone"]]}"#[..]).unwrap());
        let mut nonce=[0;16];horizon_protocol::crypto::random_bytes(&mut nonce).unwrap();
        let root=std::env::temp_dir().join(format!("horizon-slot-restore-{}-{:x}",std::process::id(),u128::from_be_bytes(nonce)));
        for (name,bytes) in [
            ("horizon-paired.toml",b"format=1\nhost-contract='paired-preview-1'\n[content]\nid='slot_restore'\nname='Slot'\nversion='0'\napi='0.2'\nside='both'\ncapabilities=['registries']\n[client]\nentry='client.wasm'\napi='0.1'\ncapabilities=[]\n[server]\nentry='server.wasm'\napi='0.3'\ncapabilities=[]\n".as_slice()),
            ("client.wasm",b"not executed".as_slice()),("server.wasm",b"not executed".as_slice()),
            ("data/slot_restore/horizon/machine/type.json",br#"{"format":1,"save-schema":1}"#.as_slice()),
            ("data/slot_restore/horizon/block/cube.json",br#"{"template":"minecraft:iron_block","machine-type":"slot_restore:type"}"#.as_slice()),
            ("items.json",br#"{"minecraft:stone":{},"minecraft:dirt":{},"minecraft:brick":{},"minecraft:cobblestone":{}}"#.as_slice()),
        ] {let path=root.join(name);fs::create_dir_all(path.parent().unwrap()).unwrap();fs::write(path,bytes).unwrap();}
        fs::create_dir_all(root.join("data/minecraft/loot_table")).unwrap();
        let prepared=PreparedBundle::read_folder(&root).unwrap();
        let binding=NativeBinding::from_prepared(&prepared,"slot_restore:type",registry).unwrap();
        let items=Items::catalog(&root).unwrap();fs::remove_dir_all(&root).unwrap();(binding,items,prepared)
    })
}
fn fixture() -> (&'static NativeBinding, &'static Items) {
    let (binding, items, _) = fixture_material();
    (binding, items)
}
fn target() -> CounterTarget {
    CounterTarget::new("minecraft:overworld", POSITION).unwrap()
}
fn chunk(state: u32) -> Chunk {
    let mut value = Chunk::empty(0, 0);
    value.set(2, 70, 2, state);
    value.set(3, 70, 2, 1);
    value
}
fn bare(state: u32) -> World {
    let mut world = World::new(BlockStateId(0), 17);
    world.game_time = TIME;
    world.insert_streamed_chunk(chunk(state));
    world
}
fn anchor() -> World {
    let mut world = bare(1);
    world
        .initialize_counter_domain(fixture().0, &target())
        .unwrap();
    world
}
fn restore_anchor(frame: &CounterFrameV2, state: u32) -> World {
    let mut world = bare(state);
    world
        .restore_counter_frame(fixture().0, frame.world_uuid, &target(), frame)
        .unwrap();
    world
}
fn caps() -> ResolvedLimits {
    ResolvedLimits::new(u32::MAX, u32::MAX).unwrap()
}
fn rich(count: u32) -> ItemStack {
    let mut value = ItemStack::new("minecraft:stone", count);
    value.damage = 3;
    let data = value.data_mut();
    data.custom_name = Some("seven retained".into());
    data.repair_cost = 2;
    data.container
        .push((3, ItemStack::new("minecraft:dirt", 1)));
    data.charged
        .push(ItemStack::new("minecraft:cobblestone", 2));
    data.bundle_contents
        .push(ItemStack::new("minecraft:brick", 1));
    data.pot_decorations = Some(Box::new([
        Some(ItemStack::new("minecraft:dirt", 1)),
        None,
        None,
        None,
    ]));
    data.set_other(
        "example:binary",
        Some(Tag::compound([
            ("byte", Tag::Byte(-8)),
            ("short", Tag::Short(-300)),
            ("int", Tag::Int(i32::MIN)),
            ("long", Tag::Long(i64::MAX)),
            ("bytes", Tag::ByteArray(vec![0, 1, 127, 128, 255])),
            ("ints", Tag::IntArray(vec![i32::MIN, 0, i32::MAX])),
            ("longs", Tag::LongArray(vec![i64::MIN, 0, i64::MAX])),
            ("text", text("native\0text")),
            (
                "mixed",
                Tag::List(vec![
                    Tag::Byte(1),
                    text("mixed"),
                    Tag::compound([("", Tag::Long(9))]),
                ]),
            ),
            ("float", Tag::Float((-1.25f32).to_bits())),
            ("double", Tag::Double(2.5f64.to_bits())),
        ])),
    );
    value
}
fn slot_key(world: &World) -> SlotKey {
    world
        .world_slot_trial
        .as_ref()
        .unwrap()
        .borrowed()
        .active
        .unwrap()
        .key
        .clone()
}
fn address(world: &World) -> *const crate::item_data::StackData {
    world
        .world_slot_trial
        .as_ref()
        .unwrap()
        .borrowed()
        .active
        .unwrap()
        .stack
        .unwrap()
        .extra
        .as_deref()
        .unwrap() as *const _
}
fn filled(value: ItemStack) -> World {
    let mut world = anchor();
    let key = world
        .initialize_persistent_world_slot(fixture().0, &target())
        .unwrap()
        .unwrap();
    let amount = value.count;
    let mut donor = SlotProjection::new(Some(value));
    let plan = match world
        .prepare_world_slot_transfer(&key, &donor, Direction::IntoMachine, amount, caps())
        .unwrap()
    {
        Preparation::Prepared(plan) => *plan,
        other => panic!("expected transfer: {other:?}"),
    };
    world
        .commit_world_slot_transfer(plan, &mut donor, caps())
        .unwrap();
    assert!(donor.into_stack().is_none());
    world
}
fn encoded(world: &World) -> Vec<u8> {
    world
        .slot_checkpoint_bytes(fixture().1, world.game_time)
        .unwrap()
}
fn decoded(world: &World) -> DecodedSlotFrame {
    decode_slot_frame(&encoded(world), fixture().1).unwrap()
}
fn write(world: &mut World, state: u32) {
    world.set(2, 70, 2, BlockStateId(state)).unwrap();
}
fn raw(tag: Tag) -> Vec<u8> {
    nbt::write_named(
        &NamedTag {
            name: JavaString::default(),
            tag,
        },
        Limits::default(),
    )
    .unwrap()
}
fn frame_tag(bytes: &[u8]) -> Tag {
    nbt::read_named(bytes, limits()).unwrap().0.tag
}
fn allocation_value(tag: &mut Tag) -> &mut Tag {
    tag.get_mut("ownership")
        .unwrap()
        .get_mut("stack")
        .unwrap()
        .get_mut("value")
        .unwrap()
}
fn book_stack(mode: usize, pages: usize) -> ItemStack {
    let mut stack = ItemStack::new("minecraft:stone", 7);
    if mode == 0 {
        stack.data_mut().writable_pages = vec![String::new(); pages];
    } else {
        stack.data_mut().written = Some(crate::item_data::WrittenBook {
            pages: vec![String::new(); pages],
            page_tags: if mode == 2 {
                vec![text(""); pages]
            } else {
                Vec::new()
            },
            ..Default::default()
        });
    }
    stack
}
fn retained_presence_without_trial(world: &mut World) {
    // Defining-module presence test only: retain the whole trial locally, never
    // invoke capture or admission, and put the exact owner back afterward.
    let trial = world.world_slot_trial.take();
    let metadata = world.persistent_world_slot_metadata();
    let rng = world.random.state();
    let clock = world.game_time;
    for _ in 0..8 {
        assert!(world.has_world_slot_trial());
    }
    assert_eq!(world.persistent_world_slot_metadata(), metadata);
    assert_eq!(world.random.state(), rng);
    assert_eq!(world.game_time, clock);
    world.world_slot_trial = trial;
}

#[test]
fn native_slot_restore_positive_seven_exact_revision_and_components() {
    let mut empty = anchor();
    empty
        .initialize_persistent_world_slot(fixture().0, &target())
        .unwrap();
    assert!(decoded(&empty).allocation.unwrap().stack.is_none());
    let mut source = filled(rich(7));
    let original = address(&source);
    let metadata = source.persistent_world_slot_metadata();
    retained_presence_without_trial(&mut source);
    assert_eq!(address(&source), original);
    let before_rng = source.random.state();
    let bytes = encoded(&source);
    assert_eq!(address(&source), original);
    assert_eq!(source.persistent_world_slot_metadata(), metadata);
    assert_eq!(source.random.state(), before_rng);
    assert_eq!(source.game_time, TIME);
    let counter = source.counter_checkpoint_frame(TIME).unwrap();
    let mut destination = restore_anchor(&counter, 1);
    let frame = decode_slot_frame(&bytes, fixture().1).unwrap();
    let candidate = frame
        .allocation
        .as_ref()
        .unwrap()
        .stack
        .as_ref()
        .unwrap()
        .extra
        .as_deref()
        .unwrap() as *const _;
    let key = destination
        .restore_persistent_world_slot(
            fixture().0,
            fixture().1,
            counter.world_uuid,
            &target(),
            frame,
        )
        .unwrap()
        .unwrap();
    assert_eq!(address(&destination), candidate);
    assert_ne!(address(&destination), original);
    let view = destination.world_slot_trial.as_ref().unwrap().borrowed();
    let active = view.active.unwrap();
    assert_eq!(active.key, &key);
    assert_eq!(active.revision, 1);
    assert_eq!(active.stack, Some(&rich(7)));
    assert_eq!(encoded(&destination), bytes);
    assert_eq!(address(&source), original);
    let tag = frame_tag(&bytes);
    let item = tag
        .get("ownership")
        .unwrap()
        .get("stack")
        .unwrap()
        .get("value")
        .unwrap();
    let nested = item
        .get("components")
        .unwrap()
        .get("minecraft:container")
        .unwrap();
    let Tag::List(entries) = nested else {
        panic!("native container list");
    };
    // Native ItemStackTemplate omits count1, unlike the required root item count.
    assert!(entries[0].get("item").unwrap().get("count").is_none());
}

#[test]
fn native_slot_restore_codec_refuses_loss_and_malformed_frames() {
    let source = filled(rich(7));
    let original = address(&source);
    let bytes = encoded(&source);
    for mode in 0..16 {
        let mut tag = frame_tag(&bytes);
        match mode {
            0 => {
                let Tag::Compound(map) = &mut tag else {
                    unreachable!()
                };
                map.remove(&"slot_high_water".into());
            }
            1 => *tag.get_mut("slot_high_water").unwrap() = Tag::ByteArray(vec![0; 7]),
            2 => *tag.get_mut("position").unwrap() = Tag::IntArray(vec![2, 70]),
            3 => *tag.get_mut("saved_game_time").unwrap() = Tag::Int(12),
            4 => {
                let Tag::Compound(map) = tag.get_mut("ownership").unwrap() else {
                    unreachable!()
                };
                map.remove(&"stack".into());
            }
            5 => {
                *tag.get_mut("ownership").unwrap().get_mut("stack").unwrap() =
                    Tag::compound([] as [(&str, Tag); 0])
            }
            6 => *allocation_value(&mut tag).get_mut("count").unwrap() = Tag::Int(0),
            7 => {
                *allocation_value(&mut tag).get_mut("count").unwrap() =
                    Tag::Long(i64::from(u32::MAX))
            }
            8 => {
                let Tag::Compound(map) = allocation_value(&mut tag) else {
                    unreachable!()
                };
                map.insert("unknown".into(), Tag::Byte(1));
            }
            9 => {
                *allocation_value(&mut tag)
                    .get_mut("components")
                    .unwrap()
                    .get_mut("minecraft:damage")
                    .unwrap() = Tag::Int(-1)
            }
            10 => {
                let Tag::Compound(map) = allocation_value(&mut tag) else {
                    unreachable!()
                };
                map.insert("components".into(), Tag::compound([] as [(&str, Tag); 0]));
            }
            11 => *tag.get_mut("ownership").unwrap().get_mut("kind").unwrap() = text("pending"),
            12 => {
                *tag.get_mut("ownership")
                    .unwrap()
                    .get_mut("slot_generation")
                    .unwrap() = unsigned(0)
            }
            13 => {
                *tag.get_mut("ownership")
                    .unwrap()
                    .get_mut("revision")
                    .unwrap() = Tag::Long(1)
            }
            14 => *tag.get_mut("template").unwrap() = text("minecraft:stone"),
            _ => *tag.get_mut("properties").unwrap() = Tag::compound([("light", Tag::Int(15))]),
        }
        // Empty components normalize to None but are not the canonical native representation.
        assert!(
            decode_slot_frame(&raw(tag), fixture().1).is_err(),
            "mode {mode}"
        );
    }
    let mut duplicate = bytes[..bytes.len() - 1].to_vec();
    duplicate.extend_from_slice(&[3, 0, 6, b'f', b'o', b'r', b'm', b'a', b't', 0, 0, 0, 1, 0]);
    assert!(decode_slot_frame(&duplicate, fixture().1).is_err());
    // A duplicate inside the nested opaque compound is also refused before a retained candidate can install.
    let needle = [1, 0, 4, b'b', b'y', b't', b'e', 248];
    let position = bytes
        .windows(needle.len())
        .position(|part| part == needle)
        .unwrap();
    let mut nested = bytes.clone();
    drop(nested.splice(position..position, needle));
    assert!(decode_slot_frame(&nested, fixture().1).is_err());
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(decode_slot_frame(&trailing, fixture().1).is_err());
    for mode in 0..17 {
        let mut value = rich(7);
        match mode {
            0 => value.count = u32::MAX,
            1 => value.damage = u32::MAX,
            2 => value.data_mut().repair_cost = u32::MAX,
            3 => value.data_mut().bundle_selected.0 = 0,
            4 => value.data_mut().container[0].1.data_mut().bundle_selected.0 = 0,
            5 => value.data_mut().charged[0].data_mut().bundle_selected.0 = 0,
            6 => {
                value.data_mut().bundle_contents[0]
                    .data_mut()
                    .bundle_selected
                    .0 = 0
            }
            7 => {
                value.data_mut().pot_decorations.as_mut().unwrap()[0]
                    .as_mut()
                    .unwrap()
                    .data_mut()
                    .bundle_selected
                    .0 = 0
            }
            8 => value
                .data_mut()
                .set_other("example:bad", Some(Tag::Float(0x80000000))),
            9 => value
                .data_mut()
                .set_other("example:bad", Some(Tag::Double(f64::NAN.to_bits()))),
            10 => value.data_mut().container[0].1.count = 0,
            11 => {
                value = ItemStack::new("minecraft:stone", 7);
                value.data_mut();
            }
            12 => value.data_mut().map_post_processing = Some(0),
            13 => value
                .data_mut()
                .enchantments
                .push((Arc::from("minecraft:sharpness"), 256)),
            14 => value.data_mut().fireworks = Some((1, Vec::new())),
            15 => value
                .data_mut()
                .set_other("example:bad", Some(Tag::String(JavaString(vec![0xd800])))),
            _ => value.item = Arc::from("example:unknown"),
        }
        let world = filled(value);
        let ptr = address(&world);
        let metadata = world.persistent_world_slot_metadata();
        assert!(
            world.slot_checkpoint_bytes(fixture().1, TIME).is_err(),
            "unsupported {mode}"
        );
        assert_eq!(address(&world), ptr);
        assert_eq!(world.persistent_world_slot_metadata(), metadata);
    }
    // Exact early wire limits, including actual synthetic compound wrappers in mixed lists.
    for (tag, valid) in [
        (
            Tag::compound([("bytes", Tag::ByteArray(vec![0; ARRAY]))]),
            true,
        ),
        (
            Tag::compound([("bytes", Tag::ByteArray(vec![0; ARRAY + 1]))]),
            false,
        ),
        (Tag::compound([("text", text(&"x".repeat(STRING)))]), true),
        (
            Tag::compound([("text", text(&"x".repeat(STRING + 1)))]),
            false,
        ),
        (
            Tag::compound([("k".repeat(STRING).as_str(), Tag::Byte(1))]),
            true,
        ),
        (
            Tag::compound([("k".repeat(STRING + 1).as_str(), Tag::Byte(1))]),
            false,
        ),
        (
            Tag::compound([("ints", Tag::IntArray(vec![0; ARRAY]))]),
            true,
        ),
        (
            Tag::compound([("longs", Tag::LongArray(vec![0; ARRAY + 1]))]),
            false,
        ),
        (
            Tag::compound([("list", Tag::List(vec![Tag::Byte(1); NODES - 2]))]),
            true,
        ),
        (
            Tag::compound([("list", Tag::List(vec![Tag::Byte(1); NODES - 1]))]),
            false,
        ),
    ] {
        assert_eq!(preflight(&raw(tag)).is_ok(), valid);
    }
    for (length, valid) in [(NODES / 2 - 1, true), (NODES / 2, false)] {
        let values = (0..length)
            .map(|i| {
                if i % 2 == 0 {
                    Tag::Byte(1)
                } else {
                    Tag::Short(2)
                }
            })
            .collect();
        assert_eq!(
            preflight(&raw(Tag::compound([("mixed", Tag::List(values))]))).is_ok(),
            valid
        );
    }
    for (depth, valid) in [(DEPTH, true), (DEPTH + 1, false)] {
        let mut tag = Tag::Byte(1);
        for _ in 0..depth {
            tag = Tag::compound([("child", tag)]);
        }
        assert_eq!(preflight(&raw(tag)).is_ok(), valid);
    }
    let mut map = Compound::new();
    for i in 0..64 {
        map.insert(
            format!("b{i:02}").as_str().into(),
            Tag::ByteArray(vec![0; ARRAY]),
        );
    }
    let oversized = raw(Tag::Compound(map.clone()));
    let excess = oversized.len() - BYTES;
    map.insert("b63".into(), Tag::ByteArray(vec![0; ARRAY - excess]));
    let mut boundary = raw(Tag::Compound(map));
    assert_eq!(boundary.len(), BYTES);
    assert!(preflight(&boundary).is_ok());
    boundary.push(0);
    assert_eq!(preflight(&boundary), Err(SlotRestoreError::Limit));
    assert_eq!(
        preflight(&[10, 0, 0, 7, 0, 1, b'x', 0x7f, 0xff, 0xff, 0xff]),
        Err(SlotRestoreError::Limit)
    );
    assert_eq!(
        preflight(&[10, 0, 0, 7, 0, 1, b'x', 0xff, 0xff, 0xff, 0xff]),
        Err(SlotRestoreError::InvalidFrame)
    );
    assert!(check_stack(&ItemStack::new("minecraft:stone", 0), fixture().1).is_err());
    // Native book pages allocate a filterable compound per page. Prove the
    // pre-encoder object budget itself rejects the first over-budget page,
    // in writable, written string-fallback and written page_tags branches.
    for mode in 0..3 {
        let per_page = if mode == 2 { 3 } else { 2 };
        let mut one = NativeBudget { nodes: 0, bytes: 0 };
        one.stack(&book_stack(mode, 1), fixture().1, 3).unwrap();
        let fixed = one.nodes - per_page;
        let page_limit = (NODES - fixed) / per_page;
        let mut at_limit = NativeBudget { nodes: 0, bytes: 0 };
        at_limit
            .stack(&book_stack(mode, page_limit), fixture().1, 3)
            .unwrap();
        assert!(at_limit.nodes <= NODES && at_limit.nodes + per_page > NODES);
        assert_eq!(
            NativeBudget { nodes: 0, bytes: 0 }.stack(
                &book_stack(mode, page_limit + 1),
                fixture().1,
                3
            ),
            Err(SlotRestoreError::Limit)
        );
        let huge = book_stack(mode, 9000);
        assert_eq!(
            NativeBudget { nodes: 0, bytes: 0 }.stack(&huge, fixture().1, 3),
            Err(SlotRestoreError::Limit)
        );
        let world = filled(huge);
        let address_before = address(&world);
        let metadata = world.persistent_world_slot_metadata();
        let random = world.random.state();
        assert_eq!(
            world.slot_checkpoint_bytes(fixture().1, TIME),
            Err(SlotRestoreError::Limit)
        );
        assert_eq!(address(&world), address_before);
        assert_eq!(world.persistent_world_slot_metadata(), metadata);
        assert_eq!(world.random.state(), random);
    }
    assert_eq!(address(&source), original);
}

#[test]
fn native_slot_restore_factory_moves_owned_input_and_failure_returns_it() {
    let source = filled(rich(7));
    let configuration = source
        .counter_trial
        .as_ref()
        .unwrap()
        .configuration()
        .clone();
    let mut value = Some(RestoreAllocation {
        generation: 4,
        revision: 9,
        stack: Some(rich(7)),
    });
    let original = value
        .as_ref()
        .unwrap()
        .stack
        .as_ref()
        .unwrap()
        .extra
        .as_deref()
        .unwrap() as *const _;
    assert!(Trial::restore_checked(configuration.clone(), 3, &mut value).is_err());
    assert_eq!(
        value
            .as_ref()
            .unwrap()
            .stack
            .as_ref()
            .unwrap()
            .extra
            .as_deref()
            .unwrap() as *const _,
        original
    );
    let trial = Trial::restore_checked(configuration, 8, &mut value).unwrap();
    assert!(value.is_none());
    let view = trial.borrowed();
    assert_eq!(view.high_water, 8);
    assert_eq!(view.active.as_ref().unwrap().revision, 9);
    assert_eq!(
        view.active
            .unwrap()
            .stack
            .unwrap()
            .extra
            .as_deref()
            .unwrap() as *const _,
        original
    );
    let counter = source.counter_checkpoint_frame(TIME).unwrap();
    let mut destination = restore_anchor(&counter, 1);
    let frame = decoded(&source);
    let ptr = frame
        .allocation
        .as_ref()
        .unwrap()
        .stack
        .as_ref()
        .unwrap()
        .extra
        .as_deref()
        .unwrap() as *const _;
    let wrong = CounterTarget::new("minecraft:overworld", [3, 70, 2]).unwrap();
    let rejected = destination
        .restore_persistent_world_slot(fixture().0, fixture().1, counter.world_uuid, &wrong, frame)
        .unwrap_err();
    assert_eq!(rejected.error, SlotRestoreError::IdentityMismatch);
    assert_eq!(
        rejected
            .frame
            .allocation
            .as_ref()
            .unwrap()
            .stack
            .as_ref()
            .unwrap()
            .extra
            .as_deref()
            .unwrap() as *const _,
        ptr
    );
    destination
        .restore_persistent_world_slot(
            fixture().0,
            fixture().1,
            counter.world_uuid,
            &target(),
            rejected.frame,
        )
        .unwrap();
    assert_eq!(address(&destination), ptr);
}

#[test]
fn native_slot_restore_fresh_keys_and_stale_plans() {
    let source = filled(rich(7));
    let old = slot_key(&source);
    let mut receiver = SlotProjection::new(None);
    let stale = match source
        .prepare_world_slot_transfer(&old, &receiver, Direction::IntoPlayer, 2, caps())
        .unwrap()
    {
        Preparation::Prepared(plan) => *plan,
        other => panic!("{other:?}"),
    };
    let counter = source.counter_checkpoint_frame(TIME).unwrap();
    let mut destination = restore_anchor(&counter, 1);
    let fresh = destination
        .restore_persistent_world_slot(
            fixture().0,
            fixture().1,
            counter.world_uuid,
            &target(),
            decoded(&source),
        )
        .unwrap()
        .unwrap();
    assert_ne!(old, fresh);
    assert_eq!(old.incarnation(), fresh.incarnation());
    assert_ne!(old.scope(), fresh.scope());
    assert_eq!(
        destination.read_world_slot(&old).unwrap_err(),
        world_slot_trial::Error::StaleKey
    );
    assert_eq!(
        destination.commit_world_slot_transfer(stale, &mut receiver, caps()),
        Err(world_slot_trial::Error::StaleKey)
    );
    assert!(receiver.borrowed().1.is_none());
    let plan = match destination
        .prepare_world_slot_transfer(&fresh, &receiver, Direction::IntoPlayer, 2, caps())
        .unwrap()
    {
        Preparation::Prepared(plan) => *plan,
        other => panic!("{other:?}"),
    };
    destination
        .commit_world_slot_transfer(plan, &mut receiver, caps())
        .unwrap();
    let machine = destination
        .world_slot_trial
        .as_ref()
        .unwrap()
        .borrowed()
        .active
        .unwrap();
    assert_eq!(machine.stack.unwrap().count, 5);
    assert_eq!(receiver.borrowed().1.unwrap().count, 2);
    assert!(
        machine
            .stack
            .unwrap()
            .same_item_same_components(receiver.borrowed().1.unwrap())
    );
    assert_eq!(
        source
            .world_slot_trial
            .as_ref()
            .unwrap()
            .borrowed()
            .active
            .unwrap()
            .stack
            .unwrap()
            .count,
        7
    );
}

#[test]
fn native_slot_restore_binding_target_clock_occupied_nonmutation() {
    let source = filled(rich(7));
    let counter = source.counter_checkpoint_frame(TIME).unwrap();
    for mode in 0..14 {
        let mut destination = restore_anchor(&counter, 1);
        let mut frame = decoded(&source);
        let mut host = target();
        let mut expected = counter.world_uuid;
        match mode {
            0 => expected = WorldId::parse("00000000-0000-4000-8000-000000000001").unwrap(),
            1 => host.dimension = "minecraft:the_nether".into(),
            2 => host.position = [3, 70, 2],
            3 => frame.header.package = "0".repeat(64),
            4 => frame.header.type_id = "slot_restore:other".into(),
            5 => frame.header.block_id = "slot_restore:other".into(),
            6 => frame.header.machine_head += 1,
            7 => frame.header.machine_generation = Some(2),
            8 => destination.game_time += 1,
            9 => {
                destination.remove_streamed_chunk(&(0, 0)).unwrap();
            }
            10 => frame.header.observed = "minecraft:stone".into(),
            11 => {
                destination
                    .initialize_persistent_world_slot(fixture().0, &target())
                    .unwrap();
            }
            12 => {
                let config = destination
                    .counter_trial
                    .as_ref()
                    .unwrap()
                    .configuration()
                    .clone();
                destination.enable_world_slot_trial(config).unwrap();
            }
            _ => {
                frame.header.machine_head = 2;
                frame.header.machine_generation = Some(2);
            }
        }
        let before = destination.counter_domain_snapshot();
        let metadata = destination.persistent_world_slot_metadata();
        let rng = destination.random.state();
        let time = destination.game_time;
        let chunk = destination.chunks.get(&(0, 0)).cloned();
        let ptr = frame
            .allocation
            .as_ref()
            .unwrap()
            .stack
            .as_ref()
            .unwrap()
            .extra
            .as_deref()
            .unwrap() as *const _;
        let result = destination
            .restore_persistent_world_slot(fixture().0, fixture().1, expected, &host, frame)
            .unwrap_err();
        assert_eq!(
            result
                .frame
                .allocation
                .as_ref()
                .unwrap()
                .stack
                .as_ref()
                .unwrap()
                .extra
                .as_deref()
                .unwrap() as *const _,
            ptr
        );
        assert_eq!(destination.counter_domain_snapshot(), before);
        assert_eq!(destination.persistent_world_slot_metadata(), metadata);
        assert_eq!(destination.random.state(), rng);
        assert_eq!(destination.game_time, time);
        if let Some(chunk) = chunk {
            assert!(Arc::ptr_eq(
                &chunk,
                destination.chunks.get(&(0, 0)).unwrap()
            ));
        }
    }
    let mut occupied = filled(rich(7));
    let ptr = address(&occupied);
    assert_eq!(
        occupied
            .restore_persistent_world_slot(
                fixture().0,
                fixture().1,
                counter.world_uuid,
                &target(),
                decoded(&source)
            )
            .unwrap_err()
            .error,
        SlotRestoreError::Occupied
    );
    assert_eq!(address(&occupied), ptr);
    // Two independently parsed native registries legitimately agree on iron's
    // runtime id and the package while disagreeing on nonmachine id2. The
    // existing counter anchor, not the supplied binding, owns physical meaning.
    static OTHER_REGISTRY: OnceLock<BlockRegistry> = OnceLock::new();
    let other_registry=OTHER_REGISTRY.get_or_init(||BlockRegistry::from_facts_json(&br#"{"defaults":{"minecraft:air":0,"minecraft:iron_block":1,"minecraft:dirt":2},"states":[["air",true,false,"","minecraft:air"],["iron",false,false,"","minecraft:iron_block"],["dirt",false,false,"","minecraft:dirt"]]}"#[..]).unwrap());
    let other_binding =
        NativeBinding::from_prepared(&fixture_material().2, "slot_restore:type", other_registry)
            .unwrap();
    assert_eq!(
        other_binding.package_identity(),
        fixture().0.package_identity()
    );
    assert_eq!(other_binding.state_id(), fixture().0.state_id());
    let mut native_source = anchor();
    write(&mut native_source, 2);
    let native_counter = native_source.counter_checkpoint_frame(TIME).unwrap();
    let mut dormant = restore_anchor(&native_counter, 2);
    assert!(
        dormant
            .initialize_persistent_world_slot(fixture().0, &target())
            .unwrap()
            .is_none()
    );
    let mut candidate = decoded(&dormant);
    candidate.header.observed = "minecraft:dirt".into();
    let bytes = encode_frame(&candidate.header, None, fixture().1).unwrap();
    let candidate = decode_slot_frame(&bytes, fixture().1).unwrap();
    let mut destination = restore_anchor(&native_counter, 2);
    let owner = destination.counter_domain_snapshot();
    let native = destination.counter_trial_snapshot();
    let chunk = destination.chunks.get(&(0, 0)).unwrap().clone();
    let random = destination.random.state();
    let rejected = destination
        .restore_persistent_world_slot(
            &other_binding,
            fixture().1,
            native_counter.world_uuid,
            &target(),
            candidate,
        )
        .unwrap_err();
    assert_eq!(rejected.error, SlotRestoreError::StateMismatch);
    assert_eq!(rejected.frame.header.observed, "minecraft:dirt");
    assert!(rejected.frame.allocation.is_none());
    assert!(destination.persistent_world_slot_metadata().is_none());
    assert!(destination.world_slot_trial.is_none());
    assert_eq!(destination.counter_domain_snapshot(), owner);
    assert_eq!(destination.counter_trial_snapshot(), native);
    assert_eq!(destination.random.state(), random);
    assert_eq!(destination.game_time, TIME);
    assert!(Arc::ptr_eq(
        &chunk,
        destination.chunks.get(&(0, 0)).unwrap()
    ));
}

#[test]
fn native_slot_restore_pending_repeated_placement_and_detached_returns() {
    let mut world = filled(rich(7));
    let ptr = address(&world);
    let old = slot_key(&world);
    let initial = world.persistent_world_slot_metadata().unwrap();
    write(&mut world, 0);
    let retirement = world.world_slot_snapshot().unwrap().pending.unwrap();
    retained_presence_without_trial(&mut world);
    assert_eq!(retirement.allocation.key, old);
    assert_eq!(retirement.game_time, TIME);
    assert_eq!(retirement.reason, RetirementReason::BlockChanged);
    for state in [1, 0, 1, 0, 1] {
        write(&mut world, state);
        assert!(world.has_world_slot_trial());
    }
    assert_eq!(world.counter_domain_snapshot().unwrap().high_water, 4);
    let metadata = world.persistent_world_slot_metadata().unwrap();
    assert_eq!(metadata.high_water, initial.high_water);
    assert_eq!(
        metadata.allocation,
        SlotAllocation::Pending {
            slot_generation: 1,
            machine_generation: 1
        }
    );
    assert_eq!(
        world.slot_checkpoint_bytes(fixture().1, TIME),
        Err(SlotRestoreError::PendingRetirement)
    );
    let configuration = world
        .world_slot_trial
        .as_ref()
        .unwrap()
        .borrowed()
        .configuration
        .clone();
    let owned = world.take_world_slot_retirement().unwrap();
    assert!(world.has_world_slot_trial());
    assert_eq!(
        world.persistent_world_slot_metadata().unwrap().status,
        SlotStatus::Detached
    );
    assert!(matches!(
        world.enable_world_slot_trial(configuration),
        Err(crate::counter_trial::ConfigError::AlreadyEnabled)
    ));
    assert!(matches!(
        world.reconcile_world_slot_trial(),
        Err(world_slot_trial::Error::Detached)
    ));
    assert_eq!(
        world.slot_checkpoint_bytes(fixture().1, TIME),
        Err(SlotRestoreError::Detached)
    );
    assert!(world.take_world_slot_retirement().is_none());
    let teardown = world.take_world_slot_trial().unwrap();
    assert!(teardown.active.is_none());
    assert!(teardown.pending.is_none());
    retained_presence_without_trial(&mut world);
    assert!(world.has_world_slot_trial());
    assert_eq!(
        world.persistent_world_slot_metadata().unwrap().high_water,
        1
    );
    let rng = world.random.state();
    let clock = world.game_time;
    let meta = world.persistent_world_slot_metadata();
    for _ in 0..8 {
        assert!(world.has_world_slot_trial());
    }
    assert_eq!(world.random.state(), rng);
    assert_eq!(world.game_time, clock);
    assert_eq!(world.persistent_world_slot_metadata(), meta);
    for state in [0, 1, 0, 1] {
        write(&mut world, state);
    }
    assert!(world.world_slot_trial.is_none());
    let (allocation, reason, time) = owned.into_parts();
    assert_eq!(reason, RetirementReason::BlockChanged);
    assert_eq!(time, TIME);
    let (_, projection, _) = allocation.into_parts();
    let stack = projection.into_stack().unwrap();
    assert_eq!(stack.extra.as_deref().unwrap() as *const _, ptr);
    assert_eq!(stack, rich(7));
}

#[test]
fn native_slot_restore_real_unload_reinsert_and_teardown() {
    for mode in 0..4 {
        let mut world = filled(rich(7));
        let key = slot_key(&world);
        let ptr = address(&world);
        let metadata = world.persistent_world_slot_metadata();
        if mode != 2 {
            world.remove_streamed_chunk(&(0, 0)).unwrap();
        }
        if mode == 0 {
            assert_eq!(
                world.read_world_slot(&key).unwrap_err(),
                world_slot_trial::Error::NotResident
            );
            assert_eq!(
                world.slot_checkpoint_bytes(fixture().1, TIME),
                Err(SlotRestoreError::NotResident)
            );
            world.insert_streamed_chunk(chunk(1));
            assert_eq!(slot_key(&world), key);
            assert_eq!(address(&world), ptr);
            assert_eq!(world.persistent_world_slot_metadata(), metadata);
        } else if mode == 1 {
            world.insert_streamed_chunk(chunk(2));
        } else if mode == 2 {
            world.insert_streamed_chunk(chunk(1));
        }
        let teardown = world.take_world_slot_trial().unwrap();
        assert!(world.has_world_slot_trial());
        let allocation = if mode == 1 || mode == 2 {
            let pending = teardown.pending.unwrap();
            let (allocation, reason, _) = pending.into_parts();
            assert_eq!(
                reason,
                if mode == 1 {
                    RetirementReason::ReloadMismatch
                } else {
                    RetirementReason::ChunkReplaced
                }
            );
            allocation
        } else {
            teardown.active.unwrap()
        };
        let (returned, projection, resident) = allocation.into_parts();
        assert_eq!(returned, key);
        if mode == 3 {
            assert!(!resident);
        }
        assert_eq!(
            projection.into_stack().unwrap().extra.as_deref().unwrap() as *const _,
            ptr
        );
        assert_eq!(
            world.persistent_world_slot_metadata().unwrap().status,
            SlotStatus::Detached
        );
    }
    let mut counter_only = anchor();
    assert!(!counter_only.has_world_slot_trial());
    let config = counter_only
        .counter_trial
        .as_ref()
        .unwrap()
        .configuration()
        .clone();
    counter_only.enable_world_slot_trial(config).unwrap();
    assert!(counter_only.has_world_slot_trial());
    let owned = counter_only.take_world_slot_trial().unwrap();
    assert!(owned.active.is_some());
    assert!(!counter_only.has_world_slot_trial());
}

#[test]
fn native_slot_restore_dormant_heads_and_exhaustion() {
    let mut source = anchor();
    write(&mut source, 0);
    let mut counter = source.counter_checkpoint_frame(TIME).unwrap();
    counter.allocation_high_water = 5;
    let mut dormant = restore_anchor(&counter, 0);
    assert!(
        dormant
            .initialize_persistent_world_slot(fixture().0, &target())
            .unwrap()
            .is_none()
    );
    retained_presence_without_trial(&mut dormant);
    assert!(dormant.has_world_slot_trial());
    let mut frame = decoded(&dormant);
    frame.header.slot_head = 20;
    let bytes = encode_frame(&frame.header, None, fixture().1).unwrap();
    let frame = decode_slot_frame(&bytes, fixture().1).unwrap();
    let mut destination = restore_anchor(&counter, 0);
    assert!(
        destination
            .restore_persistent_world_slot(
                fixture().0,
                fixture().1,
                counter.world_uuid,
                &target(),
                frame
            )
            .unwrap()
            .is_none()
    );
    assert_eq!(
        destination
            .persistent_world_slot_metadata()
            .unwrap()
            .high_water,
        20
    );
    write(&mut destination, 1);
    assert_eq!(
        destination
            .persistent_world_slot_metadata()
            .unwrap()
            .allocation,
        SlotAllocation::Dormant
    );
    assert!(
        destination
            .world_slot_trial
            .as_ref()
            .unwrap()
            .borrowed()
            .active
            .is_none()
    );
    assert!(
        destination
            .slot_checkpoint_bytes(fixture().1, TIME)
            .is_err()
    );
    let key = destination
        .reconcile_persistent_world_slot()
        .unwrap()
        .unwrap();
    assert_eq!(key.incarnation(), 21);
    assert_eq!(
        destination
            .persistent_world_slot_metadata()
            .unwrap()
            .allocation,
        SlotAllocation::Active {
            slot_generation: 21,
            machine_generation: 6
        }
    );
    let mut frame = decoded(&dormant);
    frame.header.slot_head = u64::MAX;
    let mut exhausted = restore_anchor(&counter, 0);
    exhausted
        .restore_persistent_world_slot(
            fixture().0,
            fixture().1,
            counter.world_uuid,
            &target(),
            frame,
        )
        .unwrap();
    write(&mut exhausted, 1);
    let before_counter = exhausted.counter_trial_snapshot();
    let before_domain = exhausted.counter_domain_snapshot();
    let before_slot = exhausted.persistent_world_slot_metadata();
    assert_eq!(
        exhausted.reconcile_persistent_world_slot(),
        Err(SlotLifecycleError::GenerationExhausted)
    );
    assert_eq!(exhausted.counter_trial_snapshot(), before_counter);
    assert_eq!(exhausted.counter_domain_snapshot(), before_domain);
    assert_eq!(exhausted.persistent_world_slot_metadata(), before_slot);
    assert!(
        exhausted
            .world_slot_trial
            .as_ref()
            .unwrap()
            .borrowed()
            .active
            .is_none()
    );
    assert!(exhausted.has_world_slot_trial());
    assert_eq!(
        exhausted
            .persistent_world_slot_metadata()
            .unwrap()
            .high_water,
        u64::MAX
    );
    assert_eq!(
        exhausted.persistent_world_slot_metadata().unwrap().status,
        SlotStatus::Healthy
    );
    retained_presence_without_trial(&mut exhausted);
    let held = exhausted.take_world_slot_trial().unwrap();
    assert!(held.active.is_none());
    assert!(exhausted.has_world_slot_trial());
    let source = filled(rich(7));
    let counter = source.counter_checkpoint_frame(TIME).unwrap();
    let mut frame = decoded(&source);
    frame.allocation.as_mut().unwrap().revision = u64::MAX;
    let mut destination = restore_anchor(&counter, 1);
    let key = destination
        .restore_persistent_world_slot(
            fixture().0,
            fixture().1,
            counter.world_uuid,
            &target(),
            frame,
        )
        .unwrap()
        .unwrap();
    let ptr = address(&destination);
    let mut receiver = SlotProjection::new(None);
    let plan = match destination
        .prepare_world_slot_transfer(&key, &receiver, Direction::IntoPlayer, 1, caps())
        .unwrap()
    {
        Preparation::Prepared(plan) => *plan,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        destination.commit_world_slot_transfer(plan, &mut receiver, caps()),
        Err(world_slot_trial::Error::Transfer(
            crate::slot_transfer::TransferError::RevisionExhausted
        ))
    );
    assert_eq!(address(&destination), ptr);
    assert!(receiver.borrowed().1.is_none());
    assert_eq!(
        destination
            .world_slot_trial
            .as_ref()
            .unwrap()
            .borrowed()
            .active
            .unwrap()
            .revision,
        u64::MAX
    );
}

fn pending_address(world: &World) -> *const crate::item_data::StackData {
    world
        .world_slot_trial
        .as_ref()
        .unwrap()
        .persistent_pending()
        .unwrap()
        .allocation
        .stack
        .unwrap()
        .extra
        .as_deref()
        .unwrap() as *const _
}
fn refused_take(world: &mut World, expected: &SlotKey, error: SlotLifecycleError) {
    let metadata = world.persistent_world_slot_metadata();
    let domain = world.counter_domain_snapshot();
    let counter = world.counter_trial_snapshot();
    let snapshot = format!("{:?}", world.world_slot_snapshot());
    let ptr = pending_address(world);
    let trial = world.world_slot_trial.as_ref().unwrap().as_ref() as *const _;
    let rng = world.random.state();
    let clock = world.game_time;
    assert_eq!(
        world
            .take_persistent_world_slot_retirement(expected)
            .unwrap_err(),
        error
    );
    assert_eq!(world.persistent_world_slot_metadata(), metadata);
    assert_eq!(world.counter_domain_snapshot(), domain);
    assert_eq!(world.counter_trial_snapshot(), counter);
    assert_eq!(format!("{:?}", world.world_slot_snapshot()), snapshot);
    assert_eq!(pending_address(world), ptr);
    assert_eq!(
        world.world_slot_trial.as_ref().unwrap().as_ref() as *const _,
        trial
    );
    assert_eq!(world.random.state(), rng);
    assert_eq!(world.game_time, clock);
}

#[test]
fn persistent_slot_retirement_keeps_original_owner_and_independent_heads() {
    let mut world = filled(rich(7));
    let old = slot_key(&world);
    let old_counter = world.persistent_counter_key().unwrap().unwrap();
    let ptr = address(&world);
    let trial_ptr = world.world_slot_trial.as_ref().unwrap().as_ref() as *const _;
    let mut receiver = SlotProjection::new(None);
    let stale = match world
        .prepare_world_slot_transfer(&old, &receiver, Direction::IntoPlayer, 2, caps())
        .unwrap()
    {
        Preparation::Prepared(plan) => *plan,
        other => panic!("expected stale plan: {other:?}"),
    };
    write(&mut world, 0);
    for state in [1, 0, 1, 0, 1] {
        write(&mut world, state);
        assert_eq!(pending_address(&world), ptr);
    }
    assert_eq!(world.counter_domain_snapshot().unwrap().high_water, 4);
    assert_eq!(
        world.persistent_world_slot_metadata().unwrap().high_water,
        1
    );
    assert_eq!(
        world.reconcile_persistent_world_slot(),
        Err(SlotLifecycleError::PendingRetirement)
    );
    let independent = filled(rich(7));
    refused_take(
        &mut world,
        &slot_key(&independent),
        SlotLifecycleError::StaleKey,
    );
    let before_counter = world.counter_trial_snapshot();
    let before_domain = world.counter_domain_snapshot();
    let rng = world.random.state();
    let owned = world.take_persistent_world_slot_retirement(&old).unwrap();
    assert_eq!(world.counter_trial_snapshot(), before_counter);
    assert_eq!(world.counter_domain_snapshot(), before_domain);
    assert_eq!(world.random.state(), rng);
    assert_eq!(world.game_time, TIME);
    assert_eq!(
        world.world_slot_trial.as_ref().unwrap().as_ref() as *const _,
        trial_ptr
    );
    assert_eq!(
        world.persistent_world_slot_metadata(),
        Some(SlotMetadataSnapshot {
            high_water: 1,
            allocation: SlotAllocation::Dormant,
            status: SlotStatus::Healthy,
        })
    );
    assert_eq!(
        world
            .take_persistent_world_slot_retirement(&old)
            .unwrap_err(),
        SlotLifecycleError::MissingPending
    );
    assert!(world.slot_checkpoint_bytes(fixture().1, TIME).is_err());
    assert!(world.read_world_slot(&old).is_err());
    // Same-state writes do nothing: reconciliation must explicitly close this gap.
    write(&mut world, 1);
    assert!(
        world
            .world_slot_trial
            .as_ref()
            .unwrap()
            .borrowed()
            .active
            .is_none()
    );
    let fresh = world.reconcile_persistent_world_slot().unwrap().unwrap();
    assert_eq!(fresh.incarnation(), 2);
    assert_ne!(fresh, old);
    assert_eq!(fresh.scope(), old.scope());
    assert_eq!(
        world.reconcile_persistent_world_slot().unwrap(),
        Some(fresh.clone())
    );
    assert_eq!(world.counter_trial_snapshot(), before_counter);
    assert_eq!(world.counter_domain_snapshot(), before_domain);
    assert_eq!(
        world.persistent_world_slot_metadata().unwrap().allocation,
        SlotAllocation::Active {
            slot_generation: 2,
            machine_generation: 4,
        }
    );
    assert_eq!(world.read_world_slot(&fresh).unwrap().slot.stack(), None);
    assert_eq!(
        world.read_world_slot(&old).unwrap_err(),
        world_slot_trial::Error::StaleKey
    );
    assert!(world.read_persistent_counter(&old_counter).is_err());
    assert_eq!(
        world.commit_world_slot_transfer(stale, &mut receiver, caps()),
        Err(world_slot_trial::Error::StaleKey)
    );
    assert!(receiver.into_stack().is_none());
    let (allocation, reason, clock) = owned.into_parts();
    assert_eq!(reason, RetirementReason::BlockChanged);
    assert_eq!(clock, TIME);
    let (key, projection, resident) = allocation.into_parts();
    assert_eq!(key, old);
    assert!(resident);
    let stack = projection.into_stack().unwrap();
    assert_eq!(stack.extra.as_deref().unwrap() as *const _, ptr);
    assert_eq!(stack, rich(7));
}

#[test]
fn persistent_slot_delivery_before_replacement_requires_explicit_reconcile() {
    let mut world = filled(rich(7));
    let old = slot_key(&world);
    let ptr = address(&world);
    write(&mut world, 0);
    let owned = world.take_persistent_world_slot_retirement(&old).unwrap();
    let dormant = world.persistent_world_slot_metadata();
    assert_eq!(world.reconcile_persistent_world_slot(), Ok(None));
    assert_eq!(world.reconcile_persistent_world_slot(), Ok(None));
    assert_eq!(world.persistent_world_slot_metadata(), dormant);
    assert!(world.slot_checkpoint_bytes(fixture().1, TIME).is_ok());
    write(&mut world, 1);
    assert_eq!(world.persistent_world_slot_metadata(), dormant);
    assert!(
        world
            .world_slot_trial
            .as_ref()
            .unwrap()
            .borrowed()
            .active
            .is_none()
    );
    assert!(world.slot_checkpoint_bytes(fixture().1, TIME).is_err());
    assert!(
        world
            .prepare_world_slot_transfer(
                &old,
                &SlotProjection::new(None),
                Direction::IntoPlayer,
                1,
                caps()
            )
            .is_err()
    );
    let fresh = world.reconcile_persistent_world_slot().unwrap().unwrap();
    assert_eq!(fresh.incarnation(), 2);
    assert_eq!(
        world.persistent_world_slot_metadata().unwrap().allocation,
        SlotAllocation::Active {
            slot_generation: 2,
            machine_generation: 2,
        }
    );
    let (allocation, _, _) = owned.into_parts();
    let (_, projection, _) = allocation.into_parts();
    let stack = projection.into_stack().unwrap();
    assert_eq!(stack.extra.as_deref().unwrap() as *const _, ptr);
    assert_eq!(stack, rich(7));
}

#[test]
fn persistent_slot_checked_take_refusals_preserve_nested_owner() {
    for mode in 0..10 {
        let mut world = filled(rich(7));
        let key = slot_key(&world);
        write(&mut world, 0);
        let error = match mode {
            0 => {
                world.remove_streamed_chunk(&(0, 0)).unwrap();
                SlotLifecycleError::NotResident
            }
            1 => {
                world.game_time = -1;
                SlotLifecycleError::ClockMismatch
            }
            2 => {
                world.game_time = TIME - 1;
                SlotLifecycleError::ClockMismatch
            }
            3 => {
                world.world_slot_trial.as_mut().unwrap().persistent_fault();
                SlotLifecycleError::Faulted
            }
            4 => {
                world.slot_fault();
                SlotLifecycleError::Faulted
            }
            5 => {
                world.detach_persistent_slot();
                SlotLifecycleError::Detached
            }
            6 => {
                let metadata = world
                    .counter_domain
                    .as_mut()
                    .unwrap()
                    .slot
                    .as_mut()
                    .unwrap();
                metadata.high_water += 1;
                SlotLifecycleError::BindingMismatch
            }
            7 => {
                world
                    .counter_domain
                    .as_mut()
                    .unwrap()
                    .slot
                    .as_mut()
                    .unwrap()
                    .allocation = SlotAllocation::Pending {
                    slot_generation: 1,
                    machine_generation: 2,
                };
                SlotLifecycleError::BindingMismatch
            }
            8 => {
                // Corrupt only physical context, bypassing hooks to test a stale anchor.
                Arc::make_mut(world.chunks.get_mut(&(0, 0)).unwrap()).set(2, 70, 2, 1);
                SlotLifecycleError::StateMismatch
            }
            _ => {
                let configuration = world.counter_trial.as_ref().unwrap().configuration();
                let changed = crate::counter_trial::Configuration::new(
                    crate::mutation_observer::AuthorityScope::new(
                        configuration.scope().world_instance() + 1,
                        "minecraft:overworld",
                    )
                    .unwrap(),
                    configuration.machine_type(),
                    configuration.state().clone(),
                    configuration.state_id(),
                    POSITION,
                )
                .unwrap();
                world.counter_trial = Some(Box::new(crate::counter_trial::Trial::new(
                    changed,
                    Some(BlockStateId(0)),
                )));
                SlotLifecycleError::BindingMismatch
            }
        };
        refused_take(&mut world, &key, error);
    }
    let mut no_slot = anchor();
    let unrelated = slot_key(&filled(rich(7)));
    assert_eq!(
        no_slot
            .take_persistent_world_slot_retirement(&unrelated)
            .unwrap_err(),
        SlotLifecycleError::Disabled
    );
    assert_eq!(
        no_slot.reconcile_persistent_world_slot(),
        Err(SlotLifecycleError::Disabled)
    );
    let mut active = filled(rich(7));
    let active_key = slot_key(&active);
    assert_eq!(
        active
            .take_persistent_world_slot_retirement(&active_key)
            .unwrap_err(),
        SlotLifecycleError::MissingPending
    );
}

#[test]
fn persistent_slot_reconcile_refusals_and_streamer_keep_exact_state() {
    for mode in 0..7 {
        let mut world = filled(rich(7));
        let key = slot_key(&world);
        if mode == 0 {
            write(&mut world, 0);
        } else if mode == 1 {
            world.remove_streamed_chunk(&(0, 0)).unwrap();
        } else if mode == 2 {
            world.detach_persistent_slot();
        } else if mode == 3 {
            world.slot_fault();
        } else if mode == 4 {
            world.game_time = -1;
        } else if mode == 5 {
            // Actual resident replacement retains the preexisting explicit fault.
            world.insert_streamed_chunk(chunk(1));
        } else {
            world.remove_streamed_chunk(&(0, 0)).unwrap();
            world.insert_streamed_chunk(chunk(2));
        }
        let expected = [
            SlotLifecycleError::PendingRetirement,
            SlotLifecycleError::NotResident,
            SlotLifecycleError::Detached,
            SlotLifecycleError::Faulted,
            SlotLifecycleError::ClockMismatch,
            SlotLifecycleError::Faulted,
            SlotLifecycleError::Faulted,
        ][mode];
        let before_slot = world.persistent_world_slot_metadata();
        let before_counter = world.counter_trial_snapshot();
        let before_domain = world.counter_domain_snapshot();
        let snapshot = format!("{:?}", world.world_slot_snapshot());
        let rng = world.random.state();
        let clock = world.game_time;
        assert_eq!(world.reconcile_persistent_world_slot(), Err(expected));
        assert_eq!(world.persistent_world_slot_metadata(), before_slot);
        assert_eq!(world.counter_trial_snapshot(), before_counter);
        assert_eq!(world.counter_domain_snapshot(), before_domain);
        assert_eq!(format!("{:?}", world.world_slot_snapshot()), snapshot);
        assert_eq!(world.random.state(), rng);
        assert_eq!(world.game_time, clock);
        if mode == 0 {
            assert_eq!(
                world
                    .world_slot_snapshot()
                    .unwrap()
                    .pending
                    .unwrap()
                    .allocation
                    .key,
                key
            );
        } else if mode >= 5 {
            refused_take(&mut world, &key, SlotLifecycleError::Faulted);
        }
    }
    let mut world = filled(rich(7));
    let old = slot_key(&world);
    write(&mut world, 0);
    write(&mut world, 1);
    let retained = world.take_persistent_world_slot_retirement(&old).unwrap();
    let key = world.reconcile_persistent_world_slot().unwrap().unwrap();
    let mut donor = SlotProjection::new(Some(rich(3)));
    let plan = match world
        .prepare_world_slot_transfer(&key, &donor, Direction::IntoMachine, 3, caps())
        .unwrap()
    {
        Preparation::Prepared(plan) => *plan,
        other => panic!("expected new-slot transfer: {other:?}"),
    };
    world
        .commit_world_slot_transfer(plan, &mut donor, caps())
        .unwrap();
    let ptr = address(&world);
    assert_eq!(world.advance_counter_trial(true, true), Some(Ok(true)));
    let before = world.counter_trial_snapshot().unwrap().record().unwrap();
    let counter_key = world.persistent_counter_key().unwrap().unwrap();
    let metadata = world.persistent_world_slot_metadata();
    let domain = world.counter_domain_snapshot();
    world.remove_streamed_chunk(&(0, 0)).unwrap();
    world.game_time += 100;
    assert_eq!(
        world.reconcile_persistent_world_slot(),
        Err(SlotLifecycleError::NotResident)
    );
    world.insert_streamed_chunk(chunk(1));
    assert_eq!(
        world.reconcile_persistent_world_slot(),
        Ok(Some(key.clone()))
    );
    assert_eq!(address(&world), ptr);
    assert_eq!(slot_key(&world), key);
    assert_eq!(world.persistent_world_slot_metadata(), metadata);
    assert_eq!(world.counter_domain_snapshot(), domain);
    assert_eq!(world.read_persistent_counter(&counter_key).unwrap(), before);
    assert_eq!(world.advance_counter_trial(true, true), Some(Ok(true)));
    assert_eq!(
        world
            .read_persistent_counter(&counter_key)
            .unwrap()
            .counter(),
        before.counter() + 1
    );
    assert_eq!(
        world
            .world_slot_trial
            .as_ref()
            .unwrap()
            .borrowed()
            .active
            .unwrap()
            .stack,
        Some(&rich(3))
    );
    // Retain the earlier owner throughout this new allocation's unload/reload.
    assert_eq!(retained.snapshot().allocation.slot.stack(), Some(&rich(7)));
}

#[test]
fn persistent_slot_counter_exhaustion_does_not_admit_or_drop_pending() {
    let source = filled(rich(7));
    let mut counter = source.counter_checkpoint_frame(TIME).unwrap();
    counter.allocation_high_water = u64::MAX;
    let mut frame = decoded(&source);
    frame.header.machine_head = u64::MAX;
    let mut world = restore_anchor(&counter, 1);
    let key = world
        .restore_persistent_world_slot(
            fixture().0,
            fixture().1,
            counter.world_uuid,
            &target(),
            frame,
        )
        .unwrap()
        .unwrap();
    let ptr = address(&world);
    write(&mut world, 0);
    write(&mut world, 1);
    assert_eq!(
        world.counter_domain_snapshot().unwrap().high_water,
        u64::MAX
    );
    assert_eq!(
        world.persistent_world_slot_metadata().unwrap().high_water,
        1
    );
    assert_eq!(pending_address(&world), ptr);
    refused_take(&mut world, &key, SlotLifecycleError::Faulted);
    assert_eq!(
        world.reconcile_persistent_world_slot(),
        Err(SlotLifecycleError::Faulted)
    );
}
