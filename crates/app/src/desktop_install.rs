//! Linux desktop integration, self-installed.
//!
//! On Wayland the compositor cannot use the window icon the app
//! sets at runtime: it resolves the icon from the app_id through
//! an installed `.desktop` file and the icon theme. A packaged
//! install ships both (see `packaging/zaprust.spec`), but the
//! bare release binary installs nothing, and on KDE Plasma the
//! window then wears the compositor's generic Wayland fallback
//! mark instead of its own icon (the owner's first live run,
//! 23 Nov 2026). So on Linux the app installs its own desktop
//! integration into the user's data home on startup: the themed
//! icons and a desktop entry whose Exec points at the running
//! executable. The write is idempotent and best-effort: a
//! packaged install's system files are never touched (the user
//! copies shadow them, with the same content), and any failure
//! is silent, because desktop integration must never block the
//! app from starting.

/// The desktop entry for the running executable, in the exact
/// shape `packaging/zaprust.desktop` ships: the Icon name and
/// the window class must stay `zaprust` so the entry matches the
/// app_id the window reports.
pub(crate) fn desktop_entry(exec_path: &std::path::Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Zaprust\n\
         Comment=A fast, native WhatsApp client\n\
         Exec={}\n\
         Icon=zaprust\n\
         Terminal=false\n\
         Categories=Network;InstantMessaging;Chat;\n\
         StartupWMClass=zaprust\n",
        exec_path.display()
    )
}

/// The icon set this installer writes. The staleness check is
/// content comparison (see `write_if_changed`), so a new set
/// replaces the old one on the next launch by itself; this
/// marker names the set in the installed tree, so a stale
/// install is identifiable from the files alone. Set 1 was the
/// bare black mark; set 2 is the plate set below.
const ICON_SET_VERSION: &str = "2";

/// The themed icon payloads: the plate rasters (the white mark
/// on its dark plate), embedded the same way the window icon
/// is. A freedesktop icon theme cannot follow the panel's
/// darkness, so the OS-facing set is the plated variant, which
/// stays legible on light and dark panels alike; the bare
/// marks remain the in-app assets.
fn icon_payloads() -> [(u32, &'static [u8]); 8] {
    [
        (16, include_bytes!("../../../packaging/icons/zaprust-plate-16.png")),
        (24, include_bytes!("../../../packaging/icons/zaprust-plate-24.png")),
        (32, include_bytes!("../../../packaging/icons/zaprust-plate-32.png")),
        (48, include_bytes!("../../../packaging/icons/zaprust-plate-48.png")),
        (64, include_bytes!("../../../packaging/icons/zaprust-plate-64.png")),
        (128, include_bytes!("../../../packaging/icons/zaprust-plate-128.png")),
        (256, include_bytes!("../../../packaging/icons/zaprust-plate-256.png")),
        (512, include_bytes!("../../../packaging/icons/zaprust-plate-512.png")),
    ]
}

/// Writes one file only when its content differs, so a steady
/// state costs one comparison per file per launch.
fn write_if_changed(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    if std::fs::read(path).is_ok_and(|cur| cur == bytes) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, bytes)
}

/// Installs (or refreshes) the user-level desktop entry and
/// themed icons. Called once at startup on Linux; every error
/// is swallowed by the caller-visible `()` contract.
pub(crate) fn ensure_desktop_integration() {
    let Some(dirs) = directories::BaseDirs::new() else { return };
    let data = dirs.data_dir();
    let Ok(exec_path) = std::env::current_exe() else { return };
    let entry = desktop_entry(&exec_path);
    let _ = write_if_changed(&data.join("applications/zaprust.desktop"), entry.as_bytes());
    for (size, bytes) in icon_payloads() {
        let path = data.join(format!("icons/hicolor/{size}x{size}/apps/zaprust.png"));
        let _ = write_if_changed(&path, bytes);
    }
    let _ = write_if_changed(
        &data.join("icons/hicolor/.zaprust-icon-set"),
        format!("{ICON_SET_VERSION}\n").as_bytes(),
    );
}

#[cfg(test)]
mod tests {
    use super::desktop_entry;
    use std::path::Path;

    /// The installed entry must carry the chain the compositor
    /// resolves: the themed icon name and the window class that
    /// matches the app_id, with Exec at the running binary.
    #[test]
    fn desktop_entry_carries_the_resolution_chain() {
        let entry = desktop_entry(Path::new("/home/user/zaprust-linux-x86_64"));
        assert!(entry.contains("Icon=zaprust\n"));
        assert!(entry.contains("StartupWMClass=zaprust\n"));
        assert!(entry.contains("Exec=/home/user/zaprust-linux-x86_64\n"));
        assert!(entry.contains("Name=Zaprust\n"));
    }

    /// The embedded icon payloads are the shipped rasters: PNG
    /// bytes at the sizes the theme paths will claim.
    #[test]
    fn icon_payloads_are_pngs_at_their_sizes() {
        for (size, bytes) in super::icon_payloads() {
            assert!(bytes.starts_with(b"\x89PNG"), "size {size} is not a PNG");
            let img = image::load_from_memory(bytes).expect("payload decodes");
            assert_eq!((img.width(), img.height()), (size, size));
        }
    }

    /// The installed set is the plate set, not the bare black
    /// mark it replaced: a sample above the mark, inside the
    /// plate, is the plate color #282A2C at full opacity.
    #[test]
    fn icon_payloads_are_the_plate_set() {
        for (size, bytes) in super::icon_payloads() {
            let img = image::load_from_memory(bytes).expect("payload decodes").to_rgba8();
            let sample = img.get_pixel(size / 2, size / 25);
            assert_eq!(sample.0, [40, 42, 44, 255], "size {size} is not the plate raster");
        }
    }
}
