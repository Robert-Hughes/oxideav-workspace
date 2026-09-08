//! FreeBSD/NVIDIA VDPAU -> GLX -> Vulkan bridge for the winit renderer.
//!
//! The decoded `VdpVideoSurface` remains GPU-resident. NVIDIA's
//! `GL_NV_vdpau_interop2` exposes progressive 4:2:0 video as full-frame Y and
//! interleaved UV textures. A tiny GL shader converts those into a Vulkan-owned
//! external-memory RGBA image. A raw Vulkan copy then moves that image into a
//! normal wgpu-owned RGBA texture, so wgpu's resource tracker never has to own
//! or reason about the externally-written image.
//!
//! This first implementation deliberately favours correctness over throughput:
//! GL completion and the raw Vulkan copy fence are waited synchronously before
//! the frame lease may be released. There is still no CPU pixel readback or
//! upload. The waits and final Vulkan copy can be pipelined/removed later.

use std::ffi::{c_void, CString};
use std::ptr;

use ash::vk::{self, Handle};
use glow::HasContext;
use oxideav_core::{Error, HardwareVideoFrameStorage, Result};
use oxideav_vdpau::VdpauVideoFrameStorage;
use wgpu::hal::api::Vulkan;
use x11_dl::{glx, xlib};

const GL_HANDLE_TYPE_OPAQUE_FD_EXT: u32 = 0x9586;
const GL_READ_ONLY: u32 = 0x88B8;

type GlVdpauInitNv = unsafe extern "C" fn(*const c_void, *const c_void);
type GlVdpauFiniNv = unsafe extern "C" fn();
type GlVdpauRegisterVideoSurfaceWithPictureStructureNv =
    unsafe extern "C" fn(*const c_void, u32, i32, *const u32, u8) -> isize;
type GlVdpauSurfaceAccessNv = unsafe extern "C" fn(isize, u32);
type GlVdpauMapSurfacesNv = unsafe extern "C" fn(i32, *const isize);
type GlVdpauUnmapSurfacesNv = unsafe extern "C" fn(i32, *const isize);
type GlVdpauUnregisterSurfaceNv = unsafe extern "C" fn(isize);
type GlCreateMemoryObjectsExt = unsafe extern "C" fn(i32, *mut u32);
type GlDeleteMemoryObjectsExt = unsafe extern "C" fn(i32, *const u32);
type GlImportMemoryFdExt = unsafe extern "C" fn(u32, u64, u32, i32);
type GlTextureStorageMem2dExt = unsafe extern "C" fn(u32, i32, u32, i32, i32, u32, u64);
type GlSignalVkSemaphoreNv = unsafe extern "C" fn(u64);

struct ExtFns {
    vdpau_init: GlVdpauInitNv,
    vdpau_fini: GlVdpauFiniNv,
    vdpau_register_frame: GlVdpauRegisterVideoSurfaceWithPictureStructureNv,
    vdpau_access: GlVdpauSurfaceAccessNv,
    vdpau_map: GlVdpauMapSurfacesNv,
    vdpau_unmap: GlVdpauUnmapSurfacesNv,
    vdpau_unregister: GlVdpauUnregisterSurfaceNv,
    create_memory_objects: GlCreateMemoryObjectsExt,
    delete_memory_objects: GlDeleteMemoryObjectsExt,
    import_memory_fd: GlImportMemoryFdExt,
    texture_storage_mem_2d: GlTextureStorageMem2dExt,
    signal_vk_semaphore: GlSignalVkSemaphoreNv,
}

pub struct VdpauVulkanBridge {
    width: u32,
    height: u32,
    output: wgpu::Texture,
    output_raw: vk::Image,
    output_sampled: bool,

    vk_device: ash::Device,
    vk_queue: vk::Queue,
    source_image: vk::Image,
    source_memory: vk::DeviceMemory,
    command_pool: vk::CommandPool,
    command_buffer: vk::CommandBuffer,
    copy_fence: vk::Fence,
    gl_done: vk::Semaphore,

    xlib: xlib::Xlib,
    glx: glx::Glx,
    display: *mut xlib::Display,
    context: glx::GLXContext,
    pbuffer: glx::GLXPbuffer,
    gl: glow::Context,
    ext: ExtFns,
    gl_memory: u32,
    gl_output: glow::NativeTexture,
    framebuffer: glow::NativeFramebuffer,
    program: glow::NativeProgram,
    vao: glow::NativeVertexArray,
    vdpau_device: Option<(u32, usize)>,
}

impl VdpauVulkanBridge {
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        if width == 0 || height == 0 {
            return Err(Error::invalid(
                "VDPAU Vulkan bridge requires non-zero dimensions",
            ));
        }

        let output = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("vdpau-bridge-wgpu-output"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        // Initialise the texture entirely on the GPU. This establishes a real
        // wgpu-tracked COLOR_TARGET state without any CPU upload.
        let output_view = output.create_view(&wgpu::TextureViewDescriptor::default());
        let mut init_encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("vdpau-bridge-output-init"),
        });
        {
            let _pass = init_encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("vdpau-bridge-output-init-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &output_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }
        queue.submit(Some(init_encoder.finish()));
        drop(output_view);

        // SAFETY: we only inspect backend handles and retain the public wgpu
        // Texture that owns output_raw for at least as long as this bridge.
        let hal_device = unsafe { device.as_hal::<Vulkan>() }.ok_or_else(|| {
            Error::unsupported("VDPAU GPU bridge requires the wgpu Vulkan backend")
        })?;
        let hal_queue = unsafe { queue.as_hal::<Vulkan>() }
            .ok_or_else(|| Error::unsupported("VDPAU GPU bridge requires the wgpu Vulkan queue"))?;
        let hal_output = unsafe { output.as_hal::<Vulkan>() }
            .ok_or_else(|| Error::unsupported("VDPAU GPU bridge requires a Vulkan wgpu texture"))?;

        if !hal_device
            .enabled_device_extensions()
            .contains(&ash::khr::external_memory_fd::NAME)
        {
            return Err(Error::unsupported(
                "wgpu Vulkan device did not enable VK_KHR_external_memory_fd",
            ));
        }

        let vk_device = hal_device.raw_device().clone();
        let vk_queue = hal_queue.as_raw();
        let output_raw = unsafe { hal_output.raw_handle() };
        let queue_family = hal_device.queue_family_index();
        let physical_device = hal_device.raw_physical_device();
        let instance = hal_device.shared_instance().raw_instance().clone();
        let memory_props =
            unsafe { instance.get_physical_device_memory_properties(physical_device) };

        let mut external_image = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD);
        let image_info = vk::ImageCreateInfo::default()
            .push_next(&mut external_image)
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::R8G8B8A8_UNORM)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(
                vk::ImageUsageFlags::TRANSFER_SRC
                    | vk::ImageUsageFlags::SAMPLED
                    | vk::ImageUsageFlags::COLOR_ATTACHMENT,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let source_image = unsafe { vk_device.create_image(&image_info, None) }
            .map_err(|e| Error::other(format!("VDPAU bridge vkCreateImage: {e}")))?;
        let memory_req = unsafe { vk_device.get_image_memory_requirements(source_image) };
        let memory_type = find_device_local_memory_type(memory_req.memory_type_bits, &memory_props)
            .ok_or_else(|| {
                Error::unsupported("no device-local memory type for VDPAU bridge image")
            })?;

        let mut export_info = vk::ExportMemoryAllocateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD);
        let mut dedicated_info = vk::MemoryDedicatedAllocateInfo::default().image(source_image);
        let alloc_info = vk::MemoryAllocateInfo::default()
            .push_next(&mut export_info)
            .push_next(&mut dedicated_info)
            .allocation_size(memory_req.size)
            .memory_type_index(memory_type);
        let source_memory = unsafe { vk_device.allocate_memory(&alloc_info, None) }
            .map_err(|e| Error::other(format!("VDPAU bridge vkAllocateMemory: {e}")))?;
        unsafe { vk_device.bind_image_memory(source_image, source_memory, 0) }
            .map_err(|e| Error::other(format!("VDPAU bridge vkBindImageMemory: {e}")))?;

        let pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(queue_family)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        let command_pool = unsafe { vk_device.create_command_pool(&pool_info, None) }
            .map_err(|e| Error::other(format!("VDPAU bridge vkCreateCommandPool: {e}")))?;
        let command_buffer = unsafe {
            vk_device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(command_pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )
        }
        .map_err(|e| Error::other(format!("VDPAU bridge vkAllocateCommandBuffers: {e}")))?[0];
        let copy_fence = unsafe { vk_device.create_fence(&vk::FenceCreateInfo::default(), None) }
            .map_err(|e| Error::other(format!("VDPAU bridge vkCreateFence: {e}")))?;
        let gl_done =
            unsafe { vk_device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) }
                .map_err(|e| Error::other(format!("VDPAU bridge vkCreateSemaphore: {e}")))?;

        // Establish GENERAL once. GL writes this same allocation on every frame;
        // Vulkan only ever reads it as TRANSFER_SRC in GENERAL.
        unsafe {
            vk_device
                .begin_command_buffer(
                    command_buffer,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .map_err(|e| Error::other(format!("VDPAU bridge begin init command: {e}")))?;
            let barrier = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(source_image)
                .subresource_range(color_range())
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::MEMORY_WRITE | vk::AccessFlags::MEMORY_READ);
            vk_device.cmd_pipeline_barrier(
                command_buffer,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier],
            );
            vk_device
                .end_command_buffer(command_buffer)
                .map_err(|e| Error::other(format!("VDPAU bridge end init command: {e}")))?;
            vk_device
                .queue_submit(
                    vk_queue,
                    &[vk::SubmitInfo::default().command_buffers(&[command_buffer])],
                    copy_fence,
                )
                .map_err(|e| Error::other(format!("VDPAU bridge submit init command: {e}")))?;
            vk_device
                .wait_for_fences(&[copy_fence], true, u64::MAX)
                .map_err(|e| Error::other(format!("VDPAU bridge wait init fence: {e}")))?;
            vk_device
                .reset_fences(&[copy_fence])
                .map_err(|e| Error::other(format!("VDPAU bridge reset init fence: {e}")))?;
            vk_device
                .reset_command_buffer(command_buffer, vk::CommandBufferResetFlags::empty())
                .map_err(|e| Error::other(format!("VDPAU bridge reset init command: {e}")))?;
        }

        let external_memory = ash::khr::external_memory_fd::Device::new(&instance, &vk_device);
        let memory_fd = unsafe {
            external_memory.get_memory_fd(
                &vk::MemoryGetFdInfoKHR::default()
                    .memory(source_memory)
                    .handle_type(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD),
            )
        }
        .map_err(|e| Error::other(format!("VDPAU bridge vkGetMemoryFdKHR: {e}")))?;

        // Create a private GLX pbuffer context. It may use a separate X11
        // connection from the VdpDevice; this is supported by the NVIDIA driver
        // and keeps the framework's private Display out of the presentation API.
        let xlib = xlib::Xlib::open()
            .map_err(|e| Error::unsupported(format!("VDPAU bridge Xlib unavailable: {e}")))?;
        let glx = glx::Glx::open()
            .map_err(|e| Error::unsupported(format!("VDPAU bridge GLX unavailable: {e}")))?;
        let display = unsafe { (xlib.XOpenDisplay)(ptr::null()) };
        if display.is_null() {
            return Err(Error::unsupported(
                "VDPAU bridge could not open X11 display",
            ));
        }
        let screen = unsafe { (xlib.XDefaultScreen)(display) };
        let fb_attrs = [
            glx::GLX_X_RENDERABLE,
            xlib::True,
            glx::GLX_DRAWABLE_TYPE,
            glx::GLX_PBUFFER_BIT,
            glx::GLX_RENDER_TYPE,
            glx::GLX_RGBA_BIT,
            glx::GLX_RED_SIZE,
            8,
            glx::GLX_GREEN_SIZE,
            8,
            glx::GLX_BLUE_SIZE,
            8,
            0,
        ];
        let mut config_count = 0;
        let configs = unsafe {
            (glx.glXChooseFBConfig)(display, screen, fb_attrs.as_ptr(), &mut config_count)
        };
        if configs.is_null() || config_count == 0 {
            unsafe { (xlib.XCloseDisplay)(display) };
            return Err(Error::unsupported(
                "VDPAU bridge found no GLX pbuffer framebuffer config",
            ));
        }
        let config = unsafe { *configs };
        unsafe { (xlib.XFree)(configs.cast()) };
        let context = unsafe {
            (glx.glXCreateNewContext)(
                display,
                config,
                glx::GLX_RGBA_TYPE,
                ptr::null_mut(),
                xlib::True,
            )
        };
        if context.is_null() {
            unsafe { (xlib.XCloseDisplay)(display) };
            return Err(Error::unsupported(
                "VDPAU bridge could not create GLX context",
            ));
        }
        let pb_attrs = [glx::GLX_PBUFFER_WIDTH, 1, glx::GLX_PBUFFER_HEIGHT, 1, 0];
        let pbuffer = unsafe { (glx.glXCreatePbuffer)(display, config, pb_attrs.as_ptr()) };
        if pbuffer == 0 {
            unsafe {
                (glx.glXDestroyContext)(display, context);
                (xlib.XCloseDisplay)(display);
            }
            return Err(Error::unsupported(
                "VDPAU bridge could not create GLX pbuffer",
            ));
        }
        if unsafe { (glx.glXMakeContextCurrent)(display, pbuffer, pbuffer, context) } == 0 {
            unsafe {
                (glx.glXDestroyPbuffer)(display, pbuffer);
                (glx.glXDestroyContext)(display, context);
                (xlib.XCloseDisplay)(display);
            }
            return Err(Error::unsupported(
                "VDPAU bridge could not make GLX context current",
            ));
        }

        let load_ptr = |name: &str| -> *const c_void {
            let Ok(name) = CString::new(name) else {
                return ptr::null();
            };
            unsafe { (glx.glXGetProcAddressARB)(name.as_ptr().cast()) }
                .map_or(ptr::null(), |f| f as *const () as *const c_void)
        };
        let gl = unsafe { glow::Context::from_loader_function(load_ptr) };
        let extensions = gl.supported_extensions();
        for required in [
            "GL_NV_vdpau_interop",
            "GL_NV_vdpau_interop2",
            "GL_EXT_memory_object",
            "GL_EXT_memory_object_fd",
            "GL_NV_draw_vulkan_image",
        ] {
            if !extensions.contains(required) {
                return Err(Error::unsupported(format!(
                    "VDPAU bridge requires {required} in the NVIDIA GLX context"
                )));
            }
        }

        let ext = unsafe { ExtFns::load(&glx)? };
        let mut gl_memory = 0;
        unsafe {
            (ext.create_memory_objects)(1, &mut gl_memory);
            if gl_memory == 0 {
                return Err(Error::other(
                    "VDPAU bridge glCreateMemoryObjectsEXT returned zero",
                ));
            }
            (ext.import_memory_fd)(
                gl_memory,
                memory_req.size,
                GL_HANDLE_TYPE_OPAQUE_FD_EXT,
                memory_fd,
            );
        }
        gl_check(&gl, "glImportMemoryFdEXT")?;

        let gl_output = unsafe { gl.create_texture() }
            .map_err(|e| Error::other(format!("VDPAU bridge create GL output texture: {e}")))?;
        unsafe {
            // glGenTextures does not establish a texture target. Bind once so
            // NVIDIA knows this externally-backed object is a 2D texture before
            // the EXT direct-state-access storage call.
            gl.bind_texture(glow::TEXTURE_2D, Some(gl_output));
            (ext.texture_storage_mem_2d)(
                gl_output.0.get(),
                1,
                glow::RGBA8,
                width as i32,
                height as i32,
                gl_memory,
                0,
            );
        }
        gl_check(&gl, "glTextureStorageMem2DEXT")?;
        let framebuffer = unsafe { gl.create_framebuffer() }
            .map_err(|e| Error::other(format!("VDPAU bridge create framebuffer: {e}")))?;
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(gl_output),
                0,
            );
            if gl.check_framebuffer_status(glow::FRAMEBUFFER) != glow::FRAMEBUFFER_COMPLETE {
                return Err(Error::other(
                    "VDPAU bridge external-memory framebuffer is incomplete",
                ));
            }
        }

        let program = unsafe { make_program(&gl)? };
        let vao = unsafe { gl.create_vertex_array() }
            .map_err(|e| Error::other(format!("VDPAU bridge create VAO: {e}")))?;
        unsafe {
            gl.use_program(Some(program));
            if let Some(location) = gl.get_uniform_location(program, "y_tex") {
                gl.uniform_1_i32(Some(&location), 0);
            }
            if let Some(location) = gl.get_uniform_location(program, "uv_tex") {
                gl.uniform_1_i32(Some(&location), 1);
            }
            gl.use_program(None);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        }

        drop(hal_output);
        drop(hal_queue);
        drop(hal_device);

        Ok(Self {
            width,
            height,
            output,
            output_raw,
            output_sampled: false,
            vk_device,
            vk_queue,
            source_image,
            source_memory,
            command_pool,
            command_buffer,
            copy_fence,
            gl_done,
            xlib,
            glx,
            display,
            context,
            pbuffer,
            gl,
            ext,
            gl_memory,
            gl_output,
            framebuffer,
            program,
            vao,
            vdpau_device: None,
        })
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn output_texture(&self) -> &wgpu::Texture {
        &self.output
    }

    /// Tell the bridge that wgpu has sampled the output at least once. From
    /// this point the raw destination image is expected to be in
    /// SHADER_READ_ONLY_OPTIMAL between bridge copies.
    pub fn mark_sampled(&mut self) {
        self.output_sampled = true;
    }

    pub fn copy_from_vdpau(&mut self, frame: &VdpauVideoFrameStorage) -> Result<()> {
        if frame.width() != self.width || frame.height() != self.height {
            return Err(Error::invalid("VDPAU bridge/frame dimension mismatch"));
        }
        self.make_current()?;
        self.ensure_vdpau(frame)?;

        let y = unsafe { self.gl.create_texture() }
            .map_err(|e| Error::other(format!("VDPAU bridge create Y texture: {e}")))?;
        let uv = match unsafe { self.gl.create_texture() } {
            Ok(texture) => texture,
            Err(e) => {
                unsafe { self.gl.delete_texture(y) };
                return Err(Error::other(format!("VDPAU bridge create UV texture: {e}")));
            }
        };
        let texture_names = [y.0.get(), uv.0.get()];
        let raw_surface = frame.raw_surface() as usize as *const c_void;
        let interop = unsafe {
            (self.ext.vdpau_register_frame)(
                raw_surface,
                glow::TEXTURE_2D,
                2,
                texture_names.as_ptr(),
                1,
            )
        };
        if interop == 0 {
            unsafe {
                self.gl.delete_texture(y);
                self.gl.delete_texture(uv);
            }
            return Err(Error::other(
                "glVDPAURegisterVideoSurfaceWithPictureStructureNV failed",
            ));
        }

        unsafe {
            (self.ext.vdpau_access)(interop, GL_READ_ONLY);
            (self.ext.vdpau_map)(1, &interop);

            self.gl
                .bind_framebuffer(glow::FRAMEBUFFER, Some(self.framebuffer));
            self.gl
                .viewport(0, 0, self.width as i32, self.height as i32);
            self.gl.use_program(Some(self.program));
            self.gl.bind_vertex_array(Some(self.vao));

            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(y));
            set_video_texture_params(&self.gl);
            self.gl.active_texture(glow::TEXTURE1);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(uv));
            set_video_texture_params(&self.gl);

            self.gl.draw_arrays(glow::TRIANGLES, 0, 3);
            (self.ext.signal_vk_semaphore)(self.gl_done.as_raw());

            // The lease may return its VdpVideoSurface to the decoder pool as
            // soon as this method returns. Wait until the GL shader has really
            // finished reading it before unmapping/unregistering.
            self.gl.finish();
            (self.ext.vdpau_unmap)(1, &interop);
            (self.ext.vdpau_unregister)(interop);
            self.gl.delete_texture(y);
            self.gl.delete_texture(uv);
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        }
        gl_check(&self.gl, "VDPAU interop render")?;
        self.submit_vulkan_copy()
    }

    fn make_current(&self) -> Result<()> {
        let ok = unsafe {
            (self.glx.glXMakeContextCurrent)(self.display, self.pbuffer, self.pbuffer, self.context)
        };
        if ok == 0 {
            Err(Error::other(
                "VDPAU bridge failed to make GLX context current",
            ))
        } else {
            Ok(())
        }
    }

    fn ensure_vdpau(&mut self, frame: &VdpauVideoFrameStorage) -> Result<()> {
        let next = (frame.raw_device(), frame.raw_get_proc_address() as usize);
        if self.vdpau_device == Some(next) {
            return Ok(());
        }
        unsafe {
            if self.vdpau_device.is_some() {
                (self.ext.vdpau_fini)();
            }
            (self.ext.vdpau_init)(next.0 as usize as *const c_void, next.1 as *const c_void);
        }
        gl_check(&self.gl, "glVDPAUInitNV")?;
        self.vdpau_device = Some(next);
        Ok(())
    }

    fn submit_vulkan_copy(&mut self) -> Result<()> {
        unsafe {
            self.vk_device
                .reset_command_buffer(self.command_buffer, vk::CommandBufferResetFlags::empty())
                .map_err(|e| Error::other(format!("VDPAU bridge reset command buffer: {e}")))?;
            self.vk_device
                .begin_command_buffer(
                    self.command_buffer,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .map_err(|e| Error::other(format!("VDPAU bridge begin copy command: {e}")))?;

            let (old_layout, old_access, old_stage) = if self.output_sampled {
                (
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::AccessFlags::SHADER_READ,
                    vk::PipelineStageFlags::FRAGMENT_SHADER,
                )
            } else {
                (
                    vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                    vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                    vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                )
            };
            let to_copy = vk::ImageMemoryBarrier::default()
                .old_layout(old_layout)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(self.output_raw)
                .subresource_range(color_range())
                .src_access_mask(old_access)
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE);
            self.vk_device.cmd_pipeline_barrier(
                self.command_buffer,
                old_stage,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_copy],
            );

            self.vk_device.cmd_copy_image(
                self.command_buffer,
                self.source_image,
                vk::ImageLayout::GENERAL,
                self.output_raw,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[vk::ImageCopy::default()
                    .src_subresource(color_layers())
                    .dst_subresource(color_layers())
                    .extent(vk::Extent3D {
                        width: self.width,
                        height: self.height,
                        depth: 1,
                    })],
            );

            // Return the image to exactly the state wgpu's tracker believes it
            // has. On the first frame that is COLOR_TARGET; after the first
            // wgpu sample it remains RESOURCE between frames.
            let from_copy = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(old_layout)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(self.output_raw)
                .subresource_range(color_range())
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(old_access);
            self.vk_device.cmd_pipeline_barrier(
                self.command_buffer,
                vk::PipelineStageFlags::TRANSFER,
                old_stage,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[from_copy],
            );

            self.vk_device
                .end_command_buffer(self.command_buffer)
                .map_err(|e| Error::other(format!("VDPAU bridge end copy command: {e}")))?;
            self.vk_device
                .reset_fences(&[self.copy_fence])
                .map_err(|e| Error::other(format!("VDPAU bridge reset copy fence: {e}")))?;
            let wait_stages = [vk::PipelineStageFlags::TRANSFER];
            let wait_semaphores = [self.gl_done];
            let command_buffers = [self.command_buffer];
            let submit = vk::SubmitInfo::default()
                .wait_semaphores(&wait_semaphores)
                .wait_dst_stage_mask(&wait_stages)
                .command_buffers(&command_buffers);
            self.vk_device
                .queue_submit(self.vk_queue, &[submit], self.copy_fence)
                .map_err(|e| Error::other(format!("VDPAU bridge submit copy: {e}")))?;
            self.vk_device
                .wait_for_fences(&[self.copy_fence], true, u64::MAX)
                .map_err(|e| Error::other(format!("VDPAU bridge wait copy fence: {e}")))?;
        }
        Ok(())
    }
}

impl Drop for VdpauVulkanBridge {
    fn drop(&mut self) {
        let _ = self.make_current();
        unsafe {
            // Best-effort GPU quiescence before dismantling objects borrowed by
            // both APIs. Drop must not panic.
            let _ = self.vk_device.queue_wait_idle(self.vk_queue);
            self.gl.finish();
            if self.vdpau_device.is_some() {
                (self.ext.vdpau_fini)();
            }
            self.gl.delete_program(self.program);
            self.gl.delete_vertex_array(self.vao);
            self.gl.delete_framebuffer(self.framebuffer);
            self.gl.delete_texture(self.gl_output);
            (self.ext.delete_memory_objects)(1, &self.gl_memory);

            (self.glx.glXMakeContextCurrent)(self.display, 0, 0, ptr::null_mut());
            (self.glx.glXDestroyPbuffer)(self.display, self.pbuffer);
            (self.glx.glXDestroyContext)(self.display, self.context);
            (self.xlib.XCloseDisplay)(self.display);

            self.vk_device.destroy_fence(self.copy_fence, None);
            self.vk_device.destroy_semaphore(self.gl_done, None);
            self.vk_device.destroy_command_pool(self.command_pool, None);
            self.vk_device.destroy_image(self.source_image, None);
            self.vk_device.free_memory(self.source_memory, None);
        }
    }
}

impl ExtFns {
    unsafe fn load(glx: &glx::Glx) -> Result<Self> {
        unsafe fn ptr(glx: &glx::Glx, name: &str) -> Result<*const c_void> {
            let c = CString::new(name).map_err(|_| Error::other("invalid GL extension name"))?;
            let raw = unsafe { (glx.glXGetProcAddressARB)(c.as_ptr().cast()) }
                .map_or(ptr::null(), |f| f as *const () as *const c_void);
            if raw.is_null() {
                Err(Error::unsupported(format!("missing GL entry point {name}")))
            } else {
                Ok(raw)
            }
        }

        macro_rules! load {
            ($name:literal, $ty:ty) => {{
                let raw = unsafe { ptr(glx, $name)? };
                unsafe { std::mem::transmute::<*const c_void, $ty>(raw) }
            }};
        }

        Ok(Self {
            vdpau_init: load!("glVDPAUInitNV", GlVdpauInitNv),
            vdpau_fini: load!("glVDPAUFiniNV", GlVdpauFiniNv),
            vdpau_register_frame: load!(
                "glVDPAURegisterVideoSurfaceWithPictureStructureNV",
                GlVdpauRegisterVideoSurfaceWithPictureStructureNv
            ),
            vdpau_access: load!("glVDPAUSurfaceAccessNV", GlVdpauSurfaceAccessNv),
            vdpau_map: load!("glVDPAUMapSurfacesNV", GlVdpauMapSurfacesNv),
            vdpau_unmap: load!("glVDPAUUnmapSurfacesNV", GlVdpauUnmapSurfacesNv),
            vdpau_unregister: load!("glVDPAUUnregisterSurfaceNV", GlVdpauUnregisterSurfaceNv),
            create_memory_objects: load!("glCreateMemoryObjectsEXT", GlCreateMemoryObjectsExt),
            delete_memory_objects: load!("glDeleteMemoryObjectsEXT", GlDeleteMemoryObjectsExt),
            import_memory_fd: load!("glImportMemoryFdEXT", GlImportMemoryFdExt),
            texture_storage_mem_2d: load!("glTextureStorageMem2DEXT", GlTextureStorageMem2dExt),
            signal_vk_semaphore: load!("glSignalVkSemaphoreNV", GlSignalVkSemaphoreNv),
        })
    }
}

unsafe fn make_program(gl: &glow::Context) -> Result<glow::NativeProgram> {
    const VS: &str = r#"#version 330 core
out vec2 uv;
void main() {
    vec2 p;
    if (gl_VertexID == 0) p = vec2(-1.0, -1.0);
    else if (gl_VertexID == 1) p = vec2(3.0, -1.0);
    else p = vec2(-1.0, 3.0);
    gl_Position = vec4(p, 0.0, 1.0);
    uv = p * 0.5 + 0.5;
}
"#;
    const FS: &str = r#"#version 330 core
in vec2 uv;
uniform sampler2D y_tex;
uniform sampler2D uv_tex;
out vec4 color;
void main() {
    float y = texture(y_tex, uv).r;
    vec2 c = texture(uv_tex, uv).rg - vec2(0.5);
    float r = y + 1.5748 * c.y;
    float g = y - 0.1873 * c.x - 0.4681 * c.y;
    float b = y + 1.8556 * c.x;
    color = vec4(r, g, b, 1.0);
}
"#;

    unsafe fn shader(gl: &glow::Context, kind: u32, source: &str) -> Result<glow::NativeShader> {
        let shader = unsafe { gl.create_shader(kind) }
            .map_err(|e| Error::other(format!("VDPAU bridge create shader: {e}")))?;
        unsafe {
            gl.shader_source(shader, source);
            gl.compile_shader(shader);
        }
        if !unsafe { gl.get_shader_compile_status(shader) } {
            let log = unsafe { gl.get_shader_info_log(shader) };
            unsafe { gl.delete_shader(shader) };
            return Err(Error::other(format!(
                "VDPAU bridge shader compile failed: {log}"
            )));
        }
        Ok(shader)
    }

    let vs = unsafe { shader(gl, glow::VERTEX_SHADER, VS)? };
    let fs = match unsafe { shader(gl, glow::FRAGMENT_SHADER, FS) } {
        Ok(shader) => shader,
        Err(e) => {
            unsafe { gl.delete_shader(vs) };
            return Err(e);
        }
    };
    let program = unsafe { gl.create_program() }
        .map_err(|e| Error::other(format!("VDPAU bridge create program: {e}")))?;
    unsafe {
        gl.attach_shader(program, vs);
        gl.attach_shader(program, fs);
        gl.link_program(program);
        gl.detach_shader(program, vs);
        gl.detach_shader(program, fs);
        gl.delete_shader(vs);
        gl.delete_shader(fs);
    }
    if !unsafe { gl.get_program_link_status(program) } {
        let log = unsafe { gl.get_program_info_log(program) };
        unsafe { gl.delete_program(program) };
        return Err(Error::other(format!(
            "VDPAU bridge program link failed: {log}"
        )));
    }
    Ok(program)
}

unsafe fn set_video_texture_params(gl: &glow::Context) {
    unsafe {
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MIN_FILTER,
            glow::LINEAR as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MAG_FILTER,
            glow::LINEAR as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_S,
            glow::CLAMP_TO_EDGE as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_T,
            glow::CLAMP_TO_EDGE as i32,
        );
    }
}

fn gl_check(gl: &glow::Context, where_: &str) -> Result<()> {
    let error = unsafe { gl.get_error() };
    if error == glow::NO_ERROR {
        Ok(())
    } else {
        Err(Error::other(format!(
            "VDPAU bridge {where_}: GL error 0x{error:x}"
        )))
    }
}

fn find_device_local_memory_type(
    bits: u32,
    props: &vk::PhysicalDeviceMemoryProperties,
) -> Option<u32> {
    (0..props.memory_type_count).find(|&i| {
        bits & (1 << i) != 0
            && props.memory_types[i as usize]
                .property_flags
                .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
    })
}

fn color_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .base_mip_level(0)
        .level_count(1)
        .base_array_layer(0)
        .layer_count(1)
}

fn color_layers() -> vk::ImageSubresourceLayers {
    vk::ImageSubresourceLayers::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .mip_level(0)
        .base_array_layer(0)
        .layer_count(1)
}
