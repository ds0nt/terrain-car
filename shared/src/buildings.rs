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
pub const RAMP_HALF_LENGTH: f32 = 3.0;
pub const RAMP_TILT_RAD: f32 = 0.35; // ~20 degrees

/// Local-space (chunk/origin-relative) translation and rotation for a
/// Ramp's mesh *and* collider — a pure function of where it's placed, so
/// client and server independently compute the identical transform
/// instead of one trusting the other's geometry. `local_x`/`local_z` is
/// the placement point already converted out of true space; `ground_y` is
/// the terrain height there; `rotation_y` is the yaw it was placed facing
/// (captured from the placing car's own heading — see `PlaceBuildingMsg`).
pub fn ramp_transform(local_x: f32, local_z: f32, ground_y: f32, rotation_y: f32) -> (Vec3, Quat) {
    // Lifts the box's center so its lowest corner sits near ground level
    // rather than the box's untilted bottom face — approximate (this is a
    // placeholder ramp, not exact wedge geometry), tuned to read as
    // "starts at the ground, rises going forward."
    let lift = RAMP_HALF_THICKNESS * RAMP_TILT_RAD.cos() + RAMP_HALF_LENGTH * RAMP_TILT_RAD.sin();
    let translation = Vec3::new(local_x, ground_y + lift, local_z);
    let rotation = Quat::from_rotation_y(rotation_y) * Quat::from_rotation_x(-RAMP_TILT_RAD);
    (translation, rotation)
}

/// Every new player's wallet starts here — enough to afford at least one
/// of any single building outright, so the very first building is
/// actually placeable (nothing produces resources until a building
/// already exists, so *something* has to seed the economy).
pub const STARTING_ENERGY: f32 = 60.0;
pub const STARTING_ORE: f32 = 40.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BuildingKind {
    /// v1 scope note: a named respawn/recall point for the player's single
    /// existing car (`RecallToHangarMsg`), not a multi-car garage — see
    /// the base-building plan's "Hangar, simplified for v1" section for
    /// why real "store/spawn extra cars" is deliberately deferred.
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
    /// every other kind here which is purely decorative.
    Ramp,
    /// v1 scope note: currently only produces more `Villager`s for its
    /// owner (see `villagers_spawned()` and `server::villagers`) — the
    /// long-term plan is for this to also build tanks/AA tanks, but that's
    /// a real unit/combat-AI system deserving its own pass, not shipped
    /// here. Think of this as "the foundation," not "the whole factory."
    LandFactory,
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
            // Deliberately above the starting wallet — a real investment
            // you grow into, not a turn-one option like the others.
            BuildingKind::LandFactory => (80.0, 60.0),
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
            BuildingKind::LandFactory => 30.0,
        }
    }

    /// `(energy/sec, ore/sec)` produced once construction completes —
    /// zero/zero for kinds with no direct economic output (`Hangar`,
    /// `Ramp`; `LandFactory`'s output is villagers, not a resource rate —
    /// see `villagers_spawned`).
    pub fn production_rate(self) -> (f32, f32) {
        match self {
            BuildingKind::Hangar | BuildingKind::Ramp | BuildingKind::LandFactory => (0.0, 0.0),
            BuildingKind::EnergyGenerator => (1.5, 0.0),
            BuildingKind::ExtractionFacility => (0.0, 1.0),
        }
    }

    /// Whether this kind may only be placed within
    /// `shared::deposits::DEPOSIT_CLAIM_RADIUS` of a deposit.
    pub fn requires_deposit(self) -> bool {
        matches!(self, BuildingKind::ExtractionFacility)
    }

    /// Whether this kind is a physical, drivable structure rather than a
    /// decorative economy building — see `Ramp`'s own docs.
    pub fn is_drivable_structure(self) -> bool {
        matches!(self, BuildingKind::Ramp)
    }

    /// Whether this kind periodically spawns extra `Villager`s for its
    /// owner (`server::villagers`) — see `LandFactory`'s v1 scope note.
    pub fn spawns_villagers(self) -> bool {
        matches!(self, BuildingKind::LandFactory)
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
            BuildingKind::LandFactory => "land_factory",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "hangar" => Some(BuildingKind::Hangar),
            "energy_generator" => Some(BuildingKind::EnergyGenerator),
            "extraction_facility" => Some(BuildingKind::ExtractionFacility),
            "ramp" => Some(BuildingKind::Ramp),
            "land_factory" => Some(BuildingKind::LandFactory),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_KINDS: [BuildingKind; 5] = [
        BuildingKind::Hangar,
        BuildingKind::EnergyGenerator,
        BuildingKind::ExtractionFacility,
        BuildingKind::Ramp,
        BuildingKind::LandFactory,
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
    fn only_ramp_is_a_drivable_structure() {
        for kind in ALL_KINDS {
            assert_eq!(kind.is_drivable_structure(), kind == BuildingKind::Ramp);
        }
    }

    #[test]
    fn only_land_factory_spawns_villagers() {
        for kind in ALL_KINDS {
            assert_eq!(kind.spawns_villagers(), kind == BuildingKind::LandFactory);
        }
    }
}
