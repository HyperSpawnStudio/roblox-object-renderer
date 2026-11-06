//! Studio parity harness: the dedicated capture Studio, model upload, viewport capture (Windows, and
//! macOS through `capture_mac.rs`; calibration is Windows-only).
//!
//! Captures only ever run in the Studio that has ror's own capture place open
//! ([`place::place_name`]); every other Studio is left alone. One process at a time drives a
//! capture Studio: [`CaptureStudio`] holds an exclusive lock on `<state dir>/studio/<place>.lock`
//! (other agents wait for it).

#[cfg(windows)]
pub mod calibrate;
#[cfg(any(windows, target_os = "macos"))]
#[cfg_attr(target_os = "macos", path = "capture_mac.rs")]
pub mod capture;
pub mod compare;
pub mod export;
pub mod mcp;
pub mod place;

use anyhow::{Context, Result, anyhow, bail};

use mcp::Mcp;

/// The Studio running ror's capture place.
pub struct CaptureStudio {
	pub mcp: Mcp,
	pub id: String,
	pub name: String,
	/// Held while this value lives: the capture Studio serves one process at a time.
	_lock: std::fs::File,
	/// macOS: where the viewport is in the window's captures, found by a marker ([`capture::locate`]).
	#[cfg(target_os = "macos")]
	located: Option<(ViewportKey, capture::Rect)>,
}

/// macOS: what a located viewport holds for: the window id, the window's size and the viewport's
/// (physical px).
#[cfg(target_os = "macos")]
type ViewportKey = (isize, i32, i32, u32, u32);

impl CaptureStudio {
	pub fn connect() -> Result<Self> {
		let lock = lock_capture_studio()?;
		let mut mcp = Mcp::connect()?;
		let studio = mcp.find_studio(&place::place_file_name()).context("the capture Studio is not open (run `ror studio open`)")?;
		Ok(Self {
			mcp,
			id: studio.id,
			name: studio.name.unwrap_or_default(),
			_lock: lock,
			#[cfg(target_os = "macos")]
			located: None,
		})
	}

	pub fn luau(&mut self, code: &str) -> Result<String> {
		self.mcp.run_luau(&self.id, code)
	}

	/// The viewport's logical size (`Camera.ViewportSize`).
	pub fn viewport(&mut self) -> Result<(f32, f32)> {
		let text = self.luau("local v = workspace.CurrentCamera.ViewportSize return tostring(v.X) .. ',' .. tostring(v.Y)")?;
		let (x, y) = text.trim().split_once(',').context("viewport size")?;
		Ok((x.trim().parse()?, y.trim().parse()?))
	}

	#[cfg(any(windows, target_os = "macos"))]
	pub fn window(&self) -> Result<capture::WindowInfo> {
		let title = place::place_file_name();
		let mut windows = capture::find_studio_windows(&title);
		match windows.len() {
			0 => bail!("no visible Studio window with {title} in its title"),
			1 => Ok(windows.remove(0)),
			count => bail!("{count} Studio windows have {title} in their title"),
		}
	}

	/// Captures the 3D viewport (physical pixels).
	#[cfg(windows)]
	pub fn capture_viewport(&mut self) -> Result<(u32, u32, Vec<u8>)> {
		let window = self.window()?;
		if capture::is_minimized(window.hwnd) {
			bail!("the capture Studio is minimized; restore its window");
		}
		let (width, height) = self.viewport()?;
		let scale = window.dpi as f32 / 96.0;
		let (physical_width, physical_height) = ((width * scale).round() as i32, (height * scale).round() as i32);
		let viewport = capture::find_viewport(window.hwnd, physical_width, physical_height).ok_or_else(|| {
			let sizes: Vec<String> = capture::children(window.hwnd).iter().filter(|child| child.visible && child.width >= 200 && child.height >= 200).map(|child| format!("{}x{} {}", child.width, child.height, child.class)).collect();
			anyhow!("no viewport child of {physical_width}x{physical_height} (children: {})", sizes.join(", "))
		})?;
		capture::capture(viewport.hwnd)
	}

	/// Captures the 3D viewport (physical pixels): the window captured and cropped to where a marker
	/// showed the viewport, found again whenever the window or the viewport changes size.
	#[cfg(target_os = "macos")]
	pub fn capture_viewport(&mut self) -> Result<(u32, u32, Vec<u8>)> {
		let window = self.window()?;
		let (width, height) = self.viewport()?;
		let scale = window.dpi as f32 / 96.0;
		let size = ((width * scale).round() as u32, (height * scale).round() as u32);
		let key = (window.hwnd, window.width, window.height, size.0, size.1);
		let rect = match self.located {
			Some((located, rect)) if located == key => rect,
			_ => {
				let rect = self.locate_viewport(window.hwnd, size)?;
				self.located = Some((key, rect));
				rect
			}
		};
		let (captured_width, captured_height, rgba) = capture::capture(window.hwnd)?;
		if rect.x + rect.width > captured_width || rect.y + rect.height > captured_height {
			self.located = None;
			bail!("the capture Studio's window changed size during the capture ({captured_width}x{captured_height} px)");
		}
		Ok((rect.width, rect.height, capture::crop(captured_width, &rgba, rect)))
	}

	/// The viewport's rectangle in captures of `window`: a marker over the whole viewport, a capture,
	/// the marker removed again.
	#[cfg(target_os = "macos")]
	fn locate_viewport(&mut self, window: isize, size: (u32, u32)) -> Result<capture::Rect> {
		self.luau(compare::HIDE_OVERLAYS)?;
		// removed also after a marker that failed part way: it must never stay over the viewport
		let captured = self.luau(capture::MARKER_LUAU).context("putting up the viewport marker").and_then(|_| capture::capture(window));
		self.luau(capture::UNMARK_LUAU).context("removing the viewport marker")?;
		let (width, height, rgba) = captured?;
		let rect = capture::locate(width, height, &rgba, size)?;
		let covered = capture::covered(width, &rgba, rect);
		if covered > 0 {
			eprintln!("ror: Studio draws over {covered} px of the capture Studio's viewport (a toolbar or panel over it): those pixels will differ");
		}
		Ok(rect)
	}
}

/// Waits for exclusive use of the capture Studio (a lock file per capture place).
fn lock_capture_studio() -> Result<std::fs::File> {
	let dir = crate::output::state_dir().join("studio");
	std::fs::create_dir_all(&dir)?;
	let path = dir.join(format!("{}.lock", place::place_name()));
	let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path).with_context(|| format!("opening {}", path.display()))?;
	if file.try_lock().is_err() {
		eprintln!("ror: waiting for the capture Studio ({} is held by another process)", path.display());
		file.lock().with_context(|| format!("locking {}", path.display()))?;
	}
	Ok(file)
}

/// macOS: Studio's executable in /Applications, else ~/Applications.
#[cfg(target_os = "macos")]
pub fn studio_executable() -> Option<std::path::PathBuf> {
	let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
	[Some(std::path::PathBuf::from("/Applications")), home.map(|home| home.join("Applications"))]
		.into_iter()
		.flatten()
		.map(|dir| dir.join("RobloxStudio.app").join("Contents").join("MacOS").join("RobloxStudio"))
		.find(|path| path.is_file())
}

/// RobloxStudioBeta.exe of the newest installed Studio version.
#[cfg(not(target_os = "macos"))]
pub fn studio_executable() -> Option<std::path::PathBuf> {
	let versions = std::path::Path::new(&std::env::var_os("LOCALAPPDATA")?).join("Roblox").join("Versions");
	let mut candidates: Vec<(std::time::SystemTime, std::path::PathBuf)> = std::fs::read_dir(versions)
		.ok()?
		.filter_map(|entry| entry.ok())
		.map(|entry| entry.path().join("RobloxStudioBeta.exe"))
		.filter(|path| path.is_file())
		.map(|path| (std::fs::metadata(&path).and_then(|meta| meta.modified()).unwrap_or(std::time::UNIX_EPOCH), path))
		.collect();
	candidates.sort();
	candidates.pop().map(|(_, path)| path)
}
