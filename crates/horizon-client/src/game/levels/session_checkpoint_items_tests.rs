//! Closed authored native records exercise the format contract independently of
//! the live entity encoder. No resources, subprocesses or Game are manufactured.
use super::*;

fn record() -> Tag {
    Tag::compound([
        ("id", Tag::String("minecraft:item".into())),
        ("UUID", Tag::IntArray(vec![11, -22, 33, -44])),
        (
            "Pos",
            Tag::List(vec![
                Tag::Double(2.5f64.to_bits()),
                Tag::Double(120.5f64.to_bits()),
                Tag::Double(2.5f64.to_bits()),
            ]),
        ),
        (
            "Motion",
            Tag::List(vec![
                Tag::Double(0.125f64.to_bits()),
                Tag::Double((-0.25f64).to_bits()),
                Tag::Double(0),
            ]),
        ),
        ("Rotation", Tag::List(vec![Tag::Float(0), Tag::Float(0)])),
        ("OnGround", Tag::Byte(1)),
        ("fall_distance", Tag::Double(0)),
        ("Fire", Tag::Short(-7)),
        ("Air", Tag::Short(300)),
        ("Invulnerable", Tag::Byte(0)),
        ("PortalCooldown", Tag::Int(0)),
        (
            "Item",
            Tag::compound([
                ("id", Tag::String("minecraft:stone".into())),
                ("count", Tag::Int(7)),
            ]),
        ),
        ("Health", Tag::Short(4)),
        ("Age", Tag::Short(201)),
        ("PickupDelay", Tag::Short(17)),
        ("Thrower", Tag::IntArray(vec![1, 2, 3, 4])),
        ("Owner", Tag::IntArray(vec![5, 6, 7, 8])),
    ])
}
fn field(tag: &mut Tag, key: &str, value: Tag) {
    let Tag::Compound(fields) = tag else {
        panic!("test compound")
    };
    fields.insert(key.into(), value);
}

#[test]
fn session_items_closed_native_record_preserves_scalars_and_durable_components() {
    let mut tag = record();
    let item = tag.get_mut("Item").unwrap();
    field(
        item,
        "components",
        Tag::compound([
            ("minecraft:damage", Tag::Int(7)),
            (
                "minecraft:custom_data",
                Tag::compound([("opaque", Tag::Long(-91))]),
            ),
            (
                "minecraft:container",
                Tag::List(vec![Tag::compound([
                    ("slot", Tag::Int(2)),
                    (
                        "item",
                        Tag::compound([
                            ("id", Tag::String("minecraft:dirt".into())),
                            ("count", Tag::Int(2)),
                        ]),
                    ),
                ])]),
            ),
        ]),
    );
    let (uuid, item) = decode_native_item_profile(&tag).unwrap();
    assert_eq!(uuid, [11, -22, 33, -44]);
    assert_eq!(
        (
            item.stack.count,
            item.stack.damage,
            item.health,
            item.fire_ticks,
            item.age,
            item.pickup_delay
        ),
        (7, 7, 4, -7, 201, 17)
    );
    assert_eq!(item.velocity.z.to_bits(), 0);
    assert_eq!(item.previous, item.position);
    assert_eq!((item.ticks, item.bob_offset.to_bits()), (0, 0));
    assert_eq!(item.stack.data().unwrap().container[0].1.count, 2);
    assert_eq!(entity_nbt::item_entity_to_nbt(&item, uuid, Some(&tag)), tag);
}

#[test]
fn session_items_closed_profile_rejects_missing_unknown_types_coercions_and_behavior() {
    for (key, value) in [
        ("id", Tag::String("minecraft:pig".into())),
        ("UUID", Tag::IntArray(vec![1, 2, 3])),
        ("Owner", Tag::LongArray(vec![1, 2, 3, 4])),
        ("Thrower", Tag::IntArray(vec![])),
        ("OnGround", Tag::Byte(2)),
        ("OnGround", Tag::Int(1)),
        ("Health", Tag::Int(5)),
        ("Fire", Tag::Int(0)),
        ("Age", Tag::Short(-1)),
        ("Age", Tag::Short(i16::MIN)),
        ("PickupDelay", Tag::Short(-1)),
        ("PortalCooldown", Tag::Int(-1)),
        ("Air", Tag::Short(299)),
        ("Invulnerable", Tag::Byte(1)),
        (
            "Rotation",
            Tag::List(vec![Tag::Float((-0.0f32).to_bits()), Tag::Float(0)]),
        ),
        ("fall_distance", Tag::Double((-0.0f64).to_bits())),
        ("Pos", Tag::List(vec![Tag::Double(f64::NAN.to_bits()); 3])),
        ("Motion", Tag::List(vec![Tag::Float(0); 3])),
        (
            "Motion",
            Tag::List(vec![Tag::Double((-0.0f64).to_bits()); 3]),
        ),
        ("Passengers", Tag::List(vec![])),
        ("unmodelled_behavior", Tag::Byte(0)),
    ] {
        let mut tag = record();
        field(&mut tag, key, value);
        assert!(decode_native_item_profile(&tag).is_err(), "{key}");
    }
    let Tag::Compound(all) = record() else {
        unreachable!()
    };
    for key in all
        .keys()
        .filter(|k| !matches!(k.to_string_lossy().as_str(), "Thrower" | "Owner"))
    {
        let mut tag = Tag::Compound(all.clone());
        let Tag::Compound(fields) = &mut tag else {
            unreachable!()
        };
        fields.remove(key);
        assert!(decode_native_item_profile(&tag).is_err(), "missing {key:?}");
    }
    for count in [Tag::Byte(7), Tag::Int(0), Tag::Int(-1), Tag::Long(7)] {
        let mut tag = record();
        field(tag.get_mut("Item").unwrap(), "count", count);
        assert!(decode_native_item_profile(&tag).is_err());
    }
}

#[test]
fn session_items_region_names_refuse_alias_overflow_and_unsupported_owners() {
    for name in [
        "dimensions/minecraft/overworld/entities/r.0.0.mca",
        "dimensions/minecraft/overworld/region/r.-1.2.mca",
    ] {
        assert!(native_region(name).is_ok());
    }
    for name in [
        "entities/r.0.0.mca",
        "dimensions/minecraft/the_nether/entities/r.0.0.mca",
        "dimensions/minecraft/overworld/entities/r.00.0.mca",
        "dimensions/minecraft/overworld/entities/r.+1.0.mca",
        "dimensions/minecraft/overworld/entities/r.2147483647.0.mca",
        "dimensions/minecraft/overworld/entities/r.-2147483648.0.mca",
        "dimensions/minecraft/overworld/entities/r.0.0.mca.bak",
    ] {
        assert!(native_region(name).is_err(), "{name}");
    }
}

#[test]
fn session_items_raw_records_reject_duplicate_keys_trailing_utf_and_structural_bombs() {
    // Named compound, two byte fields with the same name, then END.
    assert!(raw_nbt(&[10, 0, 0, 1, 0, 1, b'x', 1, 1, 0, 1, b'x', 2, 0]).is_err());
    // A string with an unpaired Java UTF-16 surrogate encoded as modified UTF-8.
    assert!(raw_nbt(&[10, 0, 0, 8, 0, 1, b'x', 0, 3, 0xed, 0xa0, 0x80, 0]).is_err());
    assert!(raw_nbt(&[10, 0, 0, 0, 0]).is_err());
    let mut oversized = vec![10, 0, 0, 9, 0, 1, b'x', 1];
    oversized.extend_from_slice(&16_385i32.to_be_bytes());
    assert!(raw_nbt(&oversized).is_err());
}

#[test]
fn session_items_raw_signed_zero_and_entity_mixed_list_wrappers_refuse_before_native_normalization()
{
    let raw = [10, 0, 0, 6, 0, 1, b'x', 0x80, 0, 0, 0, 0, 0, 0, 0, 0];
    assert_eq!(raw_nbt(&raw).unwrap().tag.get("x"), Some(&Tag::Double(0)));
    assert!(raw_item_nbt(&raw).is_err());
    // Encode one actual wire wrapper. write_named would add a protective second
    // wrapper around an empty-key compound, preserving that compound on read.
    let item = nbt::write_named(
        &NamedTag {
            name: "".into(),
            tag: record(),
        },
        limits(),
    )
    .unwrap();
    assert_eq!(&item[..3], &[10, 0, 0]);
    let mut raw = vec![
        10, 0, 0, // named root compound
        9, 0, 8, b'E', b'n', b't', b'i', b't', b'i', b'e', b's', 10, 0, 0, 0,
        1, // one compound list element
        10, 0, 0, // that element wraps the item under an empty compound key
    ];
    raw.extend_from_slice(&item[3..]);
    raw.extend_from_slice(&[0, 0]); // wrapper END, root END
    assert_eq!(
        raw_nbt(&raw).unwrap().tag.get("Entities"),
        Some(&Tag::List(vec![record()]))
    );
    assert!(raw_item_nbt(&raw).is_err());
}

#[test]
fn session_items_strings_are_bounded_in_utf16_units_without_expanding_the_record_cap() {
    let mut tag = record();
    field(
        tag.get_mut("Item").unwrap(),
        "components",
        Tag::compound([(
            "minecraft:custom_name",
            Tag::String("é".repeat(4096).as_str().into()),
        )]),
    );
    assert!(decode_native_item_profile(&tag).is_ok());
    field(
        tag.get_mut("Item").unwrap(),
        "components",
        Tag::compound([(
            "minecraft:custom_name",
            Tag::String("é".repeat(4097).as_str().into()),
        )]),
    );
    assert!(decode_native_item_profile(&tag).is_err());
}

#[test]
fn session_items_canonical_template_one_omission_is_supported_but_coercive_counts_are_not() {
    let mut tag = record();
    let template = Tag::compound([("id", Tag::String("minecraft:dirt".into()))]);
    let components = |template| {
        Tag::compound([(
            "minecraft:container",
            Tag::List(vec![Tag::compound([
                ("slot", Tag::Int(2)),
                ("item", template),
            ])]),
        )])
    };
    field(
        tag.get_mut("Item").unwrap(),
        "components",
        components(template.clone()),
    );
    let (_, decoded) = decode_native_item_profile(&tag).unwrap();
    assert_eq!(decoded.stack.data().unwrap().container[0].1.count, 1);
    for count in [Tag::Byte(1), Tag::Int(0), Tag::Int(-1), Tag::Long(2)] {
        let mut child = template.clone();
        field(&mut child, "count", count);
        field(
            tag.get_mut("Item").unwrap(),
            "components",
            components(child),
        );
        assert!(decode_native_item_profile(&tag).is_err());
    }
    let mut tag = record();
    let Some(Tag::Compound(item)) = tag.get_mut("Item") else {
        unreachable!()
    };
    item.remove(&"count".into());
    assert!(decode_native_item_profile(&tag).is_err());
}

#[test]
fn session_items_native_decompressors_leave_no_unclaimed_stream_or_nbt_bytes() {
    let root = NamedTag {
        name: "".into(),
        tag: Tag::compound([
            ("DataVersion", Tag::Int(DATA_VERSION)),
            ("Position", Tag::IntArray(vec![0, 0])),
            ("Entities", Tag::List(vec![])),
        ]),
    };
    for compression in [
        Compression::None,
        Compression::Gzip,
        Compression::Zlib,
        Compression::Lz4,
    ] {
        let mut record =
            horizon_world::region::ChunkRecord::from_nbt(&root, compression, 0, limits()).unwrap();
        assert_eq!(read_item_record(&record).unwrap(), root);
        record.compressed.push(99);
        assert!(read_item_record(&record).is_err(), "{compression:?}");
    }
    // Native's tolerant string reader accepts this overlong ASCII encoding;
    // the item profile rejects it before issuing checked fields.
    assert!(raw_item_nbt(&[10, 0, 0, 8, 0, 1, b'x', 0, 2, 0xc1, 0x81, 0]).is_err());
}
