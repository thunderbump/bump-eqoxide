//! Local dry-route probes; never opens a network connection.
use eqoxide_core::region_map::RegionMap;
use eqoxide_physics::movement::{CharacterController, MoveIntent, DEFAULT_MOVE_SPEED};
use eqoxide_zone_geometry::native_map::{NativeMap, NativeMapLimits};
use std::{path::Path, sync::Arc};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 4 {
        return Err("usage: route_probe MAP EAST NORTH FOOT_Z".into());
    }
    let path = Path::new(&args[0]);
    let p = [args[1].parse::<f32>()?, args[2].parse()?, args[3].parse()?];
    if p.iter().any(|v| !v.is_finite()) {
        return Err("nonfinite coordinates".into());
    }
    let mut col = NativeMap::load(path, &NativeMapLimits::default())?.into_collision(32.0)?;
    col.set_region_data(
        RegionMap::try_load(
            path.parent().ok_or("map directory")?,
            path.file_stem()
                .and_then(|s| s.to_str())
                .ok_or("zone name")?,
        )
        .map(Arc::new),
    );
    for (name, dir) in [
        ("east", [1.0, 0.0]),
        ("west", [-1.0, 0.0]),
        ("north", [0.0, 1.0]),
        ("south", [0.0, -1.0]),
    ] {
        let mut body = CharacterController::new(p);
        let mut samples = Vec::new();
        for i in 0..300 {
            body.step(
                MoveIntent {
                    wish_dir: dir,
                    speed: DEFAULT_MOVE_SPEED,
                    ..Default::default()
                },
                0.02,
                &col,
            );
            if i % 25 == 24 {
                samples.push((i + 1, body.pos, body.on_ground, body.in_water));
            }
        }
        println!("direction={name} start={p:?} halfsecond_samples={samples:?}");
    }
    Ok(())
}
