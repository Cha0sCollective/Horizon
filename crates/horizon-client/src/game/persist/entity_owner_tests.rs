//! Native private EntityMemory maps; no production privacy widening.
use super::*;
use crate::game::{
    Dim,
    integrated::transition_ownership_tests::Fixture,
    levels::{
        EntityOwnerObservationError as E,
        entity_owner_tests::{assert_refusal, memory, ready},
    },
};

fn retained(
    memory: &EntityMemory,
) -> (
    HashMap<u32, [i32; 4]>,
    HashMap<u32, Tag>,
    HashMap<(i32, i32), Vec<Tag>>,
    HashMap<(String, [u64; 3]), (Vec3, Option<[i32; 4]>, Tag)>,
) {
    (
        memory.uuids.clone(),
        memory.raw.clone(),
        memory.unknown.clone(),
        memory.anon.clone(),
    )
}
fn allocations(memory: &EntityMemory) -> Vec<usize> {
    memory
        .uuids
        .values()
        .map(|value| value as *const _ as usize)
        .chain(memory.raw.values().map(|value| value as *const _ as usize))
        .chain(memory.unknown.values().map(|value| value.as_ptr() as usize))
        .chain(memory.anon.values().map(|value| value as *const _ as usize))
        .collect()
}
fn opaque() -> Tag {
    Tag::compound([
        ("id", Tag::String("fixture:opaque_owner".into())),
        (
            "nested",
            Tag::compound([("payload", Tag::List(vec![Tag::Int(-73), Tag::Long(97)]))]),
        ),
    ])
}
#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn entity_owner_observation_refuses_every_retained_memory_map_active_and_parked() {
    for parked in [false, true] {
        for kind in [
            EntityMemoryKind::Uuids,
            EntityMemoryKind::Raw,
            EntityMemoryKind::Unknown,
            EntityMemoryKind::Anonymous,
        ] {
            for empty in [false, true] {
                let fixture = Fixture::new("entity-owner-retained-memory");
                let (mut game, _body, _receivers) = ready(&fixture);
                if parked {
                    game.enter_level(None, Dim::Nether).unwrap();
                }
                let dimension = game.dim;
                match kind {
                    EntityMemoryKind::Uuids => {
                        game.persist.uuids.insert(77, [-1, 2, -3, 4]);
                    }
                    EntityMemoryKind::Raw => {
                        game.persist.raw.insert(
                            77,
                            if empty {
                                Tag::compound([] as [(&str, Tag); 0])
                            } else {
                                opaque()
                            },
                        );
                    }
                    EntityMemoryKind::Unknown => {
                        game.persist
                            .unknown
                            .insert((-1, 2), if empty { Vec::new() } else { vec![opaque()] });
                    }
                    EntityMemoryKind::Anonymous => {
                        let position = Vec3::new(-0.1, 71.0, 33.25);
                        game.persist.anon.insert(
                            anon_key("fixture:anonymous", position),
                            (
                                position,
                                if empty { None } else { Some([-5, 6, -7, 8]) },
                                opaque(),
                            ),
                        );
                    }
                }
                let before = retained(&game.persist);
                let pointers = (
                    game.persist.uuids.get(&77).map(|v| v as *const _ as usize),
                    game.persist.raw.get(&77).map(|v| v as *const _ as usize),
                    game.persist.unknown.get(&(-1, 2)).map(Vec::as_ptr),
                );
                if parked {
                    game.enter_level(None, Dim::Overworld).unwrap();
                }
                // A parked map moved with the real native swap; snapshot its post-swap allocation.
                let after_swap = retained(memory(&game, dimension));
                assert_eq!(after_swap, before);
                let native_allocations = allocations(memory(&game, dimension));
                assert_refusal(&mut game, E::EntityMemory { dimension, kind });
                assert_eq!(allocations(memory(&game, dimension)), native_allocations);
                assert_eq!(retained(memory(&game, dimension)), before);
                if !parked {
                    assert_eq!(
                        (
                            game.persist.uuids.get(&77).map(|v| v as *const _ as usize),
                            game.persist.raw.get(&77).map(|v| v as *const _ as usize),
                            game.persist.unknown.get(&(-1, 2)).map(Vec::as_ptr)
                        ),
                        pointers
                    );
                }
            }
        }
    }
}
#[test]
fn entity_memory_observation_uses_stable_first_kind_and_retains_empty_entries() {
    let mut memory = EntityMemory::default();
    assert_eq!(memory.checkpoint_entity_memory(), None);
    let at = Vec3::new(-0.1, 71.0, 2.5);
    memory
        .anon
        .insert(anon_key("fixture:anonymous", at), (at, None, opaque()));
    assert_eq!(
        memory.checkpoint_entity_memory(),
        Some(EntityMemoryKind::Anonymous)
    );
    memory.unknown.insert((0, 0), Vec::new());
    assert_eq!(
        memory.checkpoint_entity_memory(),
        Some(EntityMemoryKind::Unknown)
    );
    memory.raw.insert(17, Tag::Int(-73));
    assert_eq!(
        memory.checkpoint_entity_memory(),
        Some(EntityMemoryKind::Raw)
    );
    memory.uuids.insert(17, [1, 2, 3, 4]);
    let before = retained(&memory);
    assert_eq!(
        memory.checkpoint_entity_memory(),
        Some(EntityMemoryKind::Uuids)
    );
    assert_eq!(retained(&memory), before);
}
