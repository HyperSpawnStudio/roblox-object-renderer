//! The local Roblox Studio install: detection and `rbxasset://` lookup.
//!
//! Detection order: [`crate::AssetConfig::studio_content`], env `ROR_ROBLOX_CONTENT` (the `content`
//! folder or the version folder containing it), the registry value
//! `HKCU\Software\Roblox\RobloxStudio\ContentFolder`, then the newest
//! `%LOCALAPPDATA%\Roblox\Versions\*\RobloxStudioBeta.exe` (on macOS the newest `RobloxStudio.app`,
//! [`newest_mac_app`]).
//!
//! Lookup order for `rbxasset://<path>`: `PlatformContent/pc/<path>`, `content/<path>`,
//! `ExtraContent/<path>`. In every install on this machine (12 versions) no relative path exists
//! in both `PlatformContent/pc` and another root, and the 71 paths in both `content` and
//! `ExtraContent` are byte-identical, so the order is unobservable today; the platform folder goes
//! first because it holds the per-platform overrides (`textures/studs.dds`, `textures/sky/*.tex`,
//! `textures/plastic`, `textures/water`, `terrain/*.json` exist only there).

use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub(crate) struct StudioInstall {
	pub content: PathBuf,
	/// Lookup roots in order
	pub roots: Vec<PathBuf>,
}

impl StudioInstall {
	/// An install from its `content` folder (or the version folder holding `content`).
	pub(crate) fn from_dir(dir: &Path) -> Option<StudioInstall> {
		let content = if dir.join("content").is_dir() && !dir.join("textures").is_dir() { dir.join("content") } else { dir.to_path_buf() };
		if !content.is_dir() {
			return None;
		}
		let version = content.parent().map(Path::to_path_buf).unwrap_or_else(|| content.clone());
		let roots: Vec<PathBuf> = [version.join("PlatformContent").join("pc"), content.clone(), version.join("ExtraContent")]
			.into_iter()
			.filter(|p| p.is_dir())
			.collect();
		Some(StudioInstall { content, roots })
	}

	/// The file for a normalised `rbxasset://` path.
	pub(crate) fn find(&self, path: &str) -> Option<PathBuf> {
		self.roots.iter().map(|root| root.join(path)).find(|candidate| candidate.is_file())
	}
}

pub(crate) fn detect(explicit: Option<&Path>) -> Option<StudioInstall> {
	if let Some(dir) = explicit {
		return StudioInstall::from_dir(dir);
	}
	if let Some(dir) = std::env::var_os("ROR_ROBLOX_CONTENT").filter(|v| !v.is_empty()) {
		return StudioInstall::from_dir(Path::new(&dir));
	}
	if let Some(found) = registry_content_folder().as_deref().and_then(StudioInstall::from_dir) {
		return Some(found);
	}
	newest_install().as_deref().and_then(StudioInstall::from_dir)
}

/// The newest app's `Contents/Resources` ([`newest_mac_app`]), which holds `content`,
/// `PlatformContent` and `ExtraContent` as a Windows version folder does.
#[cfg(target_os = "macos")]
fn newest_install() -> Option<PathBuf> {
	newest_mac_app().map(|app| app.join("Contents").join("Resources")).filter(|dir| dir.join("content").is_dir())
}

/// macOS: the newest `RobloxStudio.app`, by its executable's modification time (the Windows lookup's
/// rule), of the one Studio's updater keeps in `~/Library/Roblox/UnzippedBundle` (the build Studio
/// runs: 0.746.0.7461269 there while `/Applications` still held 0.746.0.7461264, 2026-11-06),
/// `/Applications` and `~/Applications`.
#[cfg(target_os = "macos")]
pub fn newest_mac_app() -> Option<PathBuf> {
	let home = std::env::var_os("HOME").map(PathBuf::from);
	newest_app([home.as_ref().map(|home| home.join("Library").join("Roblox").join("UnzippedBundle")), Some(PathBuf::from("/Applications")), home.map(|home| home.join("Applications"))].into_iter().flatten())
}

/// The `RobloxStudio.app` in `dirs` whose executable was modified last.
#[cfg(any(target_os = "macos", test))]
fn newest_app(dirs: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
	dirs.into_iter()
		.map(|dir| dir.join("RobloxStudio.app"))
		.filter_map(|app| {
			let modified = app.join("Contents").join("MacOS").join("RobloxStudio").metadata().ok()?.modified().unwrap_or(std::time::UNIX_EPOCH);
			Some((modified, app))
		})
		.max_by_key(|(modified, _)| *modified)
		.map(|(_, app)| app)
}

/// Newest `%LOCALAPPDATA%\Roblox\Versions\<v>` holding `RobloxStudioBeta.exe`, as its `content`.
#[cfg(not(target_os = "macos"))]
fn newest_install() -> Option<PathBuf> {
	let versions = PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("Roblox").join("Versions");
	let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
	for entry in std::fs::read_dir(versions).ok()?.flatten() {
		let exe = entry.path().join("RobloxStudioBeta.exe");
		let Ok(meta) = exe.metadata() else {
			continue;
		};
		let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
		if best.as_ref().is_none_or(|(t, _)| modified > *t) {
			best = Some((modified, entry.path().join("content")));
		}
	}
	best.map(|(_, p)| p)
}

#[cfg(windows)]
fn registry_content_folder() -> Option<PathBuf> {
	use std::os::windows::ffi::OsStringExt;
	use windows_sys::Win32::Foundation::ERROR_SUCCESS;
	use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_SZ, RegGetValueW};

	let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
	let key = wide("Software\\Roblox\\RobloxStudio");
	let value = wide("ContentFolder");
	let mut size: u32 = 0;
	// SAFETY: valid NUL-terminated wide strings; a null data pointer queries the size
	let status = unsafe { RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), std::ptr::null_mut(), &mut size) };
	if status != ERROR_SUCCESS || !(2..=64 * 1024).contains(&size) {
		return None;
	}
	let mut buffer = vec![0u16; (size as usize).div_ceil(2)];
	let mut size = (buffer.len() * 2) as u32;
	// SAFETY: `buffer` holds `size` bytes
	let status = unsafe { RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buffer.as_mut_ptr().cast(), &mut size) };
	if status != ERROR_SUCCESS {
		return None;
	}
	let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
	let path = PathBuf::from(std::ffi::OsString::from_wide(&buffer[..len]));
	(!path.as_os_str().is_empty()).then_some(path)
}

#[cfg(not(windows))]
fn registry_content_folder() -> Option<PathBuf> {
	None
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_newest_mac_app_wins() {
		let dir = std::env::temp_dir().join(format!("ror-assets-mac-apps-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		let app = |name: &str, age_days: u64| {
			let executable = dir.join(name).join("RobloxStudio.app").join("Contents").join("MacOS").join("RobloxStudio");
			std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
			let file = std::fs::File::create(&executable).unwrap();
			file.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(age_days * 86400)).unwrap();
			dir.join(name)
		};
		let (launcher, updated, empty) = (app("Applications", 4), app("UnzippedBundle", 2), dir.join("none"));
		assert_eq!(newest_app([launcher.clone(), updated.clone(), empty.clone()]), Some(updated.join("RobloxStudio.app")));
		assert_eq!(newest_app([launcher.clone(), empty.clone()]), Some(launcher.join("RobloxStudio.app")));
		assert_eq!(newest_app([empty]), None);
		let _ = std::fs::remove_dir_all(&dir);
	}

	#[test]
	fn lookup_order() {
		let dir = std::env::temp_dir().join(format!("ror-assets-studio-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		let content = dir.join("content");
		let platform = dir.join("PlatformContent").join("pc");
		let extra = dir.join("ExtraContent");
		for (root, name, data) in [
			(&platform, "textures/studs.dds", "pc"),
			(&content, "textures/studs.dds", "content"),
			(&content, "textures/face.png", "content"),
			(&extra, "textures/face.png", "extra"),
			(&extra, "textures/ui/x.png", "extra"),
		] {
			let path = root.join(name);
			std::fs::create_dir_all(path.parent().unwrap()).unwrap();
			std::fs::write(&path, data).unwrap();
		}

		// Either the version dir or the content dir identifies the install
		for start in [&dir, &content] {
			let install = StudioInstall::from_dir(start).unwrap();
			assert_eq!(install.content, content);
			let read = |p: &str| install.find(p).map(|f| std::fs::read_to_string(f).unwrap());
			assert_eq!(read("textures/studs.dds").as_deref(), Some("pc"));
			assert_eq!(read("textures/face.png").as_deref(), Some("content"));
			assert_eq!(read("textures/ui/x.png").as_deref(), Some("extra"));
			assert_eq!(read("textures/missing.png"), None);
		}
		let _ = std::fs::remove_dir_all(&dir);
	}

	#[test]
	fn detects_this_machine() {
		// Informational: the real install, when present
		if let Some(install) = detect(None) {
			assert!(install.content.is_dir());
			assert!(!install.roots.is_empty());
		}
	}
}
