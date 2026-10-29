//! Native private portal plans/opaque arrivals, observed without replay or normalization.
use super::*;
use crate::game::{
    integrated::transition_ownership_tests::Fixture,
    levels::{
        EntityOwnerObservationError as E,
        entity_owner_tests::{assert_refusal, players, ready, snapshot},
    },
};
fn retained(portals: &Portals) -> String {
    format!(
        "{:?}",
        (
            portals.player,
            portals.travel,
            portals.client_inside,
            &portals.entities,
            &portals.before,
            &portals.arrivals,
            portals.arrivals.as_ptr(),
            portals.loaded,
            portals.dirty
        )
    )
}
#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn entity_owner_observation_refuses_native_portal_travel_and_opaque_arrivals() {
    for travel in [false, true] {
        let fixture = Fixture::new("entity-owner-portal-retention");
        let (mut game, _body, _receivers) = ready(&fixture);
        let portals = &mut game.portals;
        portals.loaded = true;
        portals.dirty = true;
        portals.client_inside = true;
        portals.player.cooldown = 17;
        let mut entity = PortalState::default();
        entity.set_inside(BlockPos::new(2, 70, 2), 300);
        portals.entities.insert(77, entity);
        portals.before.insert(77, Vec3::new(-0.1, 71.0, 33.25));
        let plan = Plan {
            dim: Dim::Nether,
            approximate: BlockPos::new(-3, 81, 5),
            entry_x_axis: false,
            offset: Vec3::new(0.25, 0.5, 0.75),
            width: 0.75,
            height: 1.75,
            end: false,
        };
        if travel {
            portals.travel = Some(plan);
        } else {
            portals.arrivals.push(Arrival {
                plan,
                tag: Tag::compound([
                    ("id", Tag::String("fixture:future_entity".into())),
                    (
                        "opaque",
                        Tag::compound([(
                            "items",
                            Tag::List(vec![Tag::compound([
                                ("id", Tag::String("minecraft:diamond".into())),
                                ("count", Tag::Int(7)),
                            ])]),
                        )]),
                    ),
                ]),
            });
        }
        let before = retained(&game.portals);
        assert_refusal(
            &mut game,
            if travel {
                E::PortalTravel
            } else {
                E::PortalArrivals
            },
        );
        assert_eq!(retained(&game.portals), before);
        assert!(game.portals.loaded && game.portals.dirty);
    }
}
#[test]
#[ignore = "requires pinned HORIZON_TEST_RESOURCES and isolated process cwd/home/temp"]
fn entity_owner_observation_preserves_nonpayload_portal_maps_without_refusal() {
    let fixture = Fixture::new("entity-owner-portal-observations");
    let (mut game, _body, _receivers) = ready(&fixture);
    let mut entity = PortalState::default();
    entity.set_inside(BlockPos::new(2, 70, 2), 300);
    entity.cooldown = 17;
    game.portals.entities.insert(77, entity);
    game.portals.before.insert(77, Vec3::new(-0.1, 71.0, 33.25));
    game.portals.player.set_inside(BlockPos::new(3, 71, 4), 10);
    game.portals.loaded = false;
    game.portals.dirty = true;
    let before = retained(&game.portals);
    let native = snapshot(&mut game);
    for _ in 0..2 {
        players(&game);
        assert!(game.observe_checkpoint_entity_owners().is_ok());
    }
    assert_eq!(retained(&game.portals), before);
    assert_eq!(snapshot(&mut game), native);
}
