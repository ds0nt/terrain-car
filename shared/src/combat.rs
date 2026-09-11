use bevy::prelude::*;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Every car spawns with (and respawns to) this much health.
pub const DEFAULT_MAX_HEALTH: f32 = 100.0;

/// Minimum time between shots — enforced both client-side (`weapon_fx.rs`,
/// for responsive UX: no waiting on a round trip to know the trigger is
/// still on cooldown) and server-side (`weapons.rs`, authoritative: a
/// modified client can't out-fire this).
pub const FIRE_COOLDOWN_SECS: f32 = 0.35;

/// A car's current/max hit points. Replicated (see `protocol.rs`) so every
/// client can show every car's health, not just its own — continuous
/// replication, unlike `CarChassis`'s one-shot, since this changes
/// constantly once guns exist.
#[derive(Component, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct Health {
    pub current: f32,
    pub max: f32,
}

impl Health {
    pub fn full(max: f32) -> Self {
        Self { current: max, max }
    }

    pub fn fraction(&self) -> f32 {
        if self.max <= 0.0 {
            0.0
        } else {
            (self.current / self.max).clamp(0.0, 1.0)
        }
    }

    /// Subtracts `amount` (clamped so health never goes negative) and
    /// returns whether this killed it — pure and unit-tested so death
    /// detection can't quietly regress inside whichever ECS system ends up
    /// calling it.
    pub fn apply_damage(&mut self, amount: f32) -> bool {
        self.current = (self.current - amount).max(0.0);
        self.current <= 0.0
    }

    pub fn refill(&mut self) {
        self.current = self.max;
    }
}

/// Generic "who owns this shootable thing" marker, present on every vehicle
/// that carries a `Health` (car, tank, plane, dropship) alongside its own
/// kind-specific chassis/snapshot component. Exists purely so a targeting
/// system that doesn't care *what kind* of vehicle it's looking at (chiefly
/// `server::turrets`' auto-aim, which has to compare against every vehicle
/// kind at once) can query one uniform component instead of separately
/// handling `CarChassis`, `TankChassis`, `PlaneSnapshot`, and
/// `DropshipSnapshot` each with their own `owner_player_id` field. Not
/// replicated (no `Serialize`/`Deserialize`) — this is a server-only
/// query aid, never anything a client needs to know about; each vehicle's
/// own chassis/snapshot component already replicates its owner id for
/// every client-side purpose.
#[derive(Component, Clone, Copy)]
pub struct Combatant {
    pub owner_player_id: Uuid,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lethal_damage_clamps_to_zero_and_reports_death() {
        let mut health = Health::full(100.0);
        let died = health.apply_damage(150.0);
        assert!(died);
        assert_eq!(health.current, 0.0);
    }

    #[test]
    fn survivable_damage_does_not_report_death() {
        let mut health = Health::full(100.0);
        let died = health.apply_damage(40.0);
        assert!(!died);
        assert_eq!(health.current, 60.0);
    }

    #[test]
    fn exactly_lethal_damage_reports_death() {
        let mut health = Health::full(100.0);
        assert!(health.apply_damage(100.0));
    }

    #[test]
    fn refill_restores_to_max() {
        let mut health = Health::full(100.0);
        health.apply_damage(90.0);
        health.refill();
        assert_eq!(health.current, health.max);
    }

    #[test]
    fn fraction_is_current_over_max() {
        let mut health = Health::full(80.0);
        health.apply_damage(20.0);
        assert!((health.fraction() - 0.75).abs() < 1e-6);
    }
}
