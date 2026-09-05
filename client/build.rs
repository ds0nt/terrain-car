use std::env;
use std::path::PathBuf;

/// Bevy's default asset reader looks for an "assets" directory next to the
/// running executable (`target/<profile>/assets`), not next to the crate
/// (`client/assets`) — fine for a shipped build (release.yml copies real
/// assets alongside the binary into the release tarball), but `cargo
/// build`/`cargo run` would otherwise 404 on every asset load during local
/// development. Symlinking `target/<profile>/assets -> client/assets` here
/// fixes that without duplicating files or going stale when assets change.
fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let src = manifest_dir.join("assets");
    println!("cargo:rerun-if-changed={}", src.display());

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    // OUT_DIR is target/<profile>/build/<crate>-<hash>/out; four levels up
    // is target/<profile>, where the binary itself lands.
    let Some(profile_dir) = out_dir.ancestors().nth(3) else {
        return;
    };
    let dest = profile_dir.join("assets");

    if dest.is_symlink() || dest.exists() {
        return;
    }

    #[cfg(unix)]
    let _ = std::os::unix::fs::symlink(&src, &dest);
    #[cfg(windows)]
    let _ = std::os::windows::fs::symlink_dir(&src, &dest);
}
