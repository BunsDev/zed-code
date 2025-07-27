use std::{mem::ManuallyDrop, sync::Arc};

use ::util::ResultExt;
use anyhow::{Context, Result};
use windows::Win32::{
    Foundation::{HMODULE, HWND},
    Graphics::{
        Direct3D::*,
        Direct3D11::*,
        Dxgi::{Common::*, *},
    },
};
#[cfg(not(feature = "enable-renderdoc"))]
use windows::{Win32::Graphics::DirectComposition::*, core::Interface};

use crate::{
    platform::windows::directx_renderer::shader_resources::{
        RawShaderBytes, ShaderModule, ShaderTarget,
    },
    *,
};

const RENDER_TARGET_FORMAT: DXGI_FORMAT = DXGI_FORMAT_B8G8R8A8_UNORM;
// This configuration is used for MSAA rendering, and it's guaranteed to be supported by DirectX 11.
const MULTISAMPLE_COUNT: u32 = 4;

pub(crate) struct DirectXRenderer {
    hwnd: HWND,
    atlas: Arc<DirectXAtlas>,
    devices: ManuallyDrop<DirectXDevices>,
    resources: ManuallyDrop<DirectXResources>,
    globals: DirectXGlobalElements,
    pipelines: DirectXRenderPipelines,
    #[cfg(not(feature = "enable-renderdoc"))]
    _direct_composition: ManuallyDrop<DirectComposition>,
}

/// Direct3D objects
#[derive(Clone)]
pub(crate) struct DirectXDevices {
    adapter: IDXGIAdapter1,
    dxgi_factory: IDXGIFactory6,
    #[cfg(not(feature = "enable-renderdoc"))]
    dxgi_device: IDXGIDevice,
    device: ID3D11Device,
    device_context: ID3D11DeviceContext,
}

struct DirectXResources {
    // Direct3D rendering objects
    swap_chain: IDXGISwapChain1,
    render_target: ManuallyDrop<ID3D11Texture2D>,
    render_target_view: [Option<ID3D11RenderTargetView>; 1],
    msaa_target: ID3D11Texture2D,
    msaa_view: [Option<ID3D11RenderTargetView>; 1],

    // Cached window size and viewport
    width: u32,
    height: u32,
    viewport: [D3D11_VIEWPORT; 1],
}

struct DirectXRenderPipelines {
    shadow_pipeline: PipelineState<Shadow>,
    quad_pipeline: PipelineState<Quad>,
    paths_pipeline: PathsPipelineState,
    underline_pipeline: PipelineState<Underline>,
    mono_sprites: PipelineState<MonochromeSprite>,
    poly_sprites: PipelineState<PolychromeSprite>,
}

struct DirectXGlobalElements {
    global_params_buffer: [Option<ID3D11Buffer>; 1],
    sampler: [Option<ID3D11SamplerState>; 1],
    blend_state: ID3D11BlendState,
}

#[repr(C)]
struct DrawInstancedIndirectArgs {
    vertex_count_per_instance: u32,
    instance_count: u32,
    start_vertex_location: u32,
    start_instance_location: u32,
}

#[cfg(not(feature = "enable-renderdoc"))]
struct DirectComposition {
    comp_device: IDCompositionDevice,
    comp_target: IDCompositionTarget,
    comp_visual: IDCompositionVisual,
}

impl DirectXDevices {
    pub(crate) fn new() -> Result<Self> {
        let dxgi_factory = get_dxgi_factory()?;
        let adapter = get_adapter(&dxgi_factory)?;
        let (device, device_context) = {
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            get_device(&adapter, Some(&mut device), Some(&mut context))?;
            (device.unwrap(), context.unwrap())
        };
        #[cfg(not(feature = "enable-renderdoc"))]
        let dxgi_device: IDXGIDevice = device.cast()?;

        Ok(Self {
            adapter,
            dxgi_factory,
            #[cfg(not(feature = "enable-renderdoc"))]
            dxgi_device,
            device,
            device_context,
        })
    }
}

impl DirectXRenderer {
    pub(crate) fn new(hwnd: HWND) -> Result<Self> {
        let devices = ManuallyDrop::new(DirectXDevices::new().context("Creating DirectX devices")?);
        let atlas = Arc::new(DirectXAtlas::new(&devices.device, &devices.device_context));

        #[cfg(not(feature = "enable-renderdoc"))]
        let resources = DirectXResources::new(&devices, 1, 1).unwrap();
        #[cfg(feature = "enable-renderdoc")]
        let resources = DirectXResources::new(&devices, hwnd)?;

        let globals = DirectXGlobalElements::new(&devices.device).unwrap();
        let pipelines = DirectXRenderPipelines::new(&devices.device).unwrap();

        #[cfg(not(feature = "enable-renderdoc"))]
        let direct_composition = DirectComposition::new(&devices.dxgi_device, hwnd).unwrap();
        #[cfg(not(feature = "enable-renderdoc"))]
        direct_composition
            .set_swap_chain(&resources.swap_chain)
            .unwrap();

        Ok(DirectXRenderer {
            hwnd,
            atlas,
            devices,
            resources,
            globals,
            pipelines,
            #[cfg(not(feature = "enable-renderdoc"))]
            _direct_composition: direct_composition,
        })
    }

    pub(crate) fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.atlas.clone()
    }

    fn pre_draw(&self) -> Result<()> {
        update_buffer(
            &self.devices.device_context,
            self.globals.global_params_buffer[0].as_ref().unwrap(),
            &[GlobalParams {
                viewport_size: [
                    self.resources.viewport[0].Width,
                    self.resources.viewport[0].Height,
                ],
                ..Default::default()
            }],
        )?;
        unsafe {
            self.devices
                .device_context
                .ClearRenderTargetView(self.resources.msaa_view[0].as_ref().unwrap(), &[0.0; 4]);
            self.devices
                .device_context
                .OMSetRenderTargets(Some(&self.resources.msaa_view), None);
            self.devices
                .device_context
                .RSSetViewports(Some(&self.resources.viewport));
            self.devices.device_context.OMSetBlendState(
                &self.globals.blend_state,
                None,
                0xFFFFFFFF,
            );
        }
        Ok(())
    }

    fn present(&mut self) -> Result<()> {
        unsafe {
            self.devices.device_context.ResolveSubresource(
                &*self.resources.render_target,
                0,
                &self.resources.msaa_target,
                0,
                RENDER_TARGET_FORMAT,
            );
            self.devices
                .device_context
                .OMSetRenderTargets(Some(&self.resources.render_target_view), None);
            let result = self.resources.swap_chain.Present(0, DXGI_PRESENT(0));
            // Presenting the swap chain can fail if the DirectX device was removed or reset.
            if result == DXGI_ERROR_DEVICE_REMOVED || result == DXGI_ERROR_DEVICE_RESET {
                let reason = self.devices.device.GetDeviceRemovedReason();
                log::error!(
                    "DirectX device removed or reset when drawing. Reason: {:?}",
                    reason
                );
                self.handle_device_lost()?;
            } else {
                result.ok()?;
            }
        }
        Ok(())
    }

    fn handle_device_lost(&mut self) -> Result<()> {
        unsafe {
            ManuallyDrop::drop(&mut self.devices);
            ManuallyDrop::drop(&mut self.resources);
            #[cfg(not(feature = "enable-renderdoc"))]
            ManuallyDrop::drop(&mut self._direct_composition);
        }
        let devices =
            ManuallyDrop::new(DirectXDevices::new().context("Recreating DirectX devices")?);
        unsafe {
            devices.device_context.OMSetRenderTargets(None, None);
            devices.device_context.ClearState();
            devices.device_context.Flush();
        }
        #[cfg(not(feature = "enable-renderdoc"))]
        let resources =
            DirectXResources::new(&devices, self.resources.width, self.resources.height).unwrap();
        #[cfg(feature = "enable-renderdoc")]
        let resources = DirectXResources::new(
            &devices,
            self.resources.width,
            self.resources.height,
            self.hwnd,
        )?;
        let globals = DirectXGlobalElements::new(&devices.device).unwrap();
        let pipelines = DirectXRenderPipelines::new(&devices.device).unwrap();

        #[cfg(not(feature = "enable-renderdoc"))]
        let direct_composition = DirectComposition::new(&devices.dxgi_device, self.hwnd).unwrap();
        #[cfg(not(feature = "enable-renderdoc"))]
        direct_composition
            .set_swap_chain(&resources.swap_chain)
            .unwrap();

        self.atlas
            .handle_device_lost(&devices.device, &devices.device_context);
        self.devices = devices;
        self.resources = resources;
        self.globals = globals;
        self.pipelines = pipelines;
        #[cfg(not(feature = "enable-renderdoc"))]
        {
            self._direct_composition = direct_composition;
        }
        unsafe {
            self.devices
                .device_context
                .OMSetRenderTargets(Some(&self.resources.render_target_view), None);
        }
        Ok(())
    }

    pub(crate) fn draw(&mut self, scene: &Scene) -> Result<()> {
        self.pre_draw()?;
        for batch in scene.batches() {
            match batch {
                PrimitiveBatch::Shadows(shadows) => self.draw_shadows(shadows),
                PrimitiveBatch::Quads(quads) => self.draw_quads(quads),
                PrimitiveBatch::Paths(paths) => self.draw_paths(paths),
                PrimitiveBatch::Underlines(underlines) => self.draw_underlines(underlines),
                PrimitiveBatch::MonochromeSprites {
                    texture_id,
                    sprites,
                } => self.draw_monochrome_sprites(texture_id, sprites),
                PrimitiveBatch::PolychromeSprites {
                    texture_id,
                    sprites,
                } => self.draw_polychrome_sprites(texture_id, sprites),
                PrimitiveBatch::Surfaces(surfaces) => self.draw_surfaces(surfaces),
            }.context(format!("scene too large: {} paths, {} shadows, {} quads, {} underlines, {} mono, {} poly, {} surfaces",
                    scene.paths.len(),
                    scene.shadows.len(),
                    scene.quads.len(),
                    scene.underlines.len(),
                    scene.monochrome_sprites.len(),
                    scene.polychrome_sprites.len(),
                    scene.surfaces.len(),))?;
        }
        self.present()
    }

    pub(crate) fn resize(&mut self, new_size: Size<DevicePixels>) -> Result<()> {
        let width = new_size.width.0.max(1) as u32;
        let height = new_size.height.0.max(1) as u32;
        if self.resources.width == width && self.resources.height == height {
            return Ok(());
        }
        unsafe {
            // Clear the render target before resizing
            self.devices.device_context.OMSetRenderTargets(None, None);
            ManuallyDrop::drop(&mut self.resources.render_target);
            drop(self.resources.render_target_view[0].take().unwrap());

            let result = self.resources.swap_chain.ResizeBuffers(
                BUFFER_COUNT as u32,
                width,
                height,
                RENDER_TARGET_FORMAT,
                DXGI_SWAP_CHAIN_FLAG(0),
            );
            // Resizing the swap chain requires a call to the underlying DXGI adapter, which can return the device removed error.
            // The app might have moved to a monitor that's attached to a different graphics device.
            // When a graphics device is removed or reset, the desktop resolution often changes, resulting in a window size change.
            match result {
                Ok(_) => {}
                Err(e) => {
                    if e.code() == DXGI_ERROR_DEVICE_REMOVED || e.code() == DXGI_ERROR_DEVICE_RESET
                    {
                        let reason = self.devices.device.GetDeviceRemovedReason();
                        log::error!(
                            "DirectX device removed or reset when resizing. Reason: {:?}",
                            reason
                        );
                        self.handle_device_lost()?;
                        return Ok(());
                    }
                    log::error!("Failed to resize swap chain: {:?}", e);
                    return Err(e.into());
                }
            }

            self.resources
                .recreate_resources(&self.devices, width, height)?;
            self.devices
                .device_context
                .OMSetRenderTargets(Some(&self.resources.render_target_view), None);
        }
        Ok(())
    }

    fn draw_shadows(&mut self, shadows: &[Shadow]) -> Result<()> {
        if shadows.is_empty() {
            return Ok(());
        }
        self.pipelines.shadow_pipeline.update_buffer(
            &self.devices.device,
            &self.devices.device_context,
            shadows,
        )?;
        self.pipelines.shadow_pipeline.draw(
            &self.devices.device_context,
            &self.resources.viewport,
            &self.globals.global_params_buffer,
            shadows.len() as u32,
        )
    }

    fn draw_quads(&mut self, quads: &[Quad]) -> Result<()> {
        if quads.is_empty() {
            return Ok(());
        }
        self.pipelines.quad_pipeline.update_buffer(
            &self.devices.device,
            &self.devices.device_context,
            quads,
        )?;
        self.pipelines.quad_pipeline.draw(
            &self.devices.device_context,
            &self.resources.viewport,
            &self.globals.global_params_buffer,
            quads.len() as u32,
        )
    }

    fn draw_paths(&mut self, paths: &[Path<ScaledPixels>]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut vertices = Vec::new();
        let mut sprites = Vec::with_capacity(paths.len());
        let mut draw_indirect_commands = Vec::with_capacity(paths.len());
        let mut start_vertex_location = 0;
        for (i, path) in paths.iter().enumerate() {
            draw_indirect_commands.push(DrawInstancedIndirectArgs {
                vertex_count_per_instance: path.vertices.len() as u32,
                instance_count: 1,
                start_vertex_location,
                start_instance_location: i as u32,
            });
            start_vertex_location += path.vertices.len() as u32;

            vertices.extend(path.vertices.iter().map(|v| DirectXPathVertex {
                xy_position: v.xy_position,
                content_mask: path.content_mask.bounds,
                sprite_index: i as u32,
            }));

            sprites.push(PathSprite {
                bounds: path.bounds,
                color: path.color,
            });
        }

        self.pipelines.paths_pipeline.update_buffer(
            &self.devices.device,
            &self.devices.device_context,
            &sprites,
            &vertices,
            &draw_indirect_commands,
        )?;
        self.pipelines.paths_pipeline.draw(
            &self.devices.device_context,
            paths.len(),
            &self.resources.viewport,
            &self.globals.global_params_buffer,
        )
    }

    fn draw_underlines(&mut self, underlines: &[Underline]) -> Result<()> {
        if underlines.is_empty() {
            return Ok(());
        }
        self.pipelines.underline_pipeline.update_buffer(
            &self.devices.device,
            &self.devices.device_context,
            underlines,
        )?;
        self.pipelines.underline_pipeline.draw(
            &self.devices.device_context,
            &self.resources.viewport,
            &self.globals.global_params_buffer,
            underlines.len() as u32,
        )
    }

    fn draw_monochrome_sprites(
        &mut self,
        texture_id: AtlasTextureId,
        sprites: &[MonochromeSprite],
    ) -> Result<()> {
        if sprites.is_empty() {
            return Ok(());
        }
        self.pipelines.mono_sprites.update_buffer(
            &self.devices.device,
            &self.devices.device_context,
            sprites,
        )?;
        let texture_view = self.atlas.get_texture_view(texture_id);
        self.pipelines.mono_sprites.draw_with_texture(
            &self.devices.device_context,
            &texture_view,
            &self.resources.viewport,
            &self.globals.global_params_buffer,
            &self.globals.sampler,
            sprites.len() as u32,
        )
    }

    fn draw_polychrome_sprites(
        &mut self,
        texture_id: AtlasTextureId,
        sprites: &[PolychromeSprite],
    ) -> Result<()> {
        if sprites.is_empty() {
            return Ok(());
        }
        self.pipelines.poly_sprites.update_buffer(
            &self.devices.device,
            &self.devices.device_context,
            sprites,
        )?;
        let texture_view = self.atlas.get_texture_view(texture_id);
        self.pipelines.poly_sprites.draw_with_texture(
            &self.devices.device_context,
            &texture_view,
            &self.resources.viewport,
            &self.globals.global_params_buffer,
            &self.globals.sampler,
            sprites.len() as u32,
        )
    }

    fn draw_surfaces(&mut self, surfaces: &[PaintSurface]) -> Result<()> {
        if surfaces.is_empty() {
            return Ok(());
        }
        Ok(())
    }

    pub(crate) fn gpu_specs(&self) -> Result<GpuSpecs> {
        let desc = unsafe { self.devices.adapter.GetDesc1() }?;
        let is_software_emulated = (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0;
        let device_name = String::from_utf16_lossy(&desc.Description)
            .trim_matches(char::from(0))
            .to_string();
        let driver_name = match desc.VendorId {
            0x10DE => "NVIDIA Corporation".to_string(),
            0x1002 => "AMD Corporation".to_string(),
            0x8086 => "Intel Corporation".to_string(),
            _ => "Unknown Vendor".to_string(),
        };
        let driver_version = match desc.VendorId {
            0x10DE => nvidia::get_driver_version(),
            0x1002 => amd::get_driver_version(),
            0x8086 => intel::get_driver_version(&self.devices.adapter),
            _ => Err(anyhow::anyhow!("Unknown vendor detected.")),
        }
        .context("Failed to get gpu driver info")
        .log_err()
        .unwrap_or("Unknown Driver".to_string());
        Ok(GpuSpecs {
            is_software_emulated,
            device_name,
            driver_name,
            driver_info: driver_version,
        })
    }
}

impl DirectXResources {
    pub fn new(
        devices: &DirectXDevices,
        width: u32,
        height: u32,
        #[cfg(feature = "enable-renderdoc")] hwnd: HWND,
    ) -> Result<ManuallyDrop<Self>> {
        #[cfg(not(feature = "enable-renderdoc"))]
        let swap_chain = create_swap_chain(&devices.dxgi_factory, &devices.device, width, height)?;
        #[cfg(feature = "enable-renderdoc")]
        let swap_chain =
            create_swap_chain(&devices.dxgi_factory, &devices.device, hwnd, width, height)?;

        let (render_target, render_target_view, msaa_target, msaa_view, viewport) =
            create_resources(devices, &swap_chain, width, height)?;
        set_rasterizer_state(&devices.device, &devices.device_context)?;

        Ok(ManuallyDrop::new(Self {
            swap_chain,
            render_target,
            render_target_view,
            msaa_target,
            msaa_view,
            width,
            height,
            viewport,
        }))
    }

    #[inline]
    fn recreate_resources(
        &mut self,
        devices: &DirectXDevices,
        width: u32,
        height: u32,
    ) -> Result<()> {
        let (render_target, render_target_view, msaa_target, msaa_view, viewport) =
            create_resources(devices, &self.swap_chain, width, height)?;
        self.render_target = render_target;
        self.render_target_view = render_target_view;
        self.msaa_target = msaa_target;
        self.msaa_view = msaa_view;
        self.viewport = viewport;
        self.width = width;
        self.height = height;
        Ok(())
    }
}

impl DirectXRenderPipelines {
    pub fn new(device: &ID3D11Device) -> Result<Self> {
        let shadow_pipeline =
            PipelineState::new(device, "shadow_pipeline", ShaderModule::Shadow, 4)?;
        let quad_pipeline = PipelineState::new(device, "quad_pipeline", ShaderModule::Quad, 64)?;
        let paths_pipeline = PathsPipelineState::new(device)?;
        let underline_pipeline =
            PipelineState::new(device, "underline_pipeline", ShaderModule::Underline, 4)?;
        let mono_sprites = PipelineState::new(
            device,
            "monochrome_sprite_pipeline",
            ShaderModule::MonochromeSprite,
            512,
        )?;
        let poly_sprites = PipelineState::new(
            device,
            "polychrome_sprite_pipeline",
            ShaderModule::PolychromeSprite,
            16,
        )?;

        Ok(Self {
            shadow_pipeline,
            quad_pipeline,
            paths_pipeline,
            underline_pipeline,
            mono_sprites,
            poly_sprites,
        })
    }
}

#[cfg(not(feature = "enable-renderdoc"))]
impl DirectComposition {
    pub fn new(dxgi_device: &IDXGIDevice, hwnd: HWND) -> Result<ManuallyDrop<Self>> {
        let comp_device = get_comp_device(&dxgi_device).unwrap();
        let comp_target = unsafe { comp_device.CreateTargetForHwnd(hwnd, true) }.unwrap();
        let comp_visual = unsafe { comp_device.CreateVisual() }.unwrap();

        Ok(ManuallyDrop::new(Self {
            comp_device,
            comp_target,
            comp_visual,
        }))
    }

    pub fn set_swap_chain(&self, swap_chain: &IDXGISwapChain1) -> Result<()> {
        unsafe {
            self.comp_visual.SetContent(swap_chain)?;
            self.comp_target.SetRoot(&self.comp_visual)?;
            self.comp_device.Commit()?;
        }
        Ok(())
    }
}

impl DirectXGlobalElements {
    pub fn new(device: &ID3D11Device) -> Result<Self> {
        let global_params_buffer = unsafe {
            let desc = D3D11_BUFFER_DESC {
                ByteWidth: std::mem::size_of::<GlobalParams>() as u32,
                Usage: D3D11_USAGE_DYNAMIC,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                ..Default::default()
            };
            let mut buffer = None;
            device.CreateBuffer(&desc, None, Some(&mut buffer))?;
            [buffer]
        };

        let sampler = unsafe {
            let desc = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D11_TEXTURE_ADDRESS_WRAP,
                AddressV: D3D11_TEXTURE_ADDRESS_WRAP,
                AddressW: D3D11_TEXTURE_ADDRESS_WRAP,
                MipLODBias: 0.0,
                MaxAnisotropy: 1,
                ComparisonFunc: D3D11_COMPARISON_ALWAYS,
                BorderColor: [0.0; 4],
                MinLOD: 0.0,
                MaxLOD: D3D11_FLOAT32_MAX,
            };
            let mut output = None;
            device.CreateSamplerState(&desc, Some(&mut output))?;
            [output]
        };

        let blend_state = create_blend_state(device)?;

        Ok(Self {
            global_params_buffer,
            sampler,
            blend_state,
        })
    }
}

#[derive(Debug, Default)]
#[repr(C)]
struct GlobalParams {
    viewport_size: [f32; 2],
    _pad: u64,
}

struct PipelineState<T> {
    label: &'static str,
    vertex: ID3D11VertexShader,
    fragment: ID3D11PixelShader,
    buffer: ID3D11Buffer,
    buffer_size: usize,
    view: [Option<ID3D11ShaderResourceView>; 1],
    _marker: std::marker::PhantomData<T>,
}

struct PathsPipelineState {
    vertex: ID3D11VertexShader,
    fragment: ID3D11PixelShader,
    buffer: ID3D11Buffer,
    buffer_size: usize,
    vertex_buffer: Option<ID3D11Buffer>,
    vertex_buffer_size: usize,
    indirect_draw_buffer: ID3D11Buffer,
    indirect_buffer_size: usize,
    input_layout: ID3D11InputLayout,
    view: [Option<ID3D11ShaderResourceView>; 1],
}

impl<T> PipelineState<T> {
    fn new(
        device: &ID3D11Device,
        label: &'static str,
        shader_module: ShaderModule,
        buffer_size: usize,
    ) -> Result<Self> {
        let vertex = {
            let raw_shader = RawShaderBytes::new(shader_module, ShaderTarget::Vertex)?;
            create_vertex_shader(device, raw_shader.as_bytes())?
        };
        let fragment = {
            let raw_shader = RawShaderBytes::new(shader_module, ShaderTarget::Fragment)?;
            create_fragment_shader(device, raw_shader.as_bytes())?
        };
        let buffer = create_buffer(device, std::mem::size_of::<T>(), buffer_size)?;
        let view = create_buffer_view(device, &buffer)?;

        Ok(PipelineState {
            label,
            vertex,
            fragment,
            buffer,
            buffer_size,
            view,
            _marker: std::marker::PhantomData,
        })
    }

    fn update_buffer(
        &mut self,
        device: &ID3D11Device,
        device_context: &ID3D11DeviceContext,
        data: &[T],
    ) -> Result<()> {
        if self.buffer_size < data.len() {
            let new_buffer_size = data.len().next_power_of_two();
            log::info!(
                "Updating {} buffer size from {} to {}",
                self.label,
                self.buffer_size,
                new_buffer_size
            );
            let buffer = create_buffer(device, std::mem::size_of::<T>(), new_buffer_size)?;
            let view = create_buffer_view(device, &buffer)?;
            self.buffer = buffer;
            self.view = view;
            self.buffer_size = new_buffer_size;
        }
        update_buffer(device_context, &self.buffer, data)
    }

    fn draw(
        &self,
        device_context: &ID3D11DeviceContext,
        viewport: &[D3D11_VIEWPORT],
        global_params: &[Option<ID3D11Buffer>],
        instance_count: u32,
    ) -> Result<()> {
        set_pipeline_state(
            device_context,
            &self.view,
            D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP,
            viewport,
            &self.vertex,
            &self.fragment,
            global_params,
        );
        unsafe {
            device_context.DrawInstanced(4, instance_count, 0, 0);
        }
        Ok(())
    }

    fn draw_with_texture(
        &self,
        device_context: &ID3D11DeviceContext,
        texture: &[Option<ID3D11ShaderResourceView>],
        viewport: &[D3D11_VIEWPORT],
        global_params: &[Option<ID3D11Buffer>],
        sampler: &[Option<ID3D11SamplerState>],
        instance_count: u32,
    ) -> Result<()> {
        set_pipeline_state(
            device_context,
            &self.view,
            D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP,
            viewport,
            &self.vertex,
            &self.fragment,
            global_params,
        );
        unsafe {
            device_context.PSSetSamplers(0, Some(sampler));
            device_context.VSSetShaderResources(0, Some(texture));
            device_context.PSSetShaderResources(0, Some(texture));

            device_context.DrawInstanced(4, instance_count, 0, 0);
        }
        Ok(())
    }
}

impl PathsPipelineState {
    fn new(device: &ID3D11Device) -> Result<Self> {
        let (vertex, vertex_shader) = {
            let raw_vertex_shader = RawShaderBytes::new(ShaderModule::Paths, ShaderTarget::Vertex)?;
            (
                create_vertex_shader(device, raw_vertex_shader.as_bytes())?,
                raw_vertex_shader,
            )
        };
        let fragment = {
            let raw_shader = RawShaderBytes::new(ShaderModule::Paths, ShaderTarget::Fragment)?;
            create_fragment_shader(device, raw_shader.as_bytes())?
        };
        let buffer = create_buffer(device, std::mem::size_of::<PathSprite>(), 32)?;
        let view = create_buffer_view(device, &buffer)?;
        let vertex_buffer = Some(create_buffer(
            device,
            std::mem::size_of::<DirectXPathVertex>(),
            32,
        )?);
        let indirect_draw_buffer = create_indirect_draw_buffer(device, 32)?;
        // Create input layout
        let input_layout = unsafe {
            let mut layout = None;
            device.CreateInputLayout(
                &[
                    D3D11_INPUT_ELEMENT_DESC {
                        SemanticName: windows::core::s!("POSITION"),
                        SemanticIndex: 0,
                        Format: DXGI_FORMAT_R32G32_FLOAT,
                        InputSlot: 0,
                        AlignedByteOffset: 0,
                        InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                        InstanceDataStepRate: 0,
                    },
                    D3D11_INPUT_ELEMENT_DESC {
                        SemanticName: windows::core::s!("TEXCOORD"),
                        SemanticIndex: 0,
                        Format: DXGI_FORMAT_R32G32_FLOAT,
                        InputSlot: 0,
                        AlignedByteOffset: 8,
                        InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                        InstanceDataStepRate: 0,
                    },
                    D3D11_INPUT_ELEMENT_DESC {
                        SemanticName: windows::core::s!("TEXCOORD"),
                        SemanticIndex: 1,
                        Format: DXGI_FORMAT_R32G32_FLOAT,
                        InputSlot: 0,
                        AlignedByteOffset: 16,
                        InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                        InstanceDataStepRate: 0,
                    },
                    D3D11_INPUT_ELEMENT_DESC {
                        SemanticName: windows::core::s!("GLOBALIDX"),
                        SemanticIndex: 0,
                        Format: DXGI_FORMAT_R32_UINT,
                        InputSlot: 0,
                        AlignedByteOffset: 24,
                        InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                        InstanceDataStepRate: 0,
                    },
                ],
                vertex_shader.as_bytes(),
                Some(&mut layout),
            )?;
            layout.unwrap()
        };

        Ok(Self {
            vertex,
            fragment,
            buffer,
            buffer_size: 32,
            vertex_buffer,
            vertex_buffer_size: 32,
            indirect_draw_buffer,
            indirect_buffer_size: 32,
            input_layout,
            view,
        })
    }

    fn update_buffer(
        &mut self,
        device: &ID3D11Device,
        device_context: &ID3D11DeviceContext,
        buffer_data: &[PathSprite],
        vertices_data: &[DirectXPathVertex],
        draw_commands: &[DrawInstancedIndirectArgs],
    ) -> Result<()> {
        if self.buffer_size < buffer_data.len() {
            let new_buffer_size = buffer_data.len().next_power_of_two();
            log::info!(
                "Updating Paths Pipeline buffer size from {} to {}",
                self.buffer_size,
                new_buffer_size
            );
            let buffer = create_buffer(device, std::mem::size_of::<PathSprite>(), new_buffer_size)?;
            let view = create_buffer_view(device, &buffer)?;
            self.buffer = buffer;
            self.view = view;
            self.buffer_size = new_buffer_size;
        }
        update_buffer(device_context, &self.buffer, buffer_data)?;
        if self.vertex_buffer_size < vertices_data.len() {
            let new_vertex_buffer_size = vertices_data.len().next_power_of_two();
            log::info!(
                "Updating Paths Pipeline vertex buffer size from {} to {}",
                self.vertex_buffer_size,
                new_vertex_buffer_size
            );
            let vertex_buffer = create_buffer(
                device,
                std::mem::size_of::<DirectXPathVertex>(),
                new_vertex_buffer_size,
            )?;
            self.vertex_buffer = Some(vertex_buffer);
            self.vertex_buffer_size = new_vertex_buffer_size;
        }
        update_buffer(
            device_context,
            self.vertex_buffer.as_ref().unwrap(),
            vertices_data,
        )?;
        if self.indirect_buffer_size < draw_commands.len() {
            let new_indirect_buffer_size = draw_commands.len().next_power_of_two();
            log::info!(
                "Updating Paths Pipeline indirect buffer size from {} to {}",
                self.indirect_buffer_size,
                new_indirect_buffer_size
            );
            let indirect_draw_buffer =
                create_indirect_draw_buffer(device, new_indirect_buffer_size)?;
            self.indirect_draw_buffer = indirect_draw_buffer;
            self.indirect_buffer_size = new_indirect_buffer_size;
        }
        update_buffer(device_context, &self.indirect_draw_buffer, draw_commands)?;
        Ok(())
    }

    fn draw(
        &self,
        device_context: &ID3D11DeviceContext,
        count: usize,
        viewport: &[D3D11_VIEWPORT],
        global_params: &[Option<ID3D11Buffer>],
    ) -> Result<()> {
        set_pipeline_state(
            device_context,
            &self.view,
            D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
            viewport,
            &self.vertex,
            &self.fragment,
            global_params,
        );
        unsafe {
            const STRIDE: u32 = std::mem::size_of::<DirectXPathVertex>() as u32;
            device_context.IASetVertexBuffers(
                0,
                1,
                Some(&self.vertex_buffer),
                Some(&STRIDE),
                Some(&0),
            );
            device_context.IASetInputLayout(&self.input_layout);
        }
        for i in 0..count {
            unsafe {
                device_context.DrawInstancedIndirect(
                    &self.indirect_draw_buffer,
                    (i * std::mem::size_of::<DrawInstancedIndirectArgs>()) as u32,
                );
            }
        }
        Ok(())
    }
}

#[repr(C)]
struct DirectXPathVertex {
    xy_position: Point<ScaledPixels>,
    content_mask: Bounds<ScaledPixels>,
    sprite_index: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[repr(C)]
struct PathSprite {
    bounds: Bounds<ScaledPixels>,
    color: Background,
}

impl Drop for DirectXRenderer {
    fn drop(&mut self) {
        unsafe {
            ManuallyDrop::drop(&mut self.devices);
            ManuallyDrop::drop(&mut self.resources);
            #[cfg(not(feature = "enable-renderdoc"))]
            ManuallyDrop::drop(&mut self._direct_composition);
        }
    }
}

impl Drop for DirectXResources {
    fn drop(&mut self) {
        unsafe {
            ManuallyDrop::drop(&mut self.render_target);
        }
    }
}

#[inline]
fn get_dxgi_factory() -> Result<IDXGIFactory6> {
    #[cfg(debug_assertions)]
    let factory_flag = DXGI_CREATE_FACTORY_DEBUG;
    #[cfg(not(debug_assertions))]
    let factory_flag = DXGI_CREATE_FACTORY_FLAGS::default();
    unsafe { Ok(CreateDXGIFactory2(factory_flag)?) }
}

fn get_adapter(dxgi_factory: &IDXGIFactory6) -> Result<IDXGIAdapter1> {
    for adapter_index in 0.. {
        let adapter: IDXGIAdapter1 = unsafe {
            dxgi_factory
                .EnumAdapterByGpuPreference(adapter_index, DXGI_GPU_PREFERENCE_MINIMUM_POWER)
        }?;
        if let Ok(desc) = unsafe { adapter.GetDesc1() } {
            let gpu_name = String::from_utf16_lossy(&desc.Description)
                .trim_matches(char::from(0))
                .to_string();
            log::info!("Using GPU: {}", gpu_name);
        }
        // Check to see whether the adapter supports Direct3D 11, but don't
        // create the actual device yet.
        if get_device(&adapter, None, None).log_err().is_some() {
            return Ok(adapter);
        }
    }

    unreachable!()
}

fn get_device(
    adapter: &IDXGIAdapter1,
    device: Option<*mut Option<ID3D11Device>>,
    context: Option<*mut Option<ID3D11DeviceContext>>,
) -> Result<()> {
    #[cfg(debug_assertions)]
    let device_flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_DEBUG;
    #[cfg(not(debug_assertions))]
    let device_flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;
    Ok(unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            device_flags,
            // 4x MSAA is required for Direct3D Feature Level 10.1 or better
            // 8x MSAA is required for Direct3D Feature Level 11.0 or better
            Some(&[D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1]),
            D3D11_SDK_VERSION,
            device,
            None,
            context,
        )?
    })
}

#[cfg(not(feature = "enable-renderdoc"))]
fn get_comp_device(dxgi_device: &IDXGIDevice) -> Result<IDCompositionDevice> {
    Ok(unsafe { DCompositionCreateDevice(dxgi_device)? })
}

#[cfg(not(feature = "enable-renderdoc"))]
fn create_swap_chain(
    dxgi_factory: &IDXGIFactory6,
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<IDXGISwapChain1> {
    let desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: width,
        Height: height,
        Format: RENDER_TARGET_FORMAT,
        Stereo: false.into(),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: BUFFER_COUNT as u32,
        // Composition SwapChains only support the DXGI_SCALING_STRETCH Scaling.
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
        AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
        Flags: 0,
    };
    Ok(unsafe { dxgi_factory.CreateSwapChainForComposition(device, &desc, None)? })
}

#[cfg(feature = "enable-renderdoc")]
fn create_swap_chain(
    dxgi_factory: &IDXGIFactory6,
    device: &ID3D11Device,
    hwnd: HWND,
    width: u32,
    height: u32,
) -> Result<IDXGISwapChain1> {
    use windows::Win32::Graphics::Dxgi::DXGI_MWA_NO_ALT_ENTER;

    let desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: width,
        Height: height,
        Format: RENDER_TARGET_FORMAT,
        Stereo: false.into(),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: BUFFER_COUNT as u32,
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
        AlphaMode: DXGI_ALPHA_MODE_IGNORE,
        Flags: 0,
    };
    let swap_chain =
        unsafe { dxgi_factory.CreateSwapChainForHwnd(device, hwnd, &desc, None, None) }?;
    unsafe { dxgi_factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER) }?;
    Ok(swap_chain)
}

#[inline]
fn create_resources(
    devices: &DirectXDevices,
    swap_chain: &IDXGISwapChain1,
    width: u32,
    height: u32,
) -> Result<(
    ManuallyDrop<ID3D11Texture2D>,
    [Option<ID3D11RenderTargetView>; 1],
    ID3D11Texture2D,
    [Option<ID3D11RenderTargetView>; 1],
    [D3D11_VIEWPORT; 1],
)> {
    let (render_target, render_target_view) =
        create_render_target_and_its_view(&swap_chain, &devices.device)?;
    let (msaa_target, msaa_view) = create_msaa_target_and_its_view(&devices.device, width, height)?;
    let viewport = set_viewport(&devices.device_context, width as f32, height as f32);
    Ok((
        render_target,
        render_target_view,
        msaa_target,
        msaa_view,
        viewport,
    ))
}

#[inline]
fn create_render_target_and_its_view(
    swap_chain: &IDXGISwapChain1,
    device: &ID3D11Device,
) -> Result<(
    ManuallyDrop<ID3D11Texture2D>,
    [Option<ID3D11RenderTargetView>; 1],
)> {
    let render_target: ID3D11Texture2D = unsafe { swap_chain.GetBuffer(0) }?;
    let mut render_target_view = None;
    unsafe { device.CreateRenderTargetView(&render_target, None, Some(&mut render_target_view))? };
    Ok((
        ManuallyDrop::new(render_target),
        [Some(render_target_view.unwrap())],
    ))
}

#[inline]
fn create_msaa_target_and_its_view(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<(ID3D11Texture2D, [Option<ID3D11RenderTargetView>; 1])> {
    let msaa_target = unsafe {
        let mut output = None;
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: RENDER_TARGET_FORMAT,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: MULTISAMPLE_COUNT,
                Quality: D3D11_STANDARD_MULTISAMPLE_PATTERN.0 as u32,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        device.CreateTexture2D(&desc, None, Some(&mut output))?;
        output.unwrap()
    };
    let msaa_view = unsafe {
        let mut output = None;
        device.CreateRenderTargetView(&msaa_target, None, Some(&mut output))?;
        output.unwrap()
    };
    Ok((msaa_target, [Some(msaa_view)]))
}

#[inline]
fn set_viewport(
    device_context: &ID3D11DeviceContext,
    width: f32,
    height: f32,
) -> [D3D11_VIEWPORT; 1] {
    let viewport = [D3D11_VIEWPORT {
        TopLeftX: 0.0,
        TopLeftY: 0.0,
        Width: width,
        Height: height,
        MinDepth: 0.0,
        MaxDepth: 1.0,
    }];
    unsafe { device_context.RSSetViewports(Some(&viewport)) };
    viewport
}

#[inline]
fn set_rasterizer_state(device: &ID3D11Device, device_context: &ID3D11DeviceContext) -> Result<()> {
    let desc = D3D11_RASTERIZER_DESC {
        FillMode: D3D11_FILL_SOLID,
        CullMode: D3D11_CULL_NONE,
        FrontCounterClockwise: false.into(),
        DepthBias: 0,
        DepthBiasClamp: 0.0,
        SlopeScaledDepthBias: 0.0,
        DepthClipEnable: true.into(),
        ScissorEnable: false.into(),
        // MultisampleEnable: false.into(),
        MultisampleEnable: true.into(),
        AntialiasedLineEnable: false.into(),
    };
    let rasterizer_state = unsafe {
        let mut state = None;
        device.CreateRasterizerState(&desc, Some(&mut state))?;
        state.unwrap()
    };
    unsafe { device_context.RSSetState(&rasterizer_state) };
    Ok(())
}

// https://learn.microsoft.com/en-us/windows/win32/api/d3d11/ns-d3d11-d3d11_blend_desc
#[inline]
fn create_blend_state(device: &ID3D11Device) -> Result<ID3D11BlendState> {
    // If the feature level is set to greater than D3D_FEATURE_LEVEL_9_3, the display
    // device performs the blend in linear space, which is ideal.
    let mut desc = D3D11_BLEND_DESC::default();
    desc.RenderTarget[0].BlendEnable = true.into();
    desc.RenderTarget[0].BlendOp = D3D11_BLEND_OP_ADD;
    desc.RenderTarget[0].BlendOpAlpha = D3D11_BLEND_OP_ADD;
    desc.RenderTarget[0].SrcBlend = D3D11_BLEND_SRC_ALPHA;
    desc.RenderTarget[0].SrcBlendAlpha = D3D11_BLEND_ONE;
    desc.RenderTarget[0].DestBlend = D3D11_BLEND_INV_SRC_ALPHA;
    desc.RenderTarget[0].DestBlendAlpha = D3D11_BLEND_ONE;
    desc.RenderTarget[0].RenderTargetWriteMask = D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8;
    unsafe {
        let mut state = None;
        device.CreateBlendState(&desc, Some(&mut state))?;
        Ok(state.unwrap())
    }
}

#[inline]
fn create_vertex_shader(device: &ID3D11Device, bytes: &[u8]) -> Result<ID3D11VertexShader> {
    unsafe {
        let mut shader = None;
        device.CreateVertexShader(bytes, None, Some(&mut shader))?;
        Ok(shader.unwrap())
    }
}

#[inline]
fn create_fragment_shader(device: &ID3D11Device, bytes: &[u8]) -> Result<ID3D11PixelShader> {
    unsafe {
        let mut shader = None;
        device.CreatePixelShader(bytes, None, Some(&mut shader))?;
        Ok(shader.unwrap())
    }
}

#[inline]
fn create_buffer(
    device: &ID3D11Device,
    element_size: usize,
    buffer_size: usize,
) -> Result<ID3D11Buffer> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: (element_size * buffer_size) as u32,
        Usage: D3D11_USAGE_DYNAMIC,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        MiscFlags: D3D11_RESOURCE_MISC_BUFFER_STRUCTURED.0 as u32,
        StructureByteStride: element_size as u32,
    };
    let mut buffer = None;
    unsafe { device.CreateBuffer(&desc, None, Some(&mut buffer)) }?;
    Ok(buffer.unwrap())
}

#[inline]
fn create_buffer_view(
    device: &ID3D11Device,
    buffer: &ID3D11Buffer,
) -> Result<[Option<ID3D11ShaderResourceView>; 1]> {
    let mut view = None;
    unsafe { device.CreateShaderResourceView(buffer, None, Some(&mut view)) }?;
    Ok([view])
}

#[inline]
fn create_indirect_draw_buffer(device: &ID3D11Device, buffer_size: usize) -> Result<ID3D11Buffer> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: (std::mem::size_of::<DrawInstancedIndirectArgs>() * buffer_size) as u32,
        Usage: D3D11_USAGE_DYNAMIC,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        MiscFlags: D3D11_RESOURCE_MISC_DRAWINDIRECT_ARGS.0 as u32,
        StructureByteStride: std::mem::size_of::<DrawInstancedIndirectArgs>() as u32,
    };
    let mut buffer = None;
    unsafe { device.CreateBuffer(&desc, None, Some(&mut buffer)) }?;
    Ok(buffer.unwrap())
}

#[inline]
fn update_buffer<T>(
    device_context: &ID3D11DeviceContext,
    buffer: &ID3D11Buffer,
    data: &[T],
) -> Result<()> {
    unsafe {
        let mut dest = std::mem::zeroed();
        device_context.Map(buffer, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut dest))?;
        std::ptr::copy_nonoverlapping(data.as_ptr(), dest.pData as _, data.len());
        device_context.Unmap(buffer, 0);
    }
    Ok(())
}

#[inline]
fn set_pipeline_state(
    device_context: &ID3D11DeviceContext,
    buffer_view: &[Option<ID3D11ShaderResourceView>],
    topology: D3D_PRIMITIVE_TOPOLOGY,
    viewport: &[D3D11_VIEWPORT],
    vertex_shader: &ID3D11VertexShader,
    fragment_shader: &ID3D11PixelShader,
    global_params: &[Option<ID3D11Buffer>],
) {
    unsafe {
        device_context.VSSetShaderResources(1, Some(buffer_view));
        device_context.PSSetShaderResources(1, Some(buffer_view));
        device_context.IASetPrimitiveTopology(topology);
        device_context.RSSetViewports(Some(viewport));
        device_context.VSSetShader(vertex_shader, None);
        device_context.PSSetShader(fragment_shader, None);
        device_context.VSSetConstantBuffers(0, Some(global_params));
        device_context.PSSetConstantBuffers(0, Some(global_params));
    }
}

const BUFFER_COUNT: usize = 3;

mod shader_resources {
    use anyhow::Result;

    #[cfg(debug_assertions)]
    use windows::{
        Win32::Graphics::Direct3D::{
            Fxc::{D3DCOMPILE_DEBUG, D3DCOMPILE_SKIP_OPTIMIZATION, D3DCompileFromFile},
            ID3DBlob,
        },
        core::{HSTRING, PCSTR},
    };

    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    pub(super) enum ShaderModule {
        Quad,
        Shadow,
        Underline,
        Paths,
        MonochromeSprite,
        PolychromeSprite,
    }

    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    pub(super) enum ShaderTarget {
        Vertex,
        Fragment,
    }

    pub(super) struct RawShaderBytes<'t> {
        inner: &'t [u8],

        #[cfg(debug_assertions)]
        _blob: ID3DBlob,
    }

    impl<'t> RawShaderBytes<'t> {
        pub(super) fn new(module: ShaderModule, target: ShaderTarget) -> Result<Self> {
            #[cfg(not(debug_assertions))]
            {
                Ok(Self::from_bytes(module, target))
            }
            #[cfg(debug_assertions)]
            {
                let blob = build_shader_blob(module, target)?;
                let inner = unsafe {
                    std::slice::from_raw_parts(
                        blob.GetBufferPointer() as *const u8,
                        blob.GetBufferSize(),
                    )
                };
                Ok(Self { inner, _blob: blob })
            }
        }

        pub(super) fn as_bytes(&'t self) -> &'t [u8] {
            self.inner
        }

        #[cfg(not(debug_assertions))]
        fn from_bytes(module: ShaderModule, target: ShaderTarget) -> Self {
            let bytes = match module {
                ShaderModule::Quad => match target {
                    ShaderTarget::Vertex => QUAD_VERTEX_BYTES,
                    ShaderTarget::Fragment => QUAD_FRAGMENT_BYTES,
                },
                ShaderModule::Shadow => match target {
                    ShaderTarget::Vertex => SHADOW_VERTEX_BYTES,
                    ShaderTarget::Fragment => SHADOW_FRAGMENT_BYTES,
                },
                ShaderModule::Underline => match target {
                    ShaderTarget::Vertex => UNDERLINE_VERTEX_BYTES,
                    ShaderTarget::Fragment => UNDERLINE_FRAGMENT_BYTES,
                },
                ShaderModule::Paths => match target {
                    ShaderTarget::Vertex => PATHS_VERTEX_BYTES,
                    ShaderTarget::Fragment => PATHS_FRAGMENT_BYTES,
                },
                ShaderModule::MonochromeSprite => match target {
                    ShaderTarget::Vertex => MONOCHROME_SPRITE_VERTEX_BYTES,
                    ShaderTarget::Fragment => MONOCHROME_SPRITE_FRAGMENT_BYTES,
                },
                ShaderModule::PolychromeSprite => match target {
                    ShaderTarget::Vertex => POLYCHROME_SPRITE_VERTEX_BYTES,
                    ShaderTarget::Fragment => POLYCHROME_SPRITE_FRAGMENT_BYTES,
                },
            };
            Self { inner: bytes }
        }
    }

    #[cfg(debug_assertions)]
    pub(super) fn build_shader_blob(entry: ShaderModule, target: ShaderTarget) -> Result<ID3DBlob> {
        unsafe {
            let entry = format!(
                "{}_{}\0",
                entry.as_str(),
                match target {
                    ShaderTarget::Vertex => "vertex",
                    ShaderTarget::Fragment => "fragment",
                }
            );
            let target = match target {
                ShaderTarget::Vertex => "vs_5_0\0",
                ShaderTarget::Fragment => "ps_5_0\0",
            };

            let mut compile_blob = None;
            let mut error_blob = None;
            let shader_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("src/platform/windows/shaders.hlsl")
                .canonicalize()?;

            let entry_point = PCSTR::from_raw(entry.as_ptr());
            let target_cstr = PCSTR::from_raw(target.as_ptr());

            let ret = D3DCompileFromFile(
                &HSTRING::from(shader_path.to_str().unwrap()),
                None,
                None,
                entry_point,
                target_cstr,
                D3DCOMPILE_DEBUG | D3DCOMPILE_SKIP_OPTIMIZATION,
                0,
                &mut compile_blob,
                Some(&mut error_blob),
            );
            if ret.is_err() {
                let Some(error_blob) = error_blob else {
                    return Err(anyhow::anyhow!("{ret:?}"));
                };

                let error_string =
                    std::ffi::CStr::from_ptr(error_blob.GetBufferPointer() as *const i8)
                        .to_string_lossy();
                log::error!("Shader compile error: {}", error_string);
                return Err(anyhow::anyhow!("Compile error: {}", error_string));
            }
            Ok(compile_blob.unwrap())
        }
    }

    #[cfg(not(debug_assertions))]
    include!(concat!(env!("OUT_DIR"), "/shaders_bytes.rs"));

    #[cfg(debug_assertions)]
    impl ShaderModule {
        pub fn as_str(&self) -> &str {
            match self {
                ShaderModule::Quad => "quad",
                ShaderModule::Shadow => "shadow",
                ShaderModule::Underline => "underline",
                ShaderModule::Paths => "paths",
                ShaderModule::MonochromeSprite => "monochrome_sprite",
                ShaderModule::PolychromeSprite => "polychrome_sprite",
            }
        }
    }
}

mod nvidia {
    use std::{
        ffi::CStr,
        os::raw::{c_char, c_int, c_uint},
    };

    use anyhow::{Context, Result};
    use windows::{
        Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA},
        core::s,
    };

    // https://github.com/NVIDIA/nvapi/blob/7cb76fce2f52de818b3da497af646af1ec16ce27/nvapi_lite_common.h#L180
    const NVAPI_SHORT_STRING_MAX: usize = 64;

    // https://github.com/NVIDIA/nvapi/blob/7cb76fce2f52de818b3da497af646af1ec16ce27/nvapi_lite_common.h#L235
    #[allow(non_camel_case_types)]
    type NvAPI_ShortString = [c_char; NVAPI_SHORT_STRING_MAX];

    // https://github.com/NVIDIA/nvapi/blob/7cb76fce2f52de818b3da497af646af1ec16ce27/nvapi_lite_common.h#L447
    #[allow(non_camel_case_types)]
    type NvAPI_SYS_GetDriverAndBranchVersion_t = unsafe extern "C" fn(
        driver_version: *mut c_uint,
        build_branch_string: *mut NvAPI_ShortString,
    ) -> c_int;

    pub(super) fn get_driver_version() -> Result<String> {
        unsafe {
            // Try to load the NVIDIA driver DLL
            #[cfg(target_pointer_width = "64")]
            let nvidia_dll =
                LoadLibraryA(s!("nvapi64.dll")).context(format!("Can't load nvapi64.dll"))?;
            #[cfg(target_pointer_width = "32")]
            let nvidia_dll =
                LoadLibraryA(s!("nvapi.dll")).context(format!("Can't load nvapi.dll"))?;

            let nvapi_query_addr = GetProcAddress(nvidia_dll, s!("nvapi_QueryInterface"))
                .ok_or_else(|| anyhow::anyhow!("Failed to get nvapi_QueryInterface address"))?;
            let nvapi_query: extern "C" fn(u32) -> *mut () = std::mem::transmute(nvapi_query_addr);

            // https://github.com/NVIDIA/nvapi/blob/7cb76fce2f52de818b3da497af646af1ec16ce27/nvapi_interface.h#L41
            let nvapi_get_driver_version_ptr = nvapi_query(0x2926aaad);
            if nvapi_get_driver_version_ptr.is_null() {
                anyhow::bail!("Failed to get NVIDIA driver version function pointer");
            }
            let nvapi_get_driver_version: NvAPI_SYS_GetDriverAndBranchVersion_t =
                std::mem::transmute(nvapi_get_driver_version_ptr);

            let mut driver_version: c_uint = 0;
            let mut build_branch_string: NvAPI_ShortString = [0; NVAPI_SHORT_STRING_MAX];
            let result = nvapi_get_driver_version(
                &mut driver_version as *mut c_uint,
                &mut build_branch_string as *mut NvAPI_ShortString,
            );

            if result != 0 {
                anyhow::bail!(
                    "Failed to get NVIDIA driver version, error code: {}",
                    result
                );
            }
            let major = driver_version / 100;
            let minor = driver_version % 100;
            let branch_string = CStr::from_ptr(build_branch_string.as_ptr());
            Ok(format!(
                "{}.{} {}",
                major,
                minor,
                branch_string.to_string_lossy()
            ))
        }
    }
}

mod amd {
    use std::os::raw::{c_char, c_int, c_void};

    // https://github.com/GPUOpen-LibrariesAndSDKs/AGS_SDK/blob/5d8812d703d0335741b6f7ffc37838eeb8b967f7/ags_lib/inc/amd_ags.h#L145
    const AGS_CURRENT_VERSION: i32 = (6 << 22) | (3 << 12) | 0;

    // https://github.com/GPUOpen-LibrariesAndSDKs/AGS_SDK/blob/5d8812d703d0335741b6f7ffc37838eeb8b967f7/ags_lib/inc/amd_ags.h#L204
    // This is an opaque type, using struct to represent it properly for FFI
    #[repr(C)]
    struct AGSContext {
        _private: [u8; 0],
    }

    #[repr(C)]
    pub struct AGSGPUInfo {
        pub driver_version: *const c_char,
        pub radeon_software_version: *const c_char,
        pub num_devices: c_int,
        pub devices: *mut c_void,
    }

    unsafe extern "C" {
        fn agsInitialize(
            version: c_int,
            config: *const c_void,
            context: *mut *mut AGSContext,
            gpu_info: *mut AGSGPUInfo,
        ) -> c_int;

        fn agsDeInitialize(context: *mut AGSContext) -> c_int;
    }

    pub(super) fn get_driver_version() -> anyhow::Result<String> {
        unsafe {
            let mut context: *mut AGSContext = std::ptr::null_mut();
            let mut gpu_info: AGSGPUInfo = AGSGPUInfo {
                driver_version: std::ptr::null(),
                radeon_software_version: std::ptr::null(),
                num_devices: 0,
                devices: std::ptr::null_mut(),
            };

            let result = agsInitialize(
                AGS_CURRENT_VERSION,
                std::ptr::null(),
                &mut context,
                &mut gpu_info,
            );
            if result != 0 {
                return Err(anyhow::anyhow!(
                    "Failed to initialize AGS, error code: {}",
                    result
                ));
            }

            // Vulkan acctually returns this as the driver version
            let software_version = if !gpu_info.radeon_software_version.is_null() {
                std::ffi::CStr::from_ptr(gpu_info.radeon_software_version)
                    .to_string_lossy()
                    .into_owned()
            } else {
                "Unknown Radeon Software Version".to_string()
            };

            let driver_version = if !gpu_info.driver_version.is_null() {
                std::ffi::CStr::from_ptr(gpu_info.driver_version)
                    .to_string_lossy()
                    .into_owned()
            } else {
                "Unknown Radeon Driver Version".to_string()
            };

            agsDeInitialize(context);
            Ok(format!("{} ({})", software_version, driver_version))
        }
    }
}

mod intel {
    use windows::{
        Win32::Graphics::Dxgi::{IDXGIAdapter1, IDXGIDevice},
        core::Interface,
    };

    pub(super) fn get_driver_version(adapter: &IDXGIAdapter1) -> anyhow::Result<String> {
        let number = unsafe { adapter.CheckInterfaceSupport(&IDXGIDevice::IID as _) }?;
        Ok(format!(
            "{}.{}.{}.{}",
            number >> 48,
            (number >> 32) & 0xFFFF,
            (number >> 16) & 0xFFFF,
            number & 0xFFFF
        ))
    }
}
