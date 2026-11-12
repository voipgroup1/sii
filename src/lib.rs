//! sii —— 纯前端、WebGPU 加速的 GIF 观看与编辑器。
//!
//! 没有后端。文件读取走浏览器 File API，解码/合成/编辑全在 wasm 里，
//! 上屏走 WebGPU（wgpu + wgpu-gameui），导出在 wasm 里重新编码成 GIF 字节流。

mod audio;
mod canvas;
mod duck;
pub mod gifdoc;
mod input;
mod viewport;

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wgpu_gameui::{
    load_font_bytes, shared_font_system, DrawList, FontSystemHandle, Frame, InputState,
    KeyboardNav, Theme, UiContext, UiRenderer, UiState,
};

use gifdoc::Doc;

// ------------------------------------------------------------------ 布局常量

const TOP_H: f32 = 52.0;
const LEFT_W: f32 = 190.0;
const RIGHT_W: f32 = 272.0;
const BOTTOM_H: f32 = 104.0;

/// 内存预算：超过就提示抽帧。浏览器单标签页 wasm 内存上限大概 2~4 GB。
const MEMORY_BUDGET: usize = 768 * 1024 * 1024;

// ------------------------------------------------------------------ 应用状态

struct App {
    // GPU
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    preview: canvas::Preview,
    ui: UiRenderer,

    // UI
    ui_state: UiState,
    theme: Theme,
    draw_list: Option<DrawList>,
    input: Rc<RefCell<InputState>>,
    /// 画布尺寸。由 JS 侧 ResizeObserver 维护，每帧读一次缓存值。
    size: viewport::Size,

    // 文档
    doc: Option<Doc>,
    frame_idx: usize,
    playing: bool,
    play_acc: f32,
    last_time: f64,
    uploaded_frame: usize,
    uploaded_gen: u64,
    generation: u64,

    // 视图
    zoom: f32,
    pan: [f32; 2],

    // 参数
    delay_cs: f32,
    bright: f32,
    contrast: f32,
    sat: f32,
    export_speed: f32,

    status: String,
    duck_busy: bool,
    /// 导出是否正在后台线程跑。为 true 时按钮变灰，避免重复提交。
    exporting: bool,

    // ---- 音频 ----
    /// 打开的文件名，写 DuckDB 元数据时要带上
    gif_name: String,
    /// 音频文件名，空表示没加载
    audio_name: String,
    /// 音频总时长（毫秒）
    audio_duration_ms: u32,
    /// 播放区间起点，音频文件内的毫秒
    audio_start_ms: u32,
    /// 播放区间终点，0 表示播到结尾
    audio_end_ms: u32,
    /// 音频是否正在响。真值在 JS 侧，这里只是每帧同步过来给 UI 用。
    audio_playing: bool,
}

impl App {
    fn current_rgba(&self) -> Option<&[u8]> {
        let doc = self.doc.as_ref()?;
        doc.frames.get(self.frame_idx).map(|f| f.rgba.as_slice())
    }

    fn clamp_frame(&mut self) {
        if let Some(doc) = &self.doc {
            if doc.frames.is_empty() {
                self.frame_idx = 0;
            } else if self.frame_idx >= doc.frames.len() {
                self.frame_idx = doc.frames.len() - 1;
            }
        } else {
            self.frame_idx = 0;
        }
    }

    fn mark_dirty(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.clamp_frame();
    }

    // -------------------------------------------------------- 主循环

    fn frame(&mut self, now: f64) {
        let dt = if self.last_time <= 0.0 {
            0.016
        } else {
            ((now - self.last_time) / 1000.0).clamp(0.001, 0.1) as f32
        };
        self.last_time = now;

        // 尺寸只在 JS 侧真的变过之后才更新（ResizeObserver 驱动），这里读的是
        // 缓存值。拿到新尺寸必须重配 surface，而且这一帧直接丢掉 —— 见
        // sync_surface 的注释。
        if let Some(s) = viewport::read() {
            self.size = s;
        }
        if self.sync_surface() {
            return;
        }

        // ---- 播放推进 ----
        // 音频在响的时候，声卡时钟是唯一权威：直接按绝对时间查帧，不做任何
        // 累加，误差不累积。没音频（或已播完）就回落到 dt 累加，行为和以前一致。
        let audio_t = audio::time_ms();
        self.audio_playing = audio_t >= 0.0;

        if self.playing {
            if let Some(doc) = &self.doc {
                if doc.frames.len() > 1 {
                    if audio_t >= 0.0 {
                        // 音频文件内的绝对位置 - 截取起点 = GIF 时间轴上的位置
                        let rel = audio_t - self.audio_start_ms as f64;
                        self.frame_idx = doc.frame_at_ms(rel);
                        self.play_acc = 0.0;
                    } else {
                        let cur_delay = doc.frames[self.frame_idx].delay_ms().max(10) as f32;
                        self.play_acc += dt * 1000.0;
                        if self.play_acc >= cur_delay {
                            self.play_acc -= cur_delay;
                            self.frame_idx = (self.frame_idx + 1) % doc.frames.len();
                        }
                    }
                }
            }
        }

        // ---- 上传当前帧（只在换帧或文档变更时）----
        let need_upload =
            self.uploaded_frame != self.frame_idx || self.uploaded_gen != self.generation;
        if need_upload {
            if let Some(doc) = &self.doc {
                if self.preview.width != doc.width as u32 || self.preview.height != doc.height as u32
                {
                    self.preview
                        .resize(&self.device, doc.width as u32, doc.height as u32);
                }
            }
            if let Some(rgba) = self.current_rgba() {
                let rgba = rgba.to_vec();
                self.preview.upload(&self.queue, &rgba);
            }
            self.uploaded_frame = self.frame_idx;
            self.uploaded_gen = self.generation;
        }

        // ---- 构建 UI ----
        // 直接用真实逻辑尺寸，不兜底。兜底会让 rect_clip 和实际的帧缓冲对不上，
        // 预览就画到屏幕外面去了。窗口小到放不下时由 pw/ph 各自兜底。
        let (vw, vh) = (self.size.css_w, self.size.css_h);
        let px = LEFT_W;
        let py = TOP_H;
        let pw = (vw - LEFT_W - RIGHT_W).max(80.0);
        let ph = (vh - TOP_H - BOTTOM_H).max(80.0);

        let mut acts = Actions::default();

        let mut list = self.draw_list.take().expect("draw list 被吃了");
        // 每帧必须先清空几何，只保留字形/测量缓存和列表 identity。
        //
        // `Frame::run` 只往 DrawList 里追加，不会替你清；而这个列表是跨帧复用
        // 的（字形光栅化和文本测量缓存都挂在它上面，每帧重建就白丢了）。漏掉
        // clear() 的话，每帧都在上一帧的几何后面再追加一整份 UI：
        //   - 界面静止时看不出来，同一批图形画在同一位置，叠多少层都一样；
        //   - 一旦 resize、布局变了，旧尺寸的文字和矩形还留在列表里，和新布局
        //     一起渲染 —— 屏幕上就是旧文字、旧图形和新的一起重影；
        //   - 列表只涨不缩，拖得越久每帧要画的几何越多，还会越来越卡。
        // 这正是「resize 之后文字图形叠影」的根因。
        list.clear();
        {
            // 逐字段借用，绕开闭包对 self 的整体占用
            let ui_state = &mut self.ui_state;
            let input = &mut *self.input.borrow_mut();
            // 每帧第一件事：把 JS 侧攒下的事件搬进来。必须在 Frame::run 之前，
            // 否则 UI 读到的是上一帧的输入，点按钮会慢半拍。
            input::sync(input);
            let theme = &self.theme;
            let doc = &self.doc;
            let frame_idx = self.frame_idx;
            let playing = self.playing;
            let status = self.status.as_str();
            let zoom = self.zoom;
            let delay_cs = self.delay_cs;
            let bright = self.bright;
            let contrast = self.contrast;
            let sat = self.sat;
            let export_speed = self.export_speed;
            let duck_busy = self.duck_busy;
            let exporting = self.exporting;
            let gif_name = self.gif_name.as_str();
            let audio_name = self.audio_name.as_str();
            let audio_duration_ms = self.audio_duration_ms;
            let audio_start_ms = self.audio_start_ms;
            let audio_end_ms = self.audio_end_ms;
            let audio_playing = self.audio_playing;
            let audio_progress = audio::progress();

            Frame::new(ui_state, input, theme, &KeyboardNav)
                .dt(dt)
                .run(&mut list, |ui| {
                    build_ui(
                        ui,
                        UiView {
                            vw,
                            vh,
                            doc,
                            frame_idx,
                            playing,
                            status,
                            zoom,
                            delay_cs,
                            bright,
                            contrast,
                            sat,
                            export_speed,
                            duck_busy,
                            exporting,
                            gif_name,
                            audio_name,
                            audio_duration_ms,
                            audio_start_ms,
                            audio_end_ms,
                            audio_playing,
                            audio_progress,
                        },
                        &mut acts,
                    );
                });
        }
        self.draw_list = Some(list);
        self.input.borrow_mut().end_frame();

        self.apply_actions(acts);

        // ---- 绘制 ----
        let out = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) => t,
            wgpu::CurrentSurfaceTexture::Suboptimal(t) => {
                self.surface.configure(&self.device, &self.config);
                t
            }
            _ => {
                // Timeout / Occluded / Outdated / Lost：重新配置后跳过这一帧
                self.surface.configure(&self.device, &self.config);
                return;
            }
        };
        let view = out
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("sii frame encoder"),
            });

        // UI 渲染器的帧边界，必须每帧调一次，且要在本次 submit 的第一个 pass
        // 之前。它把 uniform / 顶点 / 实例这些 bump 分配器的游标归零。
        //
        // 漏掉的话每帧都在上一帧的游标位置继续往后追加：缓冲区一路涨，
        // 涨到 ARENA_SANITY_CAP 之后渲染器直接拒绝画东西。表现出来就是
        // 画面闪一下（第一帧）然后再也不动。
        //
        // 别和 `Frame::run` 内部那个 `UiState::begin_frame` 搞混 —— 那个管的是
        // 布局和交互状态，跟 GPU 缓冲区一点关系没有。
        self.ui.begin_frame();

        let scale = self.size.dpr;

        // 预览区域 → 裁剪空间
        let rect_clip = [
            px / vw * 2.0 - 1.0,
            1.0 - py / vh * 2.0,
            (px + pw) / vw * 2.0 - 1.0,
            1.0 - (py + ph) / vh * 2.0,
        ];

        // 图像在预览区内的位置（CSS px → 帧缓冲 px）
        let (iw, ih) = match &self.doc {
            Some(d) => {
                let dw = d.width as f32;
                let dh = d.height as f32;
                let fit = (pw / dw).min(ph / dh);
                let s = fit * self.zoom;
                (dw * s, dh * s)
            }
            None => (0.0, 0.0),
        };
        let ix = px + pw * 0.5 + self.pan[0] - iw * 0.5;
        let iy = py + ph * 0.5 + self.pan[1] - ih * 0.5;
        let img_px = [ix * scale, iy * scale, iw * scale, ih * scale];

        self.preview.draw(
            &self.queue,
            &mut encoder,
            &view,
            rect_clip,
            img_px,
            14.0,
            wgpu::Color {
                r: 0.043,
                g: 0.047,
                b: 0.055,
                a: 1.0,
            },
        );

        // 必须和 config.width/height 同源，差一个像素就会每帧重配 surface
        let phys = self.size.physical();
        if let Some(list) = &self.draw_list {
            self.ui
                .render(&self.device, &self.queue, &mut encoder, &view, phys, scale, list);
        }

        self.queue.submit(Some(encoder.finish()));
        // wgpu 30 起 SurfaceTexture 不再有显式 present()，drop 即呈现
        drop(out);
    }

    /// 尺寸变了就重新配置 surface。返回 `true` 表示"这一帧别画"。
    ///
    /// **为什么必须跳过一帧**：`canvas.width` 赋值会让浏览器立刻重建 canvas 的
    /// swapchain，而 wgpu 手里的 `SurfaceConfiguration` 还是旧的。从重配到第一
    /// 张新纹理能稳定拿到之间有一次交接，这期间画上去的内容可能落在已经被丢弃
    /// 的 buffer 上 —— 屏幕上就是旧帧残留叠着新内容，正是"resize 之后文字图形
    /// 全糊在一起"的成因。跳一帧的成本是 16 ms 的静止画面，换的是绝不花屏。
    fn sync_surface(&mut self) -> bool {
        let (w, h) = self.size.physical();
        if w == self.config.width && h == self.config.height {
            return false;
        }
        self.config.width = w;
        self.config.height = h;
        self.surface.configure(&self.device, &self.config);
        true
    }

    fn apply_actions(&mut self, acts: Actions) {
        if acts.open {
            js_call0("openFile");
        }
        if acts.export && !self.exporting {
            match self.doc.as_ref() {
                Some(doc) => {
                    // 主线程只做一次快照 memcpy，编码全部甩给 Worker
                    let snap = gifdoc::snapshot(doc);
                    let frames = doc.frames.len();
                    let mb = snap.len() as f32 / (1024.0 * 1024.0);
                    self.exporting = true;
                    self.status =
                        format!("导出中：{frames} 帧 / {mb:.1} MB 快照已交给后台线程…");
                    js_call_export(&snap, self.export_speed as i32);
                }
                None => self.status = "还没有打开 GIF".into(),
            }
        }
        if acts.toggle_play {
            // 空格的含义随状态变：
            //   音频在响     → 停（用户的第一反应就是"别响了"）
            //   有音频没在响 → 走音频时钟播，画面和声音一起起来
            //   没音频       → 老行为，纯画面播放
            if self.audio_playing {
                audio::stop();
                self.audio_playing = false;
                self.playing = false;
            } else if self.audio_duration_ms > 0 {
                let ok = audio::play(self.audio_start_ms, self.audio_end_ms);
                self.playing = ok;
                self.audio_playing = ok;
                self.play_acc = 0.0;
            } else {
                self.playing = !self.playing;
                self.play_acc = 0.0;
            }
        }
        // 手动切帧 / 拖时间轴时必须停音频，否则下一帧就被音频时钟拽回去了，
        // 用户会觉得"拖不动"。
        let mut manual_seek = false;
        if acts.prev {
            if let Some(doc) = &self.doc {
                if !doc.frames.is_empty() {
                    self.frame_idx =
                        (self.frame_idx + doc.frames.len() - 1) % doc.frames.len();
                    manual_seek = true;
                }
            }
        }
        if acts.next {
            if let Some(doc) = &self.doc {
                if !doc.frames.is_empty() {
                    self.frame_idx = (self.frame_idx + 1) % doc.frames.len();
                    manual_seek = true;
                }
            }
        }
        if let Some(i) = acts.seek {
            self.frame_idx = i;
            manual_seek = true;
        }
        if manual_seek {
            if self.audio_playing {
                audio::stop();
                self.audio_playing = false;
            }
            self.playing = false;
        }
        if acts.zoom_in {
            self.zoom = (self.zoom * 1.25).min(16.0);
        }
        if acts.zoom_out {
            self.zoom = (self.zoom / 1.25).max(0.05);
        }
        if acts.zoom_fit {
            self.zoom = 1.0;
            self.pan = [0.0, 0.0];
        }

        if acts.op_duck_index {
            match self.doc.as_ref() {
                Some(doc) => {
                    let doc = doc.clone();
                    self.duck_busy = true;
                    self.status = "正在把帧元数据写进 DuckDB…".into();
                    wasm_bindgen_futures::spawn_local(async move {
                        let res = duck::index_doc(&doc).await;
                        APP.with(|a| {
                            if let Some(app) = a.borrow_mut().as_mut() {
                                app.duck_busy = false;
                                app.status = match res {
                                    Ok(s) => s,
                                    Err(e) => format!("DuckDB 失败: {e}"),
                                };
                            }
                        });
                    });
                }
                None => self.status = "还没有打开 GIF".into(),
            }
        }

        // ---- 音频 ----
        // 顺序有讲究：先处理"换音频"和"移除"，再处理滑块，最后才是播放和入库。
        // 用户点"换一个音频"的那一帧，起止点应该保持原样（换的是曲子不是区间），
        // 但总时长要等 JS 解码完才知道，走 `audio_loaded` 回调补上。
        if acts.audio_pick {
            js_call0("pickAudio");
        }
        if acts.audio_clear {
            audio::stop();
            duck::clear_audio_local();
            self.audio_name.clear();
            self.audio_duration_ms = 0;
            self.audio_start_ms = 0;
            self.audio_end_ms = 0;
            self.audio_playing = false;
            self.playing = false;
            self.status = "音频已移除".into();
        }

        if self.audio_duration_ms > 0 {
            let dur = self.audio_duration_ms as f32;
            let s = acts.audio_start_ms.clamp(0.0, dur).round();
            let mut e = acts.audio_end_ms.clamp(0.0, dur).round();
            // 结束点只有两种合法值：0（播到结尾）或者严格大于起点。
            // 用户把结束点拖到起点左边时归零，比硬夹成 s+1 更符合直觉 ——
            // 拖到底就是"不截尾"。
            if e > 0.0 && e <= s {
                e = 0.0;
            }
            let changed = s as u32 != self.audio_start_ms || e as u32 != self.audio_end_ms;
            self.audio_start_ms = s as u32;
            self.audio_end_ms = e as u32;
            // 播放中改了区间：立刻按新区间重开，否则画面还在跟旧区间走。
            if changed && self.audio_playing {
                let ok = audio::play(self.audio_start_ms, self.audio_end_ms);
                if !ok {
                    self.audio_playing = false;
                    self.playing = false;
                }
            }
        }

        if acts.audio_play {
            if self.audio_duration_ms == 0 {
                self.status = "还没加载音频".into();
            } else {
                let ok = audio::play(self.audio_start_ms, self.audio_end_ms);
                if ok {
                    // 音频时钟从这一刻开始成为唯一时间源，累加器作废
                    self.play_acc = 0.0;
                    self.playing = true;
                    self.audio_playing = true;
                    self.status = "播放中，画面跟音频时钟走".into();
                } else {
                    self.status = "音频播不了：浏览器可能还没拿到用户手势".into();
                }
            }
        }
        if acts.audio_stop {
            audio::stop();
            self.audio_playing = false;
            self.playing = false;
            self.status = "已停止".into();
        }
        if acts.audio_save {
            if self.audio_duration_ms == 0 {
                self.status = "还没加载音频".into();
            } else {
                let gif = self.gif_name.clone();
                let name = self.audio_name.clone();
                let (dur, s, e) = (self.audio_duration_ms, self.audio_start_ms, self.audio_end_ms);
                self.status = "正在把音频对齐参数写进 DuckDB…".into();
                wasm_bindgen_futures::spawn_local(async move {
                    let res = duck::save_audio_meta(&gif, &name, dur, s, e).await;
                    APP.with(|a| {
                        if let Some(app) = a.borrow_mut().as_mut() {
                            app.status = match res {
                                Ok(s) => s,
                                Err(e) => format!("写库失败: {e}"),
                            };
                        }
                    });
                });
            }
        }

        self.delay_cs = acts.delay_cs;
        self.bright = acts.bright;
        self.contrast = acts.contrast;
        self.sat = acts.sat;
        self.export_speed = acts.export_speed;

        // ---- 编辑操作 ----
        if let Some(doc) = self.doc.as_mut() {
            let n = doc.frames.len();
            let i = self.frame_idx.min(n.saturating_sub(1));
            let mut msg: Option<String> = None;
            let mut err: Option<String> = None;

            if acts.op_duplicate {
                match gifdoc::duplicate_frame(doc, i) {
                    Ok(()) => msg = Some("已复制当前帧".into()),
                    Err(e) => err = Some(e.0),
                }
            }
            if acts.op_delete {
                match gifdoc::delete_frame(doc, i) {
                    Ok(()) => msg = Some("已删除当前帧".into()),
                    Err(e) => err = Some(e.0),
                }
            }
            if acts.op_up && i > 0 {
                let _ = gifdoc::swap_frames(doc, i, i - 1);
                self.frame_idx = i - 1;
            }
            if acts.op_down && i + 1 < n {
                let _ = gifdoc::swap_frames(doc, i, i + 1);
                self.frame_idx = i + 1;
            }
            if acts.op_flip_h {
                gifdoc::flip_horizontal(doc);
                msg = Some("已水平翻转".into());
            }
            if acts.op_flip_v {
                gifdoc::flip_vertical(doc);
                msg = Some("已垂直翻转".into());
            }
            if acts.op_invert {
                gifdoc::invert(doc);
                msg = Some("已反色".into());
            }
            if acts.op_thin {
                let dropped = gifdoc::thin_to_budget(doc, MEMORY_BUDGET);
                msg = Some(if dropped > 0 {
                    format!("内存超预算，抽掉 {dropped} 帧")
                } else {
                    "内存没超预算，不用抽帧".into()
                });
            }
            if acts.op_scale_half {
                let w = (doc.width / 2).max(1);
                let h = (doc.height / 2).max(1);
                match gifdoc::resize_bilinear(doc, w, h) {
                    Ok(()) => msg = Some(format!("已缩放到 {w}×{h}")),
                    Err(e) => err = Some(e.0),
                }
            }
            if acts.op_scale_double {
                let w = (doc.width.saturating_mul(2)).min(4096);
                let h = (doc.height.saturating_mul(2)).min(4096);
                match gifdoc::resize_nearest(doc, w, h) {
                    Ok(()) => msg = Some(format!("已缩放到 {w}×{h}")),
                    Err(e) => err = Some(e.0),
                }
            }
            if acts.op_apply_color {
                gifdoc::adjust(doc, self.bright, self.contrast, self.sat);
                self.bright = 1.0;
                self.contrast = 1.0;
                self.sat = 1.0;
                msg = Some("已应用色彩调整".into());
            }
            if acts.op_delay_to_frame {
                if let Some(f) = doc.frames.get_mut(i) {
                    f.delay_cs = (self.delay_cs.round() as u16).max(1);
                }
            }
            if acts.op_delay_to_all {
                gifdoc::set_all_delays(doc, (self.delay_cs.round() as u16).max(1));
                msg = Some("已把延迟应用到全部帧".into());
            }

            if let Some(e) = err {
                self.status = format!("操作失败: {e}");
            } else if let Some(m) = msg {
                self.status = m;
                self.mark_dirty();
            } else if acts.op_duplicate || acts.op_delete || acts.op_up || acts.op_down {
                self.mark_dirty();
            }
        } else if acts.op_duplicate
            || acts.op_delete
            || acts.op_flip_h
            || acts.op_flip_v
            || acts.op_invert
        {
            self.status = "还没有打开 GIF".into();
        }
    }
}

// ------------------------------------------------------------------ UI

#[derive(Default)]
struct Actions {
    open: bool,
    export: bool,
    toggle_play: bool,
    prev: bool,
    next: bool,
    seek: Option<usize>,
    zoom_in: bool,
    zoom_out: bool,
    zoom_fit: bool,
    op_duplicate: bool,
    op_delete: bool,
    op_up: bool,
    op_down: bool,
    op_flip_h: bool,
    op_flip_v: bool,
    op_invert: bool,
    op_thin: bool,
    op_scale_half: bool,
    op_scale_double: bool,
    op_apply_color: bool,
    op_delay_to_frame: bool,
    op_delay_to_all: bool,
    delay_cs: f32,
    bright: f32,
    contrast: f32,
    sat: f32,
    export_speed: f32,
    op_duck_index: bool,

    // ---- 音频 ----
    audio_pick: bool,
    audio_play: bool,
    audio_stop: bool,
    audio_clear: bool,
    audio_save: bool,
    audio_start_ms: f32,
    audio_end_ms: f32,
}

struct UiView<'a> {
    vw: f32,
    vh: f32,
    doc: &'a Option<Doc>,
    frame_idx: usize,
    playing: bool,
    status: &'a str,
    zoom: f32,
    delay_cs: f32,
    bright: f32,
    contrast: f32,
    sat: f32,
    export_speed: f32,
    duck_busy: bool,
    exporting: bool,
    gif_name: &'a str,
    audio_name: &'a str,
    audio_duration_ms: u32,
    audio_start_ms: u32,
    audio_end_ms: u32,
    audio_playing: bool,
    audio_progress: f32,
}

/// 把局部光标挪到绝对坐标
fn at(ui: &mut UiContext, x: f32, y: f32) {
    let c = ui.cursor();
    ui.translate(x - c[0], y - c[1]);
}

fn build_ui(ui: &mut UiContext, v: UiView<'_>, a: &mut Actions) {
    // ---------------- 顶栏 ----------------
    let mut x = 12.0;
    at(ui, x, 10.0);
    if ui.text_button("打开 GIF", Some(84.0), Some(32.0)) {
        a.open = true;
    }
    x += 92.0;

    at(ui, x, 10.0);
    let export_label = if v.exporting { "导出中…" } else { "导出 GIF" };
    if ui.text_button(export_label, Some(84.0), Some(32.0)) && !v.exporting {
        a.export = true;
    }
    x += 92.0;

    at(ui, x, 10.0);
    let play_label = if v.playing { "暂停" } else { "播放" };
    if ui.text_button(play_label, Some(64.0), Some(32.0)) {
        a.toggle_play = true;
    }
    x += 72.0;

    at(ui, x, 10.0);
    if ui.text_button("<", Some(36.0), Some(32.0)) {
        a.prev = true;
    }
    x += 44.0;

    at(ui, x, 10.0);
    if ui.text_button(">", Some(36.0), Some(32.0)) {
        a.next = true;
    }
    x += 48.0;

    at(ui, x, 10.0);
    if ui.text_button("适应", Some(52.0), Some(32.0)) {
        a.zoom_fit = true;
    }
    x += 60.0;

    at(ui, x, 10.0);
    if ui.text_button("-", Some(36.0), Some(32.0)) {
        a.zoom_out = true;
    }
    x += 44.0;

    at(ui, x, 10.0);
    if ui.text_button("+", Some(36.0), Some(32.0)) {
        a.zoom_in = true;
    }
    x += 48.0;

    at(ui, x, 18.0);
    ui.text(&format!("{}%", (v.zoom * 100.0).round() as i32), Some(80.0));

    at(ui, 12.0, 44.0);
    ui.text(v.status, Some(v.vw - 24.0));

    // ---------------- 左栏：编辑 ----------------
    let lx = 12.0;
    let mut ly = TOP_H + 16.0;
    at(ui, lx, ly);
    ui.text("编辑", Some(LEFT_W - 20.0));
    ly += 24.0;

    let btn = |ui: &mut UiContext, y: &mut f32, label: &str, flag: &mut bool| {
        at(ui, lx, *y);
        if ui.text_button(label, Some(LEFT_W - 24.0), Some(28.0)) {
            *flag = true;
        }
        *y += 34.0;
    };

    btn(ui, &mut ly, "复制帧", &mut a.op_duplicate);
    btn(ui, &mut ly, "删除帧", &mut a.op_delete);
    btn(ui, &mut ly, "上移", &mut a.op_up);
    btn(ui, &mut ly, "下移", &mut a.op_down);
    ly += 6.0;
    btn(ui, &mut ly, "水平翻转", &mut a.op_flip_h);
    btn(ui, &mut ly, "垂直翻转", &mut a.op_flip_v);
    btn(ui, &mut ly, "反色", &mut a.op_invert);
    ly += 6.0;
    btn(ui, &mut ly, "缩放到 50%", &mut a.op_scale_half);
    btn(ui, &mut ly, "缩放到 200%", &mut a.op_scale_double);
    btn(ui, &mut ly, "抽帧到内存预算", &mut a.op_thin);

    // ---------------- 右栏：属性 ----------------
    let rx = v.vw - RIGHT_W + 12.0;
    let rw = RIGHT_W - 28.0;
    let mut ry = TOP_H + 16.0;

    at(ui, rx, ry);
    ui.text("文档", Some(rw));
    ry += 24.0;

    match v.doc {
        Some(d) => {
            let mem = d.memory_bytes() as f32 / (1024.0 * 1024.0);
            let lines = [
                format!("尺寸  {} × {}", d.width, d.height),
                format!("帧数  {}", d.frames.len()),
                format!("时长  {:.2} s", d.total_ms() as f32 / 1000.0),
                format!("帧率  {:.1} fps", d.fps_estimate()),
                format!("解码内存  {mem:.1} MB"),
            ];
            for l in lines {
                at(ui, rx, ry);
                ui.text(&l, Some(rw));
                ry += 20.0;
            }
        }
        None => {
            at(ui, rx, ry);
            ui.text("还没打开文件", Some(rw));
            ry += 20.0;
        }
    }

    ry += 10.0;
    at(ui, rx, ry);
    ui.text("当前帧延迟 (1/100 秒)", Some(rw));
    ry += 22.0;
    at(ui, rx, ry);
    a.delay_cs = ui.slider(1, v.delay_cs, 1.0, 100.0, Some(rw));
    ry += 30.0;
    at(ui, rx, ry);
    ui.text(&format!("{:.0} cs ≈ {:.0} ms", a.delay_cs, a.delay_cs * 10.0), Some(rw));
    ry += 22.0;
    at(ui, rx, ry);
    if ui.text_button("写入当前帧", Some(rw), Some(26.0)) {
        a.op_delay_to_frame = true;
    }
    ry += 32.0;
    at(ui, rx, ry);
    if ui.text_button("写入全部帧", Some(rw), Some(26.0)) {
        a.op_delay_to_all = true;
    }
    ry += 38.0;

    at(ui, rx, ry);
    ui.text("色彩", Some(rw));
    ry += 22.0;
    at(ui, rx, ry);
    a.bright = ui.slider(2, v.bright, 0.4, 2.0, Some(rw));
    ry += 26.0;
    at(ui, rx, ry);
    a.contrast = ui.slider(3, v.contrast, 0.4, 2.0, Some(rw));
    ry += 26.0;
    at(ui, rx, ry);
    a.sat = ui.slider(4, v.sat, 0.0, 2.0, Some(rw));
    ry += 30.0;
    at(ui, rx, ry);
    if ui.text_button("应用到全部帧", Some(rw), Some(26.0)) {
        a.op_apply_color = true;
    }
    ry += 38.0;

    at(ui, rx, ry);
    ui.text("导出质量 (1 好 / 30 快)", Some(rw));
    ry += 22.0;
    at(ui, rx, ry);
    a.export_speed = ui.slider(5, v.export_speed, 1.0, 30.0, Some(rw));
    ry += 34.0;

    at(ui, rx, ry);
    ui.text("DuckDB（帧元数据索引）", Some(rw));
    ry += 22.0;
    at(ui, rx, ry);
    let duck_label = if v.duck_busy { "索引中…" } else { "索引到 DuckDB" };
    if ui.text_button(duck_label, Some(rw), Some(26.0)) && !v.duck_busy {
        a.op_duck_index = true;
    }

    // ---------------- 音频 ----------------
    ry += 38.0;
    at(ui, rx, ry);
    ui.text("音频", Some(rw));
    ry += 22.0;

    if v.audio_duration_ms == 0 {
        at(ui, rx, ry);
        if ui.text_button("加载音频…", Some(rw), Some(26.0)) {
            a.audio_pick = true;
        }
        ry += 30.0;
        at(ui, rx, ry);
        ui.text("mp3 / wav / ogg / m4a", Some(rw));
    } else {
        at(ui, rx, ry);
        ui.text(&shorten(v.audio_name, 20), Some(rw));
        ry += 20.0;
        at(ui, rx, ry);
        ui.text(&format!("总长 {} ms", v.audio_duration_ms), Some(rw));
        ry += 20.0;
        at(ui, rx, ry);
        // 音频对齐是"哪段音频配哪个 GIF"，不把 GIF 名字亮出来，用户切了文件
        // 之后根本不知道这条记录会存到谁头上。
        let gif_label = if v.gif_name.is_empty() {
            "配：（还没打开 GIF）".to_string()
        } else {
            format!("配：{}", shorten(v.gif_name, 14))
        };
        ui.text(&gif_label, Some(rw));
        ry += 24.0;

        at(ui, rx, ry);
        ui.text("起始 (ms)", Some(rw));
        ry += 20.0;
        at(ui, rx, ry);
        a.audio_start_ms = ui.slider(
            7,
            v.audio_start_ms as f32,
            0.0,
            v.audio_duration_ms as f32,
            Some(rw),
        );
        ry += 28.0;

        at(ui, rx, ry);
        ui.text("结束 (ms，0 = 到结尾)", Some(rw));
        ry += 20.0;
        at(ui, rx, ry);
        a.audio_end_ms = ui.slider(
            8,
            v.audio_end_ms as f32,
            0.0,
            v.audio_duration_ms as f32,
            Some(rw),
        );
        ry += 24.0;
        at(ui, rx, ry);
        let end_label = if v.audio_end_ms == 0 {
            v.audio_duration_ms
        } else {
            v.audio_end_ms
        };
        ui.text(&format!("播放 {} → {} ms", v.audio_start_ms, end_label), Some(rw));
        ry += 26.0;

        at(ui, rx, ry);
        let play_label = if v.audio_playing { "停止音频" } else { "试听这一段" };
        if ui.text_button(play_label, Some(rw), Some(26.0)) {
            if v.audio_playing {
                a.audio_stop = true;
            } else {
                a.audio_play = true;
            }
        }
        ry += 32.0;
        at(ui, rx, ry);
        // 没在播时也占这一行，画 0 —— 否则按钮会随着播放状态上下跳
        ui.progress_bar(if v.audio_playing { v.audio_progress } else { 0.0 }, Some(rw));
        ry += 26.0;
        at(ui, rx, ry);
        if ui.text_button("存到 DuckDB", Some(rw), Some(26.0)) {
            a.audio_save = true;
        }
        ry += 32.0;
        at(ui, rx, ry);
        if ui.text_button("换一个音频", Some(rw), Some(26.0)) {
            a.audio_pick = true;
        }
        ry += 32.0;
        at(ui, rx, ry);
        if ui.text_button("移除音频", Some(rw), Some(26.0)) {
            a.audio_clear = true;
        }
    }

    // ---------------- 底栏：时间轴 ----------------
    let by = v.vh - BOTTOM_H + 12.0;
    let bw = v.vw - 24.0;
    at(ui, 12.0, by);
    match v.doc {
        Some(d) => {
            let n = d.frames.len();
            ui.text(
                &format!(
                    "帧 {} / {}　·　{:.0} ms",
                    v.frame_idx + 1,
                    n,
                    d.frames[v.frame_idx.min(n - 1)].delay_ms()
                ),
                Some(bw),
            );
            at(ui, 12.0, by + 22.0);
            let cur = v.frame_idx as f32;
            let picked = ui.slider(6, cur, 0.0, (n.saturating_sub(1)).max(1) as f32, Some(bw));
            let idx = (picked.round() as usize).min(n.saturating_sub(1));
            if idx != v.frame_idx {
                a.seek = Some(idx);
            }
            at(ui, 12.0, by + 58.0);
            let progress = (v.frame_idx + 1) as f32 / n as f32;
            ui.progress_bar(progress, Some(bw));
        }
        None => {
            ui.text("拖一个 .gif 文件进窗口，或者点左上角「打开 GIF」", Some(bw));
        }
    }
}

// ------------------------------------------------------------------ JS 桥

/// 右栏只有 248 px，长文件名直接画会被裁掉，看着像坏了。按字符数截断补省略号。
fn shorten(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let head: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{head}…")
}

fn js_ns() -> Option<js_sys::Object> {
    let win = web_sys::window()?;
    let ns = js_sys::Reflect::get(&win, &JsValue::from_str("__sii")).ok()?;
    if ns.is_undefined() || ns.is_null() {
        return None;
    }
    ns.dyn_into::<js_sys::Object>().ok()
}

fn js_call0(name: &str) {
    if let Some(ns) = js_ns() {
        if let Ok(f) = js_sys::Reflect::get(&ns, &JsValue::from_str(name)) {
            if f.is_function() {
                let _ = js_sys::Function::from(f).call0(&JsValue::NULL);
            }
        }
    }
}

#[allow(dead_code)]
fn js_call_bytes(name: &str, bytes: &[u8]) {
    if let Some(ns) = js_ns() {
        if let Ok(f) = js_sys::Reflect::get(&ns, &JsValue::from_str(name)) {
            if f.is_function() {
                let arr = js_sys::Uint8Array::from(bytes);
                let _ = js_sys::Function::from(f).call1(&JsValue::NULL, &arr);
            }
        }
    }
}

/// 把快照 + 质量参数交给 JS，由它转发给 Web Worker。
/// 快照在 JS 侧会立刻被 transfer 走，所以不存在第二份副本。
fn js_call_export(bytes: &[u8], speed: i32) {
    if let Some(ns) = js_ns() {
        if let Ok(f) = js_sys::Reflect::get(&ns, &JsValue::from_str("exportAsync")) {
            if f.is_function() {
                let arr = js_sys::Uint8Array::from(bytes);
                let _ = js_sys::Function::from(f).call2(
                    &JsValue::NULL,
                    &arr,
                    &JsValue::from_f64(speed as f64),
                );
            }
        }
    }
}

// ------------------------------------------------------------------ 对外 API

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

#[wasm_bindgen(start)]
pub fn on_load() {
    console_error_panic_hook::set_once();
}

/// 加载一份 GIF（字节由 JS 的 File API 读好传进来）。
///
/// `name` 是原始文件名，只用来写 DuckDB 元数据 —— 音频对齐记录是按
/// 「哪个 GIF 配哪段音频」存的，没有名字就没法把两张表连起来。
///
/// 返回一行状态文字。
#[wasm_bindgen]
pub fn load_gif(bytes: &[u8], name: String) -> Result<String, JsValue> {
    let t0 = web_sys::window()
        .and_then(|w| w.performance())
        .map(|p| p.now())
        .unwrap_or(0.0);

    let doc = gifdoc::decode(bytes).map_err(|e| JsValue::from_str(&e.0))?;

    let t1 = web_sys::window()
        .and_then(|w| w.performance())
        .map(|p| p.now())
        .unwrap_or(0.0);

    let summary = format!(
        "{}×{} / {} 帧 / {:.2}s / {:.1} MB / 解码 {} ms",
        doc.width,
        doc.height,
        doc.frames.len(),
        doc.total_ms() as f32 / 1000.0,
        doc.memory_bytes() as f32 / (1024.0 * 1024.0),
        (t1 - t0).round() as i64
    );

    APP.with(|a| {
        if let Some(app) = a.borrow_mut().as_mut() {
            app.doc = Some(doc);
            app.gif_name = name;
            app.frame_idx = 0;
            app.playing = false;
            app.play_acc = 0.0;
            app.zoom = 1.0;
            app.pan = [0.0, 0.0];
            // 换了 GIF 就停掉音频：截取区间是「这段音频的哪一段」，
            // 换画面之后原来的对齐关系已经没意义了，留着播只会让人困惑。
            audio::stop();
            app.audio_playing = false;
            app.audio_start_ms = 0;
            app.audio_end_ms = 0;
            app.delay_cs = app
                .doc
                .as_ref()
                .and_then(|d| d.frames.first())
                .map(|f| f.delay_cs as f32)
                .unwrap_or(10.0);
            app.mark_dirty();
            app.status = summary.clone();
        }
    });

    Ok(summary)
}

/// JS 侧解码完音频后回调这里，参数是一段 JSON：`{"name": "...", "durationMs": 12345}`。
///
/// 为什么走 JSON 而不是两个参数：以后要加采样率、声道数、波形摘要都不用改
/// wasm 导出签名，JS 侧多塞字段就行。解析失败就当作没加载。
#[wasm_bindgen]
pub fn audio_loaded(meta: String) -> Result<(), JsValue> {
    let v = js_sys::JSON::parse(&meta)?;
    let name = js_sys::Reflect::get(&v, &JsValue::from_str("name"))
        .ok()
        .and_then(|x| x.as_string())
        .unwrap_or_default();
    let dur = js_sys::Reflect::get(&v, &JsValue::from_str("durationMs"))
        .ok()
        .and_then(|x| x.as_f64())
        .unwrap_or(0.0)
        .max(0.0) as u32;

    if dur == 0 {
        return Err(JsValue::from_str("音频时长为 0，解码可能失败"));
    }

    APP.with(|a| {
        if let Some(app) = a.borrow_mut().as_mut() {
            app.audio_name = name;
            app.audio_duration_ms = dur;
            // 换了音频总长就变了，旧区间可能越界。起点超了就归零，
            // 终点一律归零（播到结尾）—— 新音频还没听过，先别截。
            if app.audio_start_ms > dur {
                app.audio_start_ms = 0;
            }
            app.audio_end_ms = 0;
            app.audio_playing = false;
            app.playing = false;
            app.status = format!("音频就绪：{dur} ms");
        }
    });
    Ok(())
}

/// 把当前文档编码成 GIF 字节，由 JS 落盘。
#[wasm_bindgen]
pub fn export_gif(speed: i32) -> Result<Vec<u8>, JsValue> {
    APP.with(|a| {
        let b = a.borrow();
        let app = b.as_ref().ok_or_else(|| JsValue::from_str("还没初始化"))?;
        let doc = app
            .doc
            .as_ref()
            .ok_or_else(|| JsValue::from_str("还没有打开 GIF"))?;
        gifdoc::encode(doc, speed).map_err(|e| JsValue::from_str(&e.0))
    })
}

/// 纯函数转码：解码 → 重新编码。不依赖 UI / GPU，可以在 Node 里直接跑自检。
#[wasm_bindgen]
pub fn transcode(bytes: &[u8], speed: i32) -> Result<Vec<u8>, JsValue> {
    let doc = gifdoc::decode(bytes).map_err(|e| JsValue::from_str(&e.0))?;
    gifdoc::encode(&doc, speed).map_err(|e| JsValue::from_str(&e.0))
}

/// 把文档写进一块 JS 堆上的 Uint8Array。
///
/// 不走 `Vec<u8>` 返回，是因为那样会变成两次 88 MB 拷贝
/// （Rust 里建 Vec 一次，wasm-bindgen 再 slice 到 JS 堆一次）。
/// 这里直接分配 JS 数组、逐段 copy_from，只有一次跨边界拷贝。
fn write_snapshot_js(doc: &Doc) -> js_sys::Uint8Array {
    let n = doc.frames.len();
    let px = doc.width as usize * doc.height as usize * 4;
    let total = 12 + n * 2 + n * px;
    let out = js_sys::Uint8Array::new_with_length(total as u32);

    let mut head = [0u8; 12];
    head[0..4].copy_from_slice(&gifdoc::SNAPSHOT_MAGIC);
    head[4..6].copy_from_slice(&doc.width.to_le_bytes());
    head[6..8].copy_from_slice(&doc.height.to_le_bytes());
    head[8..12].copy_from_slice(&(n as u32).to_le_bytes());
    out.subarray(0, 12).copy_from(&head);

    let mut off: u32 = 12;
    for f in &doc.frames {
        out.subarray(off, off + 2).copy_from(&f.delay_cs.to_le_bytes());
        off += 2;
    }
    for f in &doc.frames {
        let end = off + px as u32;
        out.subarray(off, end).copy_from(&f.rgba);
        off = end;
    }
    out
}

/// 把当前文档拍成可跨线程传输的快照，直接落在一块 JS 堆内存上。
#[wasm_bindgen]
pub fn snapshot() -> Option<js_sys::Uint8Array> {
    APP.with(|a| {
        let b = a.borrow();
        b.as_ref()?.doc.as_ref().map(write_snapshot_js)
    })
}

/// 纯函数：快照字节流 → GIF 字节。给 Web Worker 用，不依赖 UI / GPU / 全局状态。
#[wasm_bindgen]
pub fn encode_snapshot(buf: &[u8], speed: i32) -> Result<Vec<u8>, JsValue> {
    let doc = gifdoc::from_snapshot(buf).map_err(|e| JsValue::from_str(&e.0))?;
    gifdoc::encode(&doc, speed).map_err(|e| JsValue::from_str(&e.0))
}

/// 自检用：GIF 字节 → 快照。让 Node 里能单独验证快照往返，不需要 GPU。
/// 走的是和 `snapshot()` 完全相同的写入路径，所以能反映真实开销。
#[wasm_bindgen]
pub fn snapshot_of(bytes: &[u8]) -> Result<js_sys::Uint8Array, JsValue> {
    let doc = gifdoc::decode(bytes).map_err(|e| JsValue::from_str(&e.0))?;
    Ok(write_snapshot_js(&doc))
}

/// Worker 回传结果：收尾导出状态，清掉按钮的灰态。
#[wasm_bindgen]
pub fn export_status(ok: bool, msg: String) {
    APP.with(|a| {
        if let Some(app) = a.borrow_mut().as_mut() {
            app.exporting = false;
            app.status = if ok { msg } else { format!("导出失败：{msg}") };
        }
    });
}

/// 键盘快捷键：由 index.html 转发进来
#[wasm_bindgen]
pub fn key(code: String) {
    APP.with(|a| {
        let mut b = a.borrow_mut();
        let Some(app) = b.as_mut() else { return };
        match code.as_str() {
            "Space" => {
                app.playing = !app.playing;
                app.play_acc = 0.0;
            }
            "Comma" => {
                if let Some(doc) = &app.doc {
                    if !doc.frames.is_empty() {
                        app.frame_idx =
                            (app.frame_idx + doc.frames.len() - 1) % doc.frames.len();
                    }
                }
            }
            "Period" => {
                if let Some(doc) = &app.doc {
                    if !doc.frames.is_empty() {
                        app.frame_idx = (app.frame_idx + 1) % doc.frames.len();
                    }
                }
            }
            "Home" => app.frame_idx = 0,
            "End" => {
                if let Some(doc) = &app.doc {
                    app.frame_idx = doc.frames.len().saturating_sub(1);
                }
            }
            "Equal" | "NumpadAdd" => app.zoom = (app.zoom * 1.25).min(16.0),
            "Minus" | "NumpadSubtract" => app.zoom = (app.zoom / 1.25).max(0.05),
            "Digit0" => {
                app.zoom = 1.0;
                app.pan = [0.0, 0.0];
            }
            _ => {}
        }
    });
}

// ------------------------------------------------------------------ 启动

#[cfg(target_arch = "wasm32")]
fn request_raf(cb: &Closure<dyn FnMut(f64)>) {
    if let Some(w) = web_sys::window() {
        let _ = w.request_animation_frame(cb.as_ref().unchecked_ref());
    }
}

/// 初始化 WebGPU 并启动渲染循环。
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub async fn start(canvas_id: String) -> Result<(), JsValue> {
    let win = web_sys::window().ok_or("no window")?;
    let doc = win.document().ok_or("no document")?;
    let el = doc
        .get_element_by_id(&canvas_id)
        .ok_or_else(|| JsValue::from_str("找不到 canvas"))?;
    let canvas: web_sys::HtmlCanvasElement = el
        .dyn_into()
        .map_err(|_| JsValue::from_str("元素不是 canvas"))?;

    // 尺寸由 JS 侧的 ResizeObserver 维护在 window.__siiSize 里（见 index.html），
    // 这里只抓住缓冲区、读一次初值。一个 DOM 查询都不做，也不碰 canvas 的
    // width/height —— 那会重置绘图缓冲。
    viewport::attach();
    let size = match viewport::read() {
        Some(s) => s,
        None => {
            // JS 侧没接上（脚本顺序错了、或者被 CSP 挡了）。量一次 DOM 兜底，
            // 并把 canvas 的像素尺寸摆正 —— 它必须和 surface config 一致，
            // 否则 configure 直接失败。
            let dpr = win.device_pixel_ratio() as f32;
            let dpr = if dpr > 0.0 { dpr } else { 1.0 };
            let s = viewport::Size {
                css_w: canvas.client_width().max(1) as f32,
                css_h: canvas.client_height().max(1) as f32,
                dpr,
                gen: 0,
            };
            let (pw, ph) = s.physical();
            canvas.set_width(pw);
            canvas.set_height(ph);
            log::warn!("尺寸桥没接上，退回启动时量一次 DOM");
            s
        }
    };
    let (init_w, init_h) = size.physical();

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let surface = instance
        .create_surface(wgpu::SurfaceTarget::Canvas(canvas.clone()))
        .map_err(|e| JsValue::from_str(&format!("create_surface: {e}")))?;

    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            // Windows 上的 Chrome 会忽略这个选项并在 console 打一条警告
            // （crbug.com/369219127），不影响功能；macOS 上它能触发独显，
            // 所以留着。看到那条 warning 不用管。
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        })
        .await
        .map_err(|e| JsValue::from_str(&format!("没有可用的 WebGPU 适配器: {e}")))?;

    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("sii device"),
            ..Default::default()
        })
        .await
        .map_err(|e| JsValue::from_str(&format!("request_device: {e}")))?;

    let caps = surface.get_capabilities(&adapter);
    let format = caps
        .formats
        .iter()
        .copied()
        .find(|f| f.is_srgb())
        .unwrap_or(caps.formats[0]);

    let config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format,
        width: init_w,
        height: init_h,
        present_mode: wgpu::PresentMode::Fifo,
        desired_maximum_frame_latency: 2,
        alpha_mode: caps.alpha_modes[0],
        view_formats: vec![],
        color_space: wgpu::SurfaceColorSpace::Auto,
    };
    surface.configure(&device, &config);

    let font_system = shared_font_system();
    // 自带的 bundled-font 是 Noto Sans 的拉丁子集，没有汉字字形，中文全是豆腐块。
    // 拉一份阿里巴巴普惠体 2.0 注册成默认 sans-serif，UI 里没显式指定字体族的
    // 文本就都能落到它上面。加载失败不阻断启动 —— 顶多中文不显示，界面还能用。
    match load_cjk_font(&font_system).await {
        Ok(family) => log::info!("中文字体就绪：{family}"),
        Err(e) => log::warn!("中文字体加载失败，中文会显示成方块：{e}"),
    }
    let ui = UiRenderer::new(&device, &queue, format, font_system.clone());
    let draw_list = DrawList::with_font_system(font_system);
    let preview = canvas::Preview::new(&device, format, 1, 1);

    let input = Rc::new(RefCell::new(InputState::default()));
    // 事件监听器全在 index.html 里，只往 `window.__siiInput` 这个 Float32Array
    // 写数，一个 wasm 边界都不过。这里抓住它，之后每帧读一次快照。
    // 拿不到只是指针交互失灵，画面照常渲染，所以不返回错误。
    input::attach();

    // 上次存的对齐参数。音频二进制没法塞进 localStorage（几 MB 直接撞 5 MB
    // 配额），所以这里只恢复「怎么配」，音频文件得用户重新选一次。
    let saved_audio = duck::load_audio_local();

    let mut app = App {
        surface,
        device,
        queue,
        config,
        preview,
        ui,
        ui_state: UiState::new(),
        theme: Theme::default(),
        draw_list: Some(draw_list),
        input: input.clone(),
        size,
        doc: None,
        frame_idx: 0,
        playing: false,
        play_acc: 0.0,
        last_time: 0.0,
        uploaded_frame: usize::MAX,
        uploaded_gen: u64::MAX,
        generation: 0,
        zoom: 1.0,
        pan: [0.0, 0.0],
        delay_cs: 10.0,
        bright: 1.0,
        contrast: 1.0,
        sat: 1.0,
        export_speed: 10.0,
        status: "就绪 —— 打开一个 GIF 开始".into(),
        duck_busy: false,
        exporting: false,
        gif_name: String::new(),
        audio_name: String::new(),
        audio_duration_ms: 0,
        audio_start_ms: 0,
        audio_end_ms: 0,
        audio_playing: false,
    };

    if let Some((name, dur, start, end)) = saved_audio {
        app.audio_name = name;
        app.audio_duration_ms = dur;
        app.audio_start_ms = start.min(dur);
        app.audio_end_ms = end;
        app.status = format!("已恢复上次的音频对齐参数（{dur} ms），重新加载音频后生效");
    }

    APP.with(|a| *a.borrow_mut() = Some(app));

    // ---- rAF 循环 ----
    let slot: Rc<RefCell<Option<Closure<dyn FnMut(f64)>>>> = Rc::new(RefCell::new(None));
    let slot2 = slot.clone();
    let closure = Closure::wrap(Box::new(move |t: f64| {
        // 尺寸不在这里量 —— 见 viewport.rs。每帧读 clientWidth 会触发 forced
        // reflow，每帧给 canvas.width 赋值会重建 swapchain，两件事都不能干。
        APP.with(|a| {
            if let Some(app) = a.borrow_mut().as_mut() {
                app.frame(t);
            }
        });
        if let Some(c) = slot2.borrow().as_ref() {
            request_raf(c);
        }
    }) as Box<dyn FnMut(f64)>);

    request_raf(&closure);
    *slot.borrow_mut() = Some(closure);

    Ok(())
}

// ------------------------------------------------------------------ 字体

/// 阿里巴巴普惠体 2.0（55 Regular）的部署路径，相对于 index.html。
const CJK_FONT_URL: &str = "assets/puhuiti-55-regular.ttf";

/// 拉取普惠体并注册成默认 sans-serif。
///
/// 为什么运行时 fetch 而不是 `include_bytes!`：字体 5.5 MB，编进 wasm 会让
/// 产物从 3.4 MB 涨到 9 MB，而且整个 wasm 得先下完才能启动 —— 等于把字体的
/// 下载时间直接叠加到白屏时间上。单独 fetch 的话 wasm 先起来、界面先出现，
/// 字体随后补上，中文从豆腐块变成正常字。
///
/// 失败的后果只有"中文不显示"，所以调用方只记日志、不中断启动。
async fn load_cjk_font(fs: &FontSystemHandle) -> Result<String, String> {
    let win = web_sys::window().ok_or_else(|| "没有 window".to_string())?;

    let resp: web_sys::Response =
        wasm_bindgen_futures::JsFuture::from(win.fetch_with_str(CJK_FONT_URL))
            .await
            .map_err(|e| format!("请求 {CJK_FONT_URL} 失败：{e:?}"))?
            .dyn_into()
            .map_err(|_| "fetch 返回的不是 Response".to_string())?;

    if !resp.ok() {
        return Err(format!("HTTP {}（{CJK_FONT_URL}）", resp.status()));
    }

    let buf = wasm_bindgen_futures::JsFuture::from(
        resp.array_buffer()
            .map_err(|e| format!("读取响应体失败：{e:?}"))?,
    )
    .await
    .map_err(|e| format!("读取响应体失败：{e:?}"))?;

    let bytes = js_sys::Uint8Array::new(&buf).to_vec();
    // 字体文件被网关塞了错误页之类的情况，早点报出来比在 fontdb 里炸强
    if bytes.len() < 1024 {
        return Err(format!("字体文件只有 {} 字节，明显不对", bytes.len()));
    }

    let handle = load_font_bytes(fs, &bytes)?;

    // 设成默认 sans-serif：cosmic-text 按族名选字体，UI 里所有没显式指定
    // 字体族的文本都会落到这个族上。
    fs.lock()
        .map_err(|_| "FontSystem 被 poison".to_string())?
        .db_mut()
        .set_sans_serif_family(handle.family().to_string());

    Ok(format!("{}（{} 字节）", handle.family(), bytes.len()))
}
