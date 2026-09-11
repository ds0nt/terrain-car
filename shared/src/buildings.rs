use bevy::prelude::{Quat, Vec3};
use serde::{Deserialize, Serialize};

/// Ramp geometry — shared by client (`building_render.rs`, visual mesh)
/// and server (physics collider) so the two can never disagree about
/// where a ramp's actual driving surface is. A tilted box rather than a
/// real wedge: simpler, and "for now" placeholder quality per the same
/// spirit as `Villager`'s glowing-orb visual.
pub const RAMP_HALF_WIDTH: f32 = 2.0;
// Thin on purpose: a tilted box (not a true wedge) has a real geometric
// step at its leading edge where the flat bottom meets the rising top
// surface — live-tested and confirmed a car can catch on that step
// momentarily before climbing over. Keeping the box thin keeps that step
// small enough to be a curb, not a wall; a true wedge mesh would remove
// it entirely but is a bigger lift than this placeholder warrants yet.
pub const RAMP_HALF_THICKNESS: f32 = 0.08;
// 4x the original 3.0 — the short ramp read as a speed bump; this gives a
// real launch runway at the same `RAMP_TILT_RAD` grade.
pub const RAMP_HALF_LENGTH: f32 = 12.0;
pub const RAMP_TILT_RAD: f32 = 0.35; // ~20 degrees

/// A road: the same driving-surface shape as `Ramp`, just flat
/// (`tilt_rad: 0.0` in `slab_dims`) and a bit wider — meant for smoothing a
/// path across rough ground or linking two ramps, not climbing anything.
pub const ROAD_HALF_WIDTH: f32 = 3.0;
pub const ROAD_HALF_THICKNESS: f32 = 0.08;
pub const ROAD_HALF_LENGTH: f32 = 12.0;

/// A platform: a flat, wide pad rather than a narrow strip — a landing/
/// staging area, still placed the same "click one edge, drag which way it
/// extends" way every other slab here is.
pub const PLATFORM_HALF_WIDTH: f32 = 6.0;
pub const PLATFORM_HALF_THICKNESS: f32 = 0.15;
pub const PLATFORM_HALF_LENGTH: f32 = 6.0;

/// A wall: thin and tall rather than wide and flat — the only slab kind
/// that actually blocks a car outright instead of giving it a surface to
/// drive on. `half_height` fills the same geometric role `half_thickness`
/// plays for every flat kind above (see `SlabDims`'s docs).
pub const WALL_HALF_WIDTH: f32 = 0.3;
pub const WALL_HALF_HEIGHT: f32 = 3.0;
pub const WALL_HALF_LENGTH: f32 = 8.0;

/// Dimensions for one of the "click one edge, drag which way it extends"
/// structural kinds (`Ramp`, `Road`, `Platform`, `Wall` — see
/// `BuildingKind::uses_slab_geometry`). `half_height` is the box's own Y
/// half-extent before tilt — thin for every flat, driveable kind, tall for
/// `Wall`, which stands upright instead of lying flat. `tilt_rad` is 0 for
/// everything except `Ramp`.
#[derive(Clone, Copy)]
pub struct SlabDims {
    pub half_width: f32,
    pub half_height: f32,
    pub half_length: f32,
    pub tilt_rad: f32,
}

/// `SlabDims` for each structural kind — `Ramp`'s own constants folded in
/// alongside the new kinds so `slab_transform` has one shared source of
/// truth for all four instead of `ramp_transform` staying a separate,
/// hand-duplicated path.
pub fn slab_dims(kind: BuildingKind) -> SlabDims {
    match kind {
        BuildingKind::Ramp => {
            SlabDims { half_width: RAMP_HALF_WIDTH, half_height: RAMP_HALF_THICKNESS, half_length: RAMP_HALF_LENGTH, tilt_rad: RAMP_TILT_RAD }
        }
        BuildingKind::Road => {
            SlabDims { half_width: ROAD_HALF_WIDTH, half_height: ROAD_HALF_THICKNESS, half_length: ROAD_HALF_LENGTH, tilt_rad: 0.0 }
        }
        BuildingKind::Platform => SlabDims {
            half_width: PLATFORM_HALF_WIDTH,
            half_height: PLATFORM_HALF_THICKNESS,
            half_length: PLATFORM_HALF_LENGTH,
            tilt_rad: 0.0,
        },
        BuildingKind::Wall => {
            SlabDims { half_width: WALL_HALF_WIDTH, half_height: WALL_HALF_HEIGHT, half_length: WALL_HALF_LENGTH, tilt_rad: 0.0 }
        }
        _ => unreachable!("slab_dims only applies to BuildingKind::uses_slab_geometry kinds"),
    }
}

/// Local-space (chunk/origin-relative) translation and rotation for one of
/// the structural slab kinds' mesh *and* collider — a pure function of
/// where it's placed, so client and server independently compute the
/// identical transform instead of one trusting the other's geometry.
/// `local_x`/`local_z` is the anchor — the actual point the player
/// clicked, always the slab's own *near* edge (the low end, for a tilted
/// `Ramp`), never its middle (see below) — `ground_y` is the terrain
/// height at that same anchor; `rotation_y` is which way it extends,
/// chosen by the drag direction (see client's `building_placement.rs`).
pub fn slab_transform(dims: SlabDims, local_x: f32, local_z: f32, ground_y: f32, rotation_y: f32) -> (Vec3, Quat) {
    let (sin_tilt, cos_tilt) = dims.tilt_rad.sin_cos();
    let (sin_yaw, cos_yaw) = rotation_y.sin_cos();
    // The mesh/collider are centered on their own middle, so the box's
    // *center* has to sit half its length beyond the anchor, out along
    // whichever way it extends — otherwise the anchor lands at the slab's
    // midpoint instead of its near edge. That's exactly what made a long
    // `Ramp` (see `RAMP_HALF_LENGTH`) settle to the terrain height under
    // its *midpoint* instead of the point actually clicked, and float or
    // clip badly at the low end on anything but flat ground.
    let half_length_offset = Vec3::new(
        dims.half_length * cos_tilt * sin_yaw,
        dims.half_length * sin_tilt,
        dims.half_length * cos_tilt * cos_yaw,
    );
    // Same idea as the old flat "lift," just now only covering the box's
    // own height (the length component moved into `half_length_offset`
    // above) — keeps the near edge's *bottom face*, not its center, flush
    // with `ground_y`.
    let base_lift = dims.half_height * cos_tilt;
    let translation = Vec3::new(local_x, ground_y + base_lift, local_z) + half_length_offset;
    let rotation = Quat::from_rotation_y(rotation_y) * Quat::from_rotation_x(-dims.tilt_rad);
    (translation, rotation)
}

/// True-space `(x, z, y)` of a slab's own *far* edge — the opposite end
/// from its stored anchor (`BuildingSnapshot::true_x`/`true_z`/`ground_y`,
/// always the slab's *near* edge, see `slab_transform`'s own docs). `y` is
/// derived rather than re-sampled from terrain: for every flat kind
/// (`tilt_rad: 0.0`) it's identical to the near edge's own `ground_y`, and
/// for `Ramp` it's correctly higher by the same rise its tilt already
/// produces. Used by client's `building_placement.rs` to snap a new slab's
/// start point onto an *existing* one's near or far edge — see that
/// module's own docs on why matching this exactly (not just "close, from an
/// independent terrain raycast") is what actually keeps two connected
/// slabs flush instead of stair-stepping at the seam.
pub fn slab_far_edge_true(dims: SlabDims, true_x: f64, true_z: f64, ground_y: f32, rotation_y: f32) -> (f64, f64, f32) {
    let (sin_yaw, cos_yaw) = rotation_y.sin_cos();
    let run = (2.0 * dims.half_length * dims.tilt_rad.cos()) as f64;
    let rise = 2.0 * dims.half_length * dims.tilt_rad.sin();
    (true_x + run * sin_yaw as f64, true_z + run * cos_yaw as f64, ground_y + rise)
}

/// A solid collider shape for a non-`Ramp` building — every kind blocks a
/// car now, not just `Ramp` (which uses its own tilted `ramp_transform`
/// path instead and never calls this). Shared by client
/// (`building_render.rs`) and server (`economy.rs`) so mesh and collider
/// dimensions can never drift apart, the same reasoning `RAMP_HALF_WIDTH`
/// etc. already follow.
pub enum ColliderShape {
    Cuboid { half_x: f32, half_y: f32, half_z: f32 },
    Cylinder { half_height: f32, radius: f32 },
}

impl ColliderShape {
    /// Half-height above `ground_y` the entity's own transform origin
    /// should sit at — every shape here is upright and vertically
    /// centered, so this is just its own half-extent along Y.
    pub fn half_height(&self) -> f32 {
        match *self {
            ColliderShape::Cuboid { half_y, .. } => half_y,
            ColliderShape::Cylinder { half_height, .. } => half_height,
        }
    }
}

/// Extra sink below the lowest sampled point of a footprint (see
/// `footprint_sample_offsets`) — a placement's terrain samples only ever
/// check a handful of discrete points, never the whole base area, so this
/// buffer covers whatever the terrain does *between* them (still buried,
/// never a sliver of daylight under an edge).
pub const FOOTPRINT_BURY_MARGIN: f32 = 0.3;

/// Local-space (x, z) offsets to sample the terrain/surface height at when
/// settling a non-`Ramp` building into the ground — every corner for a
/// `Cuboid` (plus center), several points around the rim for a `Cylinder`
/// (plus center). A single center-point raycast (the old approach) only
/// ever matches the terrain at that one spot; on a slope the rest of the
/// footprint either floats above the ground or is left half-exposed.
/// Taking the *lowest* of these samples (see callers in
/// `server::economy`) and sinking the whole building to it instead means
/// every sampled point of the base ends up buried at or below the visible
/// surface, on any slope, not just flush at its own center.
pub fn footprint_sample_offsets(shape: ColliderShape) -> Vec<(f32, f32)> {
    match shape {
        ColliderShape::Cuboid { half_x, half_z, .. } => vec![
            (0.0, 0.0),
            (half_x, half_z),
            (half_x, -half_z),
            (-half_x, half_z),
            (-half_x, -half_z),
        ],
        ColliderShape::Cylinder { radius, .. } => {
            const RIM_SAMPLES: usize = 8;
            let mut points = vec![(0.0, 0.0)];
            for i in 0..RIM_SAMPLES {
                let angle = i as f32 / RIM_SAMPLES as f32 * std::f32::consts::TAU;
                points.push((angle.cos() * radius, angle.sin() * radius));
            }
            points
        }
    }
}

/// Extents matching each kind's existing render mesh exactly (see
/// `client::building_render::building_mesh_and_transform`) — the
/// `uses_slab_geometry` kinds (`Ramp`, `Road`, `Platform`, `Wall`) aren't
/// covered here, they have their own dedicated `slab_transform` geometry.
pub fn collider_shape(kind: BuildingKind) -> ColliderShape {
    match kind {
        BuildingKind::Hangar => ColliderShape::Cuboid { half_x: 2.0, half_y: 1.25, half_z: 2.5 },
        BuildingKind::EnergyGenerator => ColliderShape::Cylinder { half_height: 1.5, radius: 1.2 },
        BuildingKind::ExtractionFacility => ColliderShape::Cylinder { half_height: 2.0, radius: 0.8 },
        BuildingKind::LandFactory => ColliderShape::Cuboid { half_x: 3.0, half_y: 1.75, half_z: 3.0 },
        BuildingKind::AirFactory => ColliderShape::Cuboid { half_x: 3.5, half_y: 2.0, half_z: 3.5 },
        BuildingKind::WarFactory => ColliderShape::Cuboid { half_x: 3.5, half_y: 2.0, half_z: 3.5 },
        BuildingKind::Dropyard => ColliderShape::Cuboid { half_x: 4.5, half_y: 2.25, half_z: 4.5 },
        // Just the base pad — the rotating head sits on top as its own
        // child entity (`client::building_render`'s turret variant,
        // `server::turrets`'s aim state), same "collider only covers the
        // stationary part" split a `Tank`'s own hull/turret pair uses.
        BuildingKind::Turret => ColliderShape::Cuboid { half_x: 1.2, half_y: 1.0, half_z: 1.2 },
        BuildingKind::Ramp | BuildingKind::Road | BuildingKind::Platform | BuildingKind::Wall => {
            unreachable!("{kind:?} uses slab_transform, never collider_shape")
        }
    }
}

/// Every new player's wallet starts here — enough to afford at least one
/// of any single building outright, so the very first building is
/// actually placeable (nothing produces resources until a building
/// already exists, so *something* has to seed the economy).
pub const STARTING_ENERGY: f32 = 1000.0;
pub const STARTING_ORE: f32 = 1000.0;

/// Caps runaway villager growth per player — shared with the client (which
/// needs it purely to show "queue full" in the Land Factory panel, see
/// `client::building_ui`) so the displayed cap can never drift from what
/// `server::villagers` actually enforces. Counts *live* villagers plus
/// however many are still queued to be built, not just live ones — a
/// full queue reads as full immediately, instead of only once every
/// already-queued villager has actually finished spawning.
pub const MAX_VILLAGERS_PER_PLAYER: u32 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BuildingKind {
    /// Spawns a car the moment it completes (`spawns_car`/
    /// `server::car_sim::spawn_cars_from_hangars`) — a car is no longer a
    /// free, automatic thing every new player gets on login; it's
    /// something you build. Full parity with `AirFactory` spawning a
    /// plane: every completed Hangar produces its own car, so a player can
    /// own several (see `CarChassis::car_id`'s docs on how a specific one
    /// gets targeted for driving/recall once there's more than one).
    /// `H`/`RecallToHangarMsg` recalls whichever car you're currently
    /// driving to whichever owned Hangar is nearest right now, not
    /// necessarily the one that originally spawned it.
    Hangar,
    /// Passively produces `energy` once built — anywhere, no deposit
    /// required.
    EnergyGenerator,
    /// Passively produces `ore` once built — but only where
    /// `shared::deposits::is_near_deposit` says yes.
    ExtractionFacility,
    /// A physical, drivable structure rather than an economy building — no
    /// production, but it gets a real collider (see client's
    /// `building_render.rs`) so a car can actually drive up it, unlike
    /// every economy-building kind here, which is purely decorative.
    /// Placed by clicking its low end and dragging the direction it climbs
    /// — see `client::building_placement` and `slab_transform`.
    Ramp,
    /// A flat driving surface — same click-and-drag placement as `Ramp`,
    /// just untilted and a bit wider, meant for smoothing a path or
    /// linking ramps rather than climbing anything.
    Road,
    /// A flat, wide pad rather than a narrow strip — same placement
    /// mechanic again, just sized for standing/staging on top of rather
    /// than driving *along*.
    Platform,
    /// The only slab kind that actually blocks a car instead of giving it
    /// a surface — thin and tall rather than wide and flat. Same
    /// click-one-end-drag-the-other placement as every other slab kind.
    Wall,
    /// v1 scope note: currently only produces more `Villager`s for its
    /// owner (see `villagers_spawned()` and `server::villagers`) — the
    /// long-term plan is for this to also build tanks/AA tanks, but that's
    /// a real unit/combat-AI system deserving its own pass, not shipped
    /// here. Think of this as "the foundation," not "the whole factory."
    LandFactory,
    /// A "starport" — the moment it completes, spawns one `ScoutPlane` for
    /// its owner nearby (see `server::aircraft`), already theirs, parked
    /// and waiting. v1 scope note: one automatic plane per completed
    /// factory, no manual queue (unlike `LandFactory`'s villager queue) —
    /// simplest thing that gets a flyable vehicle into a player's hands at
    /// all; a real production queue is a reasonable follow-up once there's
    /// more than one aircraft kind to actually choose between.
    AirFactory,
    /// Spawns one `Tank` for its owner the moment it completes — full
    /// parity with `Hangar`/`AirFactory` spawning a car/plane, just for the
    /// vehicle described in `shared::tank_physics`. One automatic tank per
    /// completed factory, same v1-simplicity reasoning `AirFactory`'s own
    /// docs give for a scout plane (no manual queue).
    WarFactory,
    /// Spawns one `Dropship` for its owner the moment it completes — same
    /// "one vehicle per completed factory" shape as `WarFactory`/
    /// `AirFactory`, just for the multi-seat transport (1 pilot + 4
    /// passenger seats, see `protocol::DropshipSnapshot`).
    Dropyard,
    /// A stationary, auto-targeting defense structure — not a factory (it
    /// doesn't spawn anything else), it *is* the unit: a rotating "turret
    /// head" (see `server::turrets`) that aims at and fires on the nearest
    /// enemy within range on its own, no manual control. Conceptually it's
    /// a vehicle with zero mobility (it aims and fires exactly like a
    /// `Tank`'s own turret does, sharing that same aim/fire shape) that
    /// just happens to be placed and built like any other building.
    Turret,
}

impl BuildingKind {
    /// `(energy, ore)` cost to place this building — deducted immediately
    /// on a successful placement, no partial refunds if it's ever removed
    /// (no removal exists yet in v1 anyway).
    pub fn cost(self) -> (f32, f32) {
        match self {
            BuildingKind::Hangar => (50.0, 20.0),
            BuildingKind::EnergyGenerator => (30.0, 10.0),
            BuildingKind::ExtractionFacility => (40.0, 30.0),
            BuildingKind::Ramp => (5.0, 10.0),
            BuildingKind::Road => (8.0, 12.0),
            BuildingKind::Platform => (15.0, 20.0),
            BuildingKind::Wall => (10.0, 8.0),
            // Deliberately above the starting wallet — a real investment
            // you grow into, not a turn-one option like the others, same
            // reasoning `LandFactory` uses (it also hands you a unit, just
            // a villager instead of a plane).
            BuildingKind::LandFactory => (80.0, 60.0),
            BuildingKind::AirFactory => (90.0, 70.0),
            // A tank/dropship each cost noticeably more than the car/plane
            // factories they parallel — combat/transport hardware, not a
            // starter vehicle.
            BuildingKind::WarFactory => (140.0, 120.0),
            BuildingKind::Dropyard => (160.0, 150.0),
            BuildingKind::Turret => (70.0, 60.0),
        }
    }

    /// Seconds from placement to completion — a `BuildingSnapshot` exists
    /// (and replicates) the moment it's placed, but doesn't produce
    /// anything and shows as "under construction" client-side until this
    /// elapses.
    pub fn build_time_secs(self) -> f32 {
        match self {
            BuildingKind::Hangar => 20.0,
            BuildingKind::EnergyGenerator => 15.0,
            BuildingKind::ExtractionFacility => 25.0,
            BuildingKind::Ramp => 8.0,
            BuildingKind::Road => 10.0,
            BuildingKind::Platform => 15.0,
            BuildingKind::Wall => 10.0,
            BuildingKind::LandFactory => 30.0,
            BuildingKind::AirFactory => 35.0,
            BuildingKind::WarFactory => 45.0,
            BuildingKind::Dropyard => 50.0,
            BuildingKind::Turret => 20.0,
        }
    }

    /// `(energy/sec, ore/sec)` produced once construction completes —
    /// zero/zero for kinds with no direct economic output (`Hangar` and
    /// every `uses_slab_geometry` kind; `LandFactory`'s output is
    /// villagers and `AirFactory`'s is a plane, neither a resource rate —
    /// see `villagers_spawned`/`spawns_scout_plane`).
    pub fn production_rate(self) -> (f32, f32) {
        match self {
            BuildingKind::Hangar
            | BuildingKind::Ramp
            | BuildingKind::Road
            | BuildingKind::Platform
            | BuildingKind::Wall
            | BuildingKind::LandFactory
            | BuildingKind::AirFactory
            | BuildingKind::WarFactory
            | BuildingKind::Dropyard
            | BuildingKind::Turret => (0.0, 0.0),
            BuildingKind::EnergyGenerator => (1.5, 0.0),
            BuildingKind::ExtractionFacility => (0.0, 1.0),
        }
    }

    /// Whether this kind may only be placed within
    /// `shared::deposits::DEPOSIT_CLAIM_RADIUS` of a deposit.
    pub fn requires_deposit(self) -> bool {
        matches!(self, BuildingKind::ExtractionFacility)
    }

    /// Whether this kind is placed by clicking one edge/end and dragging
    /// the direction it extends (`slab_transform`/`slab_dims`), rather
    /// than clicking once to drop an upright `collider_shape` obstacle —
    /// and, correspondingly, whether `server::economy::settle_ground_y`
    /// should sample just that one anchor point instead of the shape's
    /// whole footprint. `Wall` is included even though it isn't literally
    /// "drivable" (it blocks a car outright) — this is a geometry/
    /// placement classifier, not a "can you drive on top of it" one; see
    /// each variant's own docs for what actually happens when a car meets
    /// it.
    pub fn uses_slab_geometry(self) -> bool {
        matches!(self, BuildingKind::Ramp | BuildingKind::Road | BuildingKind::Platform | BuildingKind::Wall)
    }

    /// Whether this kind periodically spawns extra `Villager`s for its
    /// owner (`server::villagers`) — see `LandFactory`'s v1 scope note.
    pub fn spawns_villagers(self) -> bool {
        matches!(self, BuildingKind::LandFactory)
    }

    /// Whether this kind spawns a `ScoutPlane` for its owner the moment it
    /// completes (`server::aircraft`) — see `AirFactory`'s v1 scope note.
    pub fn spawns_scout_plane(self) -> bool {
        matches!(self, BuildingKind::AirFactory)
    }

    /// Whether this kind can spawn its owner a car the moment it completes
    /// (`server::car_sim::spawn_cars_from_hangars`) — see `Hangar`'s own
    /// docs for why this is capped at one live car per player, not one per
    /// building the way `spawns_scout_plane`/`AirFactory` is.
    pub fn spawns_car(self) -> bool {
        matches!(self, BuildingKind::Hangar)
    }

    /// Whether this kind spawns a `Tank` for its owner the moment it
    /// completes (`server::tank_sim::spawn_tanks_from_war_factories`) —
    /// see `WarFactory`'s own docs.
    pub fn spawns_tank(self) -> bool {
        matches!(self, BuildingKind::WarFactory)
    }

    /// Whether this kind spawns a `Dropship` for its owner the moment it
    /// completes (`server::dropship_sim::spawn_dropships_from_dropyards`) —
    /// see `Dropyard`'s own docs.
    pub fn spawns_dropship(self) -> bool {
        matches!(self, BuildingKind::Dropyard)
    }

    /// Whether this kind is itself a stationary auto-targeting defense unit
    /// (`server::turrets`) rather than an ordinary economy/structural
    /// building — see `Turret`'s own docs.
    pub fn is_turret(self) -> bool {
        matches!(self, BuildingKind::Turret)
    }

    /// Stable string form for the `buildings.kind` database column (a
    /// plain `text` column — see `server/migrations/0001_init.sql`) —
    /// deliberately not `serde`'s `Serialize`/`Deserialize` (those drive
    /// the *network* wire format via `BuildingSnapshot`/`PlaceBuildingMsg`,
    /// a separate concern that happens to also currently be human-readable
    /// but isn't obligated to stay that way).
    pub fn as_db_str(self) -> &'static str {
        match self {
            BuildingKind::Hangar => "hangar",
            BuildingKind::EnergyGenerator => "energy_generator",
            BuildingKind::ExtractionFacility => "extraction_facility",
            BuildingKind::Ramp => "ramp",
            BuildingKind::Road => "road",
            BuildingKind::Platform => "platform",
            BuildingKind::Wall => "wall",
            BuildingKind::LandFactory => "land_factory",
            BuildingKind::AirFactory => "air_factory",
            BuildingKind::WarFactory => "war_factory",
            BuildingKind::Dropyard => "dropyard",
            BuildingKind::Turret => "turret",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "hangar" => Some(BuildingKind::Hangar),
            "energy_generator" => Some(BuildingKind::EnergyGenerator),
            "extraction_facility" => Some(BuildingKind::ExtractionFacility),
            "ramp" => Some(BuildingKind::Ramp),
            "road" => Some(BuildingKind::Road),
            "platform" => Some(BuildingKind::Platform),
            "wall" => Some(BuildingKind::Wall),
            "land_factory" => Some(BuildingKind::LandFactory),
            "air_factory" => Some(BuildingKind::AirFactory),
            "war_factory" => Some(BuildingKind::WarFactory),
            "dropyard" => Some(BuildingKind::Dropyard),
            "turret" => Some(BuildingKind::Turret),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_KINDS: [BuildingKind; 12] = [
        BuildingKind::Hangar,
        BuildingKind::EnergyGenerator,
        BuildingKind::ExtractionFacility,
        BuildingKind::Ramp,
        BuildingKind::Road,
        BuildingKind::Platform,
        BuildingKind::Wall,
        BuildingKind::LandFactory,
        BuildingKind::AirFactory,
        BuildingKind::WarFactory,
        BuildingKind::Dropyard,
        BuildingKind::Turret,
    ];
    const STARTER_KINDS: [BuildingKind; 3] =
        [BuildingKind::Hangar, BuildingKind::EnergyGenerator, BuildingKind::ExtractionFacility];

    #[test]
    fn every_kind_has_a_positive_cost_and_build_time() {
        for kind in ALL_KINDS {
            let (energy, ore) = kind.cost();
            assert!(energy > 0.0 || ore > 0.0, "{kind:?} costs nothing at all");
            assert!(kind.build_time_secs() > 0.0, "{kind:?} has a non-positive build time");
        }
    }

    #[test]
    fn hangar_produces_nothing() {
        assert_eq!(BuildingKind::Hangar.production_rate(), (0.0, 0.0));
    }

    #[test]
    fn only_extraction_facility_requires_a_deposit() {
        assert!(!BuildingKind::Hangar.requires_deposit());
        assert!(!BuildingKind::EnergyGenerator.requires_deposit());
        assert!(BuildingKind::ExtractionFacility.requires_deposit());
    }

    #[test]
    fn db_str_round_trips_for_every_kind() {
        for kind in ALL_KINDS {
            assert_eq!(BuildingKind::from_db_str(kind.as_db_str()), Some(kind));
        }
    }

    #[test]
    fn starting_wallet_affords_every_starter_kind() {
        // LandFactory is deliberately excluded — see its own docs, it's a
        // real investment you grow into, not a turn-one option.
        for kind in STARTER_KINDS {
            let (energy, ore) = kind.cost();
            assert!(
                energy <= STARTING_ENERGY && ore <= STARTING_ORE,
                "{kind:?} costs ({energy}, {ore}) but the starting wallet is only \
                 ({STARTING_ENERGY}, {STARTING_ORE})"
            );
        }
    }

    #[test]
    fn ramp_costs_exactly_what_was_asked_for() {
        assert_eq!(BuildingKind::Ramp.cost(), (5.0, 10.0));
    }

    #[test]
    fn only_the_slab_kinds_use_slab_geometry() {
        for kind in ALL_KINDS {
            let expected = matches!(
                kind,
                BuildingKind::Ramp | BuildingKind::Road | BuildingKind::Platform | BuildingKind::Wall
            );
            assert_eq!(kind.uses_slab_geometry(), expected, "{kind:?}");
        }
    }

    #[test]
    fn every_slab_kind_has_matching_dims_and_collider_arms() {
        // slab_dims must not panic for any uses_slab_geometry kind, and
        // collider_shape must not panic for any kind that isn't one —
        // the two functions partition BuildingKind identically, or a
        // placement/render call for some kind would hit the other's
        // `unreachable!()`.
        for kind in ALL_KINDS {
            if kind.uses_slab_geometry() {
                let _ = slab_dims(kind);
            } else {
                let _ = collider_shape(kind);
            }
        }
    }

    #[test]
    fn only_land_factory_spawns_villagers() {
        for kind in ALL_KINDS {
            assert_eq!(kind.spawns_villagers(), kind == BuildingKind::LandFactory);
        }
    }

    #[test]
    fn only_air_factory_spawns_a_scout_plane() {
        for kind in ALL_KINDS {
            assert_eq!(kind.spawns_scout_plane(), kind == BuildingKind::AirFactory);
        }
    }

    #[test]
    fn only_hangar_spawns_a_car() {
        for kind in ALL_KINDS {
            assert_eq!(kind.spawns_car(), kind == BuildingKind::Hangar);
        }
    }

    #[test]
    fn only_war_factory_spawns_a_tank() {
        for kind in ALL_KINDS {
            assert_eq!(kind.spawns_tank(), kind == BuildingKind::WarFactory);
        }
    }

    #[test]
    fn only_dropyard_spawns_a_dropship() {
        for kind in ALL_KINDS {
            assert_eq!(kind.spawns_dropship(), kind == BuildingKind::Dropyard);
        }
    }

    #[test]
    fn only_turret_is_a_turret() {
        for kind in ALL_KINDS {
            assert_eq!(kind.is_turret(), kind == BuildingKind::Turret);
        }
    }
}
