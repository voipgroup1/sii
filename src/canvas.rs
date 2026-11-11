//! WebGPU 预览画布：把当前 GIF 帧作为纹理画到屏幕上。
//!
//! 设计要点：
//! - 单独一条管线，不跟 UI 混在一起，避免大纹理污染 UI 的图集
//! - 一次 draw call 画完「棋盘格背景 + 图像」，用 alpha 混合在一起
//! - 纹理尺寸跟随 GIF 原始分辨率，不缩放，缩放交给采样器

use wgpu::util::DeviceExt;

const SHADER: &str = r#"
struct View {
    // 预览区域在裁剪空间里的矩形: x0, y0, x1, y1
    rect: vec4<f32>,
    // 图像在帧缓冲像素坐标里的位置与尺寸: x, y, w, h
    img: vec4<f32>,
    // x = 棋盘格边长(px)
    misc: vec4<f32>,
};

@group(0) @binding(0) var<uniform> view: View;
@group(1) @binding(0) var frame_tex: texture_2d<f32>;
@group(1) @binding(1) var frame_samp: sampler;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
};

@vertex
fn vs(@builtin(vertex_index) idx: u32) -> VsOut {
    // 两个三角形拼成矩形
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
    );
    let c = corners[idx];
    var o: VsOut;
    let x = mix(view.rect.x, view.rect.z, c.x);
    let y = mix(view.rect.y, view.rect.w, c.y);
    o.pos = vec4<f32>(x, y, 0.0, 1.0);
    return o;
}

@fragment
fn fs(@builtin(position) frag: vec4<f32>) -> @location(0) vec4<f32> {
    // 帧缓冲像素坐标 -> 图像 uv。尺寸兜个底，避免 0 尺寸文档除出 NaN。
    let size = max(view.img.zw, vec2<f32>(1.0));
    let uv = (frag.xy - view.img.xy) / size;

    // 不能用 textureSample：它要隐式求导来选 mip level，WGSL 规范要求
    // 必须在 uniform control flow 里调用。而"这个像素是否落在图像内"依赖
    // frag 坐标（非 uniform），放进 if 里直接编译失败。
    // 改成 textureSampleLevel 显式指定 level 0 —— 这张纹理只有一级 mip，
    // 采样结果与 textureSample 完全一致，但不受控制流约束。
    let inside = uv.x >= 0.0 && uv.x <= 1.0 && uv.y >= 0.0 && uv.y <= 1.0;
    let t = textureSampleLevel(
        frame_tex,
        frame_samp,
        clamp(uv, vec2<f32>(0.0), vec2<f32>(1.0)),
        0.0,
    );
    let rgb = t.rgb;
    let a = select(0.0, t.a, inside);

    // 棋盘格背景，用来显示 GIF 的透明区域
    let cell = max(view.misc.x, 4.0);
    let cx = floor(frag.x / cell);
    let cy = floor(frag.y / cell);
    let odd = (cx + cy) % 2.0;
    // 线性空间里的深色两档
    let bg = mix(vec3<f32>(0.013, 0.013, 0.016), vec3<f32>(0.030, 0.030, 0.036), odd);

    return vec4<f32>(mix(bg, rgb, a), 1.0);
}
"#;

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct ViewUniform {
    rect: [f32; 4],
    img: [f32; 4],
    misc: [f32; 4],
}

/// 预览渲染器
pub struct Preview {
    pipeline: wgpu::RenderPipeline,
    uniform_buf: wgpu::Buffer,
    uniform_bg: wgpu::BindGroup,
    tex_bgl: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    texture: wgpu::Texture,
    tex_bg: wgpu::BindGroup,
    pub width: u32,
    pub height: u32,
}

impl Preview {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat, w: u32, h: u32) -> Self {
        let w = w.max(1);
        let h = h.max(1);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("sii preview shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });

        let uniform_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("sii preview uniform"),
            contents: bytemuck::bytes_of(&ViewUniform {
                rect: [-1.0, -1.0, 1.0, 1.0],
                img: [0.0, 0.0, 1.0, 1.0],
                misc: [16.0, 0.0, 0.0, 0.0],
            }),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let uniform_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("sii preview uniform bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let uniform_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("sii preview uniform bg"),
            layout: &uniform_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buf.as_entire_binding(),
            }],
        });

        let tex_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("sii preview texture bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("sii preview sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });

        // view 交给 bind group 持有（内部是 Arc），外面不用再留一份
        let (texture, _view, tex_bg) = Self::make_texture(device, &tex_bgl, &sampler, w, h);

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("sii preview layout"),
            bind_group_layouts: &[Some(&uniform_bgl), Some(&tex_bgl)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("sii preview pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        Self {
            pipeline,
            uniform_buf,
            uniform_bg,
            tex_bgl,
            sampler,
            texture,
            tex_bg,
            width: w,
            height: h,
        }
    }

    fn make_texture(
        device: &wgpu::Device,
        bgl: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
        w: u32,
        h: u32,
    ) -> (wgpu::Texture, wgpu::TextureView, wgpu::BindGroup) {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("sii frame texture"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("sii preview tex bg"),
            layout: bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
            ],
        });
        (texture, view, bg)
    }

    /// 尺寸变化时重建纹理（GIF 裁剪/缩放后调用）
    pub fn resize(&mut self, device: &wgpu::Device, w: u32, h: u32) {
        let w = w.max(1);
        let h = h.max(1);
        if w == self.width && h == self.height {
            return;
        }
        let (t, _view, bg) = Self::make_texture(device, &self.tex_bgl, &self.sampler, w, h);
        self.texture = t;
        self.tex_bg = bg;
        self.width = w;
        self.height = h;
    }

    /// 上传一帧 RGBA8
    pub fn upload(&self, queue: &wgpu::Queue, rgba: &[u8]) {
        let need = self.width as usize * self.height as usize * 4;
        if rgba.len() < need {
            return;
        }
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &rgba[..need],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(self.width * 4),
                rows_per_image: Some(self.height),
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
    }

    /// 画到目标视图。
    ///
    /// * `rect_clip`  预览区域（裁剪空间，-1..1）
    /// * `img_px`     图像在帧缓冲像素坐标下的位置与尺寸
    pub fn draw(
        &self,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        rect_clip: [f32; 4],
        img_px: [f32; 4],
        checker: f32,
        clear: wgpu::Color,
    ) {
        queue.write_buffer(
            &self.uniform_buf,
            0,
            bytemuck::bytes_of(&ViewUniform {
                rect: rect_clip,
                img: img_px,
                misc: [checker, 0.0, 0.0, 0.0],
            }),
        );

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("sii preview pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(clear),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.uniform_bg, &[]);
        pass.set_bind_group(1, &self.tex_bg, &[]);
        pass.draw(0..6, 0..1);
    }
}

#[cfg(test)]
mod tests {
    use super::SHADER;

    /// 用 naga（wgpu 自己的 WGSL 前端）解析并校验 shader。
    ///
    /// 能覆盖语法、类型、绑定这些硬错误，但**覆盖不到 uniformity** ——
    /// 原因见下一个测试。沙箱里没有浏览器，shader 写错了只能等用户打开页面
    /// 才发现，多一层静态检查总比没有强。
    #[test]
    fn shader_parses_and_validates() {
        let module = match naga::front::wgsl::parse_str(SHADER) {
            Ok(m) => m,
            Err(e) => panic!("WGSL 解析失败：\n{}", e.emit_to_string(SHADER)),
        };

        let mut validator = naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::empty(),
        );
        if let Err(e) = validator.validate(&module) {
            panic!("WGSL 校验失败：{e:?}");
        }
    }

    /// 禁止使用隐式求导的纹理采样函数。
    ///
    /// 为什么 naga 的 validator 帮不上忙：naga 30 的 `valid/analyzer.rs` 里有一行
    /// 硬编码 `DISABLE_UNIFORMITY_REQ_FOR_FRAGMENT_STAGE = true`，主动关掉了
    /// fragment 阶段的 uniformity 检查。于是 `textureSample` 放进依赖 frag 坐标的
    /// if 里，naga 一声不吭，但 Chrome 的 Tint 会直接拒绝编译：
    ///
    /// ```text
    /// error: 'textureSample' must only be called from uniform control flow
    /// ```
    ///
    /// 结果就是整条预览管线创建失败，画面上什么都没有。
    /// 这里退化成源码级约束：隐式 LOD 的采样一律不许用，必须显式写
    /// `textureSampleLevel(..., 0.0)`。本项目所有纹理都只有一级 mip，语义完全等价。
    #[test]
    fn shader_avoids_implicit_lod_sampling() {
        const BANNED: [&str; 3] = [
            "textureSample(",
            "textureSampleBias(",
            "textureSampleCompare(",
        ];
        for (i, raw) in SHADER.lines().enumerate() {
            // 跳过注释，否则解释性文字会被误判
            let code = raw.split("//").next().unwrap_or("");
            for pat in BANNED {
                if code.contains(pat) {
                    panic!(
                        "第 {} 行用了 `{pat}`：它隐含求导来选 mip level，\
                         WGSL 规范要求必须在 uniform control flow 中调用。\
                         改用 textureSampleLevel(tex, samp, uv, 0.0)。",
                        i + 1
                    );
                }
            }
        }
    }
}
