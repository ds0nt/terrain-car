use uuid::Uuid;

/// Folds a player's durable account id down to a `u32` seed for a
/// deterministic per-owner color (see client's `owner_color::
/// color_from_seed`) — shared because both the server (the real,
/// authoritative computation, at car/building/villager spawn time) and
/// the client (its own local guess for a just-logged-in player's own car,
/// before the server's authoritative echo replicates back — see car.rs's
/// `spawn_car_after_login`) must produce the exact same value from the
/// same `player_id`, or the guess wouldn't match and the car would
/// visibly change color the moment the real one arrives. The first 4
/// bytes are as good as any other slice of a random UUID for this
/// purpose — only used for a cosmetic hue, never anything requiring real
/// uniqueness guarantees.
pub fn seed_from_uuid(id: Uuid) -> u32 {
    let bytes = id.as_bytes();
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}
