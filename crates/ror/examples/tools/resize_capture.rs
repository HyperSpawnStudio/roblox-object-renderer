// Resizes the capture Studio's window: cargo run -p ror --example tools -- resize_capture <width> <height> (physical px)
#[cfg(windows)]
pub fn main() -> anyhow::Result<()> {
	let args: Vec<i32> = std::env::args().skip(1).map(|a| a.parse().unwrap()).collect();
	let mut studio = ror::studio::CaptureStudio::connect()?;
	let window = studio.window()?;
	ror::studio::capture::resize(window.hwnd, args[0], args[1])?;
	std::thread::sleep(std::time::Duration::from_millis(800));
	let viewport = studio.viewport()?;
	let window = studio.window()?;
	println!("window {}x{} viewport {}x{} logical", window.width, window.height, viewport.0, viewport.1);
	Ok(())
}

// The Studio calibration and capture modules are Windows-only (src/studio/mod.rs)
#[cfg(not(windows))]
pub fn main() -> anyhow::Result<()> {
	anyhow::bail!("resize_capture needs Windows: it uses the capture Studio's window (ror::studio::capture)")
}
