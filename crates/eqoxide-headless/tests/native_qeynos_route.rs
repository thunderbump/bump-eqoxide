//! Optional local native-map qualification; never connects a game client.
use eqoxide_core::region_map::RegionMap;
use eqoxide_physics::movement::{manual_wish, CharacterController, MoveIntent, DEFAULT_MOVE_SPEED};
use eqoxide_zone_geometry::native_map::{NativeMap, NativeMapLimits};
use std::{path::PathBuf, sync::Arc};

#[test]
#[ignore = "requires operator-supplied Qeynos2 native map/water via EQOXIDE_TEST_MAPS"]
fn ordinary_spawn_room_has_a_grounded_dry_exit_and_a_solid_east_wall() {
    let dir = PathBuf::from(std::env::var_os("EQOXIDE_TEST_MAPS").expect("set EQOXIDE_TEST_MAPS"));
    let map = NativeMap::load(&dir.join("qeynos2.map"), &NativeMapLimits::default()).unwrap();
    assert_eq!(
        (
            map.stats().models,
            map.stats().placements,
            map.stats().triangles
        ),
        (48, 480, 24285)
    );
    let mut col = map.into_collision(32.0).unwrap();
    let water=RegionMap::try_load(&dir,"qeynos2").unwrap();
    assert_eq!(water.node_count(),6563);
    col.set_region_data(Ok(Arc::new(water)));
    let mut wall = CharacterController::new([297.75, 344.0, 0.0]);
    for _ in 0..100 {
        wall.step(
            MoveIntent {
                wish_dir: [1.0, 0.0],
                speed: DEFAULT_MOVE_SPEED,
                ..Default::default()
            },
            0.02,
            &col,
        );
    }
    assert!((wall.pos[0] - 304.825).abs() < 0.02);
    assert_eq!(wall.pos[1], 344.0);
    for start in [[288.0, 344.0, 0.0], [304.825, 344.0, 0.0]] {
        let mut body = CharacterController::new(start);
        for target in [
            [296.0, 329.0],
            [270.0, 329.0],
            [270.0, 366.0],
            [340.0, 366.0],
        ] {
            let mut reached = false;
            for _ in 0..150 {
                let delta = [target[0] - body.pos[0], target[1] - body.pos[1]];
                let distance = (delta[0] * delta[0] + delta[1] * delta[1]).sqrt();
                if distance < 0.05 {
                    reached = true;
                    break;
                }
                let (wish, _) = manual_wish(delta);
                body.step(
                    MoveIntent {
                        wish_dir: wish,
                        speed: DEFAULT_MOVE_SPEED.min(distance / 0.02),
                        ..Default::default()
                    },
                    0.02,
                    &col,
                );
                assert!(body.on_ground);
                assert!(!body.in_water);
                assert!(body.pos[2].abs() < 0.02);
                assert!(!col.body_placement(body.pos).is_embedded());
            }
            assert!(
                reached,
                "failed target {target:?} from {start:?}, body {:?}",
                body.pos
            );
        }
    }
}
