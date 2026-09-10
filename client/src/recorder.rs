use std::fs::{self, File};
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use bevy::prelude::*;
use bevy::render::view::screenshot::{save_to_disk, Screenshot};
use bevy_rapier3d::prelude::*;

use crate::car::{CarInput, LocalCar};

/// Sibling to the real robot's `data/drivedata2/` (same schema, see below),
/// so recordings never mix with real robot data but need zero loader
/// changes — point `dataPath` at this directory instead. Overridable via
/// env var mainly so a test run doesn't have to write into the real
/// dataset tree.
const DEFAULT_OUTPUT_ROOT: &str = "../../data/drivedata2_terrain";
/// Matches the real robot capture cadence closely enough to be a
/// reasonable drop-in dataset, without the disk/CPU cost of a screenshot
/// every render frame.
const CAPTURE_HZ: f32 = 12.0;
/// Forward raycast length stand-in for the real robot's ultrasonic sensor;
/// `front_distance` is written as this when nothing is hit, matching how a
/// real ultrasonic sensor reports "nothing in range."
const FRONT_DISTANCE_MAX: f32 = 400.0;

pub struct RecorderPlugin;

impl Plugin for RecorderPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Recording>()
            .init_resource::<RecordingActive>()
            .add_systems(Update, (toggle_recording, capture_tick).chain());
    }
}

#[derive(Resource, Default)]
struct Recording {
    session: Option<Session>,
}

struct Session {
    dir: std::path::PathBuf,
    csv: File,
    time_since_capture: f32,
}

/// HUD (hud.rs) reads this to show a "REC" indicator without needing to
/// know anything else about the recorder.
#[derive(Resource, Default)]
pub struct RecordingActive(pub bool);

fn toggle_recording(
    keyboard: Res<ButtonInput<KeyCode>>,
    chat_open: Res<crate::chat::ChatOpen>,
    mut recording: ResMut<Recording>,
    mut active: ResMut<RecordingActive>,
) {
    if chat_open.0 || !keyboard.just_pressed(KeyCode::KeyL) {
        return;
    }

    if recording.session.is_some() {
        recording.session = None;
        info!("recorder: stopped");
    } else {
        match start_session() {
            Ok(session) => {
                info!("recorder: recording to {}", session.dir.display());
                recording.session = Some(session);
            }
            Err(e) => {
                error!("recorder: failed to start session: {e}");
            }
        }
    }

    active.0 = recording.session.is_some();
}

fn start_session() -> std::io::Result<Session> {
    let root = std::env::var("TERRAIN_CAR_RECORD_ROOT")
        .unwrap_or_else(|_| DEFAULT_OUTPUT_ROOT.to_string());
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let dir = std::path::Path::new(&root).join(format!("{timestamp}-terrain"));
    fs::create_dir_all(&dir)?;
    let csv = File::create(dir.join("status.csv"))?;
    Ok(Session {
        dir,
        csv,
        time_since_capture: 0.0,
    })
}

/// Appends one status.csv row and, on the same tick, kicks off an async
/// screenshot save for the matching image — throttled to `CAPTURE_HZ`
/// rather than every render frame.
///
/// Column order matches `engine/constants.py` exactly: `timestamp,
/// steering, camera_yaw, camera_pitch, throttle, front_distance, valid`.
/// `camera_yaw`/`camera_pitch` are always 0 — this car has no pan-tilt
/// camera, unlike the real robot these columns were defined for. Steering/
/// throttle are our own `[-1, 1]`-ish control values, not an attempt at
/// matching the real robot's raw PWM/degree scale — the downstream loaders
/// only read steering/throttle/images (see engine's own loader comments),
/// so schema compatibility is what matters here, not unit-for-unit
/// equivalence.
fn capture_tick(
    time: Res<Time>,
    input: Res<CarInput>,
    rapier_context: ReadRapierContext,
    mut commands: Commands,
    mut recording: ResMut<Recording>,
    car_q: Query<(Entity, &GlobalTransform), With<LocalCar>>,
) {
    let Some(session) = recording.session.as_mut() else {
        return;
    };
    session.time_since_capture += time.delta_secs();
    if session.time_since_capture < 1.0 / CAPTURE_HZ {
        return;
    }
    session.time_since_capture = 0.0;

    // `.iter().next()`, not `.single()` — a player can own several cars
    // now (see `car.rs`'s top-level docs), so this just captures whichever
    // one happens to be first rather than recording nothing at all for
    // anyone who owns more than one.
    let Some((car_entity, car_gt)) = car_q.iter().next() else {
        return;
    };
    let transform = car_gt.compute_transform();

    let front_distance = rapier_context
        .single()
        .ok()
        .and_then(|context| {
            context.cast_ray(
                transform.translation,
                *transform.forward(),
                FRONT_DISTANCE_MAX,
                true,
                QueryFilter::new().exclude_rigid_body(car_entity),
            )
        })
        .map(|(_entity, toi)| toi)
        .unwrap_or(FRONT_DISTANCE_MAX);

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();

    if let Err(e) = writeln!(
        session.csv,
        "{now},{},{},{},{},{},{}",
        input.steer, 0.0, 0.0, input.throttle, front_distance, 1.0
    ) {
        error!("recorder: failed to write status.csv row: {e}");
    }

    let image_path = session.dir.join(format!("{now}.jpg"));
    commands
        .spawn(Screenshot::primary_window())
        .observe(save_to_disk(image_path));
}
