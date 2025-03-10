use crate::{
    environment_map, prepass, EnvironmentMapLight, FogMeta, GlobalLightMeta, GpuFog, GpuLights,
    GpuPointLights, LightMeta, NotShadowCaster, NotShadowReceiver, PreviousGlobalTransform,
    ScreenSpaceAmbientOcclusionTextures, ShadowSamplers, ViewClusterBindings, ViewFogUniformOffset,
    ViewLightsUniformOffset, ViewShadowBindings, CLUSTERED_FORWARD_STORAGE_BUFFER_COUNT,
    MAX_CASCADES_PER_LIGHT, MAX_DIRECTIONAL_LIGHTS,
};
use bevy_app::Plugin;
use bevy_asset::{load_internal_asset, Assets, Handle, HandleId, HandleUntyped};
use bevy_core_pipeline::{
    prepass::ViewPrepassTextures,
    tonemapping::{
        get_lut_bind_group_layout_entries, get_lut_bindings, Tonemapping, TonemappingLuts,
    },
};
use bevy_ecs::{
    prelude::*,
    query::ROQueryItem,
    system::{lifetimeless::*, SystemParamItem, SystemState},
};
use bevy_math::{Mat3A, Mat4, Vec2};
use bevy_reflect::TypeUuid;
use bevy_render::{
    globals::{GlobalsBuffer, GlobalsUniform},
    gpu_component_array_buffer::GpuComponentArrayBufferPlugin,
    mesh::{
        skinning::{SkinnedMesh, SkinnedMeshInverseBindposes},
        GpuBufferInfo, InnerMeshVertexBufferLayout, Mesh, MeshVertexBufferLayout,
        VertexAttributeDescriptor,
    },
    prelude::Msaa,
    render_asset::RenderAssets,
    render_phase::{PhaseItem, RenderCommand, RenderCommandResult, TrackedRenderPass},
    render_resource::*,
    renderer::{RenderDevice, RenderQueue},
    texture::{
        BevyDefault, DefaultImageSampler, FallbackImageCubemap, FallbackImagesDepth,
        FallbackImagesMsaa, GpuImage, Image, ImageSampler, TextureFormatPixelInfo,
    },
    view::{ComputedVisibility, ViewTarget, ViewUniform, ViewUniformOffset, ViewUniforms},
    Extract, ExtractSchedule, Render, RenderApp, RenderSet,
};
use bevy_transform::components::GlobalTransform;
use bevy_utils::{tracing::error, HashMap, Hashed};

use crate::render::{
    morph::{extract_morphs, prepare_morphs, MorphIndex, MorphUniform},
    MeshLayouts,
};

#[derive(Default)]
pub struct MeshRenderPlugin;

const MAX_JOINTS: usize = 256;
const JOINT_SIZE: usize = std::mem::size_of::<Mat4>();
pub(crate) const JOINT_BUFFER_SIZE: usize = MAX_JOINTS * JOINT_SIZE;

pub const MESH_VERTEX_OUTPUT: HandleUntyped =
    HandleUntyped::weak_from_u64(Shader::TYPE_UUID, 2645551199423808407);
pub const MESH_VIEW_TYPES_HANDLE: HandleUntyped =
    HandleUntyped::weak_from_u64(Shader::TYPE_UUID, 8140454348013264787);
pub const MESH_VIEW_BINDINGS_HANDLE: HandleUntyped =
    HandleUntyped::weak_from_u64(Shader::TYPE_UUID, 9076678235888822571);
pub const MESH_TYPES_HANDLE: HandleUntyped =
    HandleUntyped::weak_from_u64(Shader::TYPE_UUID, 2506024101911992377);
pub const MESH_BINDINGS_HANDLE: HandleUntyped =
    HandleUntyped::weak_from_u64(Shader::TYPE_UUID, 16831548636314682308);
pub const MESH_FUNCTIONS_HANDLE: HandleUntyped =
    HandleUntyped::weak_from_u64(Shader::TYPE_UUID, 6300874327833745635);
pub const MESH_SHADER_HANDLE: HandleUntyped =
    HandleUntyped::weak_from_u64(Shader::TYPE_UUID, 3252377289100772450);
pub const SKINNING_HANDLE: HandleUntyped =
    HandleUntyped::weak_from_u64(Shader::TYPE_UUID, 13215291596265391738);
pub const MORPH_HANDLE: HandleUntyped =
    HandleUntyped::weak_from_u64(Shader::TYPE_UUID, 970982813587607345);

impl Plugin for MeshRenderPlugin {
    fn build(&self, app: &mut bevy_app::App) {
        load_internal_asset!(
            app,
            MESH_VERTEX_OUTPUT,
            "mesh_vertex_output.wgsl",
            Shader::from_wgsl
        );
        load_internal_asset!(
            app,
            MESH_VIEW_TYPES_HANDLE,
            "mesh_view_types.wgsl",
            Shader::from_wgsl_with_defs,
            vec![
                ShaderDefVal::UInt(
                    "MAX_DIRECTIONAL_LIGHTS".into(),
                    MAX_DIRECTIONAL_LIGHTS as u32
                ),
                ShaderDefVal::UInt(
                    "MAX_CASCADES_PER_LIGHT".into(),
                    MAX_CASCADES_PER_LIGHT as u32,
                )
            ]
        );
        load_internal_asset!(
            app,
            MESH_VIEW_BINDINGS_HANDLE,
            "mesh_view_bindings.wgsl",
            Shader::from_wgsl
        );
        load_internal_asset!(app, MESH_TYPES_HANDLE, "mesh_types.wgsl", Shader::from_wgsl);
        load_internal_asset!(
            app,
            MESH_FUNCTIONS_HANDLE,
            "mesh_functions.wgsl",
            Shader::from_wgsl
        );
        load_internal_asset!(app, MESH_SHADER_HANDLE, "mesh.wgsl", Shader::from_wgsl);
        load_internal_asset!(app, SKINNING_HANDLE, "skinning.wgsl", Shader::from_wgsl);
        load_internal_asset!(app, MORPH_HANDLE, "morph.wgsl", Shader::from_wgsl);

        app.add_plugins(GpuComponentArrayBufferPlugin::<MeshUniform>::default());

        if let Ok(render_app) = app.get_sub_app_mut(RenderApp) {
            render_app
                .init_resource::<SkinnedMeshUniform>()
                .init_resource::<MeshBindGroups>()
                .init_resource::<MorphUniform>()
                .add_systems(
                    ExtractSchedule,
                    (extract_meshes, extract_skinned_meshes, extract_morphs),
                )
                .add_systems(
                    Render,
                    (
                        prepare_skinned_meshes.in_set(RenderSet::Prepare),
                        prepare_morphs.in_set(RenderSet::Prepare),
                        queue_mesh_bind_group.in_set(RenderSet::Queue),
                        queue_mesh_view_bind_groups.in_set(RenderSet::Queue),
                    ),
                );
        }
    }

    fn finish(&self, app: &mut bevy_app::App) {
        let mut mesh_bindings_shader_defs = Vec::with_capacity(1);

        if let Ok(render_app) = app.get_sub_app_mut(RenderApp) {
            if let Some(per_object_buffer_batch_size) = GpuArrayBuffer::<MeshUniform>::batch_size(
                render_app.world.resource::<RenderDevice>(),
            ) {
                mesh_bindings_shader_defs.push(ShaderDefVal::UInt(
                    "PER_OBJECT_BUFFER_BATCH_SIZE".into(),
                    per_object_buffer_batch_size,
                ));
            }

            render_app.init_resource::<MeshPipeline>();
        }

        // Load the mesh_bindings shader module here as it depends on runtime information about
        // whether storage buffers are supported, or the maximum uniform buffer binding size.
        load_internal_asset!(
            app,
            MESH_BINDINGS_HANDLE,
            "mesh_bindings.wgsl",
            Shader::from_wgsl_with_defs,
            mesh_bindings_shader_defs
        );
    }
}

#[derive(Component, ShaderType, Clone)]
pub struct MeshUniform {
    pub transform: Mat4,
    pub previous_transform: Mat4,
    pub inverse_transpose_model: Mat4,
    pub flags: u32,
}

// NOTE: These must match the bit flags in bevy_pbr/src/render/mesh_types.wgsl!
bitflags::bitflags! {
    #[repr(transparent)]
    struct MeshFlags: u32 {
        const SHADOW_RECEIVER            = (1 << 0);
        // Indicates the sign of the determinant of the 3x3 model matrix. If the sign is positive,
        // then the flag should be set, else it should not be set.
        const SIGN_DETERMINANT_MODEL_3X3 = (1 << 31);
        const NONE                       = 0;
        const UNINITIALIZED              = 0xFFFF;
    }
}

pub fn extract_meshes(
    mut commands: Commands,
    mut prev_caster_commands_len: Local<usize>,
    mut prev_not_caster_commands_len: Local<usize>,
    meshes_query: Extract<
        Query<(
            Entity,
            &ComputedVisibility,
            &GlobalTransform,
            Option<&PreviousGlobalTransform>,
            &Handle<Mesh>,
            Option<With<NotShadowReceiver>>,
            Option<With<NotShadowCaster>>,
        )>,
    >,
) {
    let mut caster_commands = Vec::with_capacity(*prev_caster_commands_len);
    let mut not_caster_commands = Vec::with_capacity(*prev_not_caster_commands_len);
    let visible_meshes = meshes_query.iter().filter(|(_, vis, ..)| vis.is_visible());

    for (entity, _, transform, previous_transform, handle, not_receiver, not_caster) in
        visible_meshes
    {
        let transform = transform.compute_matrix();
        let previous_transform = previous_transform.map(|t| t.0).unwrap_or(transform);
        let mut flags = if not_receiver.is_some() {
            MeshFlags::empty()
        } else {
            MeshFlags::SHADOW_RECEIVER
        };
        if Mat3A::from_mat4(transform).determinant().is_sign_positive() {
            flags |= MeshFlags::SIGN_DETERMINANT_MODEL_3X3;
        }
        let uniform = MeshUniform {
            flags: flags.bits(),
            transform,
            previous_transform,
            inverse_transpose_model: transform.inverse().transpose(),
        };
        if not_caster.is_some() {
            not_caster_commands.push((entity, (handle.clone_weak(), uniform, NotShadowCaster)));
        } else {
            caster_commands.push((entity, (handle.clone_weak(), uniform)));
        }
    }
    *prev_caster_commands_len = caster_commands.len();
    *prev_not_caster_commands_len = not_caster_commands.len();
    commands.insert_or_spawn_batch(caster_commands);
    commands.insert_or_spawn_batch(not_caster_commands);
}

#[derive(Component)]
pub struct SkinnedMeshJoints {
    pub index: u32,
}

impl SkinnedMeshJoints {
    #[inline]
    pub fn build(
        skin: &SkinnedMesh,
        inverse_bindposes: &Assets<SkinnedMeshInverseBindposes>,
        joints: &Query<&GlobalTransform>,
        buffer: &mut BufferVec<Mat4>,
    ) -> Option<Self> {
        let inverse_bindposes = inverse_bindposes.get(&skin.inverse_bindposes)?;
        let start = buffer.len();
        let target = start + skin.joints.len().min(MAX_JOINTS);
        buffer.extend(
            joints
                .iter_many(&skin.joints)
                .zip(inverse_bindposes.iter())
                .map(|(joint, bindpose)| joint.affine() * *bindpose),
        );
        // iter_many will skip any failed fetches. This will cause it to assign the wrong bones,
        // so just bail by truncating to the start.
        if buffer.len() != target {
            buffer.truncate(start);
            return None;
        }

        // Pad to 256 byte alignment
        while buffer.len() % 4 != 0 {
            buffer.push(Mat4::ZERO);
        }
        Some(Self {
            index: start as u32,
        })
    }

    /// Updated index to be in address space based on [`SkinnedMeshUniform`] size.
    pub fn to_buffer_index(mut self) -> Self {
        self.index *= std::mem::size_of::<Mat4>() as u32;
        self
    }
}

pub fn extract_skinned_meshes(
    mut commands: Commands,
    mut previous_len: Local<usize>,
    mut uniform: ResMut<SkinnedMeshUniform>,
    query: Extract<Query<(Entity, &ComputedVisibility, &SkinnedMesh)>>,
    inverse_bindposes: Extract<Res<Assets<SkinnedMeshInverseBindposes>>>,
    joint_query: Extract<Query<&GlobalTransform>>,
) {
    uniform.buffer.clear();
    let mut values = Vec::with_capacity(*previous_len);
    let mut last_start = 0;

    for (entity, computed_visibility, skin) in &query {
        if !computed_visibility.is_visible() {
            continue;
        }
        // PERF: This can be expensive, can we move this to prepare?
        if let Some(skinned_joints) =
            SkinnedMeshJoints::build(skin, &inverse_bindposes, &joint_query, &mut uniform.buffer)
        {
            last_start = last_start.max(skinned_joints.index as usize);
            values.push((entity, skinned_joints.to_buffer_index()));
        }
    }

    // Pad out the buffer to ensure that there's enough space for bindings
    while uniform.buffer.len() - last_start < MAX_JOINTS {
        uniform.buffer.push(Mat4::ZERO);
    }

    *previous_len = values.len();
    commands.insert_or_spawn_batch(values);
}

#[derive(Resource, Clone)]
pub struct MeshPipeline {
    pub view_layout: BindGroupLayout,
    pub view_layout_multisampled: BindGroupLayout,
    // This dummy white texture is to be used in place of optional StandardMaterial textures
    pub dummy_white_gpu_image: GpuImage,
    pub clustered_forward_buffer_binding_type: BufferBindingType,
    pub mesh_layouts: MeshLayouts,
    /// `MeshUniform`s are stored in arrays in buffers. If storage buffers are available, they
    /// are used and this will be `None`, otherwise uniform buffers will be used with batches
    /// of this many `MeshUniform`s, stored at dynamic offsets within the uniform buffer.
    /// Use code like this in custom shaders:
    /// ```wgsl
    /// ##ifdef PER_OBJECT_BUFFER_BATCH_SIZE
    /// @group(2) @binding(0)
    /// var<uniform> mesh: array<Mesh, #{PER_OBJECT_BUFFER_BATCH_SIZE}u>;
    /// ##else
    /// @group(2) @binding(0)
    /// var<storage> mesh: array<Mesh>;
    /// ##endif // PER_OBJECT_BUFFER_BATCH_SIZE
    /// ```
    pub per_object_buffer_batch_size: Option<u32>,
}

impl FromWorld for MeshPipeline {
    fn from_world(world: &mut World) -> Self {
        let mut system_state: SystemState<(
            Res<RenderDevice>,
            Res<DefaultImageSampler>,
            Res<RenderQueue>,
        )> = SystemState::new(world);
        let (render_device, default_sampler, render_queue) = system_state.get_mut(world);
        let clustered_forward_buffer_binding_type = render_device
            .get_supported_read_only_binding_type(CLUSTERED_FORWARD_STORAGE_BUFFER_COUNT);

        /// Returns the appropriate bind group layout vec based on the parameters
        fn layout_entries(
            clustered_forward_buffer_binding_type: BufferBindingType,
            multisampled: bool,
        ) -> Vec<BindGroupLayoutEntry> {
            let mut entries = vec![
                // View
                BindGroupLayoutEntry {
                    binding: 0,
                    visibility: ShaderStages::VERTEX | ShaderStages::FRAGMENT,
                    ty: BindingType::Buffer {
                        ty: BufferBindingType::Uniform,
                        has_dynamic_offset: true,
                        min_binding_size: Some(ViewUniform::min_size()),
                    },
                    count: None,
                },
                // Lights
                BindGroupLayoutEntry {
                    binding: 1,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Buffer {
                        ty: BufferBindingType::Uniform,
                        has_dynamic_offset: true,
                        min_binding_size: Some(GpuLights::min_size()),
                    },
                    count: None,
                },
                // Point Shadow Texture Cube Array
                BindGroupLayoutEntry {
                    binding: 2,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Texture {
                        multisampled: false,
                        sample_type: TextureSampleType::Depth,
                        #[cfg(any(not(feature = "webgl"), not(target_arch = "wasm32")))]
                        view_dimension: TextureViewDimension::CubeArray,
                        #[cfg(all(feature = "webgl", target_arch = "wasm32"))]
                        view_dimension: TextureViewDimension::Cube,
                    },
                    count: None,
                },
                // Point Shadow Texture Array Sampler
                BindGroupLayoutEntry {
                    binding: 3,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Sampler(SamplerBindingType::Comparison),
                    count: None,
                },
                // Directional Shadow Texture Array
                BindGroupLayoutEntry {
                    binding: 4,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Texture {
                        multisampled: false,
                        sample_type: TextureSampleType::Depth,
                        #[cfg(any(not(feature = "webgl"), not(target_arch = "wasm32")))]
                        view_dimension: TextureViewDimension::D2Array,
                        #[cfg(all(feature = "webgl", target_arch = "wasm32"))]
                        view_dimension: TextureViewDimension::D2,
                    },
                    count: None,
                },
                // Directional Shadow Texture Array Sampler
                BindGroupLayoutEntry {
                    binding: 5,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Sampler(SamplerBindingType::Comparison),
                    count: None,
                },
                // PointLights
                BindGroupLayoutEntry {
                    binding: 6,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Buffer {
                        ty: clustered_forward_buffer_binding_type,
                        has_dynamic_offset: false,
                        min_binding_size: Some(GpuPointLights::min_size(
                            clustered_forward_buffer_binding_type,
                        )),
                    },
                    count: None,
                },
                // ClusteredLightIndexLists
                BindGroupLayoutEntry {
                    binding: 7,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Buffer {
                        ty: clustered_forward_buffer_binding_type,
                        has_dynamic_offset: false,
                        min_binding_size: Some(
                            ViewClusterBindings::min_size_cluster_light_index_lists(
                                clustered_forward_buffer_binding_type,
                            ),
                        ),
                    },
                    count: None,
                },
                // ClusterOffsetsAndCounts
                BindGroupLayoutEntry {
                    binding: 8,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Buffer {
                        ty: clustered_forward_buffer_binding_type,
                        has_dynamic_offset: false,
                        min_binding_size: Some(
                            ViewClusterBindings::min_size_cluster_offsets_and_counts(
                                clustered_forward_buffer_binding_type,
                            ),
                        ),
                    },
                    count: None,
                },
                // Globals
                BindGroupLayoutEntry {
                    binding: 9,
                    visibility: ShaderStages::VERTEX_FRAGMENT,
                    ty: BindingType::Buffer {
                        ty: BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: Some(GlobalsUniform::min_size()),
                    },
                    count: None,
                },
                // Fog
                BindGroupLayoutEntry {
                    binding: 10,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Buffer {
                        ty: BufferBindingType::Uniform,
                        has_dynamic_offset: true,
                        min_binding_size: Some(GpuFog::min_size()),
                    },
                    count: None,
                },
                // Screen space ambient occlusion texture
                BindGroupLayoutEntry {
                    binding: 11,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Texture {
                        multisampled: false,
                        sample_type: TextureSampleType::Float { filterable: false },
                        view_dimension: TextureViewDimension::D2,
                    },
                    count: None,
                },
            ];

            // EnvironmentMapLight
            let environment_map_entries =
                environment_map::get_bind_group_layout_entries([12, 13, 14]);
            entries.extend_from_slice(&environment_map_entries);

            // Tonemapping
            let tonemapping_lut_entries = get_lut_bind_group_layout_entries([15, 16]);
            entries.extend_from_slice(&tonemapping_lut_entries);

            if cfg!(any(not(feature = "webgl"), not(target_arch = "wasm32")))
                || (cfg!(all(feature = "webgl", target_arch = "wasm32")) && !multisampled)
            {
                entries.extend_from_slice(&prepass::get_bind_group_layout_entries(
                    [17, 18, 19],
                    multisampled,
                ));
            }

            entries
        }

        let view_layout = render_device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("mesh_view_layout"),
            entries: &layout_entries(clustered_forward_buffer_binding_type, false),
        });

        let view_layout_multisampled =
            render_device.create_bind_group_layout(&BindGroupLayoutDescriptor {
                label: Some("mesh_view_layout_multisampled"),
                entries: &layout_entries(clustered_forward_buffer_binding_type, true),
            });

        // A 1x1x1 'all 1.0' texture to use as a dummy texture to use in place of optional StandardMaterial textures
        let dummy_white_gpu_image = {
            let image = Image::default();
            let texture = render_device.create_texture(&image.texture_descriptor);
            let sampler = match image.sampler_descriptor {
                ImageSampler::Default => (**default_sampler).clone(),
                ImageSampler::Descriptor(descriptor) => render_device.create_sampler(&descriptor),
            };

            let format_size = image.texture_descriptor.format.pixel_size();
            render_queue.write_texture(
                ImageCopyTexture {
                    texture: &texture,
                    mip_level: 0,
                    origin: Origin3d::ZERO,
                    aspect: TextureAspect::All,
                },
                &image.data,
                ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(image.texture_descriptor.size.width * format_size as u32),
                    rows_per_image: None,
                },
                image.texture_descriptor.size,
            );

            let texture_view = texture.create_view(&TextureViewDescriptor::default());
            GpuImage {
                texture,
                texture_view,
                texture_format: image.texture_descriptor.format,
                sampler,
                size: Vec2::new(
                    image.texture_descriptor.size.width as f32,
                    image.texture_descriptor.size.height as f32,
                ),
                mip_level_count: image.texture_descriptor.mip_level_count,
            }
        };

        MeshPipeline {
            view_layout,
            view_layout_multisampled,
            clustered_forward_buffer_binding_type,
            dummy_white_gpu_image,
            mesh_layouts: MeshLayouts::new(&render_device),
            per_object_buffer_batch_size: GpuArrayBuffer::<MeshUniform>::batch_size(&render_device),
        }
    }
}

impl MeshPipeline {
    pub fn get_image_texture<'a>(
        &'a self,
        gpu_images: &'a RenderAssets<Image>,
        handle_option: &Option<Handle<Image>>,
    ) -> Option<(&'a TextureView, &'a Sampler)> {
        if let Some(handle) = handle_option {
            let gpu_image = gpu_images.get(handle)?;
            Some((&gpu_image.texture_view, &gpu_image.sampler))
        } else {
            Some((
                &self.dummy_white_gpu_image.texture_view,
                &self.dummy_white_gpu_image.sampler,
            ))
        }
    }
}

bitflags::bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    #[repr(transparent)]
    // NOTE: Apparently quadro drivers support up to 64x MSAA.
    /// MSAA uses the highest 3 bits for the MSAA log2(sample count) to support up to 128x MSAA.
    pub struct MeshPipelineKey: u32 {
        const NONE                              = 0;
        const HDR                               = (1 << 0);
        const TONEMAP_IN_SHADER                 = (1 << 1);
        const DEBAND_DITHER                     = (1 << 2);
        const DEPTH_PREPASS                     = (1 << 3);
        const NORMAL_PREPASS                    = (1 << 4);
        const MOTION_VECTOR_PREPASS             = (1 << 5);
        const MAY_DISCARD                       = (1 << 6); // Guards shader codepaths that may discard, allowing early depth tests in most cases
                                                            // See: https://www.khronos.org/opengl/wiki/Early_Fragment_Test
        const ENVIRONMENT_MAP                   = (1 << 7);
        const SCREEN_SPACE_AMBIENT_OCCLUSION    = (1 << 8);
        const DEPTH_CLAMP_ORTHO                 = (1 << 9);
        const TAA                               = (1 << 10);
        const MORPH_TARGETS                     = (1 << 11);
        const BLEND_RESERVED_BITS               = Self::BLEND_MASK_BITS << Self::BLEND_SHIFT_BITS; // ← Bitmask reserving bits for the blend state
        const BLEND_OPAQUE                      = (0 << Self::BLEND_SHIFT_BITS);                   // ← Values are just sequential within the mask, and can range from 0 to 3
        const BLEND_PREMULTIPLIED_ALPHA         = (1 << Self::BLEND_SHIFT_BITS);                   //
        const BLEND_MULTIPLY                    = (2 << Self::BLEND_SHIFT_BITS);                   // ← We still have room for one more value without adding more bits
        const BLEND_ALPHA                       = (3 << Self::BLEND_SHIFT_BITS);
        const MSAA_RESERVED_BITS                = Self::MSAA_MASK_BITS << Self::MSAA_SHIFT_BITS;
        const PRIMITIVE_TOPOLOGY_RESERVED_BITS  = Self::PRIMITIVE_TOPOLOGY_MASK_BITS << Self::PRIMITIVE_TOPOLOGY_SHIFT_BITS;
        const TONEMAP_METHOD_RESERVED_BITS      = Self::TONEMAP_METHOD_MASK_BITS << Self::TONEMAP_METHOD_SHIFT_BITS;
        const TONEMAP_METHOD_NONE               = 0 << Self::TONEMAP_METHOD_SHIFT_BITS;
        const TONEMAP_METHOD_REINHARD           = 1 << Self::TONEMAP_METHOD_SHIFT_BITS;
        const TONEMAP_METHOD_REINHARD_LUMINANCE = 2 << Self::TONEMAP_METHOD_SHIFT_BITS;
        const TONEMAP_METHOD_ACES_FITTED        = 3 << Self::TONEMAP_METHOD_SHIFT_BITS;
        const TONEMAP_METHOD_AGX                = 4 << Self::TONEMAP_METHOD_SHIFT_BITS;
        const TONEMAP_METHOD_SOMEWHAT_BORING_DISPLAY_TRANSFORM = 5 << Self::TONEMAP_METHOD_SHIFT_BITS;
        const TONEMAP_METHOD_TONY_MC_MAPFACE    = 6 << Self::TONEMAP_METHOD_SHIFT_BITS;
        const TONEMAP_METHOD_BLENDER_FILMIC     = 7 << Self::TONEMAP_METHOD_SHIFT_BITS;
    }
}

impl MeshPipelineKey {
    const MSAA_MASK_BITS: u32 = 0b111;
    const MSAA_SHIFT_BITS: u32 = 32 - Self::MSAA_MASK_BITS.count_ones();
    const PRIMITIVE_TOPOLOGY_MASK_BITS: u32 = 0b111;
    const PRIMITIVE_TOPOLOGY_SHIFT_BITS: u32 =
        Self::MSAA_SHIFT_BITS - Self::PRIMITIVE_TOPOLOGY_MASK_BITS.count_ones();
    const BLEND_MASK_BITS: u32 = 0b11;
    const BLEND_SHIFT_BITS: u32 =
        Self::PRIMITIVE_TOPOLOGY_SHIFT_BITS - Self::BLEND_MASK_BITS.count_ones();
    const TONEMAP_METHOD_MASK_BITS: u32 = 0b111;
    const TONEMAP_METHOD_SHIFT_BITS: u32 =
        Self::BLEND_SHIFT_BITS - Self::TONEMAP_METHOD_MASK_BITS.count_ones();

    pub fn from_msaa_samples(msaa_samples: u32) -> Self {
        let msaa_bits =
            (msaa_samples.trailing_zeros() & Self::MSAA_MASK_BITS) << Self::MSAA_SHIFT_BITS;
        Self::from_bits_retain(msaa_bits)
    }

    pub fn from_hdr(hdr: bool) -> Self {
        if hdr {
            MeshPipelineKey::HDR
        } else {
            MeshPipelineKey::NONE
        }
    }

    pub fn msaa_samples(&self) -> u32 {
        1 << ((self.bits() >> Self::MSAA_SHIFT_BITS) & Self::MSAA_MASK_BITS)
    }

    pub fn from_primitive_topology(primitive_topology: PrimitiveTopology) -> Self {
        let primitive_topology_bits = ((primitive_topology as u32)
            & Self::PRIMITIVE_TOPOLOGY_MASK_BITS)
            << Self::PRIMITIVE_TOPOLOGY_SHIFT_BITS;
        Self::from_bits_retain(primitive_topology_bits)
    }

    pub fn primitive_topology(&self) -> PrimitiveTopology {
        let primitive_topology_bits = (self.bits() >> Self::PRIMITIVE_TOPOLOGY_SHIFT_BITS)
            & Self::PRIMITIVE_TOPOLOGY_MASK_BITS;
        match primitive_topology_bits {
            x if x == PrimitiveTopology::PointList as u32 => PrimitiveTopology::PointList,
            x if x == PrimitiveTopology::LineList as u32 => PrimitiveTopology::LineList,
            x if x == PrimitiveTopology::LineStrip as u32 => PrimitiveTopology::LineStrip,
            x if x == PrimitiveTopology::TriangleList as u32 => PrimitiveTopology::TriangleList,
            x if x == PrimitiveTopology::TriangleStrip as u32 => PrimitiveTopology::TriangleStrip,
            _ => PrimitiveTopology::default(),
        }
    }
}

fn is_skinned(layout: &Hashed<InnerMeshVertexBufferLayout>) -> bool {
    layout.contains(Mesh::ATTRIBUTE_JOINT_INDEX) && layout.contains(Mesh::ATTRIBUTE_JOINT_WEIGHT)
}
pub fn setup_morph_and_skinning_defs(
    mesh_layouts: &MeshLayouts,
    layout: &Hashed<InnerMeshVertexBufferLayout>,
    offset: u32,
    key: &MeshPipelineKey,
    shader_defs: &mut Vec<ShaderDefVal>,
    vertex_attributes: &mut Vec<VertexAttributeDescriptor>,
) -> BindGroupLayout {
    let mut add_skin_data = || {
        shader_defs.push("SKINNED".into());
        vertex_attributes.push(Mesh::ATTRIBUTE_JOINT_INDEX.at_shader_location(offset));
        vertex_attributes.push(Mesh::ATTRIBUTE_JOINT_WEIGHT.at_shader_location(offset + 1));
    };
    let is_morphed = key.intersects(MeshPipelineKey::MORPH_TARGETS);
    match (is_skinned(layout), is_morphed) {
        (true, false) => {
            add_skin_data();
            mesh_layouts.skinned.clone()
        }
        (true, true) => {
            add_skin_data();
            shader_defs.push("MORPH_TARGETS".into());
            mesh_layouts.morphed_skinned.clone()
        }
        (false, true) => {
            shader_defs.push("MORPH_TARGETS".into());
            mesh_layouts.morphed.clone()
        }
        (false, false) => mesh_layouts.model_only.clone(),
    }
}

impl SpecializedMeshPipeline for MeshPipeline {
    type Key = MeshPipelineKey;

    fn specialize(
        &self,
        key: Self::Key,
        layout: &MeshVertexBufferLayout,
    ) -> Result<RenderPipelineDescriptor, SpecializedMeshPipelineError> {
        let mut shader_defs = Vec::new();
        let mut vertex_attributes = Vec::new();

        shader_defs.push("VERTEX_OUTPUT_INSTANCE_INDEX".into());

        if layout.contains(Mesh::ATTRIBUTE_POSITION) {
            shader_defs.push("VERTEX_POSITIONS".into());
            vertex_attributes.push(Mesh::ATTRIBUTE_POSITION.at_shader_location(0));
        }

        if layout.contains(Mesh::ATTRIBUTE_NORMAL) {
            shader_defs.push("VERTEX_NORMALS".into());
            vertex_attributes.push(Mesh::ATTRIBUTE_NORMAL.at_shader_location(1));
        }

        if layout.contains(Mesh::ATTRIBUTE_UV_0) {
            shader_defs.push("VERTEX_UVS".into());
            vertex_attributes.push(Mesh::ATTRIBUTE_UV_0.at_shader_location(2));
        }

        if layout.contains(Mesh::ATTRIBUTE_TANGENT) {
            shader_defs.push("VERTEX_TANGENTS".into());
            vertex_attributes.push(Mesh::ATTRIBUTE_TANGENT.at_shader_location(3));
        }

        if layout.contains(Mesh::ATTRIBUTE_COLOR) {
            shader_defs.push("VERTEX_COLORS".into());
            vertex_attributes.push(Mesh::ATTRIBUTE_COLOR.at_shader_location(4));
        }

        let mut bind_group_layout = match key.msaa_samples() {
            1 => vec![self.view_layout.clone()],
            _ => {
                shader_defs.push("MULTISAMPLED".into());
                vec![self.view_layout_multisampled.clone()]
            }
        };

        bind_group_layout.push(setup_morph_and_skinning_defs(
            &self.mesh_layouts,
            layout,
            5,
            &key,
            &mut shader_defs,
            &mut vertex_attributes,
        ));

        if key.contains(MeshPipelineKey::SCREEN_SPACE_AMBIENT_OCCLUSION) {
            shader_defs.push("SCREEN_SPACE_AMBIENT_OCCLUSION".into());
        }

        let vertex_buffer_layout = layout.get_layout(&vertex_attributes)?;

        let (label, blend, depth_write_enabled);
        let pass = key.intersection(MeshPipelineKey::BLEND_RESERVED_BITS);
        let mut is_opaque = false;
        if pass == MeshPipelineKey::BLEND_ALPHA {
            label = "alpha_blend_mesh_pipeline".into();
            blend = Some(BlendState::ALPHA_BLENDING);
            // For the transparent pass, fragments that are closer will be alpha blended
            // but their depth is not written to the depth buffer
            depth_write_enabled = false;
        } else if pass == MeshPipelineKey::BLEND_PREMULTIPLIED_ALPHA {
            label = "premultiplied_alpha_mesh_pipeline".into();
            blend = Some(BlendState::PREMULTIPLIED_ALPHA_BLENDING);
            shader_defs.push("PREMULTIPLY_ALPHA".into());
            shader_defs.push("BLEND_PREMULTIPLIED_ALPHA".into());
            // For the transparent pass, fragments that are closer will be alpha blended
            // but their depth is not written to the depth buffer
            depth_write_enabled = false;
        } else if pass == MeshPipelineKey::BLEND_MULTIPLY {
            label = "multiply_mesh_pipeline".into();
            blend = Some(BlendState {
                color: BlendComponent {
                    src_factor: BlendFactor::Dst,
                    dst_factor: BlendFactor::OneMinusSrcAlpha,
                    operation: BlendOperation::Add,
                },
                alpha: BlendComponent::OVER,
            });
            shader_defs.push("PREMULTIPLY_ALPHA".into());
            shader_defs.push("BLEND_MULTIPLY".into());
            // For the multiply pass, fragments that are closer will be alpha blended
            // but their depth is not written to the depth buffer
            depth_write_enabled = false;
        } else {
            label = "opaque_mesh_pipeline".into();
            blend = Some(BlendState::REPLACE);
            // For the opaque and alpha mask passes, fragments that are closer will replace
            // the current fragment value in the output and the depth is written to the
            // depth buffer
            depth_write_enabled = true;
            is_opaque = true;
        }

        if key.contains(MeshPipelineKey::NORMAL_PREPASS) && key.msaa_samples() == 1 && is_opaque {
            shader_defs.push("LOAD_PREPASS_NORMALS".into());
        }

        if key.contains(MeshPipelineKey::TONEMAP_IN_SHADER) {
            shader_defs.push("TONEMAP_IN_SHADER".into());

            let method = key.intersection(MeshPipelineKey::TONEMAP_METHOD_RESERVED_BITS);

            if method == MeshPipelineKey::TONEMAP_METHOD_NONE {
                shader_defs.push("TONEMAP_METHOD_NONE".into());
            } else if method == MeshPipelineKey::TONEMAP_METHOD_REINHARD {
                shader_defs.push("TONEMAP_METHOD_REINHARD".into());
            } else if method == MeshPipelineKey::TONEMAP_METHOD_REINHARD_LUMINANCE {
                shader_defs.push("TONEMAP_METHOD_REINHARD_LUMINANCE".into());
            } else if method == MeshPipelineKey::TONEMAP_METHOD_ACES_FITTED {
                shader_defs.push("TONEMAP_METHOD_ACES_FITTED ".into());
            } else if method == MeshPipelineKey::TONEMAP_METHOD_AGX {
                shader_defs.push("TONEMAP_METHOD_AGX".into());
            } else if method == MeshPipelineKey::TONEMAP_METHOD_SOMEWHAT_BORING_DISPLAY_TRANSFORM {
                shader_defs.push("TONEMAP_METHOD_SOMEWHAT_BORING_DISPLAY_TRANSFORM".into());
            } else if method == MeshPipelineKey::TONEMAP_METHOD_BLENDER_FILMIC {
                shader_defs.push("TONEMAP_METHOD_BLENDER_FILMIC".into());
            } else if method == MeshPipelineKey::TONEMAP_METHOD_TONY_MC_MAPFACE {
                shader_defs.push("TONEMAP_METHOD_TONY_MC_MAPFACE".into());
            }

            // Debanding is tied to tonemapping in the shader, cannot run without it.
            if key.contains(MeshPipelineKey::DEBAND_DITHER) {
                shader_defs.push("DEBAND_DITHER".into());
            }
        }

        if key.contains(MeshPipelineKey::MAY_DISCARD) {
            shader_defs.push("MAY_DISCARD".into());
        }

        if key.contains(MeshPipelineKey::ENVIRONMENT_MAP) {
            shader_defs.push("ENVIRONMENT_MAP".into());
        }

        if key.contains(MeshPipelineKey::TAA) {
            shader_defs.push("TAA".into());
        }

        let format = if key.contains(MeshPipelineKey::HDR) {
            ViewTarget::TEXTURE_FORMAT_HDR
        } else {
            TextureFormat::bevy_default()
        };

        // This is defined here so that custom shaders that use something other than
        // the mesh binding from bevy_pbr::mesh_bindings can easily make use of this
        // in their own shaders.
        if let Some(per_object_buffer_batch_size) = self.per_object_buffer_batch_size {
            shader_defs.push(ShaderDefVal::UInt(
                "PER_OBJECT_BUFFER_BATCH_SIZE".into(),
                per_object_buffer_batch_size,
            ));
        }

        let mut push_constant_ranges = Vec::with_capacity(1);
        if cfg!(all(feature = "webgl", target_arch = "wasm32")) {
            push_constant_ranges.push(PushConstantRange {
                stages: ShaderStages::VERTEX,
                range: 0..4,
            });
        }

        Ok(RenderPipelineDescriptor {
            vertex: VertexState {
                shader: MESH_SHADER_HANDLE.typed::<Shader>(),
                entry_point: "vertex".into(),
                shader_defs: shader_defs.clone(),
                buffers: vec![vertex_buffer_layout],
            },
            fragment: Some(FragmentState {
                shader: MESH_SHADER_HANDLE.typed::<Shader>(),
                shader_defs,
                entry_point: "fragment".into(),
                targets: vec![Some(ColorTargetState {
                    format,
                    blend,
                    write_mask: ColorWrites::ALL,
                })],
            }),
            layout: bind_group_layout,
            push_constant_ranges,
            primitive: PrimitiveState {
                front_face: FrontFace::Ccw,
                cull_mode: Some(Face::Back),
                unclipped_depth: false,
                polygon_mode: PolygonMode::Fill,
                conservative: false,
                topology: key.primitive_topology(),
                strip_index_format: None,
            },
            depth_stencil: Some(DepthStencilState {
                format: TextureFormat::Depth32Float,
                depth_write_enabled,
                depth_compare: CompareFunction::GreaterEqual,
                stencil: StencilState {
                    front: StencilFaceState::IGNORE,
                    back: StencilFaceState::IGNORE,
                    read_mask: 0,
                    write_mask: 0,
                },
                bias: DepthBiasState {
                    constant: 0,
                    slope_scale: 0.0,
                    clamp: 0.0,
                },
            }),
            multisample: MultisampleState {
                count: key.msaa_samples(),
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            label: Some(label),
        })
    }
}

/// Bind groups for meshes currently loaded.
#[derive(Resource, Default)]
pub struct MeshBindGroups {
    model_only: Option<BindGroup>,
    skinned: Option<BindGroup>,
    morph_targets: HashMap<HandleId, BindGroup>,
}
impl MeshBindGroups {
    pub fn reset(&mut self) {
        self.model_only = None;
        self.skinned = None;
        self.morph_targets.clear();
    }
    /// Get the `BindGroup` for `GpuMesh` with given `handle_id`.
    pub fn get(&self, handle_id: HandleId, is_skinned: bool, morph: bool) -> Option<&BindGroup> {
        match (is_skinned, morph) {
            (_, true) => self.morph_targets.get(&handle_id),
            (true, false) => self.skinned.as_ref(),
            (false, false) => self.model_only.as_ref(),
        }
    }
}

pub fn queue_mesh_bind_group(
    meshes: Res<RenderAssets<Mesh>>,
    mut groups: ResMut<MeshBindGroups>,
    mesh_pipeline: Res<MeshPipeline>,
    render_device: Res<RenderDevice>,
    mesh_uniforms: Res<GpuArrayBuffer<MeshUniform>>,
    skinned_mesh_uniform: Res<SkinnedMeshUniform>,
    weights_uniform: Res<MorphUniform>,
) {
    groups.reset();
    let layouts = &mesh_pipeline.mesh_layouts;
    let Some(model) = mesh_uniforms.binding() else {
        return;
    };
    groups.model_only = Some(layouts.model_only(&render_device, &model));

    let skin = skinned_mesh_uniform.buffer.buffer();
    if let Some(skin) = skin {
        groups.skinned = Some(layouts.skinned(&render_device, &model, skin));
    }

    if let Some(weights) = weights_uniform.buffer.buffer() {
        for (id, gpu_mesh) in meshes.iter() {
            if let Some(targets) = gpu_mesh.morph_targets.as_ref() {
                let group = if let Some(skin) = skin.filter(|_| is_skinned(&gpu_mesh.layout)) {
                    layouts.morphed_skinned(&render_device, &model, skin, weights, targets)
                } else {
                    layouts.morphed(&render_device, &model, weights, targets)
                };
                groups.morph_targets.insert(id.id(), group);
            }
        }
    }
}

// NOTE: This is using BufferVec because it is using a trick to allow a fixed-size array
// in a uniform buffer to be used like a variable-sized array by only writing the valid data
// into the buffer, knowing the number of valid items starting from the dynamic offset, and
// ignoring the rest, whether they're valid for other dynamic offsets or not. This trick may
// be supported later in encase, and then we should make use of it.

#[derive(Resource)]
pub struct SkinnedMeshUniform {
    pub buffer: BufferVec<Mat4>,
}

impl Default for SkinnedMeshUniform {
    fn default() -> Self {
        Self {
            buffer: BufferVec::new(BufferUsages::UNIFORM),
        }
    }
}

pub fn prepare_skinned_meshes(
    render_device: Res<RenderDevice>,
    render_queue: Res<RenderQueue>,
    mut skinned_mesh_uniform: ResMut<SkinnedMeshUniform>,
) {
    if skinned_mesh_uniform.buffer.is_empty() {
        return;
    }

    let len = skinned_mesh_uniform.buffer.len();
    skinned_mesh_uniform.buffer.reserve(len, &render_device);
    skinned_mesh_uniform
        .buffer
        .write_buffer(&render_device, &render_queue);
}

#[derive(Component)]
pub struct MeshViewBindGroup {
    pub value: BindGroup,
}

#[allow(clippy::too_many_arguments)]
pub fn queue_mesh_view_bind_groups(
    mut commands: Commands,
    render_device: Res<RenderDevice>,
    mesh_pipeline: Res<MeshPipeline>,
    shadow_samplers: Res<ShadowSamplers>,
    light_meta: Res<LightMeta>,
    global_light_meta: Res<GlobalLightMeta>,
    fog_meta: Res<FogMeta>,
    view_uniforms: Res<ViewUniforms>,
    views: Query<(
        Entity,
        &ViewShadowBindings,
        &ViewClusterBindings,
        Option<&ScreenSpaceAmbientOcclusionTextures>,
        Option<&ViewPrepassTextures>,
        Option<&EnvironmentMapLight>,
        &Tonemapping,
    )>,
    images: Res<RenderAssets<Image>>,
    mut fallback_images: FallbackImagesMsaa,
    mut fallback_depths: FallbackImagesDepth,
    fallback_cubemap: Res<FallbackImageCubemap>,
    msaa: Res<Msaa>,
    globals_buffer: Res<GlobalsBuffer>,
    tonemapping_luts: Res<TonemappingLuts>,
) {
    if let (
        Some(view_binding),
        Some(light_binding),
        Some(point_light_binding),
        Some(globals),
        Some(fog_binding),
    ) = (
        view_uniforms.uniforms.binding(),
        light_meta.view_gpu_lights.binding(),
        global_light_meta.gpu_point_lights.binding(),
        globals_buffer.buffer.binding(),
        fog_meta.gpu_fogs.binding(),
    ) {
        for (
            entity,
            view_shadow_bindings,
            view_cluster_bindings,
            ssao_textures,
            prepass_textures,
            environment_map,
            tonemapping,
        ) in &views
        {
            let fallback_ssao = fallback_images
                .image_for_samplecount(1)
                .texture_view
                .clone();

            let layout = if msaa.samples() > 1 {
                &mesh_pipeline.view_layout_multisampled
            } else {
                &mesh_pipeline.view_layout
            };

            let mut entries = vec![
                BindGroupEntry {
                    binding: 0,
                    resource: view_binding.clone(),
                },
                BindGroupEntry {
                    binding: 1,
                    resource: light_binding.clone(),
                },
                BindGroupEntry {
                    binding: 2,
                    resource: BindingResource::TextureView(
                        &view_shadow_bindings.point_light_depth_texture_view,
                    ),
                },
                BindGroupEntry {
                    binding: 3,
                    resource: BindingResource::Sampler(&shadow_samplers.point_light_sampler),
                },
                BindGroupEntry {
                    binding: 4,
                    resource: BindingResource::TextureView(
                        &view_shadow_bindings.directional_light_depth_texture_view,
                    ),
                },
                BindGroupEntry {
                    binding: 5,
                    resource: BindingResource::Sampler(&shadow_samplers.directional_light_sampler),
                },
                BindGroupEntry {
                    binding: 6,
                    resource: point_light_binding.clone(),
                },
                BindGroupEntry {
                    binding: 7,
                    resource: view_cluster_bindings.light_index_lists_binding().unwrap(),
                },
                BindGroupEntry {
                    binding: 8,
                    resource: view_cluster_bindings.offsets_and_counts_binding().unwrap(),
                },
                BindGroupEntry {
                    binding: 9,
                    resource: globals.clone(),
                },
                BindGroupEntry {
                    binding: 10,
                    resource: fog_binding.clone(),
                },
                BindGroupEntry {
                    binding: 11,
                    resource: BindingResource::TextureView(
                        ssao_textures
                            .map(|t| &t.screen_space_ambient_occlusion_texture.default_view)
                            .unwrap_or(&fallback_ssao),
                    ),
                },
            ];

            let env_map = environment_map::get_bindings(
                environment_map,
                &images,
                &fallback_cubemap,
                [12, 13, 14],
            );
            entries.extend_from_slice(&env_map);

            let tonemapping_luts =
                get_lut_bindings(&images, &tonemapping_luts, tonemapping, [15, 16]);
            entries.extend_from_slice(&tonemapping_luts);

            // When using WebGL, we can't have a depth texture with multisampling
            if cfg!(any(not(feature = "webgl"), not(target_arch = "wasm32")))
                || (cfg!(all(feature = "webgl", target_arch = "wasm32")) && msaa.samples() == 1)
            {
                entries.extend_from_slice(&prepass::get_bindings(
                    prepass_textures,
                    &mut fallback_images,
                    &mut fallback_depths,
                    &msaa,
                    [17, 18, 19],
                ));
            }

            let view_bind_group = render_device.create_bind_group(&BindGroupDescriptor {
                entries: &entries,
                label: Some("mesh_view_bind_group"),
                layout,
            });

            commands.entity(entity).insert(MeshViewBindGroup {
                value: view_bind_group,
            });
        }
    }
}

pub struct SetMeshViewBindGroup<const I: usize>;
impl<P: PhaseItem, const I: usize> RenderCommand<P> for SetMeshViewBindGroup<I> {
    type Param = ();
    type ViewWorldQuery = (
        Read<ViewUniformOffset>,
        Read<ViewLightsUniformOffset>,
        Read<ViewFogUniformOffset>,
        Read<MeshViewBindGroup>,
    );
    type ItemWorldQuery = ();

    #[inline]
    fn render<'w>(
        _item: &P,
        (view_uniform, view_lights, view_fog, mesh_view_bind_group): ROQueryItem<
            'w,
            Self::ViewWorldQuery,
        >,
        _entity: (),
        _: SystemParamItem<'w, '_, Self::Param>,
        pass: &mut TrackedRenderPass<'w>,
    ) -> RenderCommandResult {
        pass.set_bind_group(
            I,
            &mesh_view_bind_group.value,
            &[view_uniform.offset, view_lights.offset, view_fog.offset],
        );

        RenderCommandResult::Success
    }
}

pub struct SetMeshBindGroup<const I: usize>;
impl<P: PhaseItem, const I: usize> RenderCommand<P> for SetMeshBindGroup<I> {
    type Param = SRes<MeshBindGroups>;
    type ViewWorldQuery = ();
    type ItemWorldQuery = (
        Read<Handle<Mesh>>,
        Read<GpuArrayBufferIndex<MeshUniform>>,
        Option<Read<SkinnedMeshJoints>>,
        Option<Read<MorphIndex>>,
    );

    #[inline]
    fn render<'w>(
        _item: &P,
        _view: (),
        (mesh, batch_indices, skin_index, morph_index): ROQueryItem<Self::ItemWorldQuery>,
        bind_groups: SystemParamItem<'w, '_, Self::Param>,
        pass: &mut TrackedRenderPass<'w>,
    ) -> RenderCommandResult {
        let bind_groups = bind_groups.into_inner();
        let is_skinned = skin_index.is_some();
        let is_morphed = morph_index.is_some();

        let Some(bind_group) = bind_groups.get(mesh.id(), is_skinned, is_morphed) else {
            error!(
                "The MeshBindGroups resource wasn't set in the render phase. \
                It should be set by the queue_mesh_bind_group system.\n\
                This is a bevy bug! Please open an issue."
            );
            return RenderCommandResult::Failure;
        };

        let mut dynamic_offsets: [u32; 3] = Default::default();
        let mut index_count = 0;
        if let Some(mesh_index) = batch_indices.dynamic_offset {
            dynamic_offsets[index_count] = mesh_index;
            index_count += 1;
        }
        if let Some(skin_index) = skin_index {
            dynamic_offsets[index_count] = skin_index.index;
            index_count += 1;
        }
        if let Some(morph_index) = morph_index {
            dynamic_offsets[index_count] = morph_index.index;
            index_count += 1;
        }
        pass.set_bind_group(I, bind_group, &dynamic_offsets[0..index_count]);

        RenderCommandResult::Success
    }
}

pub struct DrawMesh;
impl<P: PhaseItem> RenderCommand<P> for DrawMesh {
    type Param = SRes<RenderAssets<Mesh>>;
    type ViewWorldQuery = ();
    type ItemWorldQuery = (Read<GpuArrayBufferIndex<MeshUniform>>, Read<Handle<Mesh>>);
    #[inline]
    fn render<'w>(
        _item: &P,
        _view: (),
        (batch_indices, mesh_handle): ROQueryItem<'_, Self::ItemWorldQuery>,
        meshes: SystemParamItem<'w, '_, Self::Param>,
        pass: &mut TrackedRenderPass<'w>,
    ) -> RenderCommandResult {
        if let Some(gpu_mesh) = meshes.into_inner().get(mesh_handle) {
            pass.set_vertex_buffer(0, gpu_mesh.vertex_buffer.slice(..));
            #[cfg(all(feature = "webgl", target_arch = "wasm32"))]
            pass.set_push_constants(
                ShaderStages::VERTEX,
                0,
                &(batch_indices.index as i32).to_le_bytes(),
            );
            match &gpu_mesh.buffer_info {
                GpuBufferInfo::Indexed {
                    buffer,
                    index_format,
                    count,
                } => {
                    pass.set_index_buffer(buffer.slice(..), 0, *index_format);
                    pass.draw_indexed(0..*count, 0, batch_indices.index..batch_indices.index + 1);
                }
                GpuBufferInfo::NonIndexed => {
                    pass.draw(
                        0..gpu_mesh.vertex_count,
                        batch_indices.index..batch_indices.index + 1,
                    );
                }
            }
            RenderCommandResult::Success
        } else {
            RenderCommandResult::Failure
        }
    }
}

#[cfg(test)]
mod tests {
    use super::MeshPipelineKey;
    #[test]
    fn mesh_key_msaa_samples() {
        for i in [1, 2, 4, 8, 16, 32, 64, 128] {
            assert_eq!(MeshPipelineKey::from_msaa_samples(i).msaa_samples(), i);
        }
    }
}
