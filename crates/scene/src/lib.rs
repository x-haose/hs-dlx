//! 窗口的动态背景：日冕（着色器见 scene.wgsl），用 wgpu 渲染到屏幕外的纹理，读回成 BGRA 像素交给界面显示。界面框架画
//! 不了自定义着色器，背景先在这里画成图再交过去。日冕随任务状态的样子与状态之间的过渡见 [`Mode`] 与 [`Motion`]。
//!
//! 一个 [`Renderer`] 持有一个 GPU 设备；[`Renderer::render`] 画一帧并等它读回（阻塞），由调用方放在自己的线程上按需
//! 调用。

mod corona;

use std::borrow::Cow;
use std::cell::Cell;
use std::panic::AssertUnwindSafe;
use std::time::Duration;

pub use corona::{Corona, Mode, Motion, Target, Theme};

/// 一套光的颜色（各通道线性的 0 到 1）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    /// 光晕与光芒的主色
    pub light: [f32; 3],
    /// 越往外越偏向的颜色
    pub deep: [f32; 3],
    /// 紧贴圆盘边缘的亮环与弧前端的亮点
    pub core: [f32; 3],
}

/// 一帧要画成什么样。除画面的宽高外，坐标与长度都是逻辑点，原点在左上。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Params {
    /// 画面的宽高（像素）
    pub width: u32,
    pub height: u32,
    /// 每个逻辑点几个像素
    pub scale: f32,
    pub corona: Corona,
    /// 底色（sRGB 编码的 0 到 1）：画面不比它暗；面板里也是它
    pub ground: [f32; 3],
    /// 面板（左、上、右、下）：光从朝圆盘的那条竖边按指数漫进去，面板里离那条边越远越暗
    pub panel: Option<[f32; 4]>,
    /// 面板的圆角半径
    pub panel_radius: f32,
}

/// 画好的一帧：BGRA，每像素 4 字节，逐行紧排。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// 渲染失败的原因。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// 这台机器没有能用的 GPU 适配器（或驱动不支持）
    #[error("没有能用的 GPU：{0}")]
    NoAdapter(String),
    #[error("建 GPU 设备失败：{0}")]
    NoDevice(String),
    /// 画面的宽或高为 0，或超过设备允许的纹理边长
    #[error("画面大小 {width}×{height} 不能渲染（最大边长 {max}）")]
    Size { width: u32, height: u32, max: u32 },
    /// GPU 报错（校验错误、显存不足、驱动内部错误）、设备丢失，或等画完、读回超时
    #[error("等 GPU 画完失败：{0}")]
    Gpu(String),
}

/// 等一帧画完最多多久：驱动卡住时不让调用方的线程一直挂着。
const FRAME_TIMEOUT: Duration = Duration::from_secs(2);
/// 输出的格式：与界面要的 BGRA 一致，读回后不必换通道；着色器自己做 sRGB 编码。
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;
/// 统一缓冲区里 vec4f 的个数与字节数（见 scene.wgsl 的 Params）。
const UNIFORM_VECTORS: usize = 9;
const UNIFORM_SIZE: u64 = (UNIFORM_VECTORS * 16) as u64;

/// 渲染背景的 GPU 设备与管线；画面大小变了时重建纹理与读回缓冲区。
pub struct Renderer {
    /// 只在释放时取走（见 [`Drop`] 的实现）
    gpu: Option<Gpu>,
}

/// [`Renderer`] 持有的 GPU 对象。
struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    uniforms: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    surface: Option<Surface>,
}

/// 某一大小的渲染目标与读回缓冲区。
struct Surface {
    width: u32,
    height: u32,
    /// 读回时每行的字节数：按 GPU 的要求对齐到 256
    padded_row: u32,
    texture: wgpu::Texture,
    readback: wgpu::Buffer,
}

impl Drop for Renderer {
    /// wgpu 释放队列时等 GPU 空闲，设备已坏（显存不足、驱动出错）时它 panic，与等画完时一样收住（见
    /// [`panics_contained`]）。释放时设备出错不报：渲染器已不再用，没有可报的对象。
    fn drop(&mut self) {
        if let Some(gpu) = self.gpu.take() {
            let _released = contained(move || drop(gpu));
        }
    }
}

impl Renderer {
    /// 取一个 GPU 适配器（偏好低功耗的集成显卡）并建设备与管线。
    pub fn new() -> Result<Renderer, Error> {
        // 只画到屏幕外的纹理，不要窗口
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            ..Default::default()
        }))
        .map_err(|error| Error::NoAdapter(error.to_string()))?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("hs-dlx-scene"),
            ..Default::default()
        }))
        .map_err(|error| Error::NoDevice(error.to_string()))?;
        let (pipeline, uniforms, bind_group) = scoped(&device, || {
            let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("scene"),
                source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(include_str!("scene.wgsl"))),
            });
            let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("scene"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });
            let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("scene params"),
                size: UNIFORM_SIZE,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("scene"),
                layout: &layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniforms.as_entire_binding(),
                }],
            });
            let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("scene"),
                bind_group_layouts: &[Some(&layout)],
                ..Default::default()
            });
            let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("scene"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vertex"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fragment"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: FORMAT,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            });
            Ok((pipeline, uniforms, bind_group))
        })?;
        Ok(Renderer {
            gpu: Some(Gpu {
                device,
                queue,
                pipeline,
                uniforms,
                bind_group,
                surface: None,
            }),
        })
    }

    /// 按 `params` 画一帧并读回；阻塞到 GPU 画完，最多等 2 秒（驱动卡住时报 [`Error::Gpu`]）。
    pub fn render(&mut self, params: &Params) -> Result<Frame, Error> {
        let gpu = self.gpu.as_mut().expect("GPU 对象只在释放时取走");
        let device = gpu.device.clone();
        scoped(&device, || gpu.render(params))
    }
}

impl Gpu {
    fn render(&mut self, params: &Params) -> Result<Frame, Error> {
        let max = self.device.limits().max_texture_dimension_2d;
        if params.width == 0 || params.height == 0 || params.width > max || params.height > max {
            return Err(Error::Size {
                width: params.width,
                height: params.height,
                max,
            });
        }
        if self
            .surface
            .as_ref()
            .is_none_or(|t| (t.width, t.height) != (params.width, params.height))
        {
            self.surface = Some(self.surface_for(params.width, params.height));
        }
        let surface = self.surface.as_ref().expect("上面刚建好");
        self.queue
            .write_buffer(&self.uniforms, 0, &uniform_bytes(params));
        let view = surface
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("scene"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("scene"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &surface.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &surface.readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(surface.padded_row),
                    rows_per_image: Some(surface.height),
                },
            },
            wgpu::Extent3d {
                width: surface.width,
                height: surface.height,
                depth_or_array_layers: 1,
            },
        );
        let submission = self.queue.submit([encoder.finish()]);
        let slice = surface.readback.slice(..);
        let (sender, receiver) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            // 接收端在下面等着，发送失败只会是这个函数已经因超时返回
            let _waited = sender.send(result);
        });
        let device = &self.device;
        contained(|| {
            device.poll(wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: Some(FRAME_TIMEOUT),
            })
        })?
        .map_err(|error| Error::Gpu(error.to_string()))?;
        receiver
            .recv_timeout(FRAME_TIMEOUT)
            .map_err(|_| Error::Gpu("读回没有完成".to_owned()))?
            .map_err(|error| Error::Gpu(error.to_string()))?;
        let row = surface.width as usize * 4;
        let mut bgra = Vec::with_capacity(row * surface.height as usize);
        {
            let mapped = slice
                .get_mapped_range()
                .map_err(|error| Error::Gpu(error.to_string()))?;
            for line in mapped.chunks(surface.padded_row as usize) {
                bgra.extend_from_slice(&line[..row]);
            }
        }
        surface.readback.unmap();
        Ok(Frame {
            width: surface.width,
            height: surface.height,
            bgra,
        })
    }

    fn surface_for(&self, width: u32, height: u32) -> Surface {
        let padded_row = (width * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("scene"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("scene readback"),
            size: u64::from(padded_row) * u64::from(height),
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Surface {
            width,
            height,
            padded_row,
            texture,
            readback,
        }
    }
}

thread_local! {
    /// 当前线程正在 [`contained`] 里
    static CONTAINED: Cell<bool> = const { Cell::new(false) };
}

/// 当前线程上此刻发生的 panic 会被渲染器收住、转成 [`Error::Gpu`]：只在等 GPU 画完与释放渲染器的那一刻为真。应用把 panic 当不变量
/// 被破坏、中止进程时，据此放过这一种。
pub fn panics_contained() -> bool {
    CONTAINED.get()
}

/// 做 `work`，期间的 panic 收住、转成 [`Error::Gpu`]。只用在等 GPU 画完与释放渲染器上：这两处 wgpu 遇到设备丢失、显存
/// 不足等它认为致命的错误时不返回错误而是 panic，那是外部的故障，不是本库的不变量被破坏。panic 时 wgpu 不拿着会中毒的锁（它的锁
/// 不中毒），收住后这个渲染器的设备照样能用或照样报错。
fn contained<T>(work: impl FnOnce() -> T) -> Result<T, Error> {
    CONTAINED.set(true);
    let result = std::panic::catch_unwind(AssertUnwindSafe(work));
    CONTAINED.set(false);
    result.map_err(|panic| {
        let reason = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or("没有说明");
        Error::Gpu(format!("等 GPU 画完时设备出错：{reason}"))
    })
}

/// 在错误作用域里做 `work`：期间 GPU 报的校验错误、显存不足与内部错误收下来变成 [`Error::Gpu`]（几个都有时取第一
/// 个），不交给 wgpu 默认的处理（它会 panic）。作用域按线程记，`work` 里的 GPU 调用都要在调用这个函数的线程上。
fn scoped<T>(device: &wgpu::Device, work: impl FnOnce() -> Result<T, Error>) -> Result<T, Error> {
    let scopes = [
        wgpu::ErrorFilter::Internal,
        wgpu::ErrorFilter::OutOfMemory,
        wgpu::ErrorFilter::Validation,
    ]
    .map(|filter| device.push_error_scope(filter));
    let result = work();
    // 后进先出地弹出
    let mut captured = None;
    for scope in scopes.into_iter().rev() {
        if let Some(error) = pollster::block_on(scope.pop()) {
            captured.get_or_insert(error);
        }
    }
    match captured {
        Some(error) => Err(Error::Gpu(error.to_string())),
        None => result,
    }
}

/// `params` 按 scene.wgsl 的 Params 排成的字节（vec4f 依次排开，本机字节序）：逻辑点换成像素，0 到 1 的各项限在范围里。
fn uniform_bytes(params: &Params) -> Vec<u8> {
    let scale = params.scale;
    let corona = &params.corona;
    let Palette { light, deep, core } = corona.palette;
    let ground = params.ground;
    let panel = params.panel.unwrap_or_default().map(|edge| edge * scale);
    let vectors: [[f32; 4]; UNIFORM_VECTORS] = [
        [
            params.width as f32,
            params.height as f32,
            scale,
            corona.phase.rem_euclid(1.),
        ],
        [
            corona.center[0] * scale,
            corona.center[1] * scale,
            corona.radius * scale,
            corona.strength.clamp(0., 1.),
        ],
        [
            corona.progress.clamp(0., 1.),
            corona.turn.rem_euclid(1.),
            corona.reach,
            corona.bead.clamp(0., 1.),
        ],
        [light[0], light[1], light[2], 0.],
        [deep[0], deep[1], deep[2], 0.],
        [core[0], core[1], core[2], 0.],
        [ground[0], ground[1], ground[2], 0.],
        panel,
        [
            f32::from(u8::from(params.panel.is_some())),
            params.panel_radius * scale,
            0.,
            0.,
        ],
    ];
    vectors
        .iter()
        .flatten()
        .flat_map(|value| value.to_ne_bytes())
        .collect()
}
