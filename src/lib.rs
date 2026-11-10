//! sii —— 纯前端、WebGPU 加速的 GIF 观看与编辑器。
//!
//! 没有后端。文件读取走浏览器 File API，解码/合成/编辑全在 wasm 里，
//! 上屏走 WebGPU（wgpu + wgpu-gameui），导出在 wasm 里重新编码成 GIF 字节流。

mod canvas;
mod duck;
pub mod gifdoc;
mod input;

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wgpu_gameui::{
    shared_font_system, DrawList, Frame, InputState, KeyboardNav, Theme, UiContext, UiRenderer,
    UiState,
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
    viewport: Rc<RefCell<(f32, f32)>>,

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

        self.sync_surface();

        // ---- 播放推进 ----
        if self.playing {
            if let Some(doc) = &self.doc {
                if doc.frames.len() > 1 {
                    let cur_delay = doc.frames[self.frame_idx].delay_ms().max(10) as f32;
                    self.play_acc += dt * 1000.0;
                    if self.play_acc >= cur_delay {
                        self.play_acc -= cur_delay;
                        self.frame_idx = (self.frame_idx + 1) % doc.frames.len();
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
        let (vw, vh) = *self.viewport.borrow();
        let (vw, vh) = (vw.max(320.0), vh.max(240.0));
        let px = LEFT_W;
        let py = TOP_H;
        let pw = (vw - LEFT_W - RIGHT_W).max(80.0);
        let ph = (vh - TOP_H - BOTTOM_H).max(80.0);

        let mut acts = Actions::default();

        let mut list = self.draw_list.take().expect("draw list 被吃了");
        {
            // 逐字段借用，绕开闭包对 self 的整体占用
            let ui_state = &mut self.ui_state;
            let input = &mut *self.input.borrow_mut();
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

            Frame::new(ui_state, input, theme, &KeyboardNav)
                .dt(dt)
                .run(&mut list, |ui| {
                    build_ui(
                        ui,
                        UiView {
                            vw,
                            vh,
                            px,
                            py,
                            pw,
                            ph,
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

        let dpr = self.viewport.borrow().0;
        let _ = dpr;
        let scale = web_sys::window()
            .map(|w| w.device_pixel_ratio() as f32)
            .unwrap_or(1.0);

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

        let phys = (
            (vw * scale).round().max(1.0) as u32,
            (vh * scale).round().max(1.0) as u32,
        );
        if let Some(list) = &self.draw_list {
            self.ui
                .render(&self.device, &self.queue, &mut encoder, &view, phys, scale, list);
        }

        self.queue.submit(Some(encoder.finish()));
        // wgpu 30 起 SurfaceTexture 不再有显式 present()，drop 即呈现
        drop(out);
    }

    fn sync_surface(&mut self) {
        let (vw, vh) = *self.viewport.borrow();
        let scale = web_sys::window()
            .map(|w| w.device_pixel_ratio() as f32)
            .unwrap_or(1.0);
        let w = (vw * scale).round().max(1.0) as u32;
        let h = (vh * scale).round().max(1.0) as u32;
        if w != self.config.width || h != self.config.height {
            self.config.width = w;
            self.config.height = h;
            self.surface.configure(&self.device, &self.config);
        }
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
            self.playing = !self.playing;
            self.play_acc = 0.0;
        }
        if acts.prev {
            if let Some(doc) = &self.doc {
                if !doc.frames.is_empty() {
                    self.frame_idx =
                        (self.frame_idx + doc.frames.len() - 1) % doc.frames.len();
                }
            }
        }
        if acts.next {
            if let Some(doc) = &self.doc {
                if !doc.frames.is_empty() {
                    self.frame_idx = (self.frame_idx + 1) % doc.frames.len();
                }
            }
        }
        if let Some(i) = acts.seek {
            self.frame_idx = i;
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
}

struct UiView<'a> {
    vw: f32,
    vh: f32,
    px: f32,
    py: f32,
    pw: f32,
    ph: f32,
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
/// 返回一行状态文字。
#[wasm_bindgen]
pub fn load_gif(bytes: &[u8]) -> Result<String, JsValue> {
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
            app.frame_idx = 0;
            app.playing = false;
            app.play_acc = 0.0;
            app.zoom = 1.0;
            app.pan = [0.0, 0.0];
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

    let vw = canvas.client_width().max(1) as f32;
    let vh = canvas.client_height().max(1) as f32;
    canvas.set_width((vw * win.device_pixel_ratio() as f32) as u32);
    canvas.set_height((vh * win.device_pixel_ratio() as f32) as u32);

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let surface = instance
        .create_surface(wgpu::SurfaceTarget::Canvas(canvas.clone()))
        .map_err(|e| JsValue::from_str(&format!("create_surface: {e}")))?;

    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
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
        width: (vw * win.device_pixel_ratio() as f32) as u32,
        height: (vh * win.device_pixel_ratio() as f32) as u32,
        present_mode: wgpu::PresentMode::Fifo,
        desired_maximum_frame_latency: 2,
        alpha_mode: caps.alpha_modes[0],
        view_formats: vec![],
        color_space: wgpu::SurfaceColorSpace::Auto,
    };
    surface.configure(&device, &config);

    let font_system = shared_font_system();
    let ui = UiRenderer::new(&device, &queue, format, font_system.clone());
    let draw_list = DrawList::with_font_system(font_system);
    let preview = canvas::Preview::new(&device, format, 1, 1);

    let input = Rc::new(RefCell::new(InputState::default()));
    input::attach(&canvas, input.clone());

    let viewport = Rc::new(RefCell::new((vw, vh)));

    let app = App {
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
        viewport: viewport.clone(),
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
    };

    APP.with(|a| *a.borrow_mut() = Some(app));

    // ---- rAF 循环 ----
    let slot: Rc<RefCell<Option<Closure<dyn FnMut(f64)>>>> = Rc::new(RefCell::new(None));
    let slot2 = slot.clone();
    let vp = viewport.clone();
    let closure = Closure::wrap(Box::new(move |t: f64| {
        // 每帧同步画布尺寸（CSS 变了就跟着变）
        if let Some(w) = web_sys::window() {
            if let Some(d) = w.document() {
                if let Some(e) = d.get_element_by_id("sii-canvas") {
                    if let Ok(c) = e.dyn_into::<web_sys::HtmlCanvasElement>() {
                        let nw = c.client_width().max(1) as f32;
                        let nh = c.client_height().max(1) as f32;
                        let dpr = w.device_pixel_ratio() as f32;
                        c.set_width((nw * dpr) as u32);
                        c.set_height((nh * dpr) as u32);
                        *vp.borrow_mut() = (nw, nh);
                    }
                }
            }
        }
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
