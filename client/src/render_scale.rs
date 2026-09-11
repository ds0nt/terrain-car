use bevy::camera::{ClearColorConfig, RenderTarget};
use bevy::image::Image;
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages};
use bevy::ui::IsDefaultUiCamera;
use bevy::window::PrimaryWindow;
use bevy_egui::{EguiContext, EguiGlobalSettings, PrimaryEguiContext};

use crate::camera::CarCamera;

/// Decouples the 3D scene's own internal render resolution from the
/// actual window/display resolution — the "render resolution" scaler some
/// games (the user's own reference: Dota 2) expose, distinct from
/// `settings.rs`'s window-resolution picker (which resizes the *window
/// itself* — moot in fullscreen, since the window already *is* the
/// display). Cuts GPU cost on a weak/integrated GPU without touching the
/// actual display mode at all.
///
/// How: the 3D world camera (`CarCamera`) no longer renders to the window
/// directly — it renders into an off-screen `Image` sized
/// `window_size * scale` (see `SceneRenderTarget`). A dedicated
/// `PresentCamera` then draws that texture, stretched, as a full-screen
/// `ImageNode` onto the real window. Every actual UI element (`hud.rs`'s
/// HUD, `chat.rs`, the build bar, the minimap, ...) instead renders
/// through a *third* camera (`UiCamera`) layered on top of that, always at
/// the window's own native resolution — so text and UI never get blurry
/// even at a low render scale, only the 3D world does.
pub struct RenderScalePlugin;

impl Plugin for RenderScalePlugin {
    fn build(&self, app: &mut App) {
        app
            // `bevy_egui` otherwise auto-attaches its primary context to
            // whichever camera is created first (`CarCamera`, spawned by
            // `camera::spawn_camera`) — exactly the one we're about to
            // redirect to an offscreen texture. Disabling the auto-pick and
            // manually attaching it to `UiCamera` instead (see
            // `setup_render_scale`) is what keeps every `egui` panel
            // rendering at native resolution rather than also going
            // through the low-res 3D texture.
            .insert_resource(EguiGlobalSettings { auto_create_primary_context: false, ..default() })
            .init_resource::<RenderScale>()
            .add_systems(Startup, setup_render_scale.after(crate::camera::spawn_camera))
            .add_systems(Update, sync_render_target_size);
    }
}

/// `0.0..=1.0` — see `settings.rs`'s own slider. `1.0` (the default) means
/// "render the 3D world at the window's own native resolution," identical
/// to not having this whole system at all.
#[derive(Resource)]
pub struct RenderScale {
    pub scale: f32,
}

impl Default for RenderScale {
    fn default() -> Self {
        Self { scale: 1.0 }
    }
}

/// Lowest allowed scale — low enough to matter on weak hardware, not so
/// low the image is unrecognizable.
pub const RENDER_SCALE_MIN: f32 = 0.3;

/// The offscreen texture `CarCamera` renders into, and `PresentCamera`'s
/// `ImageNode` displays — kept as a resource (not re-fetched from the
/// camera/node each time) so `sync_render_target_size` can resize this one
/// asset in place on a scale or window-size change, rather than needing to
/// recreate it and re-point every reference at a fresh handle.
#[derive(Resource)]
struct SceneRenderTarget(Handle<Image>);

/// Renders `SceneRenderTarget`'s current contents, stretched, onto the
/// real window — a plain full-screen `ImageNode`, but through its *own*
/// camera (`order` below every other window-targeting camera) rather than
/// sharing `UiCamera`'s tree, so it can't ever end up interleaved with a
/// real UI element by spawn-order accident.
#[derive(Component)]
struct PresentCamera;

/// The dedicated, always-native-resolution camera every actual UI element
/// renders through — see this module's own top-level docs.
#[derive(Component)]
struct UiCamera;

fn setup_render_scale(
    mut commands: Commands,
    mut images: ResMut<Assets<Image>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    world_camera_q: Query<Entity, With<CarCamera>>,
) {
    let (Ok(window), Ok(world_camera)) = (windows.single(), world_camera_q.single()) else {
        return;
    };

    let handle = images.add(new_render_target_image(render_target_size(window, 1.0)));

    // `RenderTarget` is its own component now (not a `Camera` field) —
    // `Camera3d`'s own required-component default already inserted one
    // pointing at the primary window when `camera::spawn_camera` spawned
    // this entity; this just replaces it.
    commands.entity(world_camera).insert(RenderTarget::Image(handle.clone().into()));
    commands.insert_resource(SceneRenderTarget(handle.clone()));

    let present_camera = commands.spawn((Camera2d, Camera { order: 0, ..default() }, PresentCamera)).id();
    // A top-level `Node`, not a child of `present_camera` — `Node`
    // entities are their own UI tree roots (parenting one under a camera
    // entity, which isn't itself part of any UI tree, doesn't make it
    // one). Routed to `present_camera` explicitly via `UiTargetCamera`
    // rather than relying on "the default camera" — `UiCamera` below
    // already claims that role via `IsDefaultUiCamera`, and this quad
    // deliberately needs the *other* camera instead.
    commands.spawn((
        Node { width: Val::Percent(100.0), height: Val::Percent(100.0), ..default() },
        ImageNode::new(handle),
        UiTargetCamera(present_camera),
    ));

    commands.spawn((
        Camera2d,
        Camera {
            order: 1,
            // Critical: without this, `UiCamera` clearing the window
            // before it draws would wipe out `PresentCamera`'s
            // already-rendered 3D-scene image right before this camera's
            // own UI draws on top of it, leaving only UI over a blank
            // background.
            clear_color: ClearColorConfig::None,
            ..default()
        },
        UiCamera,
        IsDefaultUiCamera,
        EguiContext::default(),
        PrimaryEguiContext,
    ));
}

/// Re-checks every frame (cheap — just an integer comparison; the actual
/// `Image::resize` only runs on a real change) rather than reacting to a
/// window-resize event and a render-scale change as two separate cases:
/// either one ends up wanting the exact same recomputed size, so one
/// unified check covers both without needing two code paths that could
/// drift apart.
fn sync_render_target_size(
    render_scale: Res<RenderScale>,
    windows: Query<&Window, With<PrimaryWindow>>,
    target: Option<Res<SceneRenderTarget>>,
    mut images: ResMut<Assets<Image>>,
) {
    let (Some(target), Ok(window)) = (target, windows.single()) else {
        return;
    };
    let desired = render_target_size(window, render_scale.scale);
    let Some(mut image) = images.get_mut(&target.0) else {
        return;
    };
    let current = image.texture_descriptor.size;
    if current.width != desired.width || current.height != desired.height {
        image.resize(desired);
    }
}

fn render_target_size(window: &Window, scale: f32) -> Extent3d {
    let scale = scale.clamp(RENDER_SCALE_MIN, 1.0);
    let width = ((window.physical_width() as f32) * scale).round().max(1.0) as u32;
    let height = ((window.physical_height() as f32) * scale).round().max(1.0) as u32;
    Extent3d { width, height, depth_or_array_layers: 1 }
}

fn new_render_target_image(size: Extent3d) -> Image {
    let mut image = Image {
        texture_descriptor: TextureDescriptor {
            label: Some("scene_render_target"),
            size,
            dimension: TextureDimension::D2,
            // `TextureFormat::bevy_default()` exists but is deprecated —
            // it always resolves to exactly this anyway.
            format: TextureFormat::Rgba8UnormSrgb,
            mip_level_count: 1,
            sample_count: 1,
            usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST | TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        },
        ..default()
    };
    image.resize(size);
    image
}
