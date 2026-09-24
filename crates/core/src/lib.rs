pub mod config;
pub mod feedback;
pub mod generations;
pub mod i18n;
pub mod storage;
pub mod types;

/// The addon's directory name under the game's `addons` folder, as Nexus
/// resolves it for the running DLL (`<addons>/gw2_build_optimizer/`). The
/// developer settings in `dev.cfg` derive the cache path from the same name.
pub const ADDON_DIR_NAME: &str = "gw2_build_optimizer";
