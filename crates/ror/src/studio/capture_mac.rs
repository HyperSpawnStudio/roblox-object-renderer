//! Lossless capture of a Studio viewport on macOS: the counterpart of `capture.rs` (mounted as
//! `studio::capture` there). Windows are listed and captured by `tools/maccap/maccap.swift`
//! (ScreenCaptureKit, the window's own pixels at its backing scale in sRGB: `screencapture -l`
//! colour-matches into the display's space instead), compiled on first use into
//! `<state dir>/maccap/` with `swiftc` (Xcode or the Command Line Tools; macOS 14 or later). Both need
//! the Screen Recording permission for the app running ror.
//!
//! A Studio window has no child window for its viewport here, so the viewport is found by a marker
//! ([`MARKER_LUAU`], [`locate`]): [`super::CaptureStudio::capture_viewport`] does that once per window
//! and viewport size and crops every capture to it.
//!
//! Sizes are physical pixels like the Windows module's (points times the display's scale), and
//! `dpi` is 96 times that scale, so the callers' arithmetic is the same.

#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};

use crate::output;

/// The capture Studio's window size in physical pixels to start from; [`super::compare`] then
/// corrects it by the viewport's remaining difference (Studio's panels take other widths here).
pub const CAPTURE_WINDOW: (i32, i32) = (2401, 1502);

/// The capture Studio's viewport in physical pixels (offline replays and the capture cache assume
/// it): 822 x 558 points on a 2x display.
pub const CAPTURE_VIEWPORT: (u32, u32) = (1644, 1116);

const SOURCE: &str = include_str!("../../../../tools/maccap/maccap.swift");

/// The marker's colour and how far a captured channel may be from it.
pub const MARKER: [u8; 3] = [255, 0, 255];
const MARKER_TOLERANCE: i32 = 24;

/// Luau that puts the marker over the whole viewport, above everything else under CoreGui, and
/// returns once it has been drawn; [`UNMARK_LUAU`] removes it.
pub const MARKER_LUAU: &str = r#"local CoreGui = game:GetService("CoreGui")
local old = CoreGui:FindFirstChild("__ror_marker")
if old then old:Destroy() end
local gui = Instance.new("ScreenGui")
gui.Name = "__ror_marker"
gui.IgnoreGuiInset = true
gui.DisplayOrder = 2147483647
local fill = Instance.new("Frame")
fill.Size = UDim2.fromScale(1, 1)
fill.BackgroundColor3 = Color3.fromRGB(255, 0, 255)
fill.BorderSizePixel = 0
fill.Parent = gui
gui.Parent = CoreGui
for _ = 1, 6 do game:GetService("RunService").Heartbeat:Wait() end
return 0"#;

pub const UNMARK_LUAU: &str = r#"local old = game:GetService("CoreGui"):FindFirstChild("__ror_marker")
if old then old:Destroy() end
return 0"#;

#[derive(Clone, Debug)]
pub struct WindowInfo {
	/// The window number (CGWindowID).
	pub hwnd: isize,
	pub class: String,
	pub title: String,
	pub x: i32,
	pub y: i32,
	pub width: i32,
	pub height: i32,
	pub visible: bool,
	pub dpi: u32,
	pub pid: u32,
}

#[derive(serde::Deserialize)]
struct Listed {
	id: i64,
	title: String,
	x: f64,
	y: f64,
	width: f64,
	height: f64,
	scale: f64,
	pid: u32,
}

/// The helper, built from [`SOURCE`] on first use (keyed by its hash).
fn helper() -> Result<PathBuf> {
	let dir = output::state_dir().join("maccap");
	let hash = blake3::hash(SOURCE.as_bytes()).to_hex();
	let binary = dir.join(format!("maccap-{}", &hash[..16]));
	if binary.is_file() {
		return Ok(binary);
	}
	std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
	let source = dir.join(format!("maccap-{}.swift", &hash[..16]));
	std::fs::write(&source, SOURCE)?;
	let partial = binary.with_extension("tmp");
	let built = Command::new("swiftc").arg("-O").arg(&source).arg("-o").arg(&partial).output().context("running swiftc (Xcode or the Command Line Tools)")?;
	if !built.status.success() {
		bail!("building the macOS capture helper failed: {}", String::from_utf8_lossy(&built.stderr).trim());
	}
	std::fs::rename(&partial, &binary)?;
	Ok(binary)
}

fn list(filter: &str) -> Result<Vec<WindowInfo>> {
	let output = Command::new(helper()?).arg("windows").arg(filter).output().context("listing Studio's windows")?;
	if !output.status.success() {
		bail!("listing Studio's windows failed: {}", String::from_utf8_lossy(&output.stderr).trim());
	}
	let listed: Vec<Listed> = serde_json::from_slice(&output.stdout).context("the window list")?;
	Ok(listed
		.into_iter()
		.map(|window| WindowInfo {
			hwnd: window.id as isize,
			class: "Roblox Studio".into(),
			title: window.title,
			x: (window.x * window.scale).round() as i32,
			y: (window.y * window.scale).round() as i32,
			width: (window.width * window.scale).round() as i32,
			height: (window.height * window.scale).round() as i32,
			visible: true,
			dpi: (96.0 * window.scale).round() as u32,
			pid: window.pid,
		})
		.collect())
}

/// On-screen Studio document windows whose title contains `title`. A window whose title is
/// unreadable (no Screen Recording permission) matches nothing.
pub fn find_studio_windows(title: &str) -> Vec<WindowInfo> {
	match list(title) {
		Ok(windows) => windows,
		Err(error) => {
			eprintln!("ror: {error:#}");
			Vec::new()
		}
	}
}

/// The window `hwnd` as listed now.
fn window(hwnd: isize) -> Result<WindowInfo> {
	list("")?.into_iter().find(|window| window.hwnd == hwnd).ok_or_else(|| anyhow!("Studio window {hwnd} is not on screen"))
}

/// A minimized window is not on screen, so it is never found: always false.
pub fn is_minimized(_hwnd: isize) -> bool {
	false
}

/// Child windows: a Studio window has none of its own on macOS.
pub fn children(_parent: isize) -> Vec<WindowInfo> {
	Vec::new()
}

/// Resizes a window (physical pixels) through System Events: needs the Accessibility permission for
/// the app running ror.
pub fn resize(hwnd: isize, width: i32, height: i32) -> Result<()> {
	let info = window(hwnd)?;
	let scale = info.dpi as f64 / 96.0;
	let (points_width, points_height) = ((width as f64 / scale).round() as i64, (height as f64 / scale).round() as i64);
	let quoted = info.title.replace('\\', "\\\\").replace('"', "\\\"");
	let script = format!(
		"tell application \"System Events\" to tell (first process whose unix id is {}) to set size of (first window whose name is \"{quoted}\") to {{{points_width}, {points_height}}}",
		info.pid
	);
	let output = Command::new("osascript").arg("-e").arg(&script).output().context("running osascript")?;
	if !output.status.success() {
		bail!(
			"resizing Studio's window to {points_width}x{points_height} points failed ({}): give the app running ror the Accessibility permission, or resize the window by hand",
			String::from_utf8_lossy(&output.stderr).trim()
		);
	}
	Ok(())
}

/// The id of the process that owns a window.
pub fn window_process(hwnd: isize) -> u32 {
	window(hwnd).map(|window| window.pid).unwrap_or(0)
}

/// Ends a process and waits (up to 15 s) for it to exit. Only for the capture Studio, whose place
/// ror regenerates: nothing in it needs saving.
pub fn terminate(pid: u32) -> Result<()> {
	if pid == 0 {
		bail!("no process to end");
	}
	let alive = || Command::new("kill").arg("-0").arg(pid.to_string()).output().is_ok_and(|output| output.status.success());
	let ended = Command::new("kill").arg("-TERM").arg(pid.to_string()).status().context("running kill")?;
	if !ended.success() {
		bail!("kill -TERM {pid} failed");
	}
	for _ in 0..150 {
		if !alive() {
			return Ok(());
		}
		std::thread::sleep(std::time::Duration::from_millis(100));
	}
	let _ = Command::new("kill").arg("-KILL").arg(pid.to_string()).status();
	Ok(())
}

/// Captures a window's content (physical pixels) as RGBA8.
pub fn capture(hwnd: isize) -> Result<(u32, u32, Vec<u8>)> {
	let file = output::state_dir().join("maccap").join(format!("capture-{}.png", std::process::id()));
	let _ = std::fs::remove_file(&file);
	let result = Command::new(helper()?).arg("capture").arg(hwnd.to_string()).arg(&file).output().context("capturing Studio's window")?;
	if !result.status.success() {
		bail!("capturing Studio's window failed: {}", String::from_utf8_lossy(&result.stderr).trim());
	}
	let decoded = output::decode_png(&file);
	let _ = std::fs::remove_file(&file);
	let (width, height, mut rgba) = decoded?;
	for alpha in rgba.iter_mut().skip(3).step_by(4) {
		*alpha = 255;
	}
	Ok((width, height, rgba))
}

/// A rectangle in a window capture (physical pixels).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
	pub x: u32,
	pub y: u32,
	pub width: u32,
	pub height: u32,
}

fn is_marker(pixel: &[u8]) -> bool {
	(0..3).all(|channel| (pixel[channel] as i32 - MARKER[channel] as i32).abs() <= MARKER_TOLERANCE)
}

/// The viewport in a window capture taken while [`MARKER_LUAU`] is up: the marker's bounding box,
/// which must be `size` (±4 px: an outline Studio draws over the viewport's edge pixels shortens it,
/// and the box is then centred on the marker).
pub fn locate(width: u32, height: u32, rgba: &[u8], size: (u32, u32)) -> Result<Rect> {
	let (mut left, mut top, mut right, mut bottom, mut count) = (u32::MAX, u32::MAX, 0u32, 0u32, 0u64);
	for y in 0..height {
		for x in 0..width {
			let index = ((y * width + x) * 4) as usize;
			if is_marker(&rgba[index..index + 4]) {
				count += 1;
				left = left.min(x);
				right = right.max(x);
				top = top.min(y);
				bottom = bottom.max(y);
			}
		}
	}
	if count == 0 {
		bail!("the viewport marker is not in the capture Studio's window (is its viewport visible, in Edit mode?)");
	}
	let (box_width, box_height) = (right - left + 1, bottom - top + 1);
	if (box_width as i64 - size.0 as i64).abs() > 4 || (box_height as i64 - size.1 as i64).abs() > 4 {
		bail!("the viewport marker is {box_width}x{box_height} px at {left},{top}, not the viewport's {}x{}", size.0, size.1);
	}
	if count < (box_width as u64 * box_height as u64) / 2 {
		bail!("the viewport marker covers {count} of the {} px in its box: something else in the window has its colour", box_width as u64 * box_height as u64);
	}
	let x = (left as i64 - (size.0 as i64 - box_width as i64) / 2).clamp(0, width.saturating_sub(size.0) as i64) as u32;
	let y = (top as i64 - (size.1 as i64 - box_height as i64) / 2).clamp(0, height.saturating_sub(size.1) as i64) as u32;
	Ok(Rect { x, y, width: size.0, height: size.1 })
}

/// How many pixels of `rect` are not the marker's: what Studio draws over its viewport there.
pub fn covered(width: u32, rgba: &[u8], rect: Rect) -> u64 {
	let mut count = 0;
	for y in rect.y..rect.y + rect.height {
		for x in rect.x..rect.x + rect.width {
			let index = ((y * width + x) * 4) as usize;
			if !is_marker(&rgba[index..index + 4]) {
				count += 1;
			}
		}
	}
	count
}

/// `rect` of an RGBA8 image `width` wide.
pub fn crop(width: u32, rgba: &[u8], rect: Rect) -> Vec<u8> {
	let mut out = Vec::with_capacity((rect.width * rect.height * 4) as usize);
	for y in rect.y..rect.y + rect.height {
		let start = ((y * width + rect.x) * 4) as usize;
		out.extend_from_slice(&rgba[start..start + (rect.width * 4) as usize]);
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	fn image(width: u32, height: u32, fill: impl Fn(u32, u32) -> [u8; 3]) -> Vec<u8> {
		let mut rgba = Vec::with_capacity((width * height * 4) as usize);
		for y in 0..height {
			for x in 0..width {
				let [r, g, b] = fill(x, y);
				rgba.extend_from_slice(&[r, g, b, 255]);
			}
		}
		rgba
	}

	const CHROME: [u8; 3] = [40, 40, 44];

	fn inside(x: u32, y: u32, rect: Rect) -> bool {
		x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
	}

	#[test]
	fn the_viewport_is_the_markers_box() {
		let viewport = Rect { x: 60, y: 90, width: 400, height: 300 };
		let rgba = image(600, 450, |x, y| if inside(x, y, viewport) { [251, 3, 250] } else { CHROME });
		assert_eq!(locate(600, 450, &rgba, (400, 300)).unwrap(), viewport);
		assert_eq!(covered(600, &rgba, viewport), 0);
		let error = locate(600, 450, &rgba, (200, 150)).unwrap_err().to_string();
		assert!(error.contains("400x300 px at 60,90"), "{error}");
	}

	#[test]
	fn an_outline_over_the_edge_is_centred_back() {
		let viewport = Rect { x: 20, y: 30, width: 300, height: 200 };
		let outline = Rect { x: 22, y: 32, width: 296, height: 196 };
		let rgba = image(400, 300, |x, y| if inside(x, y, outline) { MARKER } else if inside(x, y, viewport) { [0, 120, 215] } else { CHROME });
		assert_eq!(locate(400, 300, &rgba, (300, 200)).unwrap(), viewport);
	}

	#[test]
	fn an_overlay_inside_is_counted() {
		let viewport = Rect { x: 10, y: 10, width: 400, height: 250 };
		let selector = Rect { x: 350, y: 20, width: 30, height: 30 };
		let rgba = image(420, 270, |x, y| if inside(x, y, selector) { [200, 200, 200] } else if inside(x, y, viewport) { MARKER } else { CHROME });
		assert_eq!(locate(420, 270, &rgba, (400, 250)).unwrap(), viewport);
		assert_eq!(covered(420, &rgba, viewport), 900);
	}

	#[test]
	fn no_marker_or_a_scattered_one_is_an_error() {
		assert!(locate(50, 50, &image(50, 50, |_, _| CHROME), (20, 20)).is_err());
		let rgba = image(200, 200, |x, y| if (x < 10 && y < 10) || (x > 189 && y > 189) { MARKER } else { CHROME });
		let error = locate(200, 200, &rgba, (200, 200)).unwrap_err().to_string();
		assert!(error.contains("covers 200 of the 40000 px"), "{error}");
	}

	#[test]
	fn crop_keeps_rows_and_columns() {
		let rgba = image(30, 20, |x, y| [x as u8, y as u8, 7]);
		let out = crop(30, &rgba, Rect { x: 5, y: 4, width: 10, height: 6 });
		assert_eq!(out.len(), 10 * 6 * 4);
		assert_eq!(&out[..4], &[5, 4, 7, 255]);
		assert_eq!(&out[out.len() - 4..], &[14, 9, 7, 255]);
	}
}
