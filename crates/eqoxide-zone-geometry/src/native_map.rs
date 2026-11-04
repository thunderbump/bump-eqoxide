//! Bounded, CPU-only EQEmu native `.map` v2 loading.
//!
//! Mirrors `Map::LoadV2` and `RotateVertex` in
//! eqemu:zone/map.cpp@788c86269d9b53ca2b11ca8fa7183826f6c330d6:452-1002.
//! Base and terrain vertices are already server axes. Placed models are transformed
//! in model axes and then swap X/Y, without reversing the source triangle order.

use crate::collision::{Collision, CollisionBuildError, ServerTriangle};
use std::{collections::HashMap, io::Read, path::Path};

#[derive(Debug, Clone)]
pub struct NativeMapLimits {
    pub max_file_bytes: usize,
    pub max_inflated_bytes: usize,
    /// Cumulative stored vertices, including models, ignored geometry and height tiles.
    pub max_vertices: usize,
    /// Cumulative indices, including model definitions and non-colliding indices.
    pub max_indices: usize,
    pub max_models: usize,
    /// Includes grouped placements.
    pub max_placements: usize,
    pub max_tiles: usize,
    pub max_quads_per_tile: usize,
    pub max_name_bytes: usize,
    /// Expanded solid triangles. The collision grid also enforces its own bounds.
    pub max_triangles: usize,
}
impl Default for NativeMapLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: 64 * 1024 * 1024,
            max_inflated_bytes: 256 * 1024 * 1024,
            max_vertices: 4_000_000,
            max_indices: 12_000_000,
            max_models: 16_384,
            max_placements: 100_000,
            max_tiles: 65_536,
            max_quads_per_tile: 256,
            max_name_bytes: 1024,
            max_triangles: 2_000_000,
        }
    }
}

#[derive(Debug)]
pub enum NativeMapError {
    Io(std::io::Error),
    UnsupportedVersion(u32),
    Invalid(&'static str),
    Limit(&'static str),
    Collision(CollisionBuildError),
}
impl std::fmt::Display for NativeMapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "native map I/O: {e}"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported native map version {v:#x}"),
            Self::Invalid(s) => write!(f, "invalid native map: {s}"),
            Self::Limit(s) => write!(f, "native map limit exceeded: {s}"),
            Self::Collision(e) => write!(f, "native map collision: {e}"),
        }
    }
}
impl std::error::Error for NativeMapError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self { Self::Io(e) => Some(e), Self::Collision(e) => Some(e), _ => None }
    }
}
impl From<std::io::Error> for NativeMapError {
    fn from(e: std::io::Error) -> Self { Self::Io(e) }
}

type Result<T> = std::result::Result<T, NativeMapError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeMapStats {
    pub vertices: usize,
    pub indices: usize,
    pub non_colliding_vertices: usize,
    pub non_colliding_indices: usize,
    pub models: usize,
    pub placements: usize,
    pub placement_groups: usize,
    pub grouped_placements: usize,
    pub tiles: usize,
    pub triangles: usize,
}

pub struct NativeMap {
    triangles: Vec<ServerTriangle>,
    stats: NativeMapStats,
}
impl NativeMap {
    /// Reads at most `max_file_bytes + 1`, including when a file grows during reading.
    pub fn load(path: &Path, limits: &NativeMapLimits) -> Result<Self> {
        let file = std::fs::File::open(path)?;
        let cap = limits.max_file_bytes.checked_add(1)
            .ok_or(NativeMapError::Limit("file bytes"))?;
        let mut bytes = Vec::new();
        file.take(cap as u64).read_to_end(&mut bytes)?;
        Self::decode(&bytes, limits)
    }

    pub fn decode(bytes: &[u8], limits: &NativeMapLimits) -> Result<Self> {
        check(bytes.len(), limits.max_file_bytes, "file bytes")?;
        let mut outer = Cursor(bytes);
        let version = outer.u32()?;
        if version != 0x0200_0000 { return Err(NativeMapError::UnsupportedVersion(version)); }
        let compressed = outer.u32()? as usize;
        let inflated = outer.u32()? as usize;
        check(inflated, limits.max_inflated_bytes, "inflated bytes")?;
        if inflated < 40 { return Err(NativeMapError::Invalid("short payload header")); }
        let input = outer.take(compressed)?;
        outer.finish()?;
        // A fixed output buffer bounds decompression before any untrusted counts are read.
        let mut payload = vec![0; inflated];
        let flags = miniz_oxide::inflate::core::inflate_flags::TINFL_FLAG_PARSE_ZLIB_HEADER
            | miniz_oxide::inflate::core::inflate_flags::TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF;
        let (status, consumed, written) = miniz_oxide::inflate::core::decompress(
            &mut miniz_oxide::inflate::core::DecompressorOxide::default(), input,
            &mut payload, 0, flags);
        if status != miniz_oxide::inflate::TINFLStatus::Done || consumed != input.len()
            || written != inflated {
            return Err(NativeMapError::Invalid("zlib stream or inflated length"));
        }
        Self::payload(&payload, limits)
    }

    pub fn stats(&self) -> &NativeMapStats { &self.stats }

    /// Server-axis triangles in source winding, useful for independent geometry audits.
    pub fn triangles(&self) -> &[ServerTriangle] { &self.triangles }

    pub fn into_collision(self, cell_size: f32) -> Result<Collision> {
        Collision::from_server_triangles(self.triangles, cell_size).map_err(NativeMapError::Collision)
    }

    fn payload(bytes: &[u8], l: &NativeMapLimits) -> Result<Self> {
        let mut c = Cursor(bytes);
        let mut stats = NativeMapStats {
            vertices: c.u32()? as usize, indices: c.u32()? as usize,
            non_colliding_vertices: c.u32()? as usize, non_colliding_indices: c.u32()? as usize,
            models: c.u32()? as usize, placements: c.u32()? as usize,
            placement_groups: c.u32()? as usize, tiles: c.u32()? as usize,
            grouped_placements: 0, triangles: 0,
        };
        let quads = c.u32()? as usize;
        let units = c.float()?;
        check(stats.models, l.max_models, "models")?;
        check(stats.placements, l.max_placements, "placements")?;
        check(stats.placement_groups, l.max_placements, "placement groups")?;
        check(stats.tiles, l.max_tiles, "tiles")?;
        check(quads, l.max_quads_per_tile, "quads per tile")?;
        if stats.tiles > 0 && (quads == 0 || units <= 0.0) {
            return Err(NativeMapError::Invalid("terrain dimensions"));
        }
        let mut vertices_used = 0;
        let mut indices_used = 0;
        add(&mut vertices_used, stats.vertices, l.max_vertices, "vertices")?;
        add(&mut vertices_used, stats.non_colliding_vertices, l.max_vertices, "vertices")?;
        add(&mut indices_used, stats.indices, l.max_indices, "indices")?;
        add(&mut indices_used, stats.non_colliding_indices, l.max_indices, "indices")?;
        let vertices = c.vertices(stats.vertices)?;
        let mut triangles = Vec::new();
        for index in c.indices(stats.indices, vertices.len())?.chunks_exact(3) {
            push(&mut triangles, [vertices[index[0]], vertices[index[1]], vertices[index[2]]], l)?;
        }
        // Validate the ignored geometry too; it must never become collision.
        for _ in 0..stats.non_colliding_vertices { c.point()?; }
        c.indices(stats.non_colliding_indices, stats.non_colliding_vertices)?;
        let mut models = HashMap::new();
        for _ in 0..stats.models {
            let name = c.name(l.max_name_bytes)?.to_vec();
            let nv = c.u32()? as usize;
            let np = c.u32()? as usize;
            add(&mut vertices_used, nv, l.max_vertices, "vertices")?;
            add(&mut indices_used, np.checked_mul(3).ok_or(NativeMapError::Limit("indices"))?,
                l.max_indices, "indices")?;
            let vertices = c.vertices(nv)?;
            let mut polys = Vec::new();
            for _ in 0..np {
                let ix = [c.u32()? as usize, c.u32()? as usize, c.u32()? as usize];
                let visible = c.byte()?;
                if ix.iter().any(|&i| i >= nv) { return Err(NativeMapError::Invalid("model index")); }
                if visible != 0 { polys.push(ix.map(|i| vertices[i])); }
            }
            if models.insert(name, polys).is_some() { return Err(NativeMapError::Invalid("duplicate model")); }
        }
        for _ in 0..stats.placements {
            let name = c.name(l.max_name_bytes)?;
            let t = Transform::read(&mut c)?;
            let model = models.get(name).ok_or(NativeMapError::Invalid("unknown placed model"))?;
            for tri in model {
                let transformed = tri.map(|v| swap(add_point(scale(rotate(v, t.rotation), t.scale), t.position)));
                push(&mut triangles, transformed, l)?;
            }
        }
        for _ in 0..stats.placement_groups {
            let group = Transform::read(&mut c)?;
            let tile = c.point()?;
            let count = c.u32()? as usize;
            add(&mut stats.grouped_placements, count, l.max_placements.saturating_sub(stats.placements), "placements")?;
            for _ in 0..count {
                let name = c.name(l.max_name_bytes)?;
                let p = Transform::read(&mut c)?;
                let model = models.get(name).ok_or(NativeMapError::Invalid("unknown grouped model"))?;
                for tri in model { push(&mut triangles, tri.map(|v| group_vertex(v, group, tile, p)), l)?; }
            }
        }
        let quad_count = quads.checked_mul(quads).ok_or(NativeMapError::Limit("terrain quads"))?;
        let side = quads.checked_add(1).ok_or(NativeMapError::Limit("terrain side"))?;
        let height_count = side.checked_mul(side).ok_or(NativeMapError::Limit("terrain heights"))?;
        for _ in 0..stats.tiles {
            let flat = c.byte()?;
            if flat > 1 { return Err(NativeMapError::Invalid("terrain flat flag")); }
            let x = c.float()?;
            let y = c.float()?;
            if flat != 0 {
                let z = c.float()?;
                let size = quads as f32 * units;
                terrain_quad(&mut triangles, [[x,y,z], [x+size,y,z], [x+size,y+size,z], [x,y+size,z]], l)?;
            } else {
                let flags = c.take(quad_count)?;
                // Bound tile expansion as well as the stored model/base vertex totals.
                add(&mut vertices_used, height_count, l.max_vertices, "terrain vertices")?;
                let mut heights = Vec::new();
                for _ in 0..height_count { heights.push(c.float()?); }
                for (q, &flag) in flags.iter().enumerate() {
                    if flag & 1 != 0 { continue; }
                    let row = q / quads;
                    let col = q % quads;
                    let x = x + row as f32 * units;
                    let y = y + col as f32 * units;
                    let h = q + row;
                    terrain_quad(&mut triangles, [[x,y,heights[h]], [x+units,y,heights[h+side]],
                        [x+units,y+units,heights[h+side+1]], [x,y+units,heights[h+1]]], l)?;
                }
            }
        }
        c.finish()?;
        stats.triangles = triangles.len();
        Ok(Self { triangles, stats })
    }
}

fn check(n: usize, limit: usize, what: &'static str) -> Result<()> {
    if n > limit { Err(NativeMapError::Limit(what)) } else { Ok(()) }
}
fn add(n: &mut usize, amount: usize, limit: usize, what: &'static str) -> Result<()> {
    *n = n.checked_add(amount).ok_or(NativeMapError::Limit(what))?;
    check(*n, limit, what)
}
fn push(out: &mut Vec<ServerTriangle>, tri: [[f32; 3]; 3], l: &NativeMapLimits) -> Result<()> {
    if out.len() >= l.max_triangles { return Err(NativeMapError::Limit("expanded triangles")); }
    if tri.iter().flatten().any(|v| !v.is_finite() || v.abs() > 1_000_000.0) {
        return Err(NativeMapError::Invalid("transformed coordinate"));
    }
    out.push(ServerTriangle(tri));
    Ok(())
}
fn terrain_quad(out: &mut Vec<ServerTriangle>, v: [[f32; 3]; 4], l: &NativeMapLimits) -> Result<()> {
    push(out, [v[3], v[1], v[2]], l)?;
    push(out, [v[3], v[0], v[1]], l)
}

struct Cursor<'a>(&'a [u8]);
impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.0.len() { return Err(NativeMapError::Invalid("truncated payload")); }
        let (a, b) = self.0.split_at(n); self.0 = b; Ok(a)
    }
    fn byte(&mut self) -> Result<u8> { Ok(self.take(1)?[0]) }
    fn u32(&mut self) -> Result<u32> { Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap())) }
    fn float(&mut self) -> Result<f32> {
        let f = f32::from_bits(self.u32()?);
        if !f.is_finite() { return Err(NativeMapError::Invalid("non-finite float")); }
        Ok(f)
    }
    fn point(&mut self) -> Result<[f32; 3]> { Ok([self.float()?, self.float()?, self.float()?]) }
    fn name(&mut self, limit: usize) -> Result<&'a [u8]> {
        let len = self.0.iter().take(limit.saturating_add(1)).position(|&b| b == 0)
            .ok_or(NativeMapError::Invalid("unterminated or oversized model name"))?;
        if len == 0 { return Err(NativeMapError::Invalid("empty model name")); }
        let bytes = self.take(len + 1)?; Ok(&bytes[..len])
    }
    fn vertices(&mut self, n: usize) -> Result<Vec<[f32; 3]>> {
        if n > self.0.len() / 12 { return Err(NativeMapError::Invalid("truncated vertices")); }
        (0..n).map(|_| self.point()).collect()
    }
    fn indices(&mut self, n: usize, vertices: usize) -> Result<Vec<usize>> {
        if n % 3 != 0 { return Err(NativeMapError::Invalid("index count is not triangles")); }
        if n > self.0.len() / 4 { return Err(NativeMapError::Invalid("truncated indices")); }
        (0..n).map(|_| {
            let i = self.u32()? as usize;
            if i >= vertices { Err(NativeMapError::Invalid("vertex index")) } else { Ok(i) }
        }).collect()
    }
    fn finish(&self) -> Result<()> {
        if self.0.is_empty() { Ok(()) } else { Err(NativeMapError::Invalid("trailing bytes")) }
    }
}
#[derive(Clone, Copy)]
struct Transform { position: [f32; 3], rotation: [f32; 3], scale: [f32; 3] }
impl Transform {
    fn read(c: &mut Cursor<'_>) -> Result<Self> {
        Ok(Self { position: c.point()?, rotation: c.point()?, scale: c.point()? })
    }
}
fn swap(v: [f32; 3]) -> [f32; 3] { [v[1], v[0], v[2]] }
fn scale(v: [f32; 3], s: [f32; 3]) -> [f32; 3] { std::array::from_fn(|i| v[i] * s[i]) }
fn add_point(v: [f32; 3], t: [f32; 3]) -> [f32; 3] { std::array::from_fn(|i| v[i] + t[i]) }
fn rotate(mut v: [f32; 3], r: [f32; 3]) -> [f32; 3] {
    let (s, c) = r[0].sin_cos();
    v = [v[0], c*v[1] - s*v[2], s*v[1] + c*v[2]];
    let (s, c) = r[1].sin_cos();
    v = [c*v[0] + s*v[2], v[1], -s*v[0] + c*v[2]];
    let (s, c) = r[2].sin_cos();
    [c*v[0] - s*v[1], s*v[0] + c*v[1], v[2]]
}
fn group_vertex(v: [f32; 3], g: Transform, tile: [f32; 3], p: Transform) -> [f32; 3] {
    // Preserve the server's degree constant, separate rotations and correction sequence.
    // eqemu:zone/map.cpp@788c86269d9b53ca2b11ca8fa7183826f6c330d6:729-789.
    #[allow(clippy::approx_constant)]
    let degrees = |a: f32| a * 3.14159f32 / 180.0;
    let mut v = add_point(scale(v, p.scale), p.position);
    v = rotate(v, [degrees(g.rotation[0]), 0.0, 0.0]);
    v = rotate(v, [0.0, degrees(g.rotation[1]), 0.0]);
    let correction = rotate(p.position, [degrees(g.rotation[0]), 0.0, 0.0]);
    v = add_point(v, correction.map(|x| -x));
    v = rotate(v, [degrees(p.rotation[0]), 0.0, 0.0]);
    v = rotate(v, [0.0, -degrees(p.rotation[1]), 0.0]);
    v = rotate(v, [0.0, 0.0, degrees(p.rotation[2])]);
    v = add_point(v, correction);
    v = rotate(v, [0.0, 0.0, degrees(g.rotation[2])]);
    swap(add_point(add_point(scale(v, g.scale), tile), g.position))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u32s(out: &mut Vec<u8>, values: &[u32]) {
        for v in values { out.extend(v.to_le_bytes()); }
    }
    fn floats(out: &mut Vec<u8>, values: &[f32]) {
        for v in values { out.extend(v.to_le_bytes()); }
    }
    fn wrap(payload: &[u8]) -> Vec<u8> {
        let compressed = miniz_oxide::deflate::compress_to_vec_zlib(payload, 6);
        let mut out = Vec::new();
        u32s(&mut out, &[0x0200_0000, compressed.len() as u32, payload.len() as u32]);
        out.extend(compressed); out
    }
    fn header(vertices: u32, indices: u32, models: u32, placements: u32,
        groups: u32, tiles: u32, quads: u32) -> Vec<u8> {
        let mut out = Vec::new();
        u32s(&mut out, &[vertices, indices, 0, 0, models, placements, groups, tiles, quads]);
        floats(&mut out, &[if tiles > 0 { 10.0 } else { 0.0 }]); out
    }
    fn scene() -> Vec<u8> {
        let mut p = header(8, 12, 0, 0, 0, 0, 0);
        for v in [[10.,100.,2.], [30.,100.,2.], [30.,120.,2.], [10.,120.,2.],
            [20.,100.,2.], [20.,120.,2.], [20.,120.,20.], [20.,100.,20.]] {
            floats(&mut p, &v);
        }
        u32s(&mut p, &[0,1,2, 0,2,3, 4,5,6, 4,6,7]); p
    }
    fn model(p: &mut Vec<u8>) {
        p.extend(b"model\0"); u32s(p, &[3, 2]);
        for v in [[0.,0.,0.], [2.,0.,0.], [0.,3.,0.]] { floats(p, &v); }
        u32s(p, &[0,1,2]); p.push(1);
        u32s(p, &[0,2,1]); p.push(0); // Invisible polygon must not collide.
    }
    fn transform(p: &mut Vec<u8>, position: [f32;3], rotation: [f32;3], scale: [f32;3]) {
        floats(p, &position); floats(p, &rotation); floats(p, &scale);
    }
    fn decode(p: &[u8]) -> NativeMap { NativeMap::decode(&wrap(p), &NativeMapLimits::default()).unwrap() }
    fn near(a: [f32;3], b: [f32;3]) {
        for i in 0..3 { assert!((a[i]-b[i]).abs() < 0.0002, "{a:?} != {b:?}"); }
    }

    #[test]
    fn server_axes_floor_wall_and_los_are_usable_without_assets() {
        let map = decode(&scene());
        assert_eq!(map.stats().triangles, 4);
        let col = map.into_collision(8.0).unwrap();
        assert_eq!(col.nearest_floor(15.,110.,5.,1.,10.), Some(2.));
        assert_eq!(col.nearest_floor(110.,15.,5.,1.,10.), None);
        assert!(col.segment_blocked([15.,110.,8.], [25.,110.,8.]));
        assert!(!col.segment_blocked([15.,110.,25.], [25.,110.,25.]));
        assert!(!col.path_clear([15.,110.,2.], [25.,110.,2.], 1.));
    }

    #[test]
    fn direct_placement_rotates_then_scales_then_translates_and_swaps_xy() {
        let mut p = header(0, 0, 1, 1, 0, 0, 0); model(&mut p);
        p.extend(b"model\0");
        transform(&mut p, [40.,50.,6.], [0.,0.,std::f32::consts::FRAC_PI_2], [2.,3.,4.]);
        let map = decode(&p);
        assert_eq!(map.stats().triangles, 1);
        let t = map.triangles()[0].0;
        near(t[0], [50.,40.,6.]); near(t[1], [56.,40.,6.]); near(t[2], [50.,34.,6.]);
        let col = map.into_collision(8.).unwrap();
        assert!((col.floor_z(51.,39.,10.) - 6.).abs() < 0.001);
    }

    #[test]
    fn rotation_uses_all_three_axes_in_server_order() {
        let quarter = std::f32::consts::FRAC_PI_2;
        near(rotate([1.,2.,3.], [quarter,0.,0.]), [1.,-3.,2.]);
        near(rotate([1.,2.,3.], [0.,quarter,0.]), [3.,2.,-1.]);
        near(rotate([1.,2.,3.], [0.,0.,quarter]), [-2.,1.,3.]);
        near(rotate([1.,2.,3.], [quarter,quarter,quarter]), [3.,2.,-1.]);
    }

    #[test]
    fn negative_scale_preserves_transformed_winding() {
        let mut p = header(0, 0, 1, 2, 0, 0, 0); model(&mut p);
        for s in [1., -1.] {
            p.extend(b"model\0"); transform(&mut p, [0.;3], [0.;3], [s,1.,1.]);
        }
        let map = decode(&p);
        assert_eq!(map.triangles()[0].0, [[0.,0.,0.],[0.,2.,0.],[3.,0.,0.]]);
        assert_eq!(map.triangles()[1].0, [[0.,0.,0.],[0.,-2.,0.],[3.,0.,0.]]);
    }

    #[test]
    fn group_transform_keeps_tile_translation_and_local_rotation_units() {
        let mut p = header(0, 0, 1, 0, 1, 0, 0); model(&mut p);
        transform(&mut p, [100.,200.,300.], [90.,0.,90.], [2.,3.,4.]);
        floats(&mut p, &[10.,20.,30.]); u32s(&mut p, &[1]);
        p.extend(b"model\0"); transform(&mut p, [3.,4.,5.], [0.,90.,0.], [2.,3.,4.]);
        let map = decode(&p);
        assert_eq!(map.stats().grouped_placements, 1);
        // At exact quarter turns the correction is [3,-5,4], and local -Y rotation
        // maps [4,0,0] to [0,0,4] and [0,0,9] to [-9,0,0].
        for (actual, expected) in map.triangles()[0].0.into_iter().zip(
            [[229.,120.,346.], [229.,120.,362.], [202.,120.,346.]]) { near(actual, expected); }
    }

    #[test]
    fn mixed_group_rotations_match_the_pinned_cpp_transform() {
        // Golden result from the pinned server's float operation order, including
        // its asymmetric Y correction and negative local Y rotation.
        let g = Transform { position: [100.,200.,300.], rotation: [13.,-17.,31.], scale: [1.5,-2.,3.] };
        let p = Transform { position: [-3.,4.,2.], rotation: [43.,29.,-71.], scale: [0.5,1.25,-1.] };
        near(group_vertex([1.25,-2.5,3.75], g, [-4.,8.,12.], p), [206.96085,95.2483,308.74103]);
    }

    #[test]
    fn terrain_flat_and_height_tiles_match_native_winding_and_holes() {
        let mut p = header(0,0,0,0,0,1,1);
        p.push(1); floats(&mut p, &[100.,200.,7.]);
        let map = decode(&p);
        assert_eq!(map.triangles()[0].0, [[100.,210.,7.],[110.,200.,7.],[110.,210.,7.]]);
        let col = map.into_collision(8.).unwrap();
        assert_eq!(col.floor_z(102.,203.,10.),7.);
        let mut p = header(0,0,0,0,0,1,1);
        p.push(0); floats(&mut p, &[100.,200.]); p.push(0);
        floats(&mut p, &[1.,2.,3.,4.]);
        let map = decode(&p);
        assert_eq!(map.triangles()[0].0, [[100.,210.,2.],[110.,200.,3.],[110.,210.,4.]]);
        p[49] = 1; // Hole flag after header, flat flag and XY.
        assert!(decode(&p).triangles().is_empty());
    }

    #[test]
    fn non_colliding_geometry_never_enters_the_grid() {
        let mut p = header(0,0,0,0,0,0,0);
        p[8..12].copy_from_slice(&3u32.to_le_bytes());
        p[12..16].copy_from_slice(&3u32.to_le_bytes());
        for v in [[0.,0.,0.],[1.,0.,0.],[0.,1.,0.]] { floats(&mut p,&v); }
        u32s(&mut p, &[0,1,2]);
        assert!(!decode(&p).into_collision(32.).unwrap().has_triangles());
    }

    #[test]
    fn rejects_truncation_invalid_indices_names_and_transforms() {
        let p = scene();
        let bytes = wrap(&p);
        for end in 0..bytes.len() { assert!(NativeMap::decode(&bytes[..end], &NativeMapLimits::default()).is_err()); }
        for end in 0..p.len() { assert!(NativeMap::payload(&p[..end], &NativeMapLimits::default()).is_err()); }
        let mut bad = p.clone(); bad[40+8*12..40+8*12+4].copy_from_slice(&8u32.to_le_bytes());
        assert!(NativeMap::decode(&wrap(&bad), &NativeMapLimits::default()).is_err());
        let mut bad = header(0,0,1,0,0,0,0); bad.extend(b"no terminator");
        assert!(NativeMap::decode(&wrap(&bad), &NativeMapLimits::default()).is_err());
        for bad_float in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, f32::MAX] {
            let mut bad = header(0,0,1,1,0,0,0); model(&mut bad); bad.extend(b"model\0");
            transform(&mut bad,[bad_float,0.,0.],[0.;3],[1.;3]);
            assert!(NativeMap::decode(&wrap(&bad), &NativeMapLimits::default()).is_err());
        }
        for field in 0..9 {
            let mut bad = header(0,0,1,1,0,0,0); model(&mut bad); bad.extend(b"model\0");
            let offset = bad.len();
            transform(&mut bad,[0.;3],[0.;3],[1.;3]);
            bad[offset+field*4..offset+field*4+4].copy_from_slice(&f32::NAN.to_le_bytes());
            assert!(NativeMap::decode(&wrap(&bad), &NativeMapLimits::default()).is_err());
        }
        let mut bad = header(0,0,1,1,0,0,0); model(&mut bad); bad.extend(b"other\0");
        transform(&mut bad,[0.;3],[0.;3],[1.;3]);
        assert!(NativeMap::decode(&wrap(&bad), &NativeMapLimits::default()).is_err());
    }

    #[test]
    fn rejects_bad_compression_lengths_checksums_and_trailing_data() {
        let bytes = wrap(&scene());
        let mut bad = bytes.clone(); bad[0] = 1;
        assert!(NativeMap::decode(&bad, &NativeMapLimits::default()).is_err());
        let mut bad = bytes.clone(); *bad.last_mut().unwrap() ^= 1;
        assert!(NativeMap::decode(&bad, &NativeMapLimits::default()).is_err());
        let mut bad = bytes.clone(); bad[8..12].copy_from_slice(&40u32.to_le_bytes());
        assert!(NativeMap::decode(&bad, &NativeMapLimits::default()).is_err());
        let mut bad = bytes.clone(); bad.push(0);
        assert!(NativeMap::decode(&bad, &NativeMapLimits::default()).is_err());
        let n = bad.len() as u32 - 12; bad[4..8].copy_from_slice(&n.to_le_bytes());
        assert!(NativeMap::decode(&bad, &NativeMapLimits::default()).is_err());
        let mut p = scene(); p.push(0);
        assert!(NativeMap::decode(&wrap(&p), &NativeMapLimits::default()).is_err());
    }

    #[test]
    fn bounds_decode_expansion_and_grid_allocations() {
        let bytes = wrap(&scene());
        for limits in [
            NativeMapLimits { max_file_bytes: bytes.len()-1, ..Default::default() },
            NativeMapLimits { max_inflated_bytes: 40, ..Default::default() },
            NativeMapLimits { max_vertices: 7, ..Default::default() },
            NativeMapLimits { max_indices: 11, ..Default::default() },
            NativeMapLimits { max_triangles: 3, ..Default::default() },
        ] { assert!(matches!(NativeMap::decode(&bytes,&limits), Err(NativeMapError::Limit(_)))); }
        assert!(decode(&scene()).into_collision(f32::NAN).is_err());
        assert!(decode(&scene()).into_collision(0.).is_err());
        let huge = ServerTriangle([[0.,0.,0.], [100_000.,0.,0.], [0.,100_000.,0.]]);
        assert!(Collision::from_server_triangles(vec![huge], 1.).is_err());
        let spanning = ServerTriangle([[0.,0.,0.], [999.,0.,0.], [0.,999.,0.]]);
        assert!(Collision::from_server_triangles(vec![spanning;17], 1.).is_err());
        let nan = ServerTriangle([[f32::NAN;3];3]);
        assert!(Collision::from_server_triangles(vec![nan],32.).is_err());
    }
}

#[cfg(test)]
mod water_tests {
    use super::*;
    use eqoxide_core::region_map::RegionMap;
    use std::sync::Arc;

    struct Directory(std::path::PathBuf);
    impl Drop for Directory {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }

    #[test]
    fn attaches_native_water_v1_and_v2_and_preserves_zero_exit_indices() {
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = Directory(std::env::temp_dir().join(format!("eqoxide-native-map-{}-{nonce}", std::process::id())));
        std::fs::create_dir(&dir.0).unwrap();
        for version in [1u32,2] {
            let mut data = b"EQEMUWATER".to_vec();
            data.extend(version.to_le_bytes()); data.extend(3u32.to_le_bytes());
            for (number, normal, split, special, left, right) in [
                (1i32,[1f32,0.,0.],-110f32,0i32,2i32,3i32),
                (2,[0.;3],0.,1,0,0),
                (3,[0.;3],0.,3,0,0),
            ] {
                data.extend(number.to_le_bytes());
                for v in normal { data.extend(v.to_le_bytes()); }
                data.extend(split.to_le_bytes()); data.extend(0i32.to_le_bytes());
                for i in [special,left,right] { data.extend(i.to_le_bytes()); }
                if version == 2 { data.extend(0i32.to_le_bytes()); }
            }
            std::fs::write(dir.0.join("zone.wtr"), data).unwrap();
            let water = RegionMap::try_load(&dir.0,"zone").unwrap();
            assert!(water.is_water(15.,115.,3.));
            assert!(!water.is_water(115.,15.,3.), "BSP uses server Y/X ordering");
            assert_eq!(water.zone_line_at(15.,105.,3.),Some(0));
            assert_eq!(water.zone_line_indices(),vec![0]);
            let mut col = Collision::from_server_triangles(vec![ServerTriangle(
                [[10.,100.,2.],[30.,100.,2.],[10.,120.,2.]])],8.).unwrap();
            col.set_region_data(Ok(Arc::new(water)));
            assert!(col.in_water([15.,115.,3.]));
            assert_eq!(col.zone_line_indices().unwrap(),vec![0]);
            assert_eq!(col.nearest_floor(15.,105.,3.,1.,10.),Some(2.));
        }
    }
}
