//! Compatibility surface for yw-look's pre-0.5 openusd adapter.
//!
//! This is intentionally small and exists to let yw-look measure the v0.5
//! migration without rewriting its large Rust backend in one step.

use std::io::Read;

use crate::sdf::{self, FieldKey, Value};
use crate::usd::{InitialLoadSet, PrimPredicate, Stage, StageBuilder};

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum StageLoadPolicy {
    #[default]
    LoadAll,
    NoPayloads,
}

#[derive(Debug, Clone)]
pub struct SkippedPayload {
    pub asset_path: String,
    pub prim_path: sdf::Path,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum UpAxis {
    Y,
    Z,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MaterialData {
    pub diffuse_color: Option<[f32; 3]>,
    pub metallic: Option<f32>,
    pub roughness: Option<f32>,
    pub opacity: Option<f32>,
    pub emissive_color: Option<[f32; 3]>,
    pub diffuse_texture: Option<String>,
    pub wrap_s: Option<String>,
    pub wrap_t: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MeshData {
    pub points: Vec<f32>,
    pub face_vertex_indices: Vec<i32>,
    pub face_vertex_counts: Vec<i32>,
    pub normals: Option<Vec<f32>>,
    pub uvs: Option<Vec<f32>>,
    pub joint_indices: Option<Vec<u32>>,
    pub joint_weights: Option<Vec<f32>>,
    pub joints_per_vertex: usize,
    pub display_color: Option<Vec<f32>>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct GeomSubsetData {
    pub name: String,
    pub indices: Vec<u32>,
    pub material_binding: Option<sdf::Path>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SkeletonData {
    pub joints: Vec<String>,
    pub bind_transforms: Vec<[f32; 16]>,
    pub rest_transforms: Vec<[f32; 16]>,
    pub parents: Vec<Option<usize>>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SkelAnimationData {
    pub times: Vec<f64>,
    pub translations: Vec<Vec<f32>>,
    pub rotations: Vec<Vec<f32>>,
    pub scales: Vec<Vec<f32>>,
    pub joints: Vec<String>,
}

impl StageBuilder {
    pub fn load_policy(self, policy: StageLoadPolicy) -> Self {
        match policy {
            StageLoadPolicy::LoadAll => self.load(InitialLoadSet::LoadAll),
            StageLoadPolicy::NoPayloads => self.load(InitialLoadSet::LoadNone),
        }
    }
}

impl Stage {
    pub fn up_axis(&self) -> Option<UpAxis> {
        let value = self.stage_metadata("upAxis").ok().flatten()?;
        match value {
            Value::Token(token) if token.as_str() == "Y" => Some(UpAxis::Y),
            Value::Token(token) if token.as_str() == "Z" => Some(UpAxis::Z),
            Value::String(s) if s == "Y" => Some(UpAxis::Y),
            Value::String(s) if s == "Z" => Some(UpAxis::Z),
            _ => None,
        }
    }

    pub fn meters_per_unit(&self) -> Option<f64> {
        self.stage_metadata("metersPerUnit")
            .ok()
            .flatten()
            .and_then(|v| match v {
                Value::Double(v) => Some(v),
                Value::Float(v) => Some(v as f64),
                _ => None,
            })
    }

    pub fn unresolved_assets(&self) -> Vec<String> {
        let _ = self.traverse(PrimPredicate::ALL, |_| {});
        let mut unresolved: Vec<String> = self
            .composition_errors()
            .into_iter()
            .filter_map(|err| match err {
                crate::pcp::Error::UnresolvedLayer { asset_path, .. }
                | crate::pcp::Error::UnresolvedSublayer { asset_path, .. } => Some(asset_path),
                _ => None,
            })
            .collect();

        let _ = self.traverse(PrimPredicate::ALL, |prim_path| {
            for asset_path in self
                .references_in(prim_path.clone())
                .into_iter()
                .map(|reference| reference.asset_path)
                .chain(
                    self.payloads_in(prim_path.clone())
                        .into_iter()
                        .map(|payload| payload.asset_path),
                )
            {
                if !asset_path.is_empty()
                    && !asset_path.contains("${")
                    && !self.asset_resolves(&asset_path)
                    && !unresolved.iter().any(|existing| existing == &asset_path)
                {
                    unresolved.push(asset_path);
                }
            }
        });

        unresolved
    }

    pub fn skipped_payloads(&self) -> Vec<SkippedPayload> {
        if self.load() != InitialLoadSet::LoadNone {
            return Vec::new();
        }

        let mut skipped = Vec::new();
        let _ = self.traverse(PrimPredicate::ALL, |prim_path| {
            for payload in self.payloads_in(prim_path.clone()) {
                skipped.push(SkippedPayload {
                    asset_path: payload.asset_path,
                    prim_path: prim_path.clone(),
                });
            }
        });
        skipped
    }

    pub fn root_layer_is_binary(&self) -> bool {
        let id = self.root_layer().identifier().to_ascii_lowercase();
        if id.ends_with(".usdc") {
            return true;
        }
        if !id.ends_with(".usd") {
            return false;
        }
        let Ok(mut file) = std::fs::File::open(self.root_layer().identifier()) else {
            return false;
        };
        let mut magic = [0_u8; 8];
        file.read_exact(&mut magic).is_ok() && magic == *crate::usdc::MAGIC
    }

    pub fn prim_children(&self, path: impl Into<sdf::Path>) -> anyhow::Result<Vec<String>> {
        Ok(self
            .prim(path)
            .children()?
            .into_iter()
            .map(|prim| prim.path().name().unwrap_or_default().to_owned())
            .collect())
    }

    pub fn references_in(&self, path: impl Into<sdf::Path>) -> Vec<sdf::Reference> {
        authored_references_at(self, path.into())
    }

    pub fn payloads_in(&self, path: impl Into<sdf::Path>) -> Vec<sdf::Payload> {
        authored_payloads_at(self, path.into())
    }

    pub fn mesh_of(&self, prim_path: impl Into<sdf::Path>) -> anyhow::Result<Option<MeshData>> {
        let prim_path = prim_path.into();
        if read_string_field(self, prim_path.clone(), FieldKey::TypeName).as_deref() != Some("Mesh") {
            return Ok(None);
        }

        let Some(points) = read_vec3_array(self, &prim_path, "points")? else {
            return Ok(None);
        };
        let face_vertex_indices = read_i32_array(self, &prim_path, "faceVertexIndices")?.unwrap_or_default();
        let face_vertex_counts = read_i32_array(self, &prim_path, "faceVertexCounts")?.unwrap_or_default();
        let normals = read_vec3_array(self, &prim_path, "normals")?;
        let uvs = read_vec2_array(self, &prim_path, "primvars:st")?;
        let display_color = read_vec3_array(self, &prim_path, "primvars:displayColor")?;
        let joint_indices = read_u32_array(self, &prim_path, "primvars:skel:jointIndices")?;
        let joint_weights = read_f32_array(self, &prim_path, "primvars:skel:jointWeights")?;
        let joints_per_vertex = read_element_size(self, &prim_path, "primvars:skel:jointIndices")
            .or_else(|| read_element_size(self, &prim_path, "primvars:skel:jointWeights"))
            .or_else(|| {
                let point_count = points.len() / 3;
                joint_indices.as_ref().and_then(|indices| {
                    (point_count > 0 && indices.len() % point_count == 0).then_some(indices.len() / point_count)
                })
            })
            .unwrap_or(0);

        Ok(Some(MeshData {
            points,
            face_vertex_indices,
            face_vertex_counts,
            normals,
            uvs,
            joint_indices,
            joint_weights,
            joints_per_vertex,
            display_color,
        }))
    }

    pub fn bound_material(&self, mesh_path: impl Into<sdf::Path>) -> Option<sdf::Path> {
        let mesh_path = mesh_path.into();
        for rel_name in ["material:binding:preview", "material:binding:full", "material:binding"] {
            if let Some(path) = first_target_in_self_or_ancestors(self, &mesh_path, rel_name) {
                return Some(path);
            }
        }
        None
    }

    pub fn material_of(&self, mesh_path: impl Into<sdf::Path>) -> Option<MaterialData> {
        let material_path = self.bound_material(mesh_path)?;
        if read_string_field(self, material_path.clone(), FieldKey::TypeName).as_deref() != Some("Material") {
            return None;
        }

        let shader = find_preview_surface_shader(self, &material_path)?;
        let diffuse_input = shader.append_property("inputs:diffuseColor").ok()?;
        let texture = follow_texture_connection(self, &diffuse_input);
        let mut data = MaterialData {
            diffuse_color: read_vec3_input(self, &shader, "inputs:diffuseColor"),
            metallic: read_float_input(self, &shader, "inputs:metallic"),
            roughness: read_float_input(self, &shader, "inputs:roughness"),
            opacity: read_float_input(self, &shader, "inputs:opacity"),
            emissive_color: read_vec3_input(self, &shader, "inputs:emissiveColor"),
            diffuse_texture: texture.as_ref().map(|t| t.file.clone()),
            wrap_s: texture.as_ref().and_then(|t| t.wrap_s.clone()),
            wrap_t: texture.and_then(|t| t.wrap_t),
        };

        if data.diffuse_texture.is_some() && data.diffuse_color.is_none() {
            data.diffuse_color = Some([1.0, 1.0, 1.0]);
        }
        (data != MaterialData::default()).then_some(data)
    }

    pub fn geom_subsets_of(&self, mesh_path: impl Into<sdf::Path>) -> Vec<GeomSubsetData> {
        let mesh_path = mesh_path.into();
        let Ok(children) = self.prim_children(mesh_path.clone()) else {
            return Vec::new();
        };

        children
            .into_iter()
            .filter_map(|name| {
                let subset_path = sdf::Path::new(&format!("{}/{}", mesh_path.as_str(), name)).ok()?;
                if read_string_field(self, subset_path.clone(), FieldKey::TypeName).as_deref() != Some("GeomSubset") {
                    return None;
                }
                let element_type = subset_path
                    .append_property("elementType")
                    .ok()
                    .and_then(|path| read_string_field(self, path, FieldKey::Default));
                if !matches!(element_type.as_deref(), None | Some("face")) {
                    return None;
                }
                let indices = read_i32_array(self, &subset_path, "indices")
                    .ok()
                    .flatten()?
                    .into_iter()
                    .filter_map(|index| u32::try_from(index).ok())
                    .collect::<Vec<_>>();
                if indices.is_empty() {
                    return None;
                }
                let material_binding = self.bound_material(subset_path);
                Some(GeomSubsetData {
                    name,
                    indices,
                    material_binding,
                })
            })
            .collect()
    }

    pub fn skeleton_of(&self, mesh_path: impl Into<sdf::Path>) -> Option<(sdf::Path, SkeletonData)> {
        let skeleton_path = first_target_in_self_or_ancestors(self, &mesh_path.into(), "skel:skeleton")?;
        if read_string_field(self, skeleton_path.clone(), FieldKey::TypeName).as_deref() != Some("Skeleton") {
            return None;
        }
        let joints = read_string_vec_attr(self, &skeleton_path, "joints")?;
        let bind_transforms = read_mat4_vec_attr(self, &skeleton_path, "bindTransforms").unwrap_or_default();
        let rest_transforms = read_mat4_vec_attr(self, &skeleton_path, "restTransforms").unwrap_or_default();
        let parents = joint_parents(&joints);
        Some((
            skeleton_path,
            SkeletonData {
                joints,
                bind_transforms,
                rest_transforms,
                parents,
            },
        ))
    }

    pub fn skel_animation_of(&self, skeleton_path: impl Into<sdf::Path>) -> Option<SkelAnimationData> {
        let anim_path = first_target_in_self_or_ancestors(self, &skeleton_path.into(), "skel:animationSource")?;
        if read_string_field(self, anim_path.clone(), FieldKey::TypeName).as_deref() != Some("SkelAnimation") {
            return None;
        }
        let joints = read_string_vec_attr(self, &anim_path, "joints").unwrap_or_default();
        let translations = read_vec3_time_samples(self, &anim_path, "translations");
        let rotations = read_quat_time_samples(self, &anim_path, "rotations");
        let scales = read_vec3_time_samples(self, &anim_path, "scales");
        let mut times: Vec<f64> = translations
            .iter()
            .chain(rotations.iter())
            .chain(scales.iter())
            .map(|(time, _)| *time)
            .collect();
        times.sort_by(f64::total_cmp);
        times.dedup_by(|a, b| (*a - *b).abs() < f64::EPSILON);
        if times.is_empty() {
            return None;
        }
        Some(SkelAnimationData {
            translations: align_samples(&times, translations),
            rotations: align_samples(&times, rotations),
            scales: align_samples(&times, scales),
            times,
            joints,
        })
    }
}

fn read_attr(stage: &Stage, prim_path: &sdf::Path, name: &str) -> anyhow::Result<Option<Value>> {
    let attr_path = prim_path.append_property(name)?;
    stage.field::<Value>(attr_path, FieldKey::Default)
}

fn authored_references_at(stage: &Stage, path: sdf::Path) -> Vec<sdf::Reference> {
    // TODO(openusd-v050-compat): use the same composed reference-list path as
    // `pcp::compose_site::compose_references_in`. That composer currently hangs
    // off the private prim-index builder, so this PoC compatibility surface can
    // only expose the strongest authored field for composition arcs.
    match stage.field::<Value>(path, FieldKey::References) {
        Ok(Some(Value::ReferenceListOp(op))) => op.iter().cloned().collect(),
        _ => Vec::new(),
    }
}

fn authored_payloads_at(stage: &Stage, path: sdf::Path) -> Vec<sdf::Payload> {
    // TODO(openusd-v050-compat): replace with composed payload-list extraction
    // via `pcp::compose_site::collect_payloads_in` once Stage exposes a stable
    // direct-arc query. This fallback intentionally stays isolated so callers do
    // not accidentally treat it as a general composed API.
    match stage.field::<Value>(path, FieldKey::Payload) {
        Ok(Some(Value::Payload(payload))) => vec![payload],
        Ok(Some(Value::PayloadListOp(op))) => op.iter().cloned().collect(),
        _ => Vec::new(),
    }
}

fn read_vec3_array(stage: &Stage, prim_path: &sdf::Path, name: &str) -> anyhow::Result<Option<Vec<f32>>> {
    let Some(value) = read_attr(stage, prim_path, name)? else {
        return Ok(None);
    };
    Ok(flatten_vec3_value(value))
}

fn read_vec2_array(stage: &Stage, prim_path: &sdf::Path, name: &str) -> anyhow::Result<Option<Vec<f32>>> {
    let Some(value) = read_attr(stage, prim_path, name)? else {
        return Ok(None);
    };
    Ok(flatten_vec2_value(value))
}

fn read_i32_array(stage: &Stage, prim_path: &sdf::Path, name: &str) -> anyhow::Result<Option<Vec<i32>>> {
    let Some(value) = read_attr(stage, prim_path, name)? else {
        return Ok(None);
    };
    Ok(match value {
        Value::IntVec(v) => Some(v),
        Value::UintVec(v) => Some(v.into_iter().map(|v| v as i32).collect()),
        _ => None,
    })
}

fn read_u32_array(stage: &Stage, prim_path: &sdf::Path, name: &str) -> anyhow::Result<Option<Vec<u32>>> {
    let Some(value) = read_attr(stage, prim_path, name)? else {
        return Ok(None);
    };
    Ok(match value {
        Value::UintVec(v) => Some(v),
        Value::IntVec(v) => Some(v.into_iter().filter_map(|v| u32::try_from(v).ok()).collect()),
        _ => None,
    })
}

fn read_f32_array(stage: &Stage, prim_path: &sdf::Path, name: &str) -> anyhow::Result<Option<Vec<f32>>> {
    let Some(value) = read_attr(stage, prim_path, name)? else {
        return Ok(None);
    };
    Ok(match value {
        Value::FloatVec(v) => Some(v),
        Value::DoubleVec(v) => Some(v.into_iter().map(|v| v as f32).collect()),
        Value::HalfVec(v) => Some(v.into_iter().map(f32::from).collect()),
        _ => None,
    })
}

fn read_element_size(stage: &Stage, prim_path: &sdf::Path, name: &str) -> Option<usize> {
    let attr_path = prim_path.append_property(name).ok()?;
    let value: Option<Value> = stage.field(attr_path, "elementSize").ok().flatten();
    match value? {
        Value::Int(v) if v > 0 => Some(v as usize),
        Value::Uint(v) if v > 0 => Some(v as usize),
        _ => None,
    }
}

fn flatten_vec3_value(value: Value) -> Option<Vec<f32>> {
    match value {
        Value::Vec3fVec(v) => Some(v.into_iter().flat_map(<[f32; 3]>::from).collect()),
        Value::Vec3dVec(v) => Some(v.into_iter().flat_map(<[f64; 3]>::from).map(|v| v as f32).collect()),
        Value::Vec3hVec(v) => Some(
            v.into_iter()
                .flat_map(<[crate::gf::f16; 3]>::from)
                .map(f32::from)
                .collect(),
        ),
        Value::FloatVec(v) if v.len() % 3 == 0 => Some(v),
        Value::DoubleVec(v) if v.len() % 3 == 0 => Some(v.into_iter().map(|v| v as f32).collect()),
        _ => None,
    }
}

struct TextureConnection {
    file: String,
    wrap_s: Option<String>,
    wrap_t: Option<String>,
}

fn find_preview_surface_shader(stage: &Stage, material_path: &sdf::Path) -> Option<sdf::Path> {
    for child_name in stage.prim_children(material_path.clone()).ok()? {
        let child = sdf::Path::new(&format!("{}/{}", material_path.as_str(), child_name)).ok()?;
        if read_string_field(stage, child.clone(), FieldKey::TypeName).as_deref() != Some("Shader") {
            continue;
        }
        let Some(info_id) = child
            .append_property("info:id")
            .ok()
            .and_then(|path| read_string_field(stage, path, FieldKey::Default))
        else {
            continue;
        };
        if matches!(
            info_id.as_str(),
            "UsdPreviewSurface" | "ND_UsdPreviewSurface_surfaceshader"
        ) {
            return Some(child);
        }
    }
    None
}

fn follow_texture_connection(stage: &Stage, input_path: &sdf::Path) -> Option<TextureConnection> {
    let target = first_attribute_connection(stage, input_path.clone())?;
    let shader_path = target.prim_path();
    if read_string_field(stage, shader_path.clone(), FieldKey::TypeName).as_deref() != Some("Shader") {
        return None;
    }
    let info_id_path = shader_path.append_property("info:id").ok()?;
    let info_id = read_string_field(stage, info_id_path, FieldKey::Default)?;
    if !matches!(
        info_id.as_str(),
        "UsdUVTexture" | "ND_image_color3" | "ND_image_color4" | "ND_image_float" | "ND_image_vector3"
    ) {
        return None;
    }

    let file_path = shader_path.append_property("inputs:file").ok()?;
    let file = read_string_field(stage, file_path, FieldKey::Default)?;
    let wrap_s = shader_path
        .append_property("inputs:wrapS")
        .ok()
        .and_then(|p| read_string_field(stage, p, FieldKey::Default));
    let wrap_t = shader_path
        .append_property("inputs:wrapT")
        .ok()
        .and_then(|p| read_string_field(stage, p, FieldKey::Default));
    Some(TextureConnection { file, wrap_s, wrap_t })
}

fn first_target(stage: &Stage, prim_path: &sdf::Path, rel_name: &str) -> Option<sdf::Path> {
    let rel_path = prim_path.append_property(rel_name).ok()?;
    first_relationship_target(stage, rel_path)
}

fn first_target_in_self_or_ancestors(stage: &Stage, prim_path: &sdf::Path, rel_name: &str) -> Option<sdf::Path> {
    let mut path = Some(prim_path.clone());
    while let Some(current) = path {
        if let Some(target) = first_target(stage, &current, rel_name) {
            return Some(target);
        }
        path = parent_path(&current);
    }
    None
}

fn parent_path(path: &sdf::Path) -> Option<sdf::Path> {
    let value = path.as_str();
    let slash = value.rfind('/')?;
    if slash == 0 {
        return None;
    }
    sdf::Path::new(&value[..slash]).ok()
}

fn first_relationship_target(stage: &Stage, path: sdf::Path) -> Option<sdf::Path> {
    stage.relationship(path).targets().ok()?.into_iter().next()
}

fn first_attribute_connection(stage: &Stage, path: sdf::Path) -> Option<sdf::Path> {
    stage.attribute(path).connections().ok()?.into_iter().next()
}

fn read_string_field(stage: &Stage, path: sdf::Path, field: impl AsRef<str>) -> Option<String> {
    let value: Option<Value> = stage.field(path, field).ok().flatten();
    match value? {
        Value::String(v) => Some(v),
        Value::Token(v) => Some(v.as_str().to_owned()),
        Value::AssetPath(v) => Some(v.to_string()),
        _ => None,
    }
}

fn read_float_input(stage: &Stage, shader_path: &sdf::Path, input_name: &str) -> Option<f32> {
    let input_path = shader_path.append_property(input_name).ok()?;
    let value: Option<Value> = stage.field(input_path, FieldKey::Default).ok().flatten();
    match value? {
        Value::Float(v) => Some(v),
        Value::Double(v) => Some(v as f32),
        Value::Half(v) => Some(v.to_f32()),
        _ => None,
    }
}

fn read_vec3_input(stage: &Stage, shader_path: &sdf::Path, input_name: &str) -> Option<[f32; 3]> {
    let input_path = shader_path.append_property(input_name).ok()?;
    let value: Option<Value> = stage.field(input_path, FieldKey::Default).ok().flatten();
    vec3_value(value?)
}

fn vec3_value(value: Value) -> Option<[f32; 3]> {
    match value {
        Value::Vec3f(v) => Some(v.into()),
        Value::Vec3d(v) => {
            let v: [f64; 3] = v.into();
            Some([v[0] as f32, v[1] as f32, v[2] as f32])
        }
        Value::Vec3h(v) => {
            let v: [crate::gf::f16; 3] = v.into();
            Some([v[0].to_f32(), v[1].to_f32(), v[2].to_f32()])
        }
        Value::FloatVec(v) if v.len() == 3 => Some([v[0], v[1], v[2]]),
        Value::DoubleVec(v) if v.len() == 3 => Some([v[0] as f32, v[1] as f32, v[2] as f32]),
        _ => None,
    }
}

fn read_string_vec_attr(stage: &Stage, prim_path: &sdf::Path, name: &str) -> Option<Vec<String>> {
    let attr_path = prim_path.append_property(name).ok()?;
    let value: Option<Value> = stage.field(attr_path, FieldKey::Default).ok().flatten();
    match value? {
        Value::StringVec(v) => Some(v),
        Value::TokenVec(v) => Some(v.into_iter().map(|v| v.as_str().to_owned()).collect()),
        _ => None,
    }
}

fn read_mat4_vec_attr(stage: &Stage, prim_path: &sdf::Path, name: &str) -> Option<Vec<[f32; 16]>> {
    let attr_path = prim_path.append_property(name).ok()?;
    let value: Option<Value> = stage.field(attr_path, FieldKey::Default).ok().flatten();
    match value? {
        Value::Matrix4dVec(v) => Some(v.into_iter().map(|m| row_major_mat4_to_column_major_f32(m.0)).collect()),
        _ => None,
    }
}

fn row_major_mat4_to_column_major_f32(row_major: [f64; 16]) -> [f32; 16] {
    let mut out = [0.0_f32; 16];
    for row in 0..4 {
        for col in 0..4 {
            out[col * 4 + row] = row_major[row * 4 + col] as f32;
        }
    }
    out
}

fn joint_parents(joints: &[String]) -> Vec<Option<usize>> {
    joints
        .iter()
        .map(|joint| {
            joint
                .rsplit_once('/')
                .and_then(|(parent, _)| joints.iter().position(|candidate| candidate == parent))
        })
        .collect()
}

fn read_vec3_time_samples(stage: &Stage, prim_path: &sdf::Path, name: &str) -> Vec<(f64, Vec<f32>)> {
    let Some(samples) = read_time_samples(stage, prim_path, name) else {
        return Vec::new();
    };
    samples
        .into_iter()
        .filter_map(|(time, value)| flatten_vec3_value(value).map(|v| (time, v)))
        .collect()
}

fn read_quat_time_samples(stage: &Stage, prim_path: &sdf::Path, name: &str) -> Vec<(f64, Vec<f32>)> {
    let Some(samples) = read_time_samples(stage, prim_path, name) else {
        return Vec::new();
    };
    samples
        .into_iter()
        .filter_map(|(time, value)| flatten_quat_value(value).map(|v| (time, v)))
        .collect()
}

fn read_time_samples(stage: &Stage, prim_path: &sdf::Path, name: &str) -> Option<sdf::TimeSampleMap> {
    let attr_path = prim_path.append_property(name).ok()?;
    let value: Option<Value> = stage.field(attr_path, FieldKey::TimeSamples).ok().flatten();
    match value? {
        Value::TimeSamples(samples) => Some(samples),
        _ => None,
    }
}

fn flatten_quat_value(value: Value) -> Option<Vec<f32>> {
    match value {
        Value::QuatfVec(v) => Some(v.into_iter().flat_map(|q| [q.x, q.y, q.z, q.w]).collect()),
        Value::QuatdVec(v) => Some(
            v.into_iter()
                .flat_map(|q| [q.x as f32, q.y as f32, q.z as f32, q.w as f32])
                .collect(),
        ),
        Value::QuathVec(v) => Some(
            v.into_iter()
                .flat_map(|q| [q.x.to_f32(), q.y.to_f32(), q.z.to_f32(), q.w.to_f32()])
                .collect(),
        ),
        Value::Vec4fVec(v) => Some(v.into_iter().flat_map(<[f32; 4]>::from).collect()),
        Value::Vec4dVec(v) => Some(v.into_iter().flat_map(<[f64; 4]>::from).map(|v| v as f32).collect()),
        _ => None,
    }
}

fn align_samples(times: &[f64], samples: Vec<(f64, Vec<f32>)>) -> Vec<Vec<f32>> {
    times
        .iter()
        .map(|time| {
            samples
                .iter()
                .find_map(|(sample_time, value)| ((*sample_time - *time).abs() < f64::EPSILON).then(|| value.clone()))
                .unwrap_or_default()
        })
        .collect()
}

fn flatten_vec2_value(value: Value) -> Option<Vec<f32>> {
    match value {
        Value::Vec2fVec(v) => Some(v.into_iter().flat_map(<[f32; 2]>::from).collect()),
        Value::Vec2dVec(v) => Some(v.into_iter().flat_map(<[f64; 2]>::from).map(|v| v as f32).collect()),
        Value::Vec2hVec(v) => Some(
            v.into_iter()
                .flat_map(<[crate::gf::f16; 2]>::from)
                .map(f32::from)
                .collect(),
        ),
        Value::FloatVec(v) if v.len() % 2 == 0 => Some(v),
        Value::DoubleVec(v) if v.len() % 2 == 0 => Some(v.into_iter().map(|v| v as f32).collect()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mat4_conversion_transposes_row_major_to_column_major() {
        let row_major = [
            1.0, 2.0, 3.0, 4.0, //
            5.0, 6.0, 7.0, 8.0, //
            9.0, 10.0, 11.0, 12.0, //
            13.0, 14.0, 15.0, 16.0,
        ];
        assert_eq!(
            row_major_mat4_to_column_major_f32(row_major),
            [
                1.0, 5.0, 9.0, 13.0, //
                2.0, 6.0, 10.0, 14.0, //
                3.0, 7.0, 11.0, 15.0, //
                4.0, 8.0, 12.0, 16.0,
            ]
        );
    }

    #[test]
    fn bound_material_inherits_from_parent() -> anyhow::Result<()> {
        let stage = Stage::builder().in_memory("anon.usda")?;
        stage.define_prim("/World")?.set_type_name("Xform")?;
        stage.define_prim("/World/Mesh")?.set_type_name("Mesh")?;
        stage.define_prim("/Mat")?.set_type_name("Material")?;
        stage
            .define_prim("/World")?
            .create_relationship("material:binding")?
            .set_targets([sdf::path("/Mat")?])?;

        assert_eq!(
            stage
                .bound_material(sdf::path("/World/Mesh")?)
                .as_ref()
                .map(sdf::Path::as_str),
            Some("/Mat")
        );
        Ok(())
    }

    #[test]
    fn preview_surface_search_skips_shader_without_info_id() -> anyhow::Result<()> {
        let stage = Stage::builder().in_memory("anon.usda")?;
        stage.define_prim("/Mat")?.set_type_name("Material")?;
        stage.define_prim("/Mat/Utility")?.set_type_name("Shader")?;
        stage.define_prim("/Mat/Surface")?.set_type_name("Shader")?;
        stage
            .define_prim("/Mat/Surface")?
            .create_attribute("info:id", "token")?
            .set(sdf::Value::token("UsdPreviewSurface"))?;

        assert_eq!(
            find_preview_surface_shader(&stage, &sdf::path("/Mat")?)
                .as_ref()
                .map(sdf::Path::as_str),
            Some("/Mat/Surface")
        );
        Ok(())
    }
}
