//! What a scene costs Roblox to draw from a camera: draw calls and triangles per render pass as
//! Roblox's own counters report them (`Stats.SceneDrawcallCount`, the Shift+F2 render stats,
//! `SceneAnalysisService:GetTriangleCompositionAsync`), the batches behind them, and what would cut
//! them most.
//!
//! Roblox draws its world in batches: every copy of one geometry with one appearance is one draw call
//! however many there are, however far apart and whatever their colours, sizes or rotations; most of
//! a map's draw calls come from what splits batches. The rules here were measured in the capture
//! Studio against those counters, one property at a time (notes/cost.md has each measurement), and
//! `ror cost --studio` checks a whole scene's views against them.

pub mod cull;
pub mod grid;
pub mod images;
pub mod meshes;
pub mod report;
#[cfg(any(windows, target_os = "macos"))]
pub mod studio;

use std::collections::{BTreeMap, HashMap, HashSet};

use rayon::prelude::*;

use glam::Vec3;
use serde::{Deserialize, Serialize};

use crate::render::camera::Camera;
use crate::scene::extract::{GLASS, mesh_type};
use crate::scene::load::Document;
use crate::scene::{Geometry, Part, Scene, props};
use cull::{Culler, world_box};
use meshes::{MeshStats, Meshes};

/// `Enum.Material.ForceField`
const FORCEFIELD: u32 = 1584;
/// `Enum.AlphaMode.Transparency`
const ALPHA_TRANSPARENCY: u32 = 1;
/// Roblox's own weighting of a draw call against triangles (its Studio assistant's scene-analysis
/// skill: "each draw call costs roughly the equivalent of 20,000 triangles"): how opportunities rank.
pub const TRIANGLES_PER_DRAW: f64 = 20_000.0;
/// Copies of one batch draw apart by distance from the camera: under 600 studs, under 1,200, beyond
/// (cost-probes m8).
const DISTANCE_BANDS: [f32; 2] = [600.0, 1200.0];

/// The level of detail an item draws from a camera at `eye` with a vertical field of view of `fov`
/// degrees, and its triangles there (crate::lod: by the distance to its box, Performance taking its
/// whole chain, Automatic its high-quality levels; a Performance mesh without a chain the levels
/// Roblox makes at run time). Roblox's cloud levels of detail (Workspace
/// MeshStreamingAndImprovedLods) replace this where enabled; many live places have it Disabled.
fn item_level(item: &Item, eye: Vec3, fov: f32) -> (usize, u64) {
	level_under(item, item.lod, eye, fov)
}

/// [`item_level`] under another fidelity's rule.
fn level_under(item: &Item, rule: Option<ItemLod>, eye: Vec3, fov: f32) -> (usize, u64) {
	let Some(rule) = rule else { return (0, u64::from(item.triangles[0])) };
	let by_distance = crate::lod::level_at(crate::lod::box_distance(eye, item.bounds.0, item.bounds.1), fov);
	if item.triangles.len() < 2 && rule.fidelity == crate::lod::PERFORMANCE {
		return (by_distance, (f64::from(item.triangles[0]) * f64::from(crate::lod::RUNTIME_LEVELS[by_distance])).round() as u64);
	}
	let level = crate::lod::fidelity_level(rule.fidelity, by_distance, item.triangles.len(), rule.high_quality);
	(level, u64::from(item.triangles[level]))
}

/// What a MeshPart's RenderFidelity takes of its chain (crate::lod).
#[derive(Clone, Copy, Debug)]
pub struct ItemLod {
	pub fidelity: u32,
	/// The file's high-quality levels (Automatic takes those)
	pub high_quality: u8,
}

/// Which of the distance bands copies batch apart by ([`DISTANCE_BANDS`]).
fn distance_band(distance: f32) -> usize {
	DISTANCE_BANDS.iter().filter(|edge| distance >= **edge).count()
}
/// Skinned meshes outside a Humanoid's model batch per cube of this many studs (cost-probes k2).
const SKINNED_CELL: f32 = 128.0;
/// Bones a FastCluster holds: a part takes one, each Bone under it one more (cost-probes m11, m12).
const CLUSTER_BONES: u32 = 72;

/// Roblox's render passes, as `SceneAnalysisService:GetTriangleCompositionAsync` names them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Pass {
	Opaque,
	Transparent,
	Particles,
	#[serde(rename = "UI")]
	Ui,
	Sky,
	Cloud,
	GenericPostProcess,
	/// Left out of `total`: Roblox caches shadow maps, and its guidance leaves them out of budgets.
	Shadows,
}

impl Pass {
	pub fn name(self) -> &'static str {
		match self {
			Pass::Opaque => "Opaque",
			Pass::Transparent => "Transparent",
			Pass::Particles => "Particles",
			Pass::Ui => "UI",
			Pass::Sky => "Sky",
			Pass::Cloud => "Cloud",
			Pass::GenericPostProcess => "GenericPostProcess",
			Pass::Shadows => "Shadows",
		}
	}
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Count {
	pub draws: u64,
	pub triangles: u64,
}

impl Count {
	pub fn add(&mut self, draws: u64, triangles: u64) {
		self.draws += draws;
		self.triangles += triangles;
	}

	/// Draw calls plus triangles at Roblox's weighting ([`TRIANGLES_PER_DRAW`]).
	pub fn weight(&self) -> f64 {
		self.draws as f64 + self.triangles as f64 / TRIANGLES_PER_DRAW
	}
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CostOptions {
	/// `Enum.Technology` to count for, the scene's Lighting's when absent (transparent Glass depends
	/// on it).
	pub technology: Option<u32>,
	/// Graphics quality level 1-21 (21 when absent: Studio's edit quality, where most was measured).
	pub quality: Option<u32>,
	/// Frustum culling only.
	pub no_occlusion: bool,
	/// Every mesh at full detail, as Studio draws by default (its MeshPartDetailLevel setting);
	/// otherwise each MeshPart at the level of detail its distance and RenderFidelity give
	/// (crate::lod: Performance its whole chain past 250, 500, 2,000 and 3,000 studs to its box at a
	/// 70-degree field of view, Automatic its high-quality levels).
	pub full_detail: bool,
	/// With `ror cost --check`: Studio counts at the levels of detail too (its MeshPartDetailLevel
	/// DistanceBased and edit quality `quality` while it measures), not at full detail.
	#[serde(default)]
	pub studio_lod: bool,
	/// Batches, occluders used, missed occluders and tested groups listed per view in the job's report
	/// (24 when absent; cost.json lists every one).
	pub top: Option<usize>,
	/// No draw-call and heat images, only the numbers.
	pub no_images: bool,
	/// A grid run's cell size in studs (`ror cost --grid`): its `grid<x>_<z>-<yaw>` views make
	/// `grid.json` and the grid pictures instead of an image per view.
	#[serde(default)]
	pub grid: Option<f32>,
	/// The area a grid run covers, as its report names it.
	#[serde(default)]
	pub grid_area: Option<String>,
	/// Write `items.json` (`ror cost --items`): every item and, per view, the ones drawn (level,
	/// triangles) and the ones occlusion hides.
	#[serde(default)]
	pub items: bool,
	/// Every emitter in `visible.fullest` (`ror cost --all-emitters`), not the fullest few.
	#[serde(default)]
	pub all_emitters: bool,
}

impl CostOptions {
	fn quality(&self) -> u32 {
		self.quality.unwrap_or(21).clamp(1, 21)
	}
}

/// What draws, as Roblox counts it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Kind {
	Part,
	Decal,
	Texture,
	Highlight,
	Emitter,
	Beam,
	Trail,
	Billboard,
	Adornment,
	NameTag,
}

/// Which clusters a part's draws join.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Cluster {
	/// The world's instanced batches.
	Static,
	/// A Humanoid's model (a FastCluster): its parts batch only with each other. The second field
	/// counts the clusters it fills before (72 bones each).
	Character(u32, u16),
	/// A skinned mesh outside a Humanoid: FastClusters by the cell it is in, 72 bones each.
	Skinned([i32; 3], u16),
}

impl Cluster {
	/// The same model or cell's cluster of this number (from 0).
	fn numbered(self, number: u16) -> Self {
		match self {
			Cluster::Static => Cluster::Static,
			Cluster::Character(id, _) => Cluster::Character(id, number),
			Cluster::Skinned(cell, _) => Cluster::Skinned(cell, number),
		}
	}
}

/// One thing Roblox draws: its batch (`label`, shared by everything that draws with it), its pass
/// and triangles, and where it is.
#[derive(Clone, Debug)]
pub struct Item {
	pub kind: Kind,
	pub pass: Pass,
	/// Index into [`Analysis::labels`]: items with one label are one batch; `None` draws alone.
	pub label: Option<u32>,
	/// Drawn sorted by depth with the transparent geometry: one label shares a draw only with what
	/// lies at the same depth along the view (ForceField, transparent parts holding decals,
	/// Highlights; notes/cost.md "Sorted").
	pub sorted: bool,
	/// Draw calls each batch of it makes (2: ForceField, transparent Glass under Unified or below
	/// quality 16; 12: a SelectionBox).
	pub draws: u32,
	/// Triangles per level of detail (one entry: no chain), each draw's.
	pub triangles: Vec<u32>,
	/// A MeshPart whose level of detail steps down with distance (its fidelity's hold on its chain);
	/// `None`: always full detail.
	pub lod: Option<ItemLod>,
	/// A part's Size length: left out where the quality's view distance or size cull leaves it
	/// (crate::lod::Lod::drawn); `None` for what those do not cull.
	pub cull: Option<f32>,
	pub bounds: (Vec3, Vec3),
	pub center: Vec3,
	/// Index into `scene.parts` (parts, their decals and Highlights).
	pub part: Option<usize>,
	pub path: String,
	/// The geometry and appearance parts of the label (opportunities compare them).
	pub geometry: Option<u32>,
	pub appearance: Option<u32>,
	pub transparent: bool,
	pub cluster: Cluster,
	/// Drawn only within this distance of the camera (a BillboardGui's MaxDistance, a Humanoid's
	/// NameDisplayDistance).
	pub max_distance: f32,
	/// Left out of occlusion culling (AlwaysOnTop Highlights and GUIs).
	pub always: bool,
	/// Every draw of it is wasted: nothing of it shows (a Beam or emitter at Transparency 1, a Beam
	/// of width 0).
	pub invisible: bool,
	/// A FastCluster's (a Humanoid's model, skinned meshes' cell) box: the cluster is culled as one.
	pub cluster_bounds: Option<(Vec3, Vec3)>,
	/// Drawn in the depth-sorted queue of transparent things, where only consecutive items of one
	/// batch share a draw (m10): transparent parts, decals on transparent or invisible parts, every
	/// emitter, beam and trail; a ForceField part twice (its two passes, each a run of its own).
	pub queued: u8,
}

/// A part that hides what is behind it.
struct Occluder {
	bounds: (Vec3, Vec3),
	/// Its own box, to leave it out when the camera is inside (oc1: a camera inside a block
	/// sees everything behind its far side).
	cframe: glam::Affine3A,
	size: Vec3,
	triangles: Vec<[Vec3; 3]>,
	/// A mesh's distinct vertex positions (None: a part's own shape, which any size occludes).
	positions: Option<u32>,
	path: String,
	/// Its mesh's asset id, to tell apart meshes that share a path.
	mesh: Option<String>,
}

/// A large solid part that never occludes, and why: what a block inside it would hide is what
/// [`OcclusionWork::missed`] reports. (Not cut-outs and slightly transparent parts: a block would
/// show through them.)
struct NotOccluder {
	bounds: (Vec3, Vec3),
	cframe: glam::Affine3A,
	size: Vec3,
	reason: String,
	path: String,
}

/// Parts that never occlude, considered as missed occluders while they cover at least this share of
/// the view (smaller ones would hide little).
const MISSED_MIN_SHARE: f32 = 0.01;
/// Missed occluders tried per view, the largest on screen first.
const MISSED_PER_VIEW: usize = 12;
/// cost.json of more views than this is written compact (a grid's).
const PRETTY_VIEWS: usize = 64;

/// A mesh occludes only while it covers at least this many pixels of a 256-pixel-wide view per
/// distinct vertex position: staff's "if a part is too detailed and too small, we don't even consider
/// it" (b7: meshes of 370 to 1,359 positions hid as walls a quarter of the view wide, not an eighth;
/// of 1,568 to 2,044 half the view, not a quarter; blocks at every size tried).
pub const OCCLUDER_POSITIONS_PER_PIXEL: f32 = 0.67;
/// Occluders smaller than this many pixels of a 256-pixel-wide view on their longer side are not
/// rasterized (b2: 1,000 cubes 1.3 pixels wide cost what transparent ones do, 2.5 pixels wide five
/// times more).
const OCCLUDER_MIN_PIXELS: f32 = 2.0;
/// Occluders a view rasterizes at most, those covering most of it first: every one it can (b2: 4,000
/// occluder cubes cost four times 1,000; 472 views of ten maps matched Studio's draws best with no
/// limit once the occluder rules were right: -0.3 %, the 64 largest +3.9 %; notes/occlusion.md).
/// `ROR_COST_OCCLUDERS` lowers it.
const OCCLUDER_BUDGET: usize = usize::MAX;

/// A scene prepared for the cost model: everything that draws, the occluders and the meshes.
pub struct Analysis {
	pub items: Vec<Item>,
	pub labels: Vec<String>,
	/// The mesh behind a geometry label (its key in `meshes`), and whether it is a union's.
	pub geometry_meshes: HashMap<u32, (String, bool)>,
	occluders: Vec<Occluder>,
	not_occluders: Vec<NotOccluder>,
	pub meshes: Meshes,
	/// The characters (Humanoid models) by path.
	pub characters: Vec<String>,
	/// `Enum.Technology` counted for.
	pub technology: u32,
	pub quality: u32,
	pub clouds: bool,
	pub sun: Vec3,
	pub moon: Vec3,
	/// The sun's and moon's angular sizes in degrees (0: not shown).
	pub celestial: (f32, f32),
	pub warnings: Vec<String>,
	/// Workspace.MeshStreamingAndImprovedLoDs as the place sets it (Enabled ignores RenderFidelity).
	pub mesh_streaming: Option<u32>,
	/// Each item's [`tested_group`], found once for every view that tests it
	tested_groups: Vec<std::sync::OnceLock<String>>,
	/// The emitters' items (by index into `items`) with their particles, for the share of each view
	/// they cover, and the `CameraFollow` part each rides on (scene::follow).
	emitters: HashMap<usize, EmitterItem>,
	/// The beams' and trails' items, for what their ribbons shade from each view.
	beams: HashMap<usize, crate::scene::beams::Beam>,
	trails: HashMap<usize, crate::scene::trails::Trail>,
}

/// An emitter's steady state, and the part following the camera it stands on, if any.
struct EmitterItem {
	emitter: crate::scene::particles::ParticleEmitter,
	particles: Vec<crate::render::particles::Particle>,
	follow: Option<crate::scene::follow::Follow>,
}

impl EmitterItem {
	/// How many viewports' worth of blended pixels its particles shade from `camera`, moved by
	/// `offset` (a CameraFollow part's): the GPU's fill work for them, however transparent.
	fn fill(&self, camera: &Camera, aspect: f32, offset: Vec3) -> f32 {
		if offset == Vec3::ZERO {
			return crate::render::particles::shaded(&self.emitter, &self.particles, camera, aspect);
		}
		let mut emitter = self.emitter.clone();
		emitter.cframe.translation += glam::Vec3A::from(offset);
		let particles: Vec<_> = self.particles.iter().map(|particle| crate::render::particles::Particle { position: particle.position + offset, ..*particle }).collect();
		crate::render::particles::shaded(&emitter, &particles, camera, aspect)
	}
}

/// What a beam or trail shades from a view, in viewports, as its ribbon's triangles land on screen.
fn ribbon_fill(vertices: &[crate::render::beams::BeamVertex], camera: &Camera, aspect: f32) -> f64 {
	let view = camera.view_projection(aspect);
	crate::render::particles::shaded_area(&view, vertices.chunks_exact(3).map(|t| [Vec3::from(t[0].position), Vec3::from(t[1].position), Vec3::from(t[2].position)])) as f64
}

#[derive(Default)]
struct Labels {
	names: Vec<String>,
	index: HashMap<String, u32>,
}

impl Labels {
	fn of(&mut self, label: String) -> u32 {
		if let Some(&index) = self.index.get(&label) {
			return index;
		}
		let index = self.names.len() as u32;
		self.names.push(label.clone());
		self.index.insert(label, index);
		index
	}
}

fn material_name(value: u32) -> String {
	props::enum_name("Material", value).map(str::to_string).unwrap_or_else(|| format!("Material {value}"))
}

fn face_name(face: u32) -> &'static str {
	["Right", "Top", "Back", "Left", "Bottom", "Front"].get(face as usize).copied().unwrap_or("?")
}

/// A part's geometry as Roblox batches it: its shape, mesh, file mesh or union.
fn geometry_label(part: &Part) -> String {
	match &part.geometry {
		Geometry::Block => "Block".into(),
		Geometry::Ball => "Ball".into(),
		Geometry::Cylinder => "Cylinder".into(),
		Geometry::Wedge => "Wedge".into(),
		Geometry::CornerWedge => "CornerWedge".into(),
		Geometry::Truss { style } => format!("Truss {style}"),
		Geometry::MeshPart { mesh, .. } => format!("MeshPart {}", short(mesh)),
		Geometry::Special { mesh_type: kind, mesh, .. } => match (*kind, mesh) {
			(mesh_type::FILE_MESH, Some(mesh)) => format!("SpecialMesh {}", short(mesh)),
			(kind, _) => format!("SpecialMesh {}", props::enum_name("MeshType", kind).unwrap_or("?")),
		},
		Geometry::Union { .. } => format!("Union {}", meshes::mesh_key(&part.geometry).unwrap_or_default()),
		Geometry::Collision { key, .. } => format!("Collision {key:016x}"),
	}
}

/// An asset as labels show it: its id alone (`rbxassetid://123` as `123`), other content as given.
fn short(uri: &str) -> String {
	let canonical = meshes::canonical(uri);
	canonical.strip_prefix("rbxassetid://").map(str::to_string).unwrap_or(canonical)
}

/// A part's appearance as Roblox batches it: a SurfaceAppearance's maps (which then decide alone:
/// Material and TextureID no longer split; its TexturePack only when it has no maps: a large place's
/// views with copies of one mesh whose packs differ matched Studio without them), else its material (and
/// MaterialVariant) and texture.
fn appearance_label(part: &Part, scene: &Scene) -> String {
	if let Some(sa) = &part.surface_appearance {
		let maps = [&sa.color_map, &sa.normal_map, &sa.roughness_map, &sa.metalness_map];
		if maps.iter().all(|map| map.is_none()) {
			return format!("SurfaceAppearance pack {}", sa.texture_pack.as_deref().map(short).unwrap_or_else(|| "-".into()));
		}
		let maps: Vec<String> = maps.iter().map(|map| map.as_deref().map(short).unwrap_or_else(|| "-".into())).collect();
		return format!("SurfaceAppearance {}", maps.join("/"));
	}
	let mut label = material_name(part.material);
	if let Some(variant) = scene.materials.variant(part.material, &part.material_variant) {
		label.push_str(&format!(" ({})", variant.name));
	}
	if let Some(texture) = &part.texture {
		label.push_str(&format!(" texture {}", short(texture)));
	}
	label
}

/// Triangles a part's shape draws (a1, g1), per level of detail for meshes.
fn part_triangles(part: &Part, meshes: &Meshes) -> Vec<u32> {
	match &part.geometry {
		Geometry::Block | Geometry::Truss { .. } => vec![12],
		Geometry::Ball => vec![432],
		Geometry::Cylinder => vec![96],
		Geometry::Wedge | Geometry::CornerWedge => vec![10],
		Geometry::Special { mesh_type: kind, .. } if *kind != mesh_type::FILE_MESH => match *kind {
			mesh_type::SPHERE => vec![432],
			mesh_type::HEAD => vec![846],
			mesh_type::CYLINDER => vec![96],
			mesh_type::WEDGE => vec![10],
			_ => vec![12],
		},
		geometry => meshes.of(geometry).map(|stats| stats.lods.clone()).unwrap_or_else(|| vec![0]),
	}
}

/// Triangles a Decal or Texture on a face of a part draws: the face's two, or a mesh's triangles
/// facing that way (d1: 100 meshes with a Top decal, 39 triangles each of 238).
fn decal_triangles(part: &Part, face: u32, meshes: &Meshes) -> u32 {
	match &part.geometry {
		Geometry::MeshPart { .. } | Geometry::Union { .. } | Geometry::Special { mesh_type: mesh_type::FILE_MESH, .. } => {
			meshes.of(&part.geometry).map(|stats| stats.facing[face.min(5) as usize]).unwrap_or(2)
		}
		_ => 2,
	}
}

/// Whether Glass at a Transparency above 0 draws twice: always under Unified, under Future at
/// quality 14 and below (gl2: 1 draw at 16 to 21, 2 at 1 to 14).
fn glass_draws_twice(technology: u32, quality: u32) -> bool {
	match technology {
		3 | 4 => quality <= 15,
		_ => true,
	}
}

/// The Humanoid model a part belongs to: the nearest ancestor Model holding a Humanoid.
/// The Bones under a part (not those under parts inside it): each takes a bone of its FastCluster.
fn bones_under(document: &Document, referent: rbx_dom_weak::types::Ref) -> u32 {
	let Some(instance) = document.dom.get_by_ref(referent) else { return 0 };
	instance
		.children()
		.iter()
		.filter_map(|child| document.dom.get_by_ref(*child))
		.filter(|child| !props::class_is_a(child.class.as_str(), "BasePart"))
		.map(|child| (child.class == "Bone") as u32 + bones_under(document, child.referent()))
		.sum()
}

/// A FastCluster's number after the first, for its batch labels.
fn cluster_number(chunk: u16) -> String {
	if chunk == 0 { String::new() } else { format!(" (cluster {})", chunk + 1) }
}

fn character_of(document: &Document, part: rbx_dom_weak::types::Ref) -> Option<rbx_dom_weak::types::Ref> {
	let mut current = document.dom.get_by_ref(part)?.parent();
	while let Some(instance) = document.dom.get_by_ref(current) {
		if props::is_a(instance, "Model") && instance.children().iter().any(|child| document.dom.get_by_ref(*child).is_some_and(|child| child.class == "Humanoid")) {
			return Some(current);
		}
		current = instance.parent();
	}
	None
}

/// How many GUI objects under an instance show something: a BillboardGui showing nothing makes no
/// draw call (k1: "empty", "frame transparent 1").
fn shows(document: &Document, referent: rbx_dom_weak::types::Ref) -> usize {
	let Some(instance) = document.dom.get_by_ref(referent) else { return 0 };
	let mut count = 0;
	for &child in instance.children() {
		let Some(object) = document.dom.get_by_ref(child) else { continue };
		if !props::class_is_a(object.class.as_str(), "GuiObject") || !props::bool(object, "Visible", true) {
			continue;
		}
		let class = object.class.as_str();
		let background = props::f32(object, "BackgroundTransparency", 0.0) < 1.0;
		let text = matches!(class, "TextLabel" | "TextButton" | "TextBox") && props::string(object, "Text").is_some_and(|text| !text.is_empty()) && props::f32(object, "TextTransparency", 0.0) < 1.0;
		let image = matches!(class, "ImageLabel" | "ImageButton") && props::content(object, &["ImageContent", "Image"]).is_some() && props::f32(object, "ImageTransparency", 0.0) < 1.0;
		if background || text || image {
			count += 1;
		}
		count += shows(document, child);
	}
	count
}

/// The parts of an item not decided by the rule that made it.
struct ItemBase {
	bounds: (Vec3, Vec3),
	center: Vec3,
	path: String,
}

fn effect(kind: Kind, pass: Pass, draws: u32, triangles: u32, base: ItemBase, invisible: bool, max_distance: f32, always: bool) -> Item {
	Item {
		kind,
		pass,
		label: None,
		sorted: pass == Pass::Transparent,
		draws,
		triangles: vec![triangles],
		lod: None,
		cull: None,
		bounds: base.bounds,
		center: base.center,
		part: None,
		path: base.path,
		geometry: None,
		appearance: None,
		transparent: pass == Pass::Transparent,
		cluster: Cluster::Static,
		max_distance,
		always,
		invisible,
		cluster_bounds: None,
		queued: if pass == Pass::Transparent { 1 } else { 0 },
	}
}

impl Analysis {
	/// Everything that draws in `scene` (read from `document`), by the measured rules.
	pub fn new(scene: &Scene, document: &Document, assets: &ror_assets::Assets, options: &CostOptions) -> Self {
		let meshes = Meshes::load(&scene.parts, assets);
		let technology = options.technology.unwrap_or(scene.environment.technology);
		let quality = options.quality();
		let mut labels = Labels::default();
		let mut items = Vec::new();
		let mut occluders = Vec::new();
		let mut not_occluders = Vec::new();
		let mut warnings: Vec<String> = meshes.missing.iter().map(|missing| format!("mesh not loaded (its triangles count 0): {missing}")).collect();
		let mut characters: Vec<String> = Vec::new();
		let mut character_index: HashMap<rbx_dom_weak::types::Ref, u32> = HashMap::new();
		// each model or cell's clusters so far: (number of the last, bones in it)
		let mut cluster_fill: HashMap<Cluster, (u16, u32)> = HashMap::new();
		let kept_glass: HashSet<usize> = scene.kept_glass.iter().copied().collect();
		// each part's batch label, for its Highlights
		let mut part_labels: Vec<Option<u32>> = vec![None; scene.parts.len()];
		let mut geometry_meshes = HashMap::new();

		for (index, part) in scene.parts.iter().enumerate() {
			if kept_glass.contains(&index) {
				continue;
			}
			let bounds = world_box(&part.cframe, part.size);
			let center = Vec3::from(part.cframe.translation);
			let cluster = match part.referent.and_then(|referent| character_of(document, referent)) {
				Some(model) => {
					let next = characters.len() as u32;
					let id = *character_index.entry(model).or_insert_with(|| {
						characters.push(document.path_of(model));
						next
					});
					Cluster::Character(id, 0)
				}
				None if part.skeleton.is_some() => Cluster::Skinned((center / SKINNED_CELL).floor().as_ivec3().to_array(), 0),
				None => Cluster::Static,
			};
			let triangles = part_triangles(part, &meshes);
			let geometry = labels.of(geometry_label(part));
			if let Some(key) = meshes::mesh_key(&part.geometry) {
				geometry_meshes.entry(geometry).or_insert((key, matches!(part.geometry, Geometry::Union { .. })));
			}
			let appearance = labels.of(appearance_label(part, scene));
			// characters and skinned meshes keep full detail and cull with their clusters (not measured)
			let lod = match &part.geometry {
				Geometry::MeshPart { fidelity, .. } if !options.full_detail && part.class == "MeshPart" && part.skeleton.is_none() && part.body_part.is_none() => {
					let high_quality = meshes.of(&part.geometry).map_or(0, |stats| stats.high_quality);
					let steps = *fidelity == crate::lod::PERFORMANCE || (*fidelity == crate::lod::AUTOMATIC && high_quality > 1 && triangles.len() > 1);
					steps.then_some(ItemLod { fidelity: *fidelity, high_quality })
				}
				_ => None,
			};
			let cull = (cluster == Cluster::Static).then(|| part.size.length());
			let transparent = part.transparency > 0.0;
			let drawn = part.transparency < 1.0;
			let has_decals = part.decals.iter().any(|decal| decal.transparency < 1.0);
			let forcefield = part.material == FORCEFIELD;
			// a FastCluster holds 72 bones, a part one and each Bone under it one more; the next part
			// that overflows it starts another, drawn apart (m11: skinned meshes of 17 Bones four to a
			// draw, of 8 eight; m12: a Humanoid of 72 blocks one draw, 73 two, 145 three)
			let cluster = match cluster {
				Cluster::Character(..) | Cluster::Skinned(..) if drawn && !transparent => {
					let cost = 1 + part.referent.map_or(0, |referent| bones_under(document, referent));
					let fill = cluster_fill.entry(cluster).or_insert((0, 0));
					if fill.1 > 0 && fill.1 + cost > CLUSTER_BONES {
						*fill = (fill.0 + 1, 0);
					}
					fill.1 += cost;
					cluster.numbered(fill.0)
				}
				_ => cluster,
			};
			if drawn {
				let twice = forcefield || (part.material == GLASS && transparent && glass_draws_twice(technology, quality));
				let suffix = if transparent { " · transparent" } else { "" };
				let (pass, label, sorted) = match cluster {
					// a character's transparent parts draw one by one with the transparent geometry
					// (h1: 10 meshes at 0.5, Transparent +10)
					Cluster::Character(..) | Cluster::Skinned(..) if transparent => (Pass::Transparent, None, false),
					Cluster::Character(id, chunk) => (Pass::Opaque, Some(labels.of(format!("character {}{} · {}", characters[id as usize], cluster_number(chunk), labels.names[appearance as usize]))), false),
					Cluster::Skinned(cell, chunk) => (Pass::Opaque, Some(labels.of(format!("skinned meshes in cell {cell:?}{} · {}{suffix}", cluster_number(chunk), labels.names[appearance as usize]))), false),
					Cluster::Static => (Pass::Opaque, Some(labels.of(format!("{} · {}{suffix}", labels.names[geometry as usize], labels.names[appearance as usize]))), false),
				};
				let _ = has_decals;
				part_labels[index] = label;
				// ForceField's two passes are two queue entries of one draw each (a1, t1: a row across the
				// view two draws, along it two a part)
				let queued = if forcefield { 2 } else if transparent { 1 } else { 0 };
				items.push(Item {
					kind: Kind::Part,
					pass,
					label,
					sorted,
					draws: if twice && !forcefield { 2 } else { 1 },
					triangles: triangles.clone(),
					lod,
					cull,
					bounds,
					center,
					part: Some(index),
					path: part.path.clone(),
					geometry: Some(geometry),
					appearance: Some(appearance),
					transparent,
					cluster,
					max_distance: f32::INFINITY,
					always: false,
					invisible: false,
					cluster_bounds: None,
					queued,
				});
				// occluders: opaque static parts of any material and thickness (a5: Neon and Glass walls and
				// walls of 0.01 studs hid like any), not a SurfaceAppearance whose alpha cuts holes (m17: an
				// AlphaMode Transparency one hid nothing, even without a map), nor unions or trusses (staff:
				// unions "can be occluded but are not used to occlude"); notes/occlusion.md "Occluders"
				let cutout = part.surface_appearance.as_ref().is_some_and(|appearance| appearance.alpha_mode == ALPHA_TRANSPARENCY);
				if !transparent && !cutout && cluster == Cluster::Static && !forcefield {
					let (triangles, positions): (Option<Vec<[Vec3; 3]>>, Option<u32>) = match &part.geometry {
						Geometry::Block | Geometry::Wedge | Geometry::CornerWedge => (Some(cull::block_triangles(&part.cframe, part.size).to_vec()), None),
						// a ball and a cylinder by the box inside them (oc1: both hid a wall's worth)
						Geometry::Ball => {
							let side = part.size.min_element() / 3f32.sqrt();
							(Some(cull::block_triangles(&part.cframe, Vec3::splat(side)).to_vec()), None)
						}
						Geometry::Cylinder => {
							let side = part.size.y.min(part.size.z) / 2f32.sqrt();
							(Some(cull::block_triangles(&part.cframe, Vec3::new(part.size.x, side, side)).to_vec()), None)
						}
						Geometry::MeshPart { .. } => match meshes.of(&part.geometry) {
							Some(stats) => (mesh_occluder(stats, part), Some(stats.positions)),
							None => (None, None),
						},
						_ => (None, None),
					};
					if let Some(triangles) = triangles {
						let mesh = match &part.geometry {
							Geometry::MeshPart { mesh, .. } => Some(short(mesh)),
							_ => None,
						};
						occluders.push(Occluder { bounds, cframe: part.cframe, size: part.size, triangles, positions, path: part.path.clone(), mesh });
					} else {
						let reason = match &part.geometry {
							Geometry::Union { .. } => Some("a union (unions never occlude)".to_string()),
							Geometry::Truss { .. } => Some("a truss".to_string()),
							Geometry::MeshPart { .. } => meshes.of(&part.geometry).map(|stats| format!("a mesh of {} vertex positions (over 2,048)", stats.positions)),
							Geometry::Special { .. } => Some("a SpecialMesh".to_string()),
							_ => None,
						};
						if let Some(reason) = reason {
							not_occluders.push(NotOccluder { bounds, cframe: part.cframe, size: part.size, reason, path: part.path.clone() });
						}
					}
				}
			}
			// decals: one batch per class, texture and face, across every part (d2: 1000 on top
			// faces one draw, six faces six); a character's fold into its cluster (R6: none of their own)
			if matches!(cluster, Cluster::Character(..)) {
				continue;
			}
			for decal in part.decals.iter().filter(|decal| decal.transparency < 1.0) {
				let (kind, class) = if decal.tiling.is_some() { (Kind::Texture, "Texture") } else { (Kind::Decal, "Decal") };
				// and per what they lie on: its geometry, material and whether it draws opaque, transparent
				// or not at all (m1, m2: one image on blocks and two meshes three draws, on blocks of two
				// materials two; the part's TextureID splits nothing; m16: on invisible blocks of twenty
				// materials one)
				let mut material = if drawn { format!(" {}", material_name(part.material)) } else { String::new() };
				if let Some(variant) = scene.materials.variant(part.material, &part.material_variant).filter(|_| drawn) {
					material.push_str(&format!(" ({})", variant.name));
				}
				let state = if !drawn { " invisible" } else if transparent { " transparent" } else { "" };
				let label = labels.of(format!("{class} {} · {} face · on {}{material}{state}", short(&decal.texture), face_name(decal.face), labels.names[geometry as usize]));
				items.push(Item {
					kind,
					pass: Pass::Opaque,
					label: Some(label),
					// on a transparent or invisible part they join the transparent queue (t1, m10)
					sorted: false,
					draws: 1,
					triangles: vec![decal_triangles(part, decal.face, &meshes)],
					lod: None,
					cull: (cluster == Cluster::Static).then(|| part.size.length()),
					bounds,
					center,
					part: Some(index),
					path: part.path.clone(),
					geometry: None,
					appearance: None,
					transparent: transparent && drawn,
					cluster,
					max_distance: f32::INFINITY,
					always: false,
					invisible: false,
					cluster_bounds: None,
					queued: if transparent { 1 } else { 0 },
				});
			}
		}

		// Highlights: each draws its opaque parts again, a draw per batch at each depth (x1, v2)
		for (number, highlight) in scene.highlights.iter().enumerate() {
			for &index in &highlight.parts {
				let part = &scene.parts[index];
				let base = part_labels[index].map(|label| labels.names[label as usize].clone()).unwrap_or_else(|| geometry_label(part));
				let label = labels.of(format!("Highlight {} · {base}", number + 1));
				items.push(Item {
					kind: Kind::Highlight,
					pass: Pass::Opaque,
					label: Some(label),
					sorted: true,
					draws: 1,
					triangles: part_triangles(part, &meshes),
					lod: None,
					cull: None,
					bounds: world_box(&part.cframe, part.size),
					center: Vec3::from(part.cframe.translation),
					part: Some(index),
					path: part.path.clone(),
					geometry: None,
					appearance: None,
					transparent: false,
					cluster: Cluster::Static,
					max_distance: f32::INFINITY,
					always: highlight.depth_mode == 0,
					invisible: false,
					cluster_bounds: None,
					queued: 0,
				});
			}
		}

		// effects: each ParticleEmitter with live particles, each Beam and each Trail draws alone
		// (p1, tr1); a Beam at Transparency 1 or width 0 still does
		let mut emitter_items = HashMap::new();
		let (mut beam_items, mut trail_items) = (HashMap::new(), HashMap::new());
		for emitter in &scene.particles {
			let particles = crate::render::particles::simulate(emitter);
			if particles.is_empty() {
				continue;
			}
			let (mut low, mut high) = (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY));
			for particle in &particles {
				low = low.min(particle.position - Vec3::splat(particle.size));
				high = high.max(particle.position + Vec3::splat(particle.size));
			}
			let invisible = emitter.transparency.iter().all(|key| key.1 - key.2.abs() >= 1.0);
			items.push(effect(Kind::Emitter, Pass::Transparent, 1, particles.len() as u32 * 2, ItemBase { bounds: (low, high), center: (low + high) * 0.5, path: emitter.path.clone() }, invisible, f32::INFINITY, false));
			emitter_items.insert(items.len() - 1, EmitterItem { emitter: emitter.clone(), particles, follow: emitter.follow.filter(|_| emitter.births.is_none()) });
		}
		for beam in &scene.beams {
			let (a, b) = (Vec3::from(beam.attachment0.translation), Vec3::from(beam.attachment1.translation));
			// Roblox's box around a beam: its ends, widened by half its width and half its CurveSize
			// either way (m15: one 10 studs behind the camera drawn from width 21 or CurveSize 22 on,
			// 20 behind from 41 and 44)
			let reach = (beam.width0.abs().max(beam.width1.abs()) + beam.curve_size0.abs().max(beam.curve_size1.abs())) * 0.5;
			let bounds = (a.min(b) - Vec3::splat(reach), a.max(b) + Vec3::splat(reach));
			let invisible = beam.transparency.iter().all(|(_, value, envelope)| value - envelope.abs() >= 1.0) || (beam.width0 == 0.0 && beam.width1 == 0.0);
			items.push(effect(Kind::Beam, Pass::Transparent, 1, beam.segments.max(1) * 2, ItemBase { bounds, center: (a + b) * 0.5, path: beam.path.clone() }, invisible, f32::INFINITY, false));
			beam_items.insert(items.len() - 1, beam.clone());
		}
		for trail in scene.trails.iter().filter(|trail| trail.points.len() >= 2) {
			let (mut low, mut high) = (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY));
			for point in &trail.points {
				low = low.min(point.position0.min(point.position1));
				high = high.max(point.position0.max(point.position1));
			}
			items.push(effect(Kind::Trail, Pass::Transparent, 1, (trail.points.len() as u32 - 1) * 2, ItemBase { bounds: (low, high), center: (low + high) * 0.5, path: trail.path.clone() }, false, f32::INFINITY, false));
			trail_items.insert(items.len() - 1, trail.clone());
		}

		// GUIs and adornments, read from the document: a BillboardGui that shows something is a UI draw
		// whatever it holds, SurfaceGuis add none to the counted passes (v1, v2, k1); a
		// SelectionBox is twelve (an edge each), a handle adornment one (v1)
		let anchors: HashMap<&str, (Vec3, [f32; 4])> = scene
			.guis
			.iter()
			.filter_map(|gui| match &gui.kind {
				crate::scene::guis::GuiKind::Billboard(billboard) => Some((gui.path.as_str(), (billboard.anchor + billboard.world_offset, billboard.size))),
				_ => None,
			})
			.collect();
		let mut stack: Vec<rbx_dom_weak::types::Ref> = document.roots.clone();
		while let Some(referent) = stack.pop() {
			let Some(instance) = document.dom.get_by_ref(referent) else { continue };
			stack.extend(instance.children().iter().copied());
			let class = instance.class.as_str();
			if class == "BillboardGui" && props::bool(instance, "Enabled", true) {
				let path = document.path_of(referent);
				let elements = shows(document, referent);
				if let (Some(&(anchor, size)), true) = (anchors.get(path.as_str()), elements > 0) {
					let max_distance = props::f32(instance, "MaxDistance", f32::INFINITY);
					let max_distance = if max_distance > 0.0 { max_distance } else { f32::INFINITY };
					// culled by a world box around it (bb2: drawn 2-4 studs past the view's
					// edge at 25 studs, more as the camera tilts): its studs, its pixels at 30 studs
					let radius = (glam::Vec2::new(size[0] + size[1] * 0.04, size[2] + size[3] * 0.04).length() * 0.5).max(1.0);
					let base = ItemBase { bounds: (anchor - Vec3::splat(radius), anchor + Vec3::splat(radius)), center: anchor, path };
					items.push(effect(Kind::Billboard, Pass::Ui, 1, elements as u32 * 10, base, false, max_distance, props::bool(instance, "AlwaysOnTop", false)));
				}
			}
			let draws = match class {
				"SelectionBox" => 12,
				"BoxHandleAdornment" | "SphereHandleAdornment" | "CylinderHandleAdornment" | "ConeHandleAdornment" | "LineHandleAdornment" | "ImageHandleAdornment" | "SelectionSphere" => 1,
				_ => 0,
			};
			if draws > 0 && props::bool(instance, "Visible", true) {
				let target = props::reference(instance, "Adornee").filter(|target| target.is_some()).unwrap_or_else(|| instance.parent());
				let part = document.dom.get_by_ref(target).filter(|part| props::is_a(part, "BasePart"));
				if let Some(part) = part
					&& let Some(cframe) = props::cframe(part, "CFrame")
				{
					let bounds = world_box(&cframe, props::vec3(part, "Size", Vec3::new(4.0, 1.0, 2.0)));
					let base = ItemBase { bounds, center: (bounds.0 + bounds.1) * 0.5, path: document.path_of(referent) };
					items.push(effect(Kind::Adornment, Pass::Ui, draws, 12, base, false, f32::INFINITY, props::bool(instance, "AlwaysOnTop", false)));
				}
			}
			// a Humanoid with a Head shows its name over it (h1: a UI draw per R15 and R6 rig,
			// none for a model of blocks)
			if class == "Humanoid" && props::enum_value(instance, "DisplayDistanceType", 0) != 2 {
				let head = document
					.dom
					.get_by_ref(instance.parent())
					.and_then(|model| model.children().iter().filter_map(|child| document.dom.get_by_ref(*child)).find(|child| child.name == "Head" && props::is_a(child, "BasePart")));
				if let Some(head) = head
					&& let Some(cframe) = props::cframe(head, "CFrame")
				{
					let center = Vec3::from(cframe.translation);
					let base = ItemBase { bounds: (center - Vec3::ONE, center + Vec3::ONE), center, path: document.path_of(referent) };
					items.push(effect(Kind::NameTag, Pass::Ui, 1, 60, base, false, props::f32(instance, "NameDisplayDistance", 100.0), false));
				}
			}
		}

		// FastClusters are drawn whole: each part's box becomes its cluster's
		let mut boxes: HashMap<Cluster, (Vec3, Vec3)> = HashMap::new();
		for item in items.iter().filter(|item| matches!(item.cluster, Cluster::Character(..) | Cluster::Skinned(..))) {
			let entry = boxes.entry(item.cluster).or_insert(item.bounds);
			*entry = (entry.0.min(item.bounds.0), entry.1.max(item.bounds.1));
		}
		for item in &mut items {
			item.cluster_bounds = boxes.get(&item.cluster).copied();
		}
		if !matches!(technology, 4 | 5) {
			warnings.push(format!("Lighting.Technology {technology}: the rules were measured under Future and Unified"));
		}
		// Decals and Textures on meshes: counted as the mesh's triangles with a positive component toward the
		// face, which Studio does not match (notes/cost.md "Decals and Textures on meshes", case e146)
		let (mesh_decals, mesh_decal_triangles) = items
			.iter()
			.filter(|item| matches!(item.kind, Kind::Decal | Kind::Texture))
			.filter(|item| item.part.is_some_and(|index| matches!(scene.parts[index].geometry, Geometry::MeshPart { .. } | Geometry::Union { .. })))
			.fold((0u32, 0u32), |(count, sum), item| (count + 1, sum + item.triangles.first().copied().unwrap_or(0)));
		if mesh_decals >= 20 && mesh_decal_triangles >= 20_000 {
			warnings.push(format!(
				"{mesh_decals} Decals or Textures on meshes count {mesh_decal_triangles} triangles: a face's share of a mesh is not calibrated, Studio drew about a quarter of it (108 Textures on 18 meshes: 145k here, 35.7k and 72 of 108 draws in Studio); read those triangles and draws as an upper bound"
			));
		}
		let mesh_streaming = document
			.dom
			.root()
			.children()
			.iter()
			.filter_map(|child| document.dom.get_by_ref(*child))
			.find(|child| child.class == "Workspace")
			.and_then(|workspace| match props::get(workspace, "MeshStreamingAndImprovedLoDs") {
				Some(rbx_dom_weak::types::Variant::Enum(value)) => Some(value.to_u32()),
				_ => None,
			});
		let environment = &scene.environment;
		Analysis {
			tested_groups: (0..items.len()).map(|_| std::sync::OnceLock::new()).collect(),
			items,
			labels: labels.names,
			geometry_meshes,
			occluders,
			not_occluders,
			meshes,
			characters,
			technology,
			quality,
			clouds: environment.clouds.is_some(),
			sun: crate::render::environment::sun_direction(environment.clock_time, environment.geographic_latitude),
			moon: crate::render::environment::moon_direction(environment.clock_time, environment.geographic_latitude),
			// a place without a Sky draws Roblox's default one: sun 21 degrees, moon 11
			celestial: match &environment.sky {
				Some(sky) if !sky.celestial_bodies_shown => (0.0, 0.0),
				Some(sky) => (sky.sun_angular_size, sky.moon_angular_size),
				None => (21.0, 11.0),
			},
			warnings,
			mesh_streaming,
			emitters: emitter_items,
			beams: beam_items,
			trails: trail_items,
		}
	}
}

/// A cheap mesh's triangles placed in the world as a part draws them (its box scaled to the Size).
fn mesh_occluder(stats: &MeshStats, part: &Part) -> Option<Vec<[Vec3; 3]>> {
	let triangles = stats.occluder.as_ref()?;
	let (low, high) = stats.bounds;
	let extent = (high - low).max(Vec3::splat(1e-4));
	let middle = (low + high) * 0.5;
	let scale = part.size / extent;
	Some(triangles.iter().map(|triangle| triangle.map(|point| part.cframe.transform_point3((point - middle) * scale))).collect())
}

/// A batch a view draws: its draw calls, instances and triangles.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Batch {
	pub label: String,
	pub pass: Pass,
	pub kind: Kind,
	pub draws: u64,
	pub instances: u64,
	pub triangles: u64,
	/// Sorted by depth: its instances draw apart unless they lie at one depth.
	#[serde(skip_serializing_if = "std::ops::Not::not")]
	pub sorted: bool,
	/// Every draw of it is wasted (nothing of it shows).
	#[serde(skip_serializing_if = "std::ops::Not::not")]
	pub invisible: bool,
	/// Paths of a few of its instances.
	pub examples: Vec<String>,
	/// Its items (indices into [`Analysis::items`]).
	#[serde(skip)]
	pub items: Vec<usize>,
}

impl Batch {
	pub fn weight(&self) -> f64 {
		self.draws as f64 + self.triangles as f64 / TRIANGLES_PER_DRAW
	}
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Visible {
	pub parts: u64,
	pub transparent_parts: u64,
	pub meshes: u64,
	pub unions: u64,
	pub decals: u64,
	pub emitters: u64,
	pub particles: u64,
	/// How many viewports' worth of pixels the particles in view cover (each particle's square on
	/// screen, overlaps counted each time): the blending work they give the GPU, which on phones is
	/// what transparent effects cost most.
	pub particle_fill: f64,
	/// The emitters covering most of the view, the most first (at most [`FULLEST`]).
	pub fullest: Vec<EmitterFill>,
	pub beams: u64,
	pub invisible_beams: u64,
	pub trails: u64,
	pub billboards: u64,
	pub highlights: u64,
	pub adornments: u64,
	pub characters: u64,
}

/// Emitters a view lists as covering most of it.
pub const FULLEST: usize = 8;

/// An emitter's share of a view's particle fill.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmitterFill {
	pub path: String,
	/// Viewports' worth of pixels its particles cover.
	pub fill: f64,
	pub particles: u64,
}

/// What occlusion culling does for one view: the objects it tests, the occluders it rasterizes
/// first, and an estimate of its CPU time (notes/occlusion.md "Cost").
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OcclusionWork {
	/// Objects in the view the culler tests against its depth buffer, each by its screen box.
	pub tested: u64,
	/// Objects in the view it never tests (FastClusters: Humanoid models, skinned meshes).
	pub untested: u64,
	/// Parts able to occlude in the view (opaque, large enough, a mesh simple enough for its size).
	pub candidates: u64,
	/// Of them, meshes, and their triangles: what rasterizing them costs (b8: 20-30 ns a triangle on a
	/// desktop core, a block next to nothing).
	pub candidate_meshes: u64,
	pub candidate_mesh_triangles: u64,
	/// Those rasterized: the ones covering most of the view, up to the budget.
	pub occluders: u64,
	/// Of them, meshes; and their distinct vertex positions (a block counts 8).
	pub mesh_occluders: u64,
	pub occluder_positions: u64,
	/// Their pixels on a 256-pixel-wide view (overlaps counted each time), and their triangles.
	pub occluder_pixels: f64,
	pub occluder_triangles: u64,
	/// The tested objects' screen boxes in pixels of a 256-pixel-wide view, all of them and the
	/// hidden ones (a hidden box is checked to its last pixel).
	pub tested_pixels: f64,
	pub hidden_pixels: f64,
	/// Objects it hid.
	pub hidden: u64,
	/// Each occluder rasterized, the largest on screen first, with what it hides (in cost.json; the
	/// job's report keeps the first `top`).
	pub used: Vec<OccluderUse>,
	/// Large opaque parts that do not occlude, and what a block inside each would hide.
	pub missed: Vec<MissedOccluder>,
	/// Where the tested objects are: their model or folder (up to three names deep), most first.
	pub groups: Vec<TestedGroup>,
	/// Milliseconds of CPU time `queryOcclusion` takes on the culling jobs, from the counts above
	/// with the coefficients fitted to Studio's MicroProfiler ([`occlusion_ms`]).
	pub estimate_ms: f64,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OccluderUse {
	pub path: String,
	/// A mesh's asset id.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub mesh_id: Option<String>,
	/// A mesh's distinct vertex positions (8 for a part's own shape), and its triangles.
	pub positions: u32,
	pub triangles: u32,
	pub mesh: bool,
	pub pixels: f32,
	/// Objects it hides (those most of whose screen box it covers), and their triangles.
	pub hidden: u64,
	pub hidden_triangles: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MissedOccluder {
	pub path: String,
	pub reason: String,
	pub pixels: f32,
	/// Objects drawn now that a block filling its box would hide, and their triangles.
	pub would_hide: u64,
	pub would_hide_triangles: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TestedGroup {
	pub group: String,
	pub tested: u64,
	pub hidden: u64,
}

/// An instance path's names: `A.B["C.d"].E` is A, B, C.d, E.
pub fn path_names(path: &str) -> Vec<&str> {
	let mut names = Vec::new();
	let mut rest = path;
	while !rest.is_empty() {
		if let Some(quoted) = rest.strip_prefix("[\"") {
			let end = quoted.find("\"]").unwrap_or(quoted.len());
			names.push(&quoted[..end]);
			rest = quoted.get(end + 2..).unwrap_or("");
		} else {
			let end = rest.find(['.', '[']).unwrap_or(rest.len());
			if end > 0 {
				names.push(&rest[..end]);
			}
			rest = &rest[end..];
		}
		rest = rest.strip_prefix('.').unwrap_or(rest);
	}
	names
}

/// The model or folder an object belongs to for [`TestedGroup`]: its nearest ancestor with a name of
/// its own (not "Model", "Folder", "default", ...), so that copies of one prop group together
/// wherever they stand; loose parts by the folder holding them.
fn tested_group(path: &str) -> String {
	const GENERIC: [&str; 12] = ["model", "folder", "default", "part", "meshpart", "union", "mesh", "meshes", "group", "assets", "props", "workspace"];
	let names = path_names(path);
	let parents = &names[..names.len().saturating_sub(1)];
	match parents.iter().rposition(|name| {
		let name = name.to_lowercase();
		!GENERIC.contains(&name.as_str()) && !name.chars().all(|c| c.is_ascii_digit())
	}) {
		Some(index) if index > 0 => format!("{} ({})", parents[index], parents[..index].join(".")),
		_ => format!("loose parts in {}", parents.join(".")),
	}
}

/// `queryOcclusion`'s CPU time for a view's work, in milliseconds of a desktop core (fitted on this
/// machine: phones take several times as long). Most of it is rasterizing mesh occluders (ablate,
/// b8: 20-30 ns a triangle, a block next to nothing); Roblox's culler picks and drops occluders by
/// its own budget, so per view this is an estimate (472 views of ten maps: half the variance between
/// views explained, the median view within 19 %); `--check --profile` measures it.
pub fn occlusion_ms(work: &OcclusionWork) -> f64 {
	OCCLUSION_MS_BASE
		+ work.tested as f64 * OCCLUSION_MS_PER_TEST
		+ work.candidate_meshes as f64 * OCCLUSION_MS_PER_MESH
		+ work.occluder_positions as f64 * OCCLUSION_MS_PER_POSITION
		+ work.occluder_pixels * OCCLUSION_MS_PER_PIXEL
}

/// Fitted coefficients of [`occlusion_ms`] (`ror cost --check --profile` of 472 views of ten game maps,
/// least squares with no negative term; notes/occlusion.md "Cost").
const OCCLUSION_MS_BASE: f64 = 0.350;
const OCCLUSION_MS_PER_TEST: f64 = 38e-6;
const OCCLUSION_MS_PER_MESH: f64 = 1.15e-3;
const OCCLUSION_MS_PER_POSITION: f64 = 8.38e-6;
const OCCLUSION_MS_PER_PIXEL: f64 = 0.0995e-6;

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Culled {
	/// Outside the view.
	pub frustum: u64,
	/// Hidden behind occluders.
	pub occluded: u64,
	/// Beyond their MaxDistance.
	pub distance: u64,
	/// Left out by the graphics quality: past its view distance, or too small on screen (crate::lod).
	pub quality: u64,
}

/// A view's cost, as Roblox's counters would report it from that camera.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewCost {
	pub name: String,
	/// Every pass but Shadows (Roblox's guidance leaves them out of budgets).
	pub total: Count,
	pub passes: BTreeMap<Pass, Count>,
	pub visible: Visible,
	pub culled: Culled,
	pub occlusion: OcclusionWork,
	/// Every batch drawn, the costliest first ([`Batch::weight`]); summary.json keeps the top ones.
	pub batches: Vec<Batch>,
	pub opportunities: Vec<report::Opportunity>,
	/// What RenderFidelity Performance would change for each mesh with a level-of-detail chain
	/// that draws full detail now.
	#[serde(skip_serializing_if = "Vec::is_empty")]
	pub level_gains: Vec<report::LevelGain>,
	/// Where the draws go: draw calls and the instances they draw, by kind (batches of copies,
	/// parts drawn alone, decals, sorted parts, characters, effects, GUIs, the sky).
	pub breakdown: BTreeMap<&'static str, (u64, u64)>,
	/// Each item drawn (index into [`Analysis::items`], its level of detail, the triangles its draws
	/// take), for `items.json` (`--items`).
	#[serde(skip)]
	pub drawn: Vec<(u32, u8, u64)>,
	/// Each item in view that occlusion culling hides, for `items.json`.
	#[serde(skip)]
	pub hidden: Vec<u32>,
}

/// One entry of `items.json`'s table (`ror cost --items`): a thing the scene draws, where it is and
/// what it costs at each level of detail.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ItemEntry<'a> {
	path: &'a str,
	/// Its part's index in the scene (`ror render --ids` numbers parts the same way, plus one).
	#[serde(skip_serializing_if = "Option::is_none")]
	part: Option<usize>,
	kind: Kind,
	pass: Pass,
	/// The mesh's asset id or content key (MeshParts and unions), if any.
	#[serde(skip_serializing_if = "Option::is_none")]
	mesh: Option<&'a str>,
	/// Triangles per level of detail (one draw's).
	triangles: &'a [u32],
	draws: u32,
	/// RenderFidelity's hold on the chain (0 Automatic, 1 Precise, 2 Performance), when it steps.
	#[serde(skip_serializing_if = "Option::is_none")]
	fidelity: Option<u32>,
	min: [f32; 3],
	max: [f32; 3],
	center: [f32; 3],
	transparent: bool,
	/// Its part's CastShadow, Transparency, Size and class.
	#[serde(skip_serializing_if = "Option::is_none")]
	cast_shadow: Option<bool>,
	#[serde(skip_serializing_if = "Option::is_none")]
	part_transparency: Option<f32>,
	#[serde(skip_serializing_if = "Option::is_none")]
	size: Option<[f32; 3]>,
	#[serde(skip_serializing_if = "Option::is_none")]
	class: Option<&'a str>,
}

/// `items.json`: every item the analysis holds and, per view, what it draws and what occlusion hides.
fn write_items(path: &std::path::Path, scene: &Scene, analysis: &Analysis, costs: &[ViewCost]) -> anyhow::Result<()> {
	let items: Vec<ItemEntry> = analysis
		.items
		.iter()
		.map(|item| {
			let part = item.part.and_then(|index| scene.parts.get(index));
			ItemEntry {
				path: &item.path,
				part: item.part,
				kind: item.kind,
				pass: item.pass,
				mesh: item.geometry.and_then(|geometry| analysis.geometry_meshes.get(&geometry)).map(|(key, _)| key.as_str()),
				triangles: &item.triangles,
				draws: item.draws,
				fidelity: item.lod.map(|lod| lod.fidelity),
				min: item.bounds.0.to_array(),
				max: item.bounds.1.to_array(),
				center: item.center.to_array(),
				transparent: item.transparent,
				cast_shadow: part.map(|part| part.cast_shadow),
				part_transparency: part.map(|part| part.transparency),
				size: part.map(|part| part.size.to_array()),
				class: part.map(|part| part.class.as_str()),
			}
		})
		.collect();
	let views: Vec<serde_json::Value> = costs
		.iter()
		.map(|cost| serde_json::json!({ "name": cost.name, "drawn": cost.drawn, "hidden": cost.hidden }))
		.collect();
	crate::output::write_json_atomic(path, &serde_json::json!({ "items": items, "views": views }), false)
}

/// What kind of draw a batch is, for [`ViewCost::breakdown`].
fn breakdown_kind(analysis: &Analysis, batch: &Batch) -> &'static str {
	let first = &analysis.items[batch.items[0]];
	match batch.kind {
		Kind::Part => match first.cluster {
			Cluster::Character(..) => "character draws",
			Cluster::Skinned(..) => "skinned-mesh clusters",
			Cluster::Static if batch.sorted => "sorted parts (ForceField, transparent with decals)",
			Cluster::Static if batch.instances > 1 => "batches of copies",
			Cluster::Static => "parts drawn alone",
		},
		Kind::Decal | Kind::Texture => "decal batches",
		Kind::Highlight => "highlight draws",
		Kind::Emitter => "emitters",
		Kind::Beam => "beams",
		Kind::Trail => "trails",
		Kind::Billboard | Kind::Adornment | Kind::NameTag => "GUIs and adornments",
	}
}

/// Draws of the sky: one per face of the skybox the view sees, and the sun and moon (each a square
/// of its angular size) when in view (sky.luau: 1 at FOV 20 looking level, 3 at 70, 7 at 120).
fn sky(camera: &Camera, aspect: f32, sun: Vec3, moon: Vec3, (sun_size, moon_size): (f32, f32)) -> Count {
	let culler = Culler::new(&Camera { eye: Vec3::ZERO, world: glam::Affine3A { translation: glam::Vec3A::ZERO, ..camera.world }, ..*camera }, aspect);
	let mut count = Count::default();
	const DISTANCE: f32 = 1000.0;
	for axis in [Vec3::X, Vec3::Y, Vec3::Z, Vec3::NEG_X, Vec3::NEG_Y, Vec3::NEG_Z] {
		let center = axis * DISTANCE;
		let half = (Vec3::ONE - axis.abs()) * DISTANCE;
		if culler.in_frustum((center - half - axis.abs(), center + half + axis.abs())) {
			count.add(1, 9);
		}
	}
	for (direction, size) in [(sun, sun_size), (moon, moon_size)] {
		if size <= 0.0 {
			continue;
		}
		let point = direction.normalize_or_zero() * DISTANCE;
		let half = (size.to_radians() * 0.5).tan() * DISTANCE;
		if culler.in_frustum((point - Vec3::splat(half), point + Vec3::splat(half))) {
			count.add(1, 2);
		}
	}
	count
}

impl Analysis {
	/// The cost from one camera (`aspect`: the view's width over its height).
	pub fn view(&self, name: &str, camera: &Camera, aspect: f32, options: &CostOptions) -> ViewCost {
		let mut culler = Culler::new(camera, aspect);
		// the quality's view distance and size cull (crate::lod)
		let lod = crate::lod::Lod { quality: self.quality, cull: true };
		let mut occlusion = OcclusionWork::default();
		// meshes in the view too detailed for their size there (missed occluders)
		let too_detailed: std::cell::RefCell<Vec<(f32, &Occluder, u32)>> = std::cell::RefCell::new(Vec::new());
		if !options.no_occlusion {
			// the occluders covering most of the view, as many as Roblox's budget takes: in the view, not
			// around the camera, large enough, and a mesh simple enough for its size on screen
			let mut ranked: Vec<(f32, &Occluder)> = self
				.occluders
				.iter()
				.filter(|occluder| {
					let local = occluder.cframe.inverse().transform_point3(camera.eye).abs();
					if local.cmple(occluder.size * 0.5).all() || !culler.in_frustum(occluder.bounds) {
						return false;
					}
					let (pixels, side) = culler.screen_extent(occluder.bounds);
					match occluder.positions {
						Some(positions) if positions as f32 > OCCLUDER_POSITIONS_PER_PIXEL * pixels => {
							too_detailed.borrow_mut().push((pixels, occluder, positions));
							false
						}
						_ => side >= OCCLUDER_MIN_PIXELS,
					}
				})
				.map(|occluder| {
					// how much of the view it covers: each face pair's area as seen from the camera
					let center = Vec3::from(occluder.cframe.translation);
					let to = center - camera.eye;
					let distance = to.length().max(1.0);
					let direction = to / distance;
					let half = occluder.size * 0.5;
					let axes = [Vec3::from(occluder.cframe.matrix3.x_axis), Vec3::from(occluder.cframe.matrix3.y_axis), Vec3::from(occluder.cframe.matrix3.z_axis)];
					let area = half.y * half.z * direction.dot(axes[0]).abs() + half.x * half.z * direction.dot(axes[1]).abs() + half.x * half.y * direction.dot(axes[2]).abs();
					(area / (distance * distance), occluder)
				})
				.collect();
			ranked.sort_by(|a, b| b.0.total_cmp(&a.0));
			let budget: usize = std::env::var("ROR_COST_OCCLUDERS").ok().and_then(|value| value.parse().ok()).unwrap_or(OCCLUDER_BUDGET);
			occlusion.candidates = ranked.len() as u64;
			for (_, occluder) in &ranked {
				if occluder.positions.is_some() {
					occlusion.candidate_meshes += 1;
					occlusion.candidate_mesh_triangles += occluder.triangles.len() as u64;
				}
			}
			for (rank, (_, occluder)) in ranked.into_iter().take(budget).enumerate() {
				let pixels = culler.screen_extent(occluder.bounds).0;
				occlusion.occluders += 1;
				occlusion.occluder_positions += occluder.positions.unwrap_or(8) as u64;
				occlusion.mesh_occluders += occluder.positions.is_some() as u64;
				occlusion.occluder_pixels += pixels as f64;
				occlusion.occluder_triangles += occluder.triangles.len() as u64;
				occlusion.used.push(OccluderUse {
					path: occluder.path.clone(),
					mesh_id: occluder.mesh.clone(),
					positions: occluder.positions.unwrap_or(8),
					triangles: occluder.triangles.len() as u32,
					mesh: occluder.positions.is_some(),
					pixels,
					hidden: 0,
					hidden_triangles: 0,
				});
				culler.occlude_as(&occluder.triangles, rank as u16);
			}
		}
		// tested objects drawn, for the missed occluders below; tested objects by group
		let mut shown: Vec<(usize, u64)> = Vec::new();
		let mut tested_groups: HashMap<&str, (u64, u64)> = HashMap::new();
		let mut culled = Culled::default();
		let mut visible = Visible::default();
		// batches by label, depth (sorted ones) and level of detail; items without a label alone
		let mut groups: HashMap<(u32, i64, u8, usize), usize> = HashMap::new();
		let mut batches: Vec<Batch> = Vec::new();
		let mut particles_pass = 0u64;
		// the transparent queue: (depth, label (MAX: alone), pass, item, triangles)
		let mut queue: Vec<(f32, u32, u8, usize, u64)> = Vec::new();
		let mut characters_seen: HashSet<u32> = HashSet::new();
		let record = options.items;
		let mut drawn: Vec<(u32, u8, u64)> = Vec::new();
		let mut hidden: Vec<u32> = Vec::new();
		let mut character_opaque: HashMap<u32, (HashSet<u32>, u64)> = HashMap::new();
		let look = -Vec3::from(camera.world.matrix3.z_axis);
		let mut fills: Vec<EmitterFill> = Vec::new();
		for (index, stored) in self.items.iter().enumerate() {
			// an emitter on a CameraFollow part stands under this view's camera, as a game's client
			// keeps it (scene::follow)
			let moved;
			let (item, offset) = match self.emitters.get(&index).and_then(|emitter| emitter.follow) {
				Some(follow) => {
					let offset = follow.offset(camera.eye, look);
					moved = Item { bounds: (stored.bounds.0 + offset, stored.bounds.1 + offset), center: stored.center + offset, ..stored.clone() };
					(&moved, offset)
				}
				None => (stored, Vec3::ZERO),
			};
			if !culler.in_frustum(item.cluster_bounds.unwrap_or(item.bounds)) {
				culled.frustum += 1;
				continue;
			}
			// the particle pass takes every emitter in view, hidden or not (o1: emitters behind
			// a wall lose their own draws, not the pass's)
			if item.kind == Kind::Emitter {
				particles_pass += item.triangles[0] as u64;
			}
			let distance = item.center.distance(camera.eye);
			if distance > item.max_distance {
				culled.distance += 1;
				continue;
			}
			if let Some(diameter) = item.cull
				&& !lod.drawn(camera.eye, camera.fov, item.bounds, item.center, diameter)
			{
				culled.quality += 1;
				continue;
			}
			let clustered = matches!(item.cluster, Cluster::Character(..) | Cluster::Skinned(..));
			let tested = !item.always && !clustered;
			if !item.always {
				if clustered {
					occlusion.untested += 1;
				} else {
					occlusion.tested += 1;
				}
			}
			let (level, triangles) = item_level(item, camera.eye, camera.fov);
			if tested && !options.no_occlusion {
				let group = tested_groups.entry(self.tested_groups[index].get_or_init(|| tested_group(&item.path)).as_str()).or_default();
				group.0 += 1;
				let pixels = culler.screen_extent(item.bounds).0 as f64;
				occlusion.tested_pixels += pixels;
				if !culler.visible(item.bounds) {
					occlusion.hidden_pixels += pixels;
					group.1 += 1;
					if let Some(owner) = culler.hidden_by(item.bounds).and_then(|owner| occlusion.used.get_mut(owner as usize)) {
						owner.hidden += 1;
						owner.hidden_triangles += triangles;
					}
					culled.occluded += 1;
					if record {
						hidden.push(index as u32);
					}
					continue;
				}
				shown.push((index, triangles));
			}
			if record {
				drawn.push((index as u32, level as u8, triangles * item.draws as u64));
			}
			let depth = if item.sorted { (culler.depth_of(item.center) * 100.0).round() as i64 } else { 0 };
			// copies batch per distance band from the camera, 600 and 1,200 studs apart (m8: two blocks
			// or meshes at 50 and 602 studs two draws, at 900 and 1,250 two, none further to 6,000)
			let band = if item.cluster == Cluster::Static { distance_band(distance) as u8 } else { 0 };
			let key = match item.label {
				Some(label) => (label, depth, (level as u8) << 4 | band, usize::MAX),
				None => (u32::MAX, 0, 0, index),
			};
			if item.queued > 0 {
				let depth = culler.depth_of(item.center);
				for pass in 0..item.queued {
					queue.push((depth, item.label.unwrap_or(u32::MAX), pass, index, triangles));
				}
			}
			let batch = if item.queued > 0 { usize::MAX } else { *groups.entry(key).or_insert_with(|| {
				batches.push(Batch {
					label: match item.label {
						Some(label) => self.labels[label as usize].clone(),
						None => format!("{:?} {}", item.kind, item.path),
					},
					pass: item.pass,
					kind: item.kind,
					draws: item.draws as u64,
					instances: 0,
					triangles: 0,
					sorted: item.sorted,
					invisible: item.invisible,
					examples: Vec::new(),
					items: Vec::new(),
				});
				batches.len() - 1
			}) };
			if batch != usize::MAX {
				let entry = &mut batches[batch];
				entry.instances += 1;
				entry.triangles += triangles * item.draws as u64;
				if entry.examples.len() < 3 && !entry.examples.contains(&item.path) {
					entry.examples.push(item.path.clone());
				}
				entry.items.push(index);
			}
			match item.kind {
				Kind::Part => {
					visible.parts += 1;
					if item.transparent {
						visible.transparent_parts += 1;
					}
					if let Cluster::Character(id, _) = item.cluster {
						characters_seen.insert(id);
						if item.pass == Pass::Opaque {
							let entry = character_opaque.entry(id).or_default();
							entry.0.extend(item.appearance);
							entry.1 += triangles;
						}
					}
					match item.geometry.and_then(|geometry| self.geometry_meshes.get(&geometry)) {
						Some((_, true)) => visible.unions += 1,
						Some((_, false)) => visible.meshes += 1,
						None => {}
					}
				}
				Kind::Decal | Kind::Texture => visible.decals += 1,
				Kind::Highlight => visible.highlights += 1,
				Kind::Emitter => {
					visible.emitters += 1;
					visible.particles += triangles / 2;
					if let Some(emitter) = self.emitters.get(&index) {
						let fill = emitter.fill(camera, aspect, offset) as f64;
						visible.particle_fill += fill;
						fills.push(EmitterFill { path: item.path.clone(), fill, particles: triangles / 2 });
					}
				}
				Kind::Beam => {
					visible.beams += 1;
					if item.invisible {
						visible.invisible_beams += 1;
					}
					if let Some(beam) = self.beams.get(&index) {
						let (mut vertices, back) = (Vec::new(), Vec3::from(camera.world.matrix3.z_axis));
						crate::render::beams::triangles(beam, camera.eye, back, &mut vertices);
						let fill = ribbon_fill(&vertices, camera, aspect);
						visible.particle_fill += fill;
						fills.push(EmitterFill { path: item.path.clone(), fill, particles: 0 });
					}
				}
				Kind::Trail => {
					visible.trails += 1;
					if let Some(trail) = self.trails.get(&index) {
						let mut vertices = Vec::new();
						crate::render::trails::triangles(trail, camera.eye, &mut vertices);
						let fill = ribbon_fill(&vertices, camera, aspect);
						visible.particle_fill += fill;
						fills.push(EmitterFill { path: item.path.clone(), fill, particles: 0 });
					}
				}
				Kind::Billboard => visible.billboards += 1,
				Kind::Adornment => visible.adornments += 1,
				Kind::NameTag => {}
			}
		}
		fills.sort_by(|a, b| b.fill.total_cmp(&a.fill));
		if !options.all_emitters {
			fills.truncate(FULLEST);
		}
		visible.fullest = fills;
		// the queue back to front, ties by batch: a run of one batch is one draw (m10)
		queue.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)).then(a.3.cmp(&b.3)));
		let mut previous: Option<(u32, u8)> = None;
		for (_, label, pass, index, triangles) in queue {
			let item = &self.items[index];
			let continues = label != u32::MAX && previous == Some((label, pass));
			previous = Some((label, pass));
			if !continues {
				batches.push(Batch {
					label: match item.label {
						Some(label) => self.labels[label as usize].clone(),
						None => format!("{:?} {}", item.kind, item.path),
					},
					pass: item.pass,
					kind: item.kind,
					draws: item.draws as u64,
					instances: 0,
					triangles: 0,
					sorted: true,
					invisible: item.invisible,
					examples: Vec::new(),
					items: Vec::new(),
				});
			}
			let entry = batches.last_mut().expect("a batch");
			if pass == 0 {
				entry.instances += 1;
			}
			entry.items.push(index);
			entry.triangles += triangles * item.draws as u64;
			if entry.examples.len() < 3 && !entry.examples.contains(&item.path) {
				entry.examples.push(item.path.clone());
			}
		}
		let mut passes: BTreeMap<Pass, Count> = BTreeMap::new();
		for batch in &batches {
			passes.entry(batch.pass).or_default().add(batch.draws, batch.triangles);
		}
		// the particle pass: one draw over every live particle in view (p1)
		if particles_pass > 0 {
			passes.entry(Pass::Particles).or_default().add(1, particles_pass);
		}
		let sky = sky(camera, aspect, self.sun, self.moon, self.celestial);
		if sky.draws > 0 {
			passes.entry(Pass::Sky).or_default().add(sky.draws, sky.triangles);
		}
		// Terrain's Clouds: five cloud draws and a transparent one (x1)
		if self.clouds {
			passes.entry(Pass::Cloud).or_default().add(5, 5);
			passes.entry(Pass::Transparent).or_default().add(1, 1);
		}
		// characters under Unified: two post-process draws once any is in view, and dynamic shadows of
		// a draw per opaque appearance plus one (h1: blocks 2, three materials 4, R15 3, R6 2);
		// under Future neither (h2: their shadows are cached like the world's)
		visible.characters = characters_seen.len() as u64;
		if !characters_seen.is_empty() && self.technology == 5 {
			passes.entry(Pass::GenericPostProcess).or_default().add(2, 2);
			for id in &characters_seen {
				let (appearances, triangles) = character_opaque.get(id).cloned().unwrap_or_default();
				passes.entry(Pass::Shadows).or_default().add(appearances.len() as u64 + 1, triangles + 12);
			}
		}
		let mut total = Count::default();
		for (pass, count) in &passes {
			if *pass != Pass::Shadows {
				total.add(count.draws, count.triangles);
			}
		}
		let opportunities = report::opportunities(self, &batches);
		let level_gains = report::level_gains(self, &batches, camera.eye, camera.fov);
		let mut breakdown: BTreeMap<&'static str, (u64, u64)> = BTreeMap::new();
		for batch in &batches {
			let entry = breakdown.entry(breakdown_kind(self, batch)).or_default();
			entry.0 += batch.draws;
			entry.1 += batch.instances;
		}
		let fixed: u64 = [Pass::Sky, Pass::Cloud, Pass::Particles, Pass::GenericPostProcess].iter().filter_map(|pass| passes.get(pass)).map(|count| count.draws).sum();
		if fixed > 0 {
			breakdown.insert("sky, clouds and shared passes", (fixed, 0));
		}
		batches.sort_by(|a, b| b.weight().total_cmp(&a.weight()));
		occlusion.hidden = culled.occluded;
		if options.no_occlusion {
			occlusion.tested = 0;
		} else {
			// what a block filling each large non-occluder would hide of what is drawn now
			let mut missed: Vec<(f32, (Vec3, Vec3), &glam::Affine3A, Vec3, String, &String)> = self
				.not_occluders
				.iter()
				.filter(|part| {
					let local = part.cframe.inverse().transform_point3(camera.eye).abs();
					!local.cmple(part.size * 0.5).all() && culler.in_frustum(part.bounds)
				})
				.map(|part| (culler.screen_extent(part.bounds).0, part.bounds, &part.cframe, part.size, part.reason.clone(), &part.path))
				.collect();
			for (pixels, occluder, positions) in too_detailed.into_inner() {
				let reason = format!("a mesh of {positions} vertex positions, too detailed for {pixels:.0} pixels on screen here (it occludes from {:.0})", positions as f32 / OCCLUDER_POSITIONS_PER_PIXEL);
				missed.push((pixels, occluder.bounds, &occluder.cframe, occluder.size, reason, &occluder.path));
			}
			let least = MISSED_MIN_SHARE * cull::BUFFER_WIDTH as f32 * cull::BUFFER_WIDTH as f32 / aspect.max(0.01);
			missed.retain(|entry| entry.0 >= least);
			missed.sort_by(|a, b| b.0.total_cmp(&a.0));
			for (pixels, _, cframe, size, reason, path) in missed.into_iter().take(MISSED_PER_VIEW) {
				let mut trial = culler.clone();
				for triangle in cull::block_triangles(cframe, size) {
					trial.occlude(triangle);
				}
				let mut entry = MissedOccluder { path: path.clone(), reason, pixels, would_hide: 0, would_hide_triangles: 0 };
				for &(index, triangles) in &shown {
					// not what lies inside the part itself
					let item = &self.items[index];
					if item.path != entry.path && !trial.visible(item.bounds) {
						entry.would_hide += 1;
						entry.would_hide_triangles += triangles;
					}
				}
				if entry.would_hide > 0 {
					occlusion.missed.push(entry);
				}
			}
			occlusion.missed.sort_by(|a, b| b.would_hide.cmp(&a.would_hide));
			let mut groups: Vec<TestedGroup> = tested_groups.into_iter().map(|(group, (tested, hidden))| TestedGroup { group: group.to_string(), tested, hidden }).collect();
			groups.sort_by(|a, b| b.tested.cmp(&a.tested).then(a.group.cmp(&b.group)));
			groups.truncate(12);
			occlusion.groups = groups;
		}
		occlusion.estimate_ms = if options.no_occlusion { 0.0 } else { occlusion_ms(&occlusion) };
		ViewCost { name: name.to_string(), total, passes, visible, culled, occlusion, batches, opportunities, level_gains, breakdown, drawn, hidden }
	}

	/// What the whole scene holds, whatever a view sees.
	pub fn scene_counts(&self) -> SceneCounts {
		let mut counts = SceneCounts::default();
		let mut batches = HashSet::new();
		let mut alone = 0;
		let mut meshes = HashSet::new();
		for item in &self.items {
			match item.label {
				Some(label) if !item.sorted => {
					batches.insert(label);
				}
				_ => alone += 1,
			}
			match item.kind {
				Kind::Part => {
					counts.parts += 1;
					if item.transparent {
						counts.transparent_parts += 1;
					}
					match item.geometry.and_then(|geometry| self.geometry_meshes.get(&geometry).map(|mesh| (geometry, mesh))) {
						Some((_, (_, true))) => counts.unions += 1,
						Some((geometry, (_, false))) => {
							counts.meshes += 1;
							meshes.insert(geometry);
						}
						None => {}
					}
				}
				Kind::Decal | Kind::Texture => counts.decals += 1,
				Kind::Emitter => counts.emitters += 1,
				Kind::Beam => {
					counts.beams += 1;
					if item.invisible {
						counts.invisible_beams += 1;
					}
				}
				Kind::Trail => counts.trails += 1,
				Kind::Billboard => counts.billboards += 1,
				Kind::Highlight => counts.highlights += 1,
				_ => {}
			}
		}
		counts.unique_meshes = meshes.len() as u64;
		counts.meshes_without_lods = self.meshes.stats.values().filter(|stats| stats.lods.len() == 1 && !stats.skinned).count() as u64;
		counts.characters = self.characters.len() as u64;
		counts.batches = batches.len() as u64 + alone;
		counts
	}
}

/// What the whole scene holds, whatever a view sees.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SceneCounts {
	pub parts: u64,
	pub transparent_parts: u64,
	pub meshes: u64,
	pub unique_meshes: u64,
	/// Distinct meshes whose file has no level-of-detail chain (no step down whatever the distance).
	pub meshes_without_lods: u64,
	pub unions: u64,
	pub decals: u64,
	pub emitters: u64,
	pub beams: u64,
	pub invisible_beams: u64,
	pub trails: u64,
	pub billboards: u64,
	pub highlights: u64,
	pub characters: u64,
	/// Batches were everything in view at once (no culling).
	pub batches: u64,
}

/// The cost of a job's views, and the opportunities over them all.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CostReport {
	pub technology: u32,
	pub quality: u32,
	pub occlusion: bool,
	/// Workspace.MeshStreamingAndImprovedLoDs as the file sets it (0 Default, 1 Disabled, 2 Enabled).
	#[serde(skip_serializing_if = "Option::is_none")]
	pub mesh_streaming: Option<u32>,
	pub scene: SceneCounts,
	pub views: Vec<ViewCost>,
	/// The views' opportunities, what they would save on average over the views first.
	pub opportunities: Vec<report::Summary>,
	/// The report as text (`ror cost` prints it).
	pub lines: Vec<String>,
	pub warnings: Vec<String>,
}

/// A view's cost images and numbers for a job: the analysis of `scene` (read from `document`), each
/// view's cost from its camera and size (`shots`), `<view>.draws.png` and `<view>.heat.png` (unless
/// `no_images`), and `cost.json` with every batch. The report returned keeps each view's costliest
/// batches only.
pub fn run(
	renderer: &crate::render::Renderer,
	scene: &Scene,
	document: &Document,
	options: &CostOptions,
	views: &[crate::job::NamedView],
	shots: &[(Camera, u32, u32)],
	out: &std::path::Path,
	gpu: Option<&crate::budget::GpuGate>,
) -> anyhow::Result<CostReport> {
	let analysis = Analysis::new(scene, document, &renderer.assets, options);
	// each view on every core (a map's 20-stud grid, 440 views: 4.6 s one after another)
	let costs: Vec<ViewCost> = views.par_iter().zip(shots.par_iter()).map(|(view, (camera, width, height))| analysis.view(&view.name, camera, *width as f32 / (*height).max(1) as f32, options)).collect();
	let mut grid_lines = Vec::new();
	if let Some(spacing) = options.grid {
		let cameras: Vec<Camera> = shots.iter().map(|(camera, _, _)| *camera).collect();
		let cells = grid::cells(&costs, &cameras);
		grid::write(&cells, spacing, out)?;
		if !options.no_images && !cells.is_empty() {
			let (spec, width, height) = grid::overhead(&cells, spacing);
			let camera = spec.place(None, width, height);
			let background = views.first().map(|view| view.view.background).unwrap_or_default();
			let request = crate::render::ViewRequest { width, height, camera: spec, background, msaa: 4, bloom: false, ..Default::default() };
			let (drawn, _) = renderer.render_gated(scene, std::slice::from_ref(&request), gpu)?;
			if let Some(image) = drawn.first() {
				let values: [(&str, fn(&grid::Cell) -> f64); 3] = [("draws", |cell| cell.draws), ("triangles", |cell| cell.triangles), ("occlusion", |cell| cell.occlusion_ms)];
				for (suffix, value) in values {
					let painted = grid::paint(&image.rgba, &camera, image.width, image.height, &cells, spacing, value);
					crate::output::write_png(&out.join(format!("grid.{suffix}.png")), image.width, image.height, &painted, true)?;
				}
			}
		}
		grid_lines = grid::lines(&cells, spacing, options.grid_area.as_deref().unwrap_or("the map"));
	}
	if !options.no_images && options.grid.is_none() {
		let mut scenes = Vec::new();
		let mut requests = Vec::new();
		let mut names = Vec::new();
		for ((view, (camera, width, height)), cost) in views.iter().zip(shots).zip(&costs) {
			for (heat, suffix) in [(false, "draws"), (true, "heat")] {
				scenes.push(images::scene(scene, &analysis, cost, heat));
				requests.push(crate::render::ViewRequest {
					width: *width,
					height: *height,
					camera: camera.spec(),
					background: view.view.background,
					msaa: 4,
					bloom: false,
					..Default::default()
				});
				names.push(format!("{}.{suffix}.png", view.name));
			}
		}
		for ((cost_scene, request), name) in scenes.iter().zip(&requests).zip(&names) {
			let (drawn, _) = renderer.render_gated(cost_scene, std::slice::from_ref(request), gpu)?;
			if let Some(image) = drawn.first() {
				crate::output::write_png(&out.join(name), image.width, image.height, &image.rgba, true)?;
			}
		}
	}
	let mut report = CostReport {
		technology: analysis.technology,
		quality: analysis.quality,
		occlusion: !options.no_occlusion,
		mesh_streaming: analysis.mesh_streaming,
		scene: analysis.scene_counts(),
		opportunities: report::summarize(&costs),
		views: costs,
		lines: Vec::new(),
		warnings: analysis.warnings.clone(),
	};
	if options.items {
		write_items(&out.join("items.json"), scene, &analysis, &report.views)?;
	}
	report.lines = report::lines(&report);
	report.lines.extend(grid_lines);
	// a grid's hundreds of views written compact: pretty, a map's 20-stud grid (440 views) wrote
	// 140 MB here, 80 % of it each view's occluders
	if report.views.len() <= PRETTY_VIEWS {
		crate::output::write_json_atomic(&out.join("cost.json"), &report, true)?;
	} else {
		write_compact(&out.join("cost.json"), &mut report)?;
	}
	// the report the job returns (summary.json, the service's answer, --json) keeps each view's first
	// `top` batches, occluders used, missed occluders and tested groups; cost.json keeps every one (a
	// grid's summary.json held every occluder of every view: 134 MB, its --json 74 MB more)
	let top = options.top.unwrap_or(24);
	for view in &mut report.views {
		view.batches.truncate(top);
		view.occlusion.used.truncate(top);
		view.occlusion.missed.truncate(top);
		view.occlusion.groups.truncate(top);
	}
	Ok(report)
}

/// `report` as compact JSON, as serde writes it, its views serialized on every core a batch at a time
/// and written into the rest of it in order (a grid's 440 views: 80 MB).
fn write_compact(path: &std::path::Path, report: &mut CostReport) -> anyhow::Result<()> {
	use std::io::Write;
	let views = std::mem::take(&mut report.views);
	let shell = serde_json::to_vec(&*report);
	report.views = views;
	let shell = shell?;
	const MARK: &[u8] = b"\"views\":[]";
	let Some(at) = shell.windows(MARK.len()).position(|window| window == MARK) else {
		return crate::output::write_json_atomic(path, &*report, false);
	};
	let views = &report.views;
	crate::output::write_atomic_with(path, |writer| {
		writer.write_all(&shell[..at + MARK.len() - 1])?;
		let batch = rayon::current_num_threads().max(1) * 4;
		for (index, chunk) in views.chunks(batch).enumerate() {
			let written: Vec<Vec<u8>> = chunk.par_iter().map(serde_json::to_vec).collect::<Result<_, _>>()?;
			for (offset, view) in written.iter().enumerate() {
				if index + offset > 0 {
					writer.write_all(b",")?;
				}
				writer.write_all(view)?;
			}
		}
		writer.write_all(&shell[at + MARK.len() - 1..])?;
		Ok(())
	})
}

#[cfg(test)]
mod tests {
	use glam::{Affine3A, Vec3};
	use rbx_dom_weak::InstanceBuilder;
	use rbx_dom_weak::types::{Enum, NumberSequence, NumberSequenceKeypoint, Ref};

	use super::*;
	use crate::scene::build::{ModelBuilder, PartSpec};

	const PLASTIC: u32 = 256;
	const WOOD: u32 = 512;

	fn analyze(model: ModelBuilder) -> (Analysis, Scene) {
		let document = model.into_document();
		let scene = crate::scene::extract::extract(&document);
		let state = std::env::temp_dir().join(format!("ror-cost-test-{}", std::process::id()));
		let assets = ror_assets::Assets::new(ror_assets::AssetConfig { offline: true, ..ror_assets::AssetConfig::new(&state) });
		(Analysis::new(&scene, &document, &assets, &CostOptions { technology: Some(4), ..Default::default() }), scene)
	}

	/// The view from 80 studs south and 60 up, looking at the origin.
	fn view(analysis: &Analysis) -> ViewCost {
		let camera = crate::render::camera::CameraSpec::LookAt { eye: [0.0, 60.0, 80.0], target: [0.0, 0.0, 0.0], fov: 70.0 }.place(None, 1644, 1116);
		analysis.view("test", &camera, 1644.0 / 1116.0, &CostOptions::default())
	}

	fn opaque(view: &ViewCost) -> Count {
		view.passes.get(&Pass::Opaque).copied().unwrap_or_default()
	}

	fn grid(model: &mut ModelBuilder, count: usize, spec: impl Fn(usize) -> PartSpec) -> Vec<Ref> {
		(0..count)
			.map(|index| {
				let position = Vec3::new((index % 10) as f32 * 6.0 - 27.0, 2.0, (index / 10) as f32 * 6.0 - 27.0);
				model.part(&PartSpec { cframe: Affine3A::from_translation(position), ..spec(index) })
			})
			.collect()
	}

	#[test]
	fn copies_batch_whatever_their_colour_size_and_transparency_but_not_their_material() {
		let mut model = ModelBuilder::new("Blocks");
		grid(&mut model, 100, |index| PartSpec { size: Vec3::splat(1.0 + (index % 4) as f32), color: [index as u8, 50, 200], material: PLASTIC, ..Default::default() });
		let (analysis, _) = analyze(model);
		assert_eq!(opaque(&view(&analysis)), Count { draws: 1, triangles: 1200 });

		let mut model = ModelBuilder::new("Materials");
		grid(&mut model, 100, |index| PartSpec { material: if index % 2 == 0 { PLASTIC } else { WOOD }, size: Vec3::splat(3.0), ..Default::default() });
		assert_eq!(opaque(&view(&analyze(model).0)).draws, 2);

		// transparent copies are a batch of their own, however transparent
		let mut model = ModelBuilder::new("Transparent");
		grid(&mut model, 100, |index| PartSpec { transparency: if index % 2 == 0 { 0.0 } else { 0.1 + (index % 9) as f32 / 10.0 }, size: Vec3::splat(3.0), ..Default::default() });
		let view = view(&analyze(model).0);
		assert_eq!(opaque(&view).draws, 2);
		assert_eq!(view.visible.transparent_parts, 50);
	}

	#[test]
	fn shapes_are_batches_of_their_own() {
		let mut model = ModelBuilder::new("Shapes");
		grid(&mut model, 40, |index| PartSpec { shape: Some([1, 0, 2, 1][index % 4]), size: Vec3::splat(3.0), ..Default::default() });
		model.part(&PartSpec { class: "WedgePart", size: Vec3::splat(3.0), ..Default::default() });
		// Block, Ball, Cylinder, Wedge
		assert_eq!(opaque(&view(&analyze(model).0)), Count { draws: 4, triangles: 20 * 12 + 10 * 432 + 10 * 96 + 10 });
	}

	#[test]
	fn decals_batch_by_texture_and_face() {
		let mut model = ModelBuilder::new("Decals");
		let parts = grid(&mut model, 20, |_| PartSpec { size: Vec3::splat(3.0), ..Default::default() });
		for (index, part) in parts.iter().enumerate() {
			let face = if index % 2 == 0 { 1 } else { 5 };
			model.child(*part, InstanceBuilder::new("Decal").with_property("Texture", "rbxasset://textures/face.png").with_property("Face", Enum::from_u32(face)));
		}
		let view = view(&analyze(model).0);
		// the blocks, then the decals on their top and front faces
		assert_eq!(opaque(&view).draws, 3);
		assert!(view.opportunities.iter().any(|opportunity| opportunity.kind == "decalFaces" && opportunity.saves.draws == 1));
	}

	#[test]
	fn forcefield_draws_each_part_twice() {
		let mut model = ModelBuilder::new("ForceField");
		for index in 0..4 {
			model.part(&PartSpec { cframe: Affine3A::from_translation(Vec3::new(0.0, 2.0, index as f32 * 6.0)), size: Vec3::splat(3.0), material: FORCEFIELD, ..Default::default() });
		}
		// four depths from this camera: a pair of draws each
		assert_eq!(opaque(&view(&analyze(model).0)).draws, 8);
	}

	#[test]
	fn beams_draw_alone_even_when_they_show_nothing() {
		let mut model = ModelBuilder::new("Beams");
		let parts = grid(&mut model, 4, |_| PartSpec { transparency: 1.0, size: Vec3::ONE, ..Default::default() });
		for (index, part) in parts.iter().enumerate() {
			let a = model.child(*part, InstanceBuilder::new("Attachment").with_property("CFrame", crate::scene::build::cframe(Affine3A::from_translation(Vec3::new(-3.0, 0.0, 0.0)))));
			let b = model.child(*part, InstanceBuilder::new("Attachment").with_property("CFrame", crate::scene::build::cframe(Affine3A::from_translation(Vec3::new(3.0, 2.0, 0.0)))));
			let mut beam = InstanceBuilder::new("Beam").with_property("Attachment0", a).with_property("Attachment1", b).with_property("Segments", 10i32);
			if index == 0 {
				beam = beam.with_property("Transparency", NumberSequence { keypoints: vec![NumberSequenceKeypoint::new(0.0, 1.0, 0.0), NumberSequenceKeypoint::new(1.0, 1.0, 0.0)] });
			}
			model.child(*part, beam);
		}
		let view = view(&analyze(model).0);
		assert_eq!(view.passes.get(&Pass::Transparent).copied().unwrap_or_default(), Count { draws: 4, triangles: 80 });
		assert_eq!(view.visible.invisible_beams, 1);
		// the invisible parts holding them draw nothing
		assert_eq!(opaque(&view).draws, 0);
	}

	#[test]
	fn a_humanoid_model_draws_apart_from_the_world() {
		let mut model = ModelBuilder::new("World");
		grid(&mut model, 10, |_| PartSpec { size: Vec3::splat(3.0), ..Default::default() });
		let character = model.child(model.model, InstanceBuilder::new("Model").with_name("Npc"));
		model.child(character, InstanceBuilder::new("Humanoid"));
		for index in 0..10 {
			let spec = PartSpec { cframe: Affine3A::from_translation(Vec3::new(index as f32 * 6.0 - 27.0, 2.0, 30.0)), size: Vec3::splat(3.0), ..Default::default() };
			let part = model.part(&spec);
			model.dom.transfer_within(part, character);
		}
		let (analysis, _) = analyze(model);
		let view = view(&analysis);
		// the world's blocks, and the character's (the same blocks, its own cluster)
		assert_eq!(opaque(&view).draws, 2);
		assert_eq!(view.visible.characters, 1);
	}

	#[test]
	fn a_humanoid_model_fills_a_cluster_per_72_parts() {
		for (parts, draws) in [(72, 1), (73, 2), (145, 3)] {
			let mut model = ModelBuilder::new("World");
			let character = model.child(model.model, InstanceBuilder::new("Model").with_name("Npc"));
			model.child(character, InstanceBuilder::new("Humanoid"));
			for index in 0..parts {
				let position = Vec3::new((index % 15) as f32 * 2.0 - 14.0, 2.0, (index / 15) as f32 * 2.0 - 10.0);
				let part = model.part(&PartSpec { cframe: Affine3A::from_translation(position), size: Vec3::ONE, ..Default::default() });
				model.dom.transfer_within(part, character);
			}
			assert_eq!(opaque(&view(&analyze(model).0)).draws, draws, "{parts} parts");
		}
	}

	#[test]
	fn a_beams_box_reaches_half_its_curve_size() {
		// a beam 15 studs behind the camera: drawn once its box (world-aligned, so its corner reaches
		// 1.4 times as far along this view) comes past it
		for (curve, drawn) in [(18.0f32, 0), (24.0, 1)] {
			let mut model = ModelBuilder::new("Beam");
			let eye = Vec3::new(0.0, 60.0, 80.0);
			let behind = eye + eye.normalize() * 15.0;
			let holder = model.part(&PartSpec { cframe: Affine3A::from_translation(behind), transparency: 1.0, size: Vec3::ONE, ..Default::default() });
			let a = model.child(holder, InstanceBuilder::new("Attachment"));
			let b = model.child(holder, InstanceBuilder::new("Attachment").with_property("CFrame", crate::scene::build::cframe(Affine3A::from_translation(Vec3::X))));
			let beam = InstanceBuilder::new("Beam").with_property("Attachment0", a).with_property("Attachment1", b).with_property("Width0", 0.0f32).with_property("Width1", 0.0f32).with_property("CurveSize0", curve).with_property("CurveSize1", -curve);
			model.child(holder, beam);
			let view = view(&analyze(model).0);
			assert_eq!(view.passes.get(&Pass::Transparent).copied().unwrap_or_default().draws, drawn, "CurveSize {curve}");
		}
	}

	#[test]
	fn a_wall_hides_what_is_behind_it() {
		let mut model = ModelBuilder::new("Wall");
		grid(&mut model, 20, |index| PartSpec { material: [256, 512, 784, 800, 816, 832, 848, 864, 880, 896, 912, 928, 944, 960, 976, 992, 1008, 1024, 1040, 1056][index], size: Vec3::splat(2.0), ..Default::default() });
		let wall = PartSpec { cframe: Affine3A::from_translation(Vec3::new(0.0, 30.0, 45.0)), size: Vec3::new(400.0, 200.0, 4.0), material: PLASTIC, ..Default::default() };
		model.part(&wall);
		let (analysis, _) = analyze(model);
		let view = view(&analysis);
		assert_eq!(opaque(&view).draws, 1);
		assert_eq!(view.culled.occluded, 20);
		let frustum = analysis.view("test", &crate::render::camera::CameraSpec::LookAt { eye: [0.0, 60.0, 80.0], target: [0.0, 0.0, 0.0], fov: 70.0 }.place(None, 1644, 1116), 1644.0 / 1116.0, &CostOptions { no_occlusion: true, ..Default::default() });
		assert_eq!(opaque(&frustum).draws, 20);
		// the work: 21 objects tested, all of them occluders (the cubes too: a few pixels wide), the
		// wall, the largest, hiding the 20
		assert_eq!((view.occlusion.tested, view.occlusion.occluders, view.occlusion.hidden), (21, 21, 20));
		assert_eq!(view.occlusion.used[0].hidden, 20);
	}

	#[test]
	fn any_material_and_thickness_occludes_and_a_ball_does() {
		const NEON: u32 = 288;
		for (wall, label) in [
			(PartSpec { size: Vec3::new(400.0, 200.0, 4.0), material: NEON, ..Default::default() }, "Neon"),
			(PartSpec { size: Vec3::new(400.0, 200.0, 0.05), material: PLASTIC, ..Default::default() }, "0.05 studs"),
			(PartSpec { size: Vec3::splat(400.0), shape: Some(0), ..Default::default() }, "a ball"),
		] {
			let mut model = ModelBuilder::new("Wall");
			grid(&mut model, 20, |_| PartSpec { size: Vec3::splat(2.0), ..Default::default() });
			let at = if label == "a ball" { Vec3::new(0.0, 30.0, -160.0) } else { Vec3::new(0.0, 30.0, 45.0) };
			model.part(&PartSpec { cframe: Affine3A::from_translation(at), ..wall });
			let view = view(&analyze(model).0);
			if label == "a ball" {
				// a ball behind the grid hides nothing of it, and nothing hides the ball
				assert_eq!(view.occlusion.hidden, 0, "{label}");
			} else {
				assert_eq!(view.occlusion.hidden, 20, "{label}");
			}
		}
		// a ball halfway between the camera and the grid, covering it
		let mut model = ModelBuilder::new("Ball");
		grid(&mut model, 20, |_| PartSpec { size: Vec3::splat(2.0), ..Default::default() });
		model.part(&PartSpec { cframe: Affine3A::from_translation(Vec3::new(0.0, 30.0, 40.0)), size: Vec3::splat(60.0), shape: Some(0), ..Default::default() });
		assert_eq!(view(&analyze(model).0).occlusion.hidden, 20);
	}

	#[test]
	fn tested_objects_group_by_their_named_model() {
		assert_eq!(path_names(r#"Map.Assets.Bush["Plane.049"].Leaf"#), vec!["Map", "Assets", "Bush", "Plane.049", "Leaf"]);
		assert_eq!(tested_group("MainLobby.Assets.Model.Model.challengedoor1.default"), "challengedoor1 (MainLobby.Assets.Model.Model)");
		assert_eq!(tested_group("MainLobby.Assets.Model.Part"), "loose parts in MainLobby.Assets.Model");
		assert_eq!(tested_group(r#"Map.Assets.Bush["Plane.049"]"#), "Bush (Map.Assets)");
	}
}
