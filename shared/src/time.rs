use std::time::{SystemTime, UNIX_EPOCH};

/// Current wall-clock time as Unix-epoch seconds — this project's
/// established way of comparing "when" across client and server (they
/// don't share a common elapsed-since-startup clock, but do share real
/// time), used for `BuildingSnapshot::build_complete_at` and
/// `server::persistence`'s own timestamp columns. Extracted here once a
/// third call site (`server::villagers`) needed the exact same few lines
/// `economy.rs` and client's `building_render.rs` already had independently.
pub fn now_unix() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is set before the Unix epoch")
        .as_secs_f64()
}
