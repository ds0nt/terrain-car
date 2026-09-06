use serde::{Deserialize, Serialize};

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
        }
    }

    /// `(energy/sec, ore/sec)` produced once construction completes —
    /// zero/zero for `Hangar`, which has no economic output.
    pub fn production_rate(self) -> (f32, f32) {
        match self {
            BuildingKind::Hangar => (0.0, 0.0),
            BuildingKind::EnergyGenerator => (1.5, 0.0),
            BuildingKind::ExtractionFacility => (0.0, 1.0),
        }
    }

    /// Whether this kind may only be placed within
    /// `shared::deposits::DEPOSIT_CLAIM_RADIUS` of a deposit.
    pub fn requires_deposit(self) -> bool {
        matches!(self, BuildingKind::ExtractionFacility)
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
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "hangar" => Some(BuildingKind::Hangar),
            "energy_generator" => Some(BuildingKind::EnergyGenerator),
            "extraction_facility" => Some(BuildingKind::ExtractionFacility),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_KINDS: [BuildingKind; 3] =
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
    fn starting_wallet_affords_at_least_one_of_each_kind() {
        for kind in ALL_KINDS {
            let (energy, ore) = kind.cost();
            assert!(
                energy <= STARTING_ENERGY && ore <= STARTING_ORE,
                "{kind:?} costs ({energy}, {ore}) but the starting wallet is only \
                 ({STARTING_ENERGY}, {STARTING_ORE})"
            );
        }
    }
}
