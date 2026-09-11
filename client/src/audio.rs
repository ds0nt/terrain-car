use std::io::Cursor;

use bevy::prelude::*;
use rodio::speakers::{available_outputs, Output, SpeakersBuilder};
use rodio::MixerDeviceSink;

/// A short, synthesized "crack" — white noise burst over a low thump,
/// generated as a placeholder sound effect (no external asset needed, same
/// "simple procedural placeholder" spirit this project's visuals already
/// follow — see `car_render.rs`'s bow, `building_render.rs`'s side logos).
/// Embedded via `include_bytes!` rather than loaded through
/// `AssetServer` at runtime: there's no asset-loading race to handle for
/// one small, always-needed sound, and it sidesteps `bevy_audio`'s own
/// `AudioSource` asset type entirely, which this module doesn't use (see
/// this module's own top-level docs on why).
const GUNSHOT_WAV: &[u8] = include_bytes!("../assets/sounds/gunshot.wav");

/// Drives audio output directly through `rodio`, entirely independent of
/// Bevy's own built-in `AudioPlugin`/`AudioPlayer`/`PlaybackSettings` —
/// deliberately so, not out of preference: `bevy_audio`'s device-opening
/// code (`AudioOutput`) is a private, `pub(crate)` type that always opens
/// the OS's *default* output device with no way to pick a different one,
/// so genuine device selection (the whole point of this module — the
/// user's own ask for an audio-device settings menu) is only possible by
/// bypassing it and managing an output stream ourselves. This does mean
/// none of Bevy's own audio ergonomics (spatial audio components, the
/// `AudioSink` component, asset-based sound loading) are available — this
/// game doesn't have enough sound content yet to miss them, and the
/// moment it does, this module is exactly the one place that would need
/// to grow to cover it.
pub struct AudioPlugin;

impl Plugin for AudioPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<AudioDevices>()
            .init_resource::<SelectedAudioDevice>()
            .init_resource::<AudioOutputStream>()
            .add_message::<PlaySfx>()
            .add_systems(Startup, (refresh_audio_devices, open_selected_output).chain())
            .add_systems(Update, (apply_audio_device_change, play_queued_sfx));
    }
}

/// Every currently-available output device — refreshed at startup and
/// whenever `refresh_audio_devices` is called again (see `settings.rs`'s
/// own "Refresh devices" button, for a device plugged in mid-session).
/// `pub` so `settings.rs` can list it in a dropdown.
#[derive(Resource, Default)]
pub struct AudioDevices {
    pub outputs: Vec<Output>,
}

/// Re-enumerates available output devices — cheap enough (a handful of
/// devices, typically) to call freely, not something that needs its own
/// throttling. A plain function taking `&mut AudioDevices` (not a
/// `ResMut`-based system) so `settings.rs`'s own "Refresh devices" button
/// can call it directly against the `ResMut` it already holds, instead of
/// needing to run a whole separate system for one on-click action.
pub fn refresh_audio_devices_now(devices: &mut AudioDevices) {
    devices.outputs = available_outputs().unwrap_or_default();
}

fn refresh_audio_devices(mut devices: ResMut<AudioDevices>) {
    refresh_audio_devices_now(&mut devices);
}

/// Which output device to use, by display name — `None` means "the OS
/// default." Stored as a name, not an `Output` directly: `Output` carries
/// a live `cpal::Device` handle tied to one specific enumeration snapshot,
/// while a name survives re-enumeration (`AudioDevices` is refreshed
/// independently) and is trivial to show/select in `settings.rs`'s own
/// `ComboBox`. Changing this (`settings.rs`'s dropdown writes it directly)
/// is what `apply_audio_device_change` watches via ordinary Bevy change
/// detection to know when to reopen the output stream.
#[derive(Resource, Default, Clone, PartialEq, Eq)]
pub struct SelectedAudioDevice {
    pub name: Option<String>,
}

/// The live, currently-open output stream/mixer — `None` whenever no
/// device could be opened at all (same "warn and continue silently mute"
/// tolerance `bevy_audio`'s own `AudioOutput::default()` has for the
/// identical failure).
#[derive(Resource, Default)]
struct AudioOutputStream(Option<MixerDeviceSink>);

/// Opens (or reopens) the output stream for whatever `SelectedAudioDevice`
/// currently names, matched by display name against the most recently
/// refreshed `AudioDevices` list. Falls back to the system default if the
/// named device can no longer be found (unplugged, renamed) rather than
/// going silent outright.
fn open_output(name: Option<&str>, devices: &AudioDevices) -> Option<MixerDeviceSink> {
    let matched = name.and_then(|n| devices.outputs.iter().find(|d| d.to_string() == n));
    let builder = match matched {
        Some(output) => SpeakersBuilder::new().device(output.clone()).ok(),
        None => None,
    };
    let builder = match builder {
        Some(b) => Some(b),
        None => SpeakersBuilder::new().default_device().ok(),
    }?;
    let mut sink = builder.default_config().ok()?.open_mixer().ok()?;
    sink.log_on_drop(false);
    Some(sink)
}

fn open_selected_output(
    selected: Res<SelectedAudioDevice>,
    devices: Res<AudioDevices>,
    mut output: ResMut<AudioOutputStream>,
) {
    output.0 = open_output(selected.name.as_deref(), &devices);
    if output.0.is_none() {
        warn!("audio: no output device could be opened — running silent");
    }
}

/// Reopens the output stream whenever `settings.rs` changes
/// `SelectedAudioDevice` — ordinary Bevy change detection (`is_changed`),
/// not a dedicated event, since this is the only writer.
fn apply_audio_device_change(
    selected: Res<SelectedAudioDevice>,
    devices: Res<AudioDevices>,
    mut output: ResMut<AudioOutputStream>,
) {
    if !selected.is_changed() {
        return;
    }
    output.0 = open_output(selected.name.as_deref(), &devices);
    if output.0.is_none() {
        warn!("audio: no output device could be opened for the newly selected device — running silent");
    }
}

/// Fire-and-forget one-shot sound effect request — carries the raw
/// embedded WAV bytes to decode and play (currently always
/// `GUNSHOT_WAV`; `pub` field so a future sound just passes a different
/// `include_bytes!` slice, no protocol change needed).
#[derive(Message, Clone, Copy)]
pub struct PlaySfx {
    pub bytes: &'static [u8],
}

fn play_queued_sfx(mut events: MessageReader<PlaySfx>, output: Res<AudioOutputStream>) {
    let Some(sink) = &output.0 else {
        events.clear();
        return;
    };
    for event in events.read() {
        // `rodio::play` decodes internally — it wants the raw `Read + Seek`
        // byte source directly, not an already-built `Decoder` (which
        // implements `Source`/`Iterator`, not `Read`/`Seek`).
        match rodio::play(sink.mixer(), Cursor::new(event.bytes)) {
            // Detached, not held — a one-shot effect should keep playing
            // to completion on its own, independent of whatever entity
            // triggered it (already despawned by the time a gunshot's
            // muzzle flash fades, for instance).
            Ok(player) => player.detach(),
            Err(err) => warn!("audio: failed to play a sound effect: {err}"),
        }
    }
}

/// `PlaySfx { bytes: GUNSHOT_WAV }` — `weapon_fx.rs`'s own gunfire reaction
/// uses this constant rather than reaching into this module's private
/// asset bytes directly.
pub fn gunshot_sfx() -> PlaySfx {
    PlaySfx { bytes: GUNSHOT_WAV }
}
