//! CPU-only native geometry qualification. No client, renderer or server connection.
//! Usage: native_map_probe MAP [SERVER_X SERVER_Y REFERENCE_FOOT_Z]
use eqoxide_zone_geometry::native_map::{NativeMap, NativeMapLimits};
use eqoxide_core::region_map::RegionMap;
use std::{path::Path, sync::Arc};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 1 && args.len() != 4 { return Err("usage: native_map_probe MAP [X Y FOOT_Z]".into()); }
    let path = Path::new(&args[0]);
    let map = NativeMap::load(path, &NativeMapLimits::default())?;
    println!("native_map_stats={:?}", map.stats());
    let z_min = map.triangles().iter().flat_map(|t| t.0).map(|v| v[2]).fold(f32::MAX, f32::min);
    let mut col = map.into_collision(32.0)?;
    let zone = path.file_stem().and_then(|s| s.to_str()).ok_or("invalid zone filename")?;
    col.set_region_data(RegionMap::try_load(path.parent().unwrap_or(Path::new(".")), zone).map(Arc::new));
    println!("grid_origin={:?} grid_cols={} grid_rows={} grid_z_max={}", col.origin, col.cols, col.rows, col.z_max);
    match col.region_map() {
        Some(water) => println!("water_nodes={} zone_line_indices={:?}", water.node_count(), water.zone_line_indices()),
        None => println!("water_absent={}", col.region_data_absent().unwrap().as_str()),
    }
    if args.len() == 4 {
        let p: [f32;3] = [args[1].parse()?, args[2].parse()?, args[3].parse()?];
        if p.iter().any(|v| !v.is_finite()) { return Err("coordinates must be finite".into()); }
        let floor = col.nearest_floor(p[0], p[1], p[2], 10., 100.);
        println!("server_point={p:?} nearest_floor={floor:?} water={}", col.in_water(p));
        println!("vertical_los_hit={:?}", col.nearest_hit_t([p[0],p[1],p[2]+10.], [p[0],p[1],p[2]-100.]));
        if let Some(water) = col.region_map() {
            let east_max = col.origin[0] + col.cols as f32 * col.cell_size;
            let north_max = col.origin[1] + col.rows as f32 * col.cell_size;
            // Scan the bounded zone mesh extent, rather than declaring absent water from one dry point.
            let bounds = (col.origin[0], east_max, col.origin[1], north_max, z_min, col.z_max);
            let boxes = water.water_region_aabbs(bounds);
            let mut extent = [[f32::MAX, f32::MIN]; 3];
            for (east, north, z) in &boxes {
                for (axis, range) in [east, north, z].into_iter().enumerate() {
                    extent[axis][0] = extent[axis][0].min(range[0]);
                    extent[axis][1] = extent[axis][1].max(range[1]);
                }
            }
            println!("water_region_boxes={} extent={extent:?}", boxes.len());
            println!("point_water_surface={:?} point_water_bottom={:?}",
                water.surface_z(p[0],p[1],p[2]), water.bottom_z(p[0],p[1],p[2]));
            let grid = col.build_water_grid(&eqoxide_zone_geometry::body::PLAYER_BODY);
            println!("water_grid_columns={} spans={} nodes={} unbounded_below={}",
                grid.wet_column_count(), grid.span_count(), grid.node_count(), grid.unbounded_below_count());
        }
    }
    Ok(())
}
