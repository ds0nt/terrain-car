use bevy::math::DVec3;
use bevy::prelude::*;
use noise::{NoiseFn, Perlin};

// Chunked, streamed terrain: chunks spawn/despawn around whatever entity
// carries `TerrainTracker` (the car) on the client, and around whatever the
// server is simulating on the server. Height is a pure function of *true*
// (world-origin-relative) (x, z) at f64 precision — chunk edges always
// agree exactly because neighboring chunks sample the same function at the
// same true coordinates, no stitching needed, and content doesn't change
// when `WorldOrigin` rebases (see worldspace.rs). This is also what lets
// client and server (and every connected client) independently regenerate
// byte-identical terrain from just a shared seed, without ever sending mesh
// or collider data over the network.
//
// Chunk coordinates are i64, not i32: at CHUNK_SIZE=160, i64 spans roughly
// 1.5e21 meters (~155,000 light-years) before overflowing — a full galaxy's
// diameter, with room to spare. i32 would have overflowed around 2.3 AU.
pub const CHUNK_SIZE: f32 = 160.0;
pub const CHUNK_RESOLUTION: usize = 49; // vertices per side -> 48 quads
const HEIGHT_SCALE: f32 = 20.0;
const MOUNTAIN_HEIGHT: f32 = 380.0;
const CANYON_DEPTH: f32 = 100.0;
// Continental-scale elevation: whole highland/lowland *regions*, hundreds
// of km across, that everything else (mountains, canyons, hills) sits on
// top of as detail. Without this, every noise layer topped out around a
// 1-2.5km wavelength, so elevation just bounced around the same band no
// matter how far you drove — there was nothing operating at the scale that
// actually makes one part of the world different from another.
const CONTINENTAL_SCALE: f32 = 1400.0;
const CONTINENTAL_WAVELENGTH: f64 = 180_000.0;
// The missing middle ground between "continent" and "one mountain range":
// broad plateau/basin rolling at a few-km scale.
const REGIONAL_SCALE: f32 = 220.0;
const REGIONAL_WAVELENGTH: f64 = 14_000.0;

pub type ChunkCoord = (i64, i64);

pub fn world_to_chunk(true_pos: DVec3) -> ChunkCoord {
    (
        (true_pos.x / CHUNK_SIZE as f64).round() as i64,
        (true_pos.z / CHUNK_SIZE as f64).round() as i64,
    )
}

#[derive(Resource)]
pub struct TerrainNoise {
    base: Perlin,
    warp: Perlin,
    ridge: Perlin,
    mask: Perlin,
    detail: Perlin,
    temperature: Perlin,
    moisture: Perlin,
    canyon: Perlin,
    canyon_region: Perlin,
    continental: Perlin,
    regional: Perlin,
}

impl Default for TerrainNoise {
    fn default() -> Self {
        Self {
            base: Perlin::new(1990),
            warp: Perlin::new(77),
            ridge: Perlin::new(303),
            mask: Perlin::new(555),
            detail: Perlin::new(4021),
            temperature: Perlin::new(88),
            moisture: Perlin::new(212),
            canyon: Perlin::new(917),
            canyon_region: Perlin::new(4477),
            continental: Perlin::new(3141),
            regional: Perlin::new(2718),
        }
    }
}

/// A fresh, time-derived seed — used both to reseed `TerrainNoise` and to
/// pick a new true-space spawn point on regenerate, so "regenerate" lands
/// somewhere new entirely rather than reusing true (0, 0), which also
/// happens to sit exactly on a Perlin lattice point (gradient noise is ~0
/// there for any seed, so every regenerate was landing in a suspiciously
/// similar "average" climate).
pub fn random_seed() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(1)
        .max(1)
}

impl TerrainNoise {
    pub fn from_seed(base_seed: u32) -> Self {
        let seed = |salt: u32| base_seed.wrapping_mul(2_654_435_761).wrapping_add(salt);
        Self {
            base: Perlin::new(seed(1)),
            warp: Perlin::new(seed(2)),
            ridge: Perlin::new(seed(3)),
            mask: Perlin::new(seed(4)),
            detail: Perlin::new(seed(5)),
            temperature: Perlin::new(seed(6)),
            moisture: Perlin::new(seed(7)),
            canyon: Perlin::new(seed(8)),
            canyon_region: Perlin::new(seed(9)),
            continental: Perlin::new(seed(10)),
            regional: Perlin::new(seed(11)),
        }
    }
}

fn fbm(noise: &Perlin, x: f64, z: f64, octaves: u32, base_freq: f64) -> f32 {
    let mut amplitude = 1.0;
    let mut frequency = base_freq;
    let mut sum = 0.0;
    let mut max_amplitude = 0.0;
    for _ in 0..octaves {
        sum += noise.get([x * frequency, z * frequency]) * amplitude;
        max_amplitude += amplitude;
        amplitude *= 0.5;
        frequency *= 2.0;
    }
    (sum / max_amplitude) as f32
}

/// Ridged fBm: `1 - |noise|`, sharpened, so ridgelines read as narrow
/// mountain crests instead of smooth rolling bumps.
/// `sharpness` controls how narrow/crested the ridgelines read: near 1.0 is
/// soft and rolling, higher values (2.0+) get spiky fast, especially
/// compounded across several octaves — that compounding is what was making
/// mountains read as jagged noise rather than peaks.
fn fbm_ridged(noise: &Perlin, x: f64, z: f64, octaves: u32, base_freq: f64, sharpness: f64) -> f32 {
    let mut amplitude = 1.0;
    let mut frequency = base_freq;
    let mut sum = 0.0;
    let mut max_amplitude = 0.0;
    for _ in 0..octaves {
        let n = noise.get([x * frequency, z * frequency]);
        let ridge = (1.0 - n.abs()).powf(sharpness);
        sum += ridge * amplitude;
        max_amplitude += amplitude;
        amplitude *= 0.5;
        frequency *= 2.0;
    }
    (sum / max_amplitude) as f32
}

fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Large-scale (temperature, moisture) climate sample in [0, 1] each, used
/// both to pick canyon regions (deserts) and to blend biome ground colors.
/// Kilometers-wide frequency so climate reads as actual "regions" you drive
/// through, not per-hill noise.
pub fn climate_at(n: &TerrainNoise, x: f64, z: f64) -> (f32, f32) {
    let t = (fbm(&n.temperature, x, z, 2, 1.0 / 2500.0) * 0.5 + 0.5).clamp(0.0, 1.0);
    let m = (fbm(&n.moisture, x, z, 2, 1.0 / 2200.0) * 0.5 + 0.5).clamp(0.0, 1.0);
    (t, m)
}

/// A human-readable label for the HUD's world-type readout — nearest of the
/// same four climate corners `terrain_color` blends between, plus a rough
/// elevation qualifier. Not exact at the blend boundaries (nothing this
/// simple would be), but it's a readout, not a simulation.
///
/// The elevation qualifier looks at *local relief* (height minus the broad
/// continental/regional trend), not absolute height — otherwise a whole
/// lowland basin sitting at, say, -800m of continental elevation would
/// read as "CANYON" even on perfectly flat ground, and a highland plateau
/// would read as "MOUNTAINS" with no mountain in sight.
pub fn biome_label(n: &TerrainNoise, x: f64, z: f64) -> &'static str {
    let (temperature, moisture) = climate_at(n, x, z);
    let base = match (temperature >= 0.5, moisture >= 0.5) {
        (false, false) => "TUNDRA",
        (false, true) => "TAIGA",
        (true, false) => "DESERT",
        (true, true) => "GRASSLAND",
    };

    let baseline = fbm(&n.continental, x, z, 3, 1.0 / CONTINENTAL_WAVELENGTH) * CONTINENTAL_SCALE
        + fbm(&n.regional, x, z, 3, 1.0 / REGIONAL_WAVELENGTH) * REGIONAL_SCALE;
    let local_relief = height_at(n, x, z) - baseline;

    if local_relief > MOUNTAIN_HEIGHT * 0.2 {
        match base {
            "DESERT" => "MOUNTAINS (ARID)",
            "TUNDRA" | "TAIGA" => "MOUNTAINS (ALPINE)",
            _ => "MOUNTAINS",
        }
    } else if local_relief < -CANYON_DEPTH * 0.25 {
        "CANYON"
    } else {
        base
    }
}

/// True-space terrain height (see module docs for why `x`/`z` are f64).
/// Layered at four very different wavelengths, largest first, each sitting
/// as "detail" on top of the one before it:
///
/// 1. Continental (~180km): which whole regions are highland vs. lowland.
/// 2. Regional (~14km): plateau/basin rolling within a region.
/// 3. Mountain ranges (~1.4km mask, ~2.4km ridge wavelength): where ranges
///    sit, and their shape — double domain-warped (warp the coordinates,
///    then warp *that* warp again at a smaller scale) so ranges branch and
///    curve organically instead of reading as one noise function stretched
///    big. Ridge sharpening is intentionally mild — a high exponent plus
///    many octaves is what was making peaks read as spiky noise instead of
///    mountains.
/// 4. Fine detail (~55-90m): the close-range texture, kept low-amplitude so
///    it stays texture and doesn't fight the silhouette from #3.
///
/// Canyon country (inverted ridges) carves through the hot/dry (desert)
/// climate band, since real canyonlands are almost always arid.
pub fn height_at(n: &TerrainNoise, x: f64, z: f64) -> f32 {
    let continental = fbm(&n.continental, x, z, 3, 1.0 / CONTINENTAL_WAVELENGTH) * CONTINENTAL_SCALE;
    let regional = fbm(&n.regional, x, z, 3, 1.0 / REGIONAL_WAVELENGTH) * REGIONAL_SCALE;

    // Pass 1: broad warp.
    let warp1_freq = 1.0 / 500.0;
    let warp1_amplitude = 70.0;
    let w1x = x + n.warp.get([x * warp1_freq, z * warp1_freq]) * warp1_amplitude;
    let w1z = z + n.warp.get([x * warp1_freq + 91.7, z * warp1_freq + 91.7]) * warp1_amplitude;

    // Pass 2: warp the warp, at a tighter scale — this is what gives the
    // ridgelines their organic, non-repeating branching look instead of
    // one smooth wiggle.
    let warp2_freq = 1.0 / 130.0;
    let warp2_amplitude = 20.0;
    let wx = w1x + n.warp.get([w1x * warp2_freq + 13.3, w1z * warp2_freq + 13.3]) * warp2_amplitude;
    let wz = w1z + n.warp.get([w1x * warp2_freq + 47.1, w1z * warp2_freq + 47.1]) * warp2_amplitude;

    let plains = fbm(&n.base, wx, wz, 6, 1.0 / 180.0) * HEIGHT_SCALE;

    // Highlands (high continental elevation) get more mountain ranges than
    // basins — real-world elevation and mountain presence correlate, and it
    // gives regions a more distinct identity than "jagged everywhere."
    let continental_bias = (continental / CONTINENTAL_SCALE).clamp(-1.0, 1.0) * 0.18;
    let mountain_mask_raw = fbm(&n.mask, x, z, 3, 1.0 / 1400.0);
    let mountain_mask = smoothstep((mountain_mask_raw - 0.05 - continental_bias) / 0.35);
    let ridges = fbm_ridged(&n.ridge, wx, wz, 5, 1.0 / 260.0, 1.3) * MOUNTAIN_HEIGHT;
    // Fine ridged detail at ~3x frequency, layered on top at low amplitude
    // — texture on the silhouette #ridges already established, not enough
    // to dominate and read as jagged noise on its own.
    let fine_ridges =
        fbm_ridged(&n.ridge, wx * 3.0, wz * 3.0, 3, 1.0 / 90.0, 1.3) * MOUNTAIN_HEIGHT * 0.08;

    let (temperature, moisture) = climate_at(n, x, z);
    let desert_weight = temperature * (1.0 - moisture);
    let canyon_region = smoothstep((fbm(&n.canyon_region, x, z, 2, 1.0 / 1800.0) - 0.05) / 0.3);
    let canyon_ridges = fbm_ridged(&n.canyon, wx, wz, 4, 1.0 / 300.0, 1.5);
    let canyon_carve = canyon_ridges * CANYON_DEPTH * desert_weight * canyon_region;

    let detail = n.detail.get([x * 0.08, z * 0.08]) as f32 * 0.35;

    continental + regional + plains + mountain_mask * (ridges + fine_ridges) - canyon_carve
        + detail
}

/// Cosmetic high-frequency texture noise sampled at the same frequency
/// `height_at`'s own detail layer uses — exposed separately (rather than
/// folded into `height_at`'s return value) because the client's vertex-color
/// pass (`terrain_color`) needs this exact value alongside height, not
/// derived from it.
pub fn detail_at(n: &TerrainNoise, x: f64, z: f64) -> f32 {
    n.detail.get([x * 1.3, z * 1.3]) as f32
}

pub fn slope_at(n: &TerrainNoise, x: f64, z: f64, sample_step: f32) -> Vec3 {
    let step = sample_step as f64;
    let hl = height_at(n, x - step, z);
    let hr = height_at(n, x + step, z);
    let hd = height_at(n, x, z - step);
    let hu = height_at(n, x, z + step);
    Vec3::new(hl - hr, 2.0 * sample_step, hd - hu).normalize()
}

/// Finds a reasonably flat spot within `search_radius` of `near_true`, for
/// spawning/resetting onto. Terrain here can be genuinely extreme (100km+
/// mountain ranges are the point), so landing exactly on the input point
/// unchecked is a coin flip between "grassy field" and "vertical cliff
/// face, tumbling." Grid-samples slope around the point and keeps the
/// flattest candidate — cheap (each sample is 4 height_at calls) and only
/// runs on spawn/reset, never per-frame.
pub fn find_flat_spawn(n: &TerrainNoise, near_true: DVec3, search_radius: f64) -> DVec3 {
    const STEPS: i32 = 8;
    let mut best = near_true;
    let mut best_flatness = f32::MIN;
    for i in -STEPS..=STEPS {
        for j in -STEPS..=STEPS {
            let x = near_true.x + (i as f64 / STEPS as f64) * search_radius;
            let z = near_true.z + (j as f64 / STEPS as f64) * search_radius;
            let normal = slope_at(n, x, z, 4.0);
            // Prefer flat ground, breaking ties toward the search center so
            // spawn doesn't wander further than it needs to.
            let dist_penalty = (((i * i + j * j) as f32).sqrt()) * 0.002;
            let flatness = normal.y - dist_penalty;
            if flatness > best_flatness {
                best_flatness = flatness;
                best = DVec3::new(x, 0.0, z);
            }
        }
    }
    best
}

/// Bilinear blend across a (temperature, moisture) climate square — corners
/// ordered [cold_dry, cold_wet, hot_dry, hot_wet].
fn biome_lerp(corners: [Vec3; 4], temperature: f32, moisture: f32) -> Vec3 {
    let cold = corners[0].lerp(corners[1], moisture);
    let hot = corners[2].lerp(corners[3], moisture);
    cold.lerp(hot, temperature)
}

fn biome_lerp_f32(corners: [f32; 4], temperature: f32, moisture: f32) -> f32 {
    let cold = corners[0] + (corners[1] - corners[0]) * moisture;
    let hot = corners[2] + (corners[3] - corners[2]) * moisture;
    cold + (hot - cold) * temperature
}

/// Blends a paint color from climate + height + slope, the same way a lot
/// of hand-authored planet/terrain shaders do it, so we get whole climate
/// regions (tundra, taiga, desert, grassland) with grass/dirt/rock/snow
/// variation inside each — without needing a single external texture asset.
pub fn terrain_color(height: f32, slope: f32, detail: f32, temperature: f32, moisture: f32) -> Color {
    // [cold_dry, cold_wet, hot_dry, hot_wet]
    let lowland_corners = [
        Vec3::new(0.45, 0.48, 0.42), // tundra: pale, sparse
        Vec3::new(0.15, 0.35, 0.28), // taiga: deep mossy green
        Vec3::new(0.76, 0.62, 0.38), // desert: sand
        Vec3::new(0.22, 0.48, 0.16), // grassland/savanna
    ];
    let rock_corners = [
        Vec3::new(0.40, 0.42, 0.46),
        Vec3::new(0.34, 0.36, 0.35),
        Vec3::new(0.55, 0.35, 0.22), // canyon rust/sandstone
        Vec3::new(0.42, 0.40, 0.36),
    ];
    // Snow line height, as a fraction of MOUNTAIN_HEIGHT. The desert corner
    // is set far above any possible peak so it effectively never snows.
    let snow_line_corners = [0.22, 0.32, 10.0, 0.62];

    let lowland_base = biome_lerp(lowland_corners, temperature, moisture);
    let rock_base = biome_lerp(rock_corners, temperature, moisture);
    let snow_line = biome_lerp_f32(snow_line_corners, temperature, moisture);

    // High-frequency noise breaks up each biome's ground into brighter
    // patches and darker dirt/bare patches, so it doesn't read as a single
    // flat color swatch even within one climate region.
    let brightened = lowland_base * 1.15;
    let lowland = lowland_base.lerp(brightened, (detail * 0.5 + 0.5).clamp(0.0, 1.0));
    let dirt_amount = (1.0 - (detail * 0.5 + 0.5)).max(0.0).powf(3.0) * 0.7;
    let lowland = lowland.lerp(lowland_base * 0.7, dirt_amount.clamp(0.0, 1.0));

    // Blend toward bare rock only on genuinely steep faces, not the gentle
    // rolling hills that make up most of this terrain.
    let slope_t = ((slope - 0.5) / 0.35).clamp(0.0, 1.0);
    let with_rock = lowland.lerp(rock_base, slope_t);

    // Snow caps above the biome's snow line, softened at the boundary.
    let snow = Vec3::new(0.92, 0.93, 0.95);
    let snow_t =
        ((height - MOUNTAIN_HEIGHT * snow_line) / (MOUNTAIN_HEIGHT * 0.15)).clamp(0.0, 1.0);
    let final_color = with_rock.lerp(snow, snow_t);

    Color::srgb(final_color.x, final_color.y, final_color.z)
}
