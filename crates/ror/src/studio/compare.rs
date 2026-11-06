//! `ror compare`: the same model, lighting and camera in the capture Studio and in ror, captured /
//! rendered at the viewport's physical size, and compared pixel by pixel.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use glam::Vec3;
use serde::Serialize;

use super::CaptureStudio;
use crate::output;
use crate::render::{Background, Renderer, ViewRequest};
use crate::scene::environment::PostEffect;
use crate::scene::{self, Environment};

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PixelMetrics {
	/// Share of pixels whose largest channel difference is at most 2 (of 255).
	pub exact_percent: f64,
	pub within8_percent: f64,
	pub within32_percent: f64,
	pub mean_abs_error: f64,
	pub psnr: f64,
	/// Mean signed difference per channel (ror - Studio), for bias checks.
	pub mean_signed: [f64; 3],
}

pub fn metrics(a: &[u8], b: &[u8]) -> PixelMetrics {
	let pixels = (a.len() / 4).max(1);
	let (mut exact, mut within8, mut within32) = (0usize, 0usize, 0usize);
	let mut abs_sum = 0f64;
	let mut squared = 0f64;
	let mut signed = [0f64; 3];
	for (pa, pb) in a.chunks_exact(4).zip(b.chunks_exact(4)) {
		let mut largest = 0i32;
		for channel in 0..3 {
			let difference = pa[channel] as i32 - pb[channel] as i32;
			largest = largest.max(difference.abs());
			abs_sum += difference.abs() as f64;
			squared += (difference * difference) as f64;
			signed[channel] += difference as f64;
		}
		exact += (largest <= 2) as usize;
		within8 += (largest <= 8) as usize;
		within32 += (largest <= 32) as usize;
	}
	let mse = squared / (pixels * 3) as f64;
	PixelMetrics {
		exact_percent: exact as f64 * 100.0 / pixels as f64,
		within8_percent: within8 as f64 * 100.0 / pixels as f64,
		within32_percent: within32 as f64 * 100.0 / pixels as f64,
		mean_abs_error: abs_sum / (pixels * 3) as f64,
		psnr: if mse > 0.0 { 10.0 * (255.0 * 255.0 / mse).log10() } else { f64::INFINITY },
		mean_signed: signed.map(|value| value / pixels as f64),
	}
}

/// |a - b| amplified 4x, and a side-by-side strip (Studio | ror | diff).
fn diff_images(width: u32, height: u32, studio: &[u8], ror: &[u8]) -> (Vec<u8>, Vec<u8>) {
	let mut diff = Vec::with_capacity(studio.len());
	for (pa, pb) in studio.chunks_exact(4).zip(ror.chunks_exact(4)) {
		for channel in 0..3 {
			diff.push(((pa[channel] as i32 - pb[channel] as i32).unsigned_abs() * 4).min(255) as u8);
		}
		diff.push(255);
	}
	let mut strip = Vec::with_capacity(studio.len() * 3);
	for row in 0..height as usize {
		let range = row * width as usize * 4..(row + 1) * width as usize * 4;
		strip.extend_from_slice(&studio[range.clone()]);
		strip.extend_from_slice(&ror[range.clone()]);
		strip.extend_from_slice(&diff[range]);
	}
	(diff, strip)
}

fn color3(value: Vec3) -> String {
	format!("Color3.new({}, {}, {})", value.x, value.y, value.z)
}

/// Luau that makes the capture place's Lighting match an environment: its properties, and only
/// the Sky / Atmosphere / post effects the environment has.
pub fn lighting_script(environment: &Environment) -> String {
	let mut lines = vec![
		"local L = game:GetService(\"Lighting\")".to_string(),
		"for _, child in L:GetChildren() do child:Destroy() end".to_string(),
		format!("L.LightingStyle = Enum.LightingStyle:FromValue({})", environment.lighting_style),
		format!("L.PrioritizeLightingQuality = {}", environment.prioritize_lighting_quality),
		format!("L.Ambient = {}", color3(environment.ambient)),
		format!("L.OutdoorAmbient = {}", color3(environment.outdoor_ambient)),
		format!("L.Brightness = {}", environment.brightness),
		format!("L.ClockTime = {}", environment.clock_time),
		format!("L.GeographicLatitude = {}", environment.geographic_latitude),
		format!("L.ColorShift_Top = {}", color3(environment.color_shift_top)),
		format!("L.ColorShift_Bottom = {}", color3(environment.color_shift_bottom)),
		format!("L.EnvironmentDiffuseScale = {}", environment.environment_diffuse_scale),
		format!("L.EnvironmentSpecularScale = {}", environment.environment_specular_scale),
		format!("L.ExposureCompensation = {}", environment.exposure_compensation),
		format!("L.ShadowSoftness = {}", environment.shadow_softness),
		format!("L.GlobalShadows = {}", environment.global_shadows),
		format!("L.FogColor = {}", color3(environment.fog_color)),
		format!("L.FogStart = {}", environment.fog_start),
		format!("L.FogEnd = {}", environment.fog_end),
	];
	if let Some(sky) = &environment.sky {
		lines.push("local sky = Instance.new(\"Sky\")".into());
		let names = ["SkyboxBk", "SkyboxDn", "SkyboxFt", "SkyboxLf", "SkyboxRt", "SkyboxUp"];
		for (name, uri) in names.iter().zip(&sky.skybox) {
			lines.push(format!("sky.{name} = {uri:?}"));
		}
		lines.push(format!(
			"sky.SkyboxOrientation = Vector3.new({}, {}, {})",
			sky.skybox_orientation.x, sky.skybox_orientation.y, sky.skybox_orientation.z
		));
		lines.push(format!("sky.CelestialBodiesShown = {}", sky.celestial_bodies_shown));
		lines.push(format!("sky.SunTextureId = {:?}", sky.sun_texture));
		lines.push(format!("sky.SunAngularSize = {}", sky.sun_angular_size));
		lines.push(format!("sky.MoonTextureId = {:?}", sky.moon_texture));
		lines.push(format!("sky.MoonAngularSize = {}", sky.moon_angular_size));
		lines.push(format!("sky.StarCount = {}", sky.star_count));
		lines.push("sky.Parent = L".into());
	}
	if let Some(atmosphere) = &environment.atmosphere {
		lines.push(format!(
			"local a = Instance.new(\"Atmosphere\") a.Density = {} a.Offset = {} a.Color = {} a.Decay = {} a.Glare = {} a.Haze = {} a.Parent = L",
			atmosphere.density,
			atmosphere.offset,
			color3(atmosphere.color),
			color3(atmosphere.decay),
			atmosphere.glare,
			atmosphere.haze
		));
	}
	for effect in &environment.effects {
		lines.push(match effect {
			PostEffect::Bloom { intensity, size, threshold } => format!("local e = Instance.new(\"BloomEffect\") e.Intensity = {intensity} e.Size = {size} e.Threshold = {threshold} e.Parent = L"),
			PostEffect::ColorCorrection { brightness, contrast, saturation, tint } => format!(
				"local e = Instance.new(\"ColorCorrectionEffect\") e.Brightness = {brightness} e.Contrast = {contrast} e.Saturation = {saturation} e.TintColor = {} e.Parent = L",
				color3(*tint)
			),
			PostEffect::ColorGrading { tonemapper } => format!("local e = Instance.new(\"ColorGradingEffect\") e.TonemapperPreset = Enum.TonemapperPreset:FromValue({tonemapper}) e.Parent = L"),
			PostEffect::SunRays { intensity, spread } => format!("local e = Instance.new(\"SunRaysEffect\") e.Intensity = {intensity} e.Spread = {spread} e.Parent = L"),
			PostEffect::DepthOfField { far_intensity, focus_distance, in_focus_radius, near_intensity } => format!(
				"local e = Instance.new(\"DepthOfFieldEffect\") e.FarIntensity = {far_intensity} e.FocusDistance = {focus_distance} e.InFocusRadius = {in_focus_radius} e.NearIntensity = {near_intensity} e.Parent = L"
			),
			PostEffect::Blur { size } => format!("local e = Instance.new(\"BlurEffect\") e.Size = {size} e.Parent = L"),
		});
	}
	if let Some(clouds) = &environment.clouds {
		lines.push(format!(
			"local c = Instance.new(\"Clouds\") c.Cover = {} c.Density = {} c.Color = {} c.Parent = workspace.Terrain",
			clouds.cover,
			clouds.density,
			color3(clouds.color)
		));
	}
	lines.join("\n")
}

/// Luau that removes the Terrain's Clouds before [`lighting_script`] runs (they live outside the
/// Lighting it clears); not part of the capture key, so captures without clouds keep theirs.
const CLEAR_CLOUDS: &str = "for _, child in workspace.Terrain:GetChildren() do if child:IsA(\"Clouds\") then child:Destroy() end end";

/// Luau that hides Studio's viewport overlays in the capture Studio (view selector, plugin toasts)
/// and clears the selection.
pub const HIDE_OVERLAYS: &str = r#"local CoreGui = game:GetService("CoreGui")
for _, name in {"ViewSelectorScreenGui", "RojoNotifications"} do
	local gui = CoreGui:FindFirstChild(name)
	if gui then gui.Enabled = false end
end
game:GetService("Selection"):Set({})"#;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewReport {
	pub name: String,
	pub width: u32,
	pub height: u32,
	pub camera: [f32; 12],
	pub fov: f32,
	pub pixels: PixelMetrics,
	/// With levels of detail (`lod`): ror's side at full detail, every part drawn (ror-full.png), against
	/// the same capture
	#[serde(skip_serializing_if = "Option::is_none")]
	pub full_detail: Option<PixelMetrics>,
	/// With levels of detail: what ror's side drew at them
	#[serde(skip_serializing_if = "Option::is_none")]
	pub lod: Option<crate::render::LodView>,
	pub capture_attempts: u32,
	pub studio_ms: f64,
	pub ror_ms: f64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompareReport {
	pub input: String,
	pub out: PathBuf,
	pub studio: String,
	pub views: Vec<ViewReport>,
	pub missing: Vec<String>,
}

pub struct CompareOptions {
	pub input: PathBuf,
	pub select: Option<String>,
	pub views: Vec<crate::job::NamedView>,
	pub out: PathBuf,
	/// Replaces the scene's environment (e.g. a calibration environment).
	pub environment: Option<Environment>,
	/// Both sides as a game draws them at a graphics quality: Studio's MeshPartDetailLevel
	/// DistanceBased and edit quality while it captures ([`game_lod_on`]), ror's levels of detail
	/// (crate::lod); ror's side at full detail is compared too.
	pub lod: Option<crate::lod::Lod>,
}

/// Luau that has the capture Studio draw as a game does at a graphics quality: its MeshPartDetailLevel
/// DistanceBased (Studio's default, Level00, draws every mesh at full detail) and its edit quality,
/// the values before kept in `_G` until [`GAME_LOD_OFF`] puts them back (Studio keeps these settings
/// for every place it opens).
pub fn game_lod_on(quality: u32) -> String {
	format!(
		"local r = settings().Rendering
_G.RorDetail = _G.RorDetail or r.MeshPartDetailLevel
_G.RorQuality = _G.RorQuality or r.EditQualityLevel
r.MeshPartDetailLevel = Enum.MeshPartDetailLevel.DistanceBased
r.EditQualityLevel = {quality}
return tostring(r.MeshPartDetailLevel) .. \" \" .. tostring(r.EditQualityLevel)"
	)
}

/// Puts back the capture Studio's settings [`game_lod_on`] changed.
pub const GAME_LOD_OFF: &str = "local r = settings().Rendering
if _G.RorDetail then r.MeshPartDetailLevel = _G.RorDetail end
if _G.RorQuality then r.EditQualityLevel = _G.RorQuality end
_G.RorDetail, _G.RorQuality = nil, nil
return tostring(r.MeshPartDetailLevel) .. \" \" .. tostring(r.EditQualityLevel)";

/// The binary model of the instances a document draws (for SerializationService).
pub fn model_bytes(document: &scene::load::Document) -> Result<Vec<u8>> {
	let mut bytes = Vec::new();
	rbx_binary::to_writer(&mut bytes, &document.dom, &document.roots).map_err(|error| anyhow!("re-encoding the model: {error}"))?;
	Ok(bytes)
}

/// The document's MaterialVariants (its MaterialService's, else any in it) as a binary model, and
/// the base material overrides (`MaterialService.<Material>Name`) as (property, variant) pairs; None
/// when it has neither.
pub fn material_bytes(document: &scene::load::Document, materials: &scene::Materials) -> Result<Option<(Vec<u8>, Vec<(String, String)>)>> {
	let start = document.material_service.unwrap_or_else(|| document.dom.root_ref());
	let mut variants = Vec::new();
	let mut stack = vec![start];
	while let Some(referent) = stack.pop() {
		let Some(instance) = document.dom.get_by_ref(referent) else {
			continue;
		};
		if instance.class == "MaterialVariant" {
			variants.push(referent);
			continue;
		}
		stack.extend(instance.children().iter().rev().copied());
	}
	let names = crate::scene::props::database().enums.get("Material").map(|descriptor| &descriptor.items);
	let overrides: Vec<(String, String)> = materials
		.overrides
		.iter()
		.filter_map(|(value, variant)| names?.iter().find(|(_, item)| **item == *value).map(|(name, _)| (format!("{name}Name"), variant.clone())))
		.collect();
	if variants.is_empty() && overrides.is_empty() {
		return Ok(None);
	}
	let mut bytes = Vec::new();
	rbx_binary::to_writer(&mut bytes, &document.dom, &variants).map_err(|error| anyhow!("encoding the MaterialVariants: {error}"))?;
	Ok(Some((bytes, overrides)))
}

/// A model loaded into the capture Studio, captured under any lighting and camera.
#[cfg(any(windows, target_os = "macos"))]
pub struct CaptureSession<'a> {
	/// None: offline, captures only come from the cache ([`offline_studio`])
	pub studio: Option<&'a mut CaptureStudio>,
	pub width: u32,
	pub height: u32,
	/// Hash of the model, lighting and Studio: with the camera and size, the key of a capture
	model_key: u64,
	lighting_key: u64,
	/// Reuse captures under `<state dir>/captures` (off: always capture)
	pub reuse: bool,
	pub reused: u32,
	/// The capture place's material generation (its `RorMaterials` attribute): ror's side of a
	/// comparison draws the same one.
	pub materials_2022: bool,
}

fn fnv(bytes: &[u8], seed: u64) -> u64 {
	let mut hash = seed ^ 0xcbf29ce484222325;
	for &byte in bytes {
		hash ^= byte as u64;
		hash = hash.wrapping_mul(0x100000001b3);
	}
	hash
}

/// How long a newly loaded model gets for its textures to reach the image before the first capture.
const MODEL_SETTLE_MS: u64 = 3000;

/// `ROR_CAPTURE_OFFLINE=<Studio version>|<materials>` (e.g. `0.741.19.7411056|2022`): compare and
/// calibrate replay the cached captures of that Studio and material generation without a Studio
/// (a capture that is not cached is an error).
pub fn offline_studio() -> Option<String> {
	std::env::var("ROR_CAPTURE_OFFLINE").ok().filter(|identity| identity.contains('|'))
}

#[cfg(any(windows, target_os = "macos"))]
impl<'a> CaptureSession<'a> {
	/// A session over the capture cache alone, for the Studio and materials `identity` names
	/// ([`offline_studio`]) at the capture Studio's viewport size.
	pub fn offline(model: &[u8], identity: &str) -> Self {
		let model_key = fnv(model, fnv(identity.as_bytes(), 0));
		let materials_2022 = identity.rsplit('|').next().is_none_or(|materials| materials.trim() != "pre2022");
		Self { studio: None, width: 1644, height: 1116, model_key, lighting_key: 0, reuse: true, reused: 0, materials_2022 }
	}

	/// Uploads `model` (binary rbxm) into the capture place's `Workspace.RorCapture`.
	pub fn start(studio: &'a mut CaptureStudio, model: &[u8]) -> Result<Self> {
		Self::start_settled(studio, model, MODEL_SETTLE_MS)
	}

	/// [`CaptureSession::start`], giving the model's textures `settle_ms` to reach the image (runs that
	/// play their effect once uncaptured first need less).
	pub fn start_settled(studio: &'a mut CaptureStudio, model: &[u8], settle_ms: u64) -> Result<Self> {
		let (width, height) = Self::size_window(studio)?;
		let id = studio.id.clone();
		studio.mcp.upload(&id, "__ror_model", model)?;
		studio
			.luau(&format!(
				"{HIDE_OVERLAYS}
local old = workspace:FindFirstChild(\"RorCapture\") if old then old:Destroy() end
local folder = Instance.new(\"Folder\")
folder.Name = \"RorCapture\"
for _, instance in game:GetService(\"SerializationService\"):DeserializeInstancesAsync(_G.__ror_model) do instance.Parent = folder end
folder.Parent = workspace
game:GetService(\"ContentProvider\"):PreloadAsync({{folder}})
return #folder:GetDescendants()"
			))
			.context("loading the model into the capture Studio")?;
		// SurfaceAppearance maps reach the image well after PreloadAsync returns: frames settle with
		// the defaults first (sa-metal-ids: maps drawn at metalness 0 in one capture, metallic on a
		// recapture). Wait for the request queue to empty, then give the textures time to apply.
		studio
			.luau(
				"local provider = game:GetService(\"ContentProvider\")
local start = os.clock()
while provider.RequestQueueSize > 0 and os.clock() - start < 20 do task.wait(0.1) end
return provider.RequestQueueSize",
			)
			.context("waiting for the capture Studio's assets")?;
		std::thread::sleep(std::time::Duration::from_millis(settle_ms));
		Ok(Self::over(studio, model, width, height))
	}

	/// A session over what the capture Studio holds already (`model`: its bytes, for the capture key),
	/// its window sized for captures.
	pub fn attach(studio: &'a mut CaptureStudio, model: &[u8]) -> Result<Self> {
		let (width, height) = Self::size_window(studio)?;
		Ok(Self::over(studio, model, width, height))
	}

	fn over(studio: &'a mut CaptureStudio, model: &[u8], width: u32, height: u32) -> Self {
		let version = studio.luau("return version()").unwrap_or_default();
		// the material generation changes how textured materials look
		let materials = studio.luau(&format!("return tostring(workspace:GetAttribute(\"{}\"))", super::place::MATERIALS_ATTRIBUTE)).unwrap_or_default();
		let model_key = fnv(model, fnv(format!("{version}|{materials}").as_bytes(), 0));
		let materials_2022 = materials.trim() != "pre2022";
		Self { studio: Some(studio), width, height, model_key, lighting_key: 0, reuse: std::env::var("ROR_RECAPTURE").is_err(), reused: 0, materials_2022 }
	}

	/// Puts the capture Studio's window back to the capture viewport; the viewport's size in physical
	/// pixels.
	fn size_window(studio: &mut CaptureStudio) -> Result<(u32, u32)> {
		let mut window = studio.window()?;
		let (mut logical_width, mut logical_height) = studio.viewport()?;
		// the capture Studio is ror's own: a window someone maximized or resized goes back to the
		// capture viewport (offline replays and the capture cache assume it). Its window size first,
		// then by the viewport's remaining difference (a maximized window's docked panels keep the
		// widths they took)
		let (target_width, target_height) = super::capture::CAPTURE_VIEWPORT;
		for attempt in 0..4 {
			let scale = window.dpi as f32 / 96.0;
			let (width, height) = ((logical_width * scale).round() as i32, (logical_height * scale).round() as i32);
			if (width, height) == (target_width as i32, target_height as i32) {
				break;
			}
			let (window_width, window_height) = if attempt == 0 && (window.width, window.height) != super::capture::CAPTURE_WINDOW {
				super::capture::CAPTURE_WINDOW
			} else {
				(window.width + target_width as i32 - width, window.height + target_height as i32 - height)
			};
			super::capture::resize(window.hwnd, window_width, window_height)?;
			std::thread::sleep(std::time::Duration::from_millis(1500));
			window = studio.window()?;
			(logical_width, logical_height) = studio.viewport()?;
			eprintln!("ror: resized the capture Studio's window to {window_width}x{window_height} (viewport {logical_width}x{logical_height} logical)");
		}
		if (logical_width - logical_width.round()).abs() > 0.01 || (logical_height - logical_height.round()).abs() > 0.01 {
			bail!("the capture Studio's viewport is {logical_width}x{logical_height} logical px; resize its window to whole logical pixels (captures blur otherwise)");
		}
		let scale = window.dpi as f32 / 96.0;
		Ok(((logical_width * scale).round() as u32, (logical_height * scale).round() as u32))
	}

	/// Puts the compared document's MaterialVariants and base material overrides ([`material_bytes`])
	/// into the capture place's MaterialService, in place of an earlier comparison's: Studio draws a
	/// part's MaterialVariant only from there (a part with MaterialVariant "CircleGrid" captured
	/// as plain Plastic without it). Part of the capture key when there are any. Before
	/// [`CaptureSession::set_environment`].
	pub fn set_materials(&mut self, materials: Option<&(Vec<u8>, Vec<(String, String)>)>) -> Result<()> {
		if let Some(studio) = self.studio.as_mut() {
			if let Some((bytes, _)) = materials {
				let id = studio.id.clone();
				studio.mcp.upload(&id, "__ror_materials", bytes)?;
			}
			let overrides: String = materials.map(|(_, overrides)| overrides.iter().map(|(property, variant)| format!("service[{property:?}] = {variant:?}\n")).collect()).unwrap_or_default();
			studio
				.luau(&format!(
					"local service = game:GetService(\"MaterialService\")
for _, child in service:GetChildren() do if child:GetAttribute(\"RorCapture\") then child:Destroy() end end
for _, item in Enum.Material:GetEnumItems() do pcall(function() service[item.Name .. \"Name\"] = item.Name end) end
if {} and _G.__ror_materials then
	for _, instance in game:GetService(\"SerializationService\"):DeserializeInstancesAsync(_G.__ror_materials) do
		instance:SetAttribute(\"RorCapture\", true)
		instance.Parent = service
	end
end
_G.__ror_materials = nil
{overrides}return #service:GetChildren()",
					materials.is_some()
				))
				.context("setting the capture place's MaterialVariants")?;
		}
		if let Some((bytes, overrides)) = materials {
			let text: String = overrides.iter().map(|(property, variant)| format!("{property}={variant};")).collect();
			self.model_key = fnv(bytes, fnv(text.as_bytes(), self.model_key));
		}
		Ok(())
	}

	pub fn set_environment(&mut self, environment: &Environment) -> Result<()> {
		let script = lighting_script(environment);
		if let Some(studio) = self.studio.as_mut() {
			studio.luau(&format!("{CLEAR_CLOUDS}
{script}
return 0")).context("setting the capture lighting")?;
		}
		self.lighting_key = fnv(script.as_bytes(), self.model_key);
		Ok(())
	}

	/// Makes Luau that changes the loaded model's state (e.g. emits and freezes particles) part of
	/// the capture cache key, after `set_environment`; the caller runs it unless `cached`.
	pub fn key_setup(&mut self, code: &str) {
		self.lighting_key = fnv(code.as_bytes(), self.lighting_key);
	}

	/// Whether a capture from `camera` would come from the cache.
	pub fn cached(&self, camera: &crate::render::camera::Camera) -> bool {
		self.reuse && self.cache_path(camera).exists()
	}

	pub(crate) fn cache_path(&self, camera: &crate::render::camera::Camera) -> PathBuf {
		let c = camera.cframe_components();
		let key = fnv(format!("{c:?}|{}|{}x{}", camera.fov, self.width, self.height).as_bytes(), self.lighting_key);
		output::state_dir().join("captures").join(format!("{key:016x}.png"))
	}

	/// Captures the viewport from a camera once two consecutive frames agree (lighting and
	/// shadows settle over a few frames). Returns the pixels and the number of captures taken.
	pub fn capture(&mut self, camera: &crate::render::camera::Camera) -> Result<(Vec<u8>, u32)> {
		let cached = self.cache_path(camera);
		if self.reuse
			&& let Ok((width, height, rgba)) = output::decode_png(&cached)
			&& (width, height) == (self.width, self.height)
		{
			self.reused += 1;
			return Ok((rgba, 0));
		}
		if self.studio.is_none() {
			bail!("offline: no cached capture {}", cached.display());
		}
		self.set_camera(camera)?;
		let mut previous: Option<Vec<u8>> = None;
		let mut attempts = 0;
		loop {
			std::thread::sleep(std::time::Duration::from_millis(if attempts == 0 { 700 } else { 400 }));
			let (width, height, rgba) = self.studio()?.capture_viewport()?;
			if (width, height) != (self.width, self.height) {
				bail!("captured {width}x{height}, expected {}x{}", self.width, self.height);
			}
			attempts += 1;
			if previous.as_ref().is_some_and(|previous| metrics(previous, &rgba).exact_percent >= 99.99) || attempts >= 8 {
				output::write_png(&cached, self.width, self.height, &rgba, true)?;
				return Ok((rgba, attempts));
			}
			previous = Some(rgba);
		}
	}

	/// The capture Studio (an error offline).
	fn studio(&mut self) -> Result<&mut CaptureStudio> {
		match self.studio.as_deref_mut() {
			Some(studio) => Ok(studio),
			None => bail!("offline: this needs the capture Studio"),
		}
	}

	pub fn set_camera(&mut self, camera: &crate::render::camera::Camera) -> Result<()> {
		let c = camera.cframe_components();
		self.studio()?.luau(&format!(
			"{HIDE_OVERLAYS}
local camera = workspace.CurrentCamera
camera.CameraType = Enum.CameraType.Scriptable
camera.FieldOfView = {}
camera.CFrame = CFrame.new({}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {})
return 0",
			camera.fov, c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7], c[8], c[9], c[10], c[11]
		))?;
		Ok(())
	}

	/// Captures `count` frames `interval_ms` apart without waiting for the image to settle and
	/// without the capture cache: the time course of an animated scene, with each frame's capture
	/// time in seconds after the first.
	pub fn capture_frames(&mut self, camera: &crate::render::camera::Camera, count: u32, interval_ms: u64) -> Result<Vec<(f64, Vec<u8>)>> {
		self.set_camera(camera)?;
		std::thread::sleep(std::time::Duration::from_millis(700));
		let start = Instant::now();
		let mut frames = Vec::new();
		for index in 0..count {
			if index > 0 {
				std::thread::sleep(std::time::Duration::from_millis(interval_ms));
			}
			let time = start.elapsed().as_secs_f64();
			let (width, height, rgba) = self.studio()?.capture_viewport()?;
			if (width, height) != (self.width, self.height) {
				bail!("captured {width}x{height}, expected {}x{}", self.width, self.height);
			}
			frames.push((time, rgba));
		}
		Ok(frames)
	}
}

/// Writes studio.png and ror.png into `directory`; with `derived` also diff.png and side-by-side.png,
/// images made from those two (`tools/analysis/side_by_side.py` draws them again from them).
/// Without, earlier ones are removed: they would show an older comparison.
pub fn write_comparison(directory: &Path, width: u32, height: u32, studio: &[u8], ror: &[u8], derived: bool) -> Result<()> {
	std::fs::create_dir_all(directory)?;
	output::write_png(&directory.join("studio.png"), width, height, studio, true)?;
	output::write_png(&directory.join("ror.png"), width, height, ror, true)?;
	if derived {
		let (diff, strip) = diff_images(width, height, studio, ror);
		output::write_png(&directory.join("diff.png"), width, height, &diff, true)?;
		output::write_png(&directory.join("side-by-side.png"), width * 3, height, &strip, true)?;
	} else {
		for name in ["diff.png", "side-by-side.png"] {
			let _ = std::fs::remove_file(directory.join(name));
		}
	}
	Ok(())
}

/// Whether calibration runs write diff.png and side-by-side.png too (`ROR_DIFF_IMAGES=1`; `ror
/// compare` always does). Until 2026-10-06 every run did: those two were 47 % of out/ (10.7 of
/// 22.7 GB), and their PNG encoding two thirds of a run's.
pub fn derived_images() -> bool {
	std::env::var_os("ROR_DIFF_IMAGES").is_some_and(|value| value != "0")
}

#[cfg(any(windows, target_os = "macos"))]
pub fn run(renderer: &Renderer, options: &CompareOptions) -> Result<CompareReport> {
	// a file, else an asset id or content URI (as `ror render` takes them)
	let mut document = if options.input.is_file() {
		scene::load::read_file(&options.input)?
	} else {
		let input = options.input.to_string_lossy();
		let uri = if input.chars().all(|c| c.is_ascii_digit()) { format!("rbxassetid://{input}") } else { input.to_string() };
		let fetched = renderer.assets.fetch(&uri).map_err(|error| anyhow::anyhow!("{input} is not a file, and as an asset: {error}"))?;
		scene::load::read_bytes(&fetched.bytes)?
	};
	if let Some(select) = &options.select {
		document.select(select)?;
	}
	// Studio scrolls beams' textures from when they enter the world, so a capture of a scrolling beam
	// never settles and keeps whatever phase its last attempt caught (the ring under two characters:
	// its band half as tall in ror's still render): comparisons hold beams still on
	// both sides, as they leave out the clouds below
	document.hold_beams();
	let mut scene = scene::extract::extract(&document);
	if let Some(environment) = &options.environment {
		scene.environment = environment.clone();
	}
	// Studio's Terrain clouds move from frame to frame (calibrate clouds: no two captures agree) and
	// ror does not draw them: comparisons leave them out
	scene.environment.clouds = None;
	let bytes = model_bytes(&document)?;
	let mut studio = match offline_studio() {
		Some(_) => None,
		None => Some(CaptureStudio::connect()?),
	};
	let studio_name = studio.as_ref().map(|studio| studio.name.clone()).or_else(offline_studio).unwrap_or_default();
	let mut session = match studio.as_mut() {
		Some(studio) => CaptureSession::start(studio, &bytes)?,
		None => CaptureSession::offline(&bytes, &offline_studio().unwrap_or_default()),
	};
	session.set_materials(material_bytes(&document, &scene.materials)?.as_ref())?;
	session.set_environment(&scene.environment)?;
	scene.materials.use_2022 = session.materials_2022;
	let (width, height) = (session.width, session.height);
	// captured as a game draws: keyed apart from full-detail captures of the same model and camera
	if let Some(lod) = options.lod {
		let code = game_lod_on(lod.quality);
		session.key_setup(&code);
		if let Some(studio) = session.studio.as_deref_mut() {
			studio.luau(&code).context("setting the capture Studio's levels of detail")?;
		}
	}
	let compared = compare_views(renderer, options, &scene, &mut session, width, height);
	if options.lod.is_some()
		&& let Some(studio) = session.studio.as_deref_mut()
	{
		studio.luau(GAME_LOD_OFF).context("putting back the capture Studio's levels of detail")?;
	}
	let (reports, mut missing) = compared?;
	missing.sort();
	missing.dedup();
	let report = CompareReport { input: options.input.display().to_string(), out: options.out.clone(), studio: studio_name, views: reports, missing };
	output::write_atomic(&options.out.join("report.json"), serde_json::to_string_pretty(&report)?.as_bytes())?;
	Ok(report)
}

/// Each view captured in Studio and drawn by ror ([`run`]); the assets ror missed.
#[cfg(any(windows, target_os = "macos"))]
fn compare_views(renderer: &Renderer, options: &CompareOptions, scene: &scene::Scene, session: &mut CaptureSession, width: u32, height: u32) -> Result<(Vec<ViewReport>, Vec<String>)> {
	std::fs::create_dir_all(&options.out)?;
	let mut reports = Vec::new();
	let mut missing = Vec::new();
	for view in &options.views {
		let request = ViewRequest { width, height, background: Background::Sky, lod: options.lod.or(view.view.lod), ..view.view.clone() };
		let camera = request.camera.place(renderer.framing_bounds(scene), width, height);
		let studio_start = Instant::now();
		let (captured, attempts) = session.capture(&camera)?;
		let studio_ms = studio_start.elapsed().as_secs_f64() * 1000.0;

		let ror_start = Instant::now();
		let (images, stats) = renderer.render(scene, &[ViewRequest { camera: camera.spec(), ..request.clone() }])?;
		let ror_ms = ror_start.elapsed().as_secs_f64() * 1000.0;
		missing.extend(stats.missing);
		let image = images.into_iter().next().context("no image")?;

		let directory = if options.views.len() == 1 { options.out.clone() } else { options.out.join(&view.name) };
		write_comparison(&directory, width, height, &captured, &image.rgba, true)?;
		// under levels of detail, ror's full-detail side against the same capture too
		let full_detail = match request.lod {
			Some(_) => {
				let (images, _) = renderer.render(scene, &[ViewRequest { camera: camera.spec(), lod: None, ..request.clone() }])?;
				let full = images.into_iter().next().context("no image")?;
				output::write_png(&directory.join("ror-full.png"), width, height, &full.rgba, true)?;
				Some(metrics(&full.rgba, &captured))
			}
			None => None,
		};
		// `ROR_CALIBRATE_SOURCE=1`: what the bloom reads, as calibrate writes it (`linear16.png`,
		// `linear2.png`; tools/analysis/bloom_fit.py)
		if std::env::var_os("ROR_CALIBRATE_SOURCE").is_some() {
			for scale in [16u32, 2] {
				let linear = ViewRequest { camera: camera.spec(), linear_scale: Some(1.0 / scale as f32), ..request.clone() };
				let (images, _) = renderer.render(scene, std::slice::from_ref(&linear))?;
				let image = images.into_iter().next().context("no image")?;
				output::write_png(&directory.join(format!("linear{scale}.png")), width, height, &image.rgba, true)?;
			}
		}
		reports.push(ViewReport {
			name: view.name.clone(),
			width,
			height,
			camera: camera.cframe_components(),
			fov: camera.fov,
			pixels: metrics(&image.rgba, &captured),
			full_detail,
			lod: image.lod,
			capture_attempts: attempts,
			studio_ms,
			ror_ms,
		});
	}
	Ok((reports, missing))
}

/// Reads a PNG pair and reports their metrics (for re-checking saved captures).
pub fn compare_files(a: &Path, b: &Path) -> Result<PixelMetrics> {
	let (wa, ha, pa) = output::decode_png(a)?;
	let (wb, hb, pb) = output::decode_png(b)?;
	if (wa, ha) != (wb, hb) {
		bail!("sizes differ: {wa}x{ha} vs {wb}x{hb}");
	}
	Ok(metrics(&pa, &pb))
}
