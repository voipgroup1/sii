//! GIF 文档模型：解码（含 disposal 合成）、编辑操作、重新编码。
//!
//! 这一层刻意不依赖任何 wasm/GPU API，可以在原生环境下 `cargo test` 跑通，
//! 是整套实验里唯一能被完整验证的部分。

use std::io::Cursor;

// ---------------------------------------------------------------- 错误类型

#[derive(Debug, Clone)]
pub struct GifError(pub String);

impl std::fmt::Display for GifError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GifError {}

impl From<gif::DecodingError> for GifError {
    fn from(e: gif::DecodingError) -> Self {
        GifError(format!("解码失败: {e}"))
    }
}

impl From<gif::EncodingError> for GifError {
    fn from(e: gif::EncodingError) -> Self {
        GifError(format!("编码失败: {e}"))
    }
}

// ---------------------------------------------------------------- 数据结构

/// 一帧完整画布（已经过 disposal 合成，可直接上屏）
#[derive(Clone)]
pub struct Frame {
    /// GIF 延迟，单位 1/100 秒（GIF 规范如此）
    pub delay_cs: u16,
    /// RGBA8，长度 = width * height * 4
    pub rgba: Vec<u8>,
}

impl Frame {
    pub fn delay_ms(&self) -> u32 {
        self.delay_cs as u32 * 10
    }
}

/// 一份 GIF 文档
#[derive(Clone)]
pub struct Doc {
    pub width: u16,
    pub height: u16,
    pub frames: Vec<Frame>,
    pub loop_infinite: bool,
}

impl Doc {
    pub fn frame_bytes(&self) -> usize {
        self.width as usize * self.height as usize * 4
    }
    /// 全部解码帧占用的内存
    pub fn memory_bytes(&self) -> usize {
        self.frame_bytes() * self.frames.len()
    }
    /// 总时长（毫秒）
    pub fn total_ms(&self) -> u32 {
        self.frames.iter().map(|f| f.delay_ms()).sum()
    }
    pub fn fps_estimate(&self) -> f32 {
        let t = self.total_ms();
        if t == 0 || self.frames.is_empty() {
            return 0.0;
        }
        self.frames.len() as f32 * 1000.0 / t as f32
    }
}

// ---------------------------------------------------------------- 解码

/// 解码整份 GIF，逐帧按 disposal 规则合成成完整画布。
///
/// 这是 GIF 播放器最容易做错的地方：
/// - 每帧可能只是"局部矩形"（`left/top/width/height`），要画到上一帧的结果上
/// - `dispose` 决定本帧显示完之后、下一帧绘制之前，这块区域怎么处理
///   - `Any` / `Keep`  → 保留
///   - `Background`    → 该矩形清成透明
///   - `Previous`      → 整幅画布回滚到本帧绘制之前的状态
pub fn decode(bytes: &[u8]) -> Result<Doc, GifError> {
    let mut opts = gif::DecodeOptions::new();
    opts.set_color_output(gif::ColorOutput::Indexed);
    let mut dec = opts.read_info(Cursor::new(bytes))?;

    let w = dec.width();
    let h = dec.height();
    if w == 0 || h == 0 {
        return Err(GifError("GIF 尺寸为 0".into()));
    }

    let global_palette: Option<Vec<u8>> = dec.global_palette().map(|p| p.to_vec());
    let stride = w as usize * 4;
    let mut canvas = vec![0u8; stride * h as usize];
    let mut frames: Vec<Frame> = Vec::new();
    let loop_infinite = true;

    while let Some(frame) = dec.read_next_frame()? {
        // Previous 语义要求先留一份"本帧绘制前"的快照
        let saved: Option<Vec<u8>> = if frame.dispose == gif::DisposalMethod::Previous {
            Some(canvas.clone())
        } else {
            None
        };

        let palette: &[u8] = match (&frame.palette, &global_palette) {
            (Some(p), _) if !p.is_empty() => &p[..],
            (_, Some(g)) if !g.is_empty() => &g[..],
            _ => return Err(GifError("帧缺少调色板".into())),
        };

        let fx = frame.left as usize;
        let fy = frame.top as usize;
        let fw = frame.width as usize;
        let fh = frame.height as usize;

        for y in 0..fh {
            let cy = fy + y;
            if cy >= h as usize {
                break;
            }
            for x in 0..fw {
                let cx = fx + x;
                if cx >= w as usize {
                    break;
                }
                let idx = frame.buffer[y * fw + x];
                if Some(idx) == frame.transparent {
                    continue;
                }
                let pi = idx as usize * 3;
                if pi + 2 >= palette.len() {
                    continue;
                }
                let o = cy * stride + cx * 4;
                canvas[o] = palette[pi];
                canvas[o + 1] = palette[pi + 1];
                canvas[o + 2] = palette[pi + 2];
                canvas[o + 3] = 255;
            }
        }

        frames.push(Frame {
            delay_cs: frame.delay,
            rgba: canvas.clone(),
        });

        // 为下一帧准备画布
        match frame.dispose {
            gif::DisposalMethod::Background => {
                for y in 0..fh {
                    let cy = fy + y;
                    if cy >= h as usize {
                        break;
                    }
                    let base = cy * stride;
                    for x in 0..fw {
                        let cx = fx + x;
                        if cx >= w as usize {
                            break;
                        }
                        let o = base + cx * 4;
                        canvas[o..o + 4].fill(0);
                    }
                }
            }
            gif::DisposalMethod::Previous => {
                if let Some(s) = saved {
                    canvas = s;
                }
            }
            _ => {}
        }
    }

    if frames.is_empty() {
        return Err(GifError("GIF 里没有任何帧".into()));
    }

    Ok(Doc {
        width: w,
        height: h,
        frames,
        loop_infinite,
    })
}

// ---------------------------------------------------------------- 编码

/// 把当前文档重新编码成 GIF 字节流。
///
/// `speed` 走 NeuQuant 量化：1 = 质量最好最慢，30 = 最快最糊。
pub fn encode(doc: &Doc, speed: i32) -> Result<Vec<u8>, GifError> {
    if doc.frames.is_empty() {
        return Err(GifError("没有帧可导出".into()));
    }
    let mut out: Vec<u8> = Vec::new();
    {
        let mut enc = gif::Encoder::new(&mut out, doc.width, doc.height, &[])?;
        enc.set_repeat(if doc.loop_infinite {
            gif::Repeat::Infinite
        } else {
            gif::Repeat::Finite(0)
        })?;

        for f in &doc.frames {
            let mut rgba = f.rgba.clone();
            let mut gf = gif::Frame::from_rgba_speed(doc.width, doc.height, &mut rgba, speed);
            // 0 延迟在浏览器里会被当成"越快越好"，兜底成 1cs
            gf.delay = f.delay_cs.max(1);
            enc.write_frame(&gf)?;
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- 编辑操作

/// 编辑操作的错误
#[derive(Debug, Clone)]
pub struct EditError(pub String);

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn bad_index(i: usize, n: usize) -> EditError {
    EditError(format!("帧下标 {i} 越界（共 {n} 帧）"))
}

/// 删除一帧
pub fn delete_frame(doc: &mut Doc, i: usize) -> Result<(), EditError> {
    if i >= doc.frames.len() {
        return Err(bad_index(i, doc.frames.len()));
    }
    if doc.frames.len() == 1 {
        return Err(EditError("至少保留一帧".into()));
    }
    doc.frames.remove(i);
    Ok(())
}

/// 复制一帧，插到原帧后面
pub fn duplicate_frame(doc: &mut Doc, i: usize) -> Result<(), EditError> {
    if i >= doc.frames.len() {
        return Err(bad_index(i, doc.frames.len()));
    }
    let f = doc.frames[i].clone();
    doc.frames.insert(i + 1, f);
    Ok(())
}

/// 交换两帧位置（用于上移/下移）
pub fn swap_frames(doc: &mut Doc, a: usize, b: usize) -> Result<(), EditError> {
    let n = doc.frames.len();
    if a >= n {
        return Err(bad_index(a, n));
    }
    if b >= n {
        return Err(bad_index(b, n));
    }
    doc.frames.swap(a, b);
    Ok(())
}

/// 统一设置所有帧的延迟
pub fn set_all_delays(doc: &mut Doc, delay_cs: u16) {
    for f in &mut doc.frames {
        f.delay_cs = delay_cs;
    }
}

/// 按目标帧率重算延迟
pub fn set_fps(doc: &mut Doc, fps: f32) {
    if fps <= 0.01 {
        return;
    }
    let cs = (100.0 / fps).round().max(1.0) as u16;
    set_all_delays(doc, cs);
}

/// 水平翻转
pub fn flip_horizontal(doc: &mut Doc) {
    let w = doc.width as usize;
    let h = doc.height as usize;
    for f in &mut doc.frames {
        for y in 0..h {
            let row = y * w * 4;
            for x in 0..w / 2 {
                let a = row + x * 4;
                let b = row + (w - 1 - x) * 4;
                for k in 0..4 {
                    f.rgba.swap(a + k, b + k);
                }
            }
        }
    }
}

/// 垂直翻转
pub fn flip_vertical(doc: &mut Doc) {
    let w = doc.width as usize;
    let h = doc.height as usize;
    let stride = w * 4;
    for f in &mut doc.frames {
        for y in 0..h / 2 {
            let a = y * stride;
            let b = (h - 1 - y) * stride;
            for k in 0..stride {
                f.rgba.swap(a + k, b + k);
            }
        }
    }
}

/// 裁剪到指定矩形（越界部分会被夹紧）
pub fn crop(doc: &mut Doc, x: u16, y: u16, cw: u16, ch: u16) -> Result<(), EditError> {
    let x = x.min(doc.width.saturating_sub(1));
    let y = y.min(doc.height.saturating_sub(1));
    let cw = cw.max(1).min(doc.width - x);
    let ch = ch.max(1).min(doc.height - y);
    if cw < 1 || ch < 1 {
        return Err(EditError("裁剪区域为空".into()));
    }
    let src_w = doc.width as usize;
    let new_stride = cw as usize * 4;
    for f in &mut doc.frames {
        let mut out = vec![0u8; new_stride * ch as usize];
        for row in 0..ch as usize {
            let s = ((y as usize + row) * src_w + x as usize) * 4;
            let d = row * new_stride;
            out[d..d + new_stride].copy_from_slice(&f.rgba[s..s + new_stride]);
        }
        f.rgba = out;
    }
    doc.width = cw;
    doc.height = ch;
    Ok(())
}

/// 最近邻缩放（保留硬边，适合像素风 GIF）
pub fn resize_nearest(doc: &mut Doc, nw: u16, nh: u16) -> Result<(), EditError> {
    if nw == 0 || nh == 0 {
        return Err(EditError("目标尺寸不能为 0".into()));
    }
    let sw = doc.width as usize;
    let sh = doc.height as usize;
    let new_stride = nw as usize * 4;
    for f in &mut doc.frames {
        let mut out = vec![0u8; new_stride * nh as usize];
        for dy in 0..nh as usize {
            let sy = dy * sh / nh as usize;
            for dx in 0..nw as usize {
                let sx = dx * sw / nw as usize;
                let s = (sy * sw + sx) * 4;
                let d = dy * new_stride + dx * 4;
                out[d..d + 4].copy_from_slice(&f.rgba[s..s + 4]);
            }
        }
        f.rgba = out;
    }
    doc.width = nw;
    doc.height = nh;
    Ok(())
}

/// 双线性缩放（平滑，适合照片类 GIF）
pub fn resize_bilinear(doc: &mut Doc, nw: u16, nh: u16) -> Result<(), EditError> {
    if nw == 0 || nh == 0 {
        return Err(EditError("目标尺寸不能为 0".into()));
    }
    let sw = doc.width as usize;
    let sh = doc.height as usize;
    let new_stride = nw as usize * 4;
    let xr = sw as f32 / nw as f32;
    let yr = sh as f32 / nh as f32;

    for f in &mut doc.frames {
        let mut out = vec![0u8; new_stride * nh as usize];
        for dy in 0..nh as usize {
            let fy = (dy as f32 + 0.5) * yr - 0.5;
            let y0 = fy.floor().max(0.0) as usize;
            let y1 = (y0 + 1).min(sh - 1);
            let wy = (fy - y0 as f32).clamp(0.0, 1.0);
            for dx in 0..nw as usize {
                let fx = (dx as f32 + 0.5) * xr - 0.5;
                let x0 = fx.floor().max(0.0) as usize;
                let x1 = (x0 + 1).min(sw - 1);
                let wx = (fx - x0 as f32).clamp(0.0, 1.0);

                let p00 = (y0 * sw + x0) * 4;
                let p10 = (y0 * sw + x1) * 4;
                let p01 = (y1 * sw + x0) * 4;
                let p11 = (y1 * sw + x1) * 4;
                let d = dy * new_stride + dx * 4;
                for k in 0..4 {
                    let top = f.rgba[p00 + k] as f32 * (1.0 - wx) + f.rgba[p10 + k] as f32 * wx;
                    let bot = f.rgba[p01 + k] as f32 * (1.0 - wx) + f.rgba[p11 + k] as f32 * wx;
                    out[d + k] = (top * (1.0 - wy) + bot * wy).round() as u8;
                }
            }
        }
        f.rgba = out;
    }
    doc.width = nw;
    doc.height = nh;
    Ok(())
}

/// 亮度 / 对比度 / 饱和度调整。三个参数都是 1.0 表示不变。
pub fn adjust(doc: &mut Doc, brightness: f32, contrast: f32, saturation: f32) {
    for f in &mut doc.frames {
        for px in f.rgba.chunks_exact_mut(4) {
            let mut r = px[0] as f32 / 255.0;
            let mut g = px[1] as f32 / 255.0;
            let mut b = px[2] as f32 / 255.0;

            r = (r * brightness - 0.5) * contrast + 0.5;
            g = (g * brightness - 0.5) * contrast + 0.5;
            b = (b * brightness - 0.5) * contrast + 0.5;

            let l = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            r = l + (r - l) * saturation;
            g = l + (g - l) * saturation;
            b = l + (b - l) * saturation;

            px[0] = (r.clamp(0.0, 1.0) * 255.0).round() as u8;
            px[1] = (g.clamp(0.0, 1.0) * 255.0).round() as u8;
            px[2] = (b.clamp(0.0, 1.0) * 255.0).round() as u8;
        }
    }
}

/// 反色
pub fn invert(doc: &mut Doc) {
    for f in &mut doc.frames {
        for px in f.rgba.chunks_exact_mut(4) {
            px[0] = 255 - px[0];
            px[1] = 255 - px[1];
            px[2] = 255 - px[2];
        }
    }
}

/// 按像素预算抽帧：内存吃不住时按等间隔丢帧，保住关键帧
pub fn thin_to_budget(doc: &mut Doc, max_bytes: usize) -> usize {
    let fb = doc.frame_bytes();
    if fb == 0 {
        return 0;
    }
    let budget_frames = (max_bytes / fb).max(1);
    if doc.frames.len() <= budget_frames {
        return 0;
    }
    let step = (doc.frames.len() as f32 / budget_frames as f32).ceil() as usize;
    let mut kept = Vec::with_capacity(budget_frames);
    let mut i = 0;
    while i < doc.frames.len() && kept.len() < budget_frames {
        kept.push(doc.frames[i].clone());
        i += step.max(1);
    }
    let dropped = doc.frames.len() - kept.len();
    doc.frames = kept;
    dropped
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个 2x2、2 帧、带局部更新和透明色的小 GIF 用于往返测试
    fn synth_gif() -> Vec<u8> {
        let mut out = Vec::new();
        {
            let palette: &[u8] = &[255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0];
            let mut enc = gif::Encoder::new(&mut out, 4, 4, palette).unwrap();
            enc.set_repeat(gif::Repeat::Infinite).unwrap();

            let mut f1 = gif::Frame::default();
            f1.width = 4;
            f1.height = 4;
            f1.buffer = std::borrow::Cow::Owned(vec![0u8; 16]);
            f1.delay = 10;
            enc.write_frame(&f1).unwrap();

            let mut f2 = gif::Frame::default();
            f2.left = 1;
            f2.top = 1;
            f2.width = 2;
            f2.height = 2;
            f2.buffer = std::borrow::Cow::Owned(vec![1u8; 4]);
            f2.delay = 20;
            f2.dispose = gif::DisposalMethod::Background;
            enc.write_frame(&f2).unwrap();
        }
        out
    }

    #[test]
    fn decode_reports_size_and_frames() {
        let doc = decode(&synth_gif()).unwrap();
        assert_eq!(doc.width, 4);
        assert_eq!(doc.height, 4);
        assert_eq!(doc.frames.len(), 2);
        assert_eq!(doc.frames[0].delay_cs, 10);
        assert_eq!(doc.frames[1].delay_cs, 20);
    }

    #[test]
    fn decode_composites_partial_frame_onto_canvas() {
        let doc = decode(&synth_gif()).unwrap();
        // 第一帧全红
        let f0 = &doc.frames[0].rgba;
        assert_eq!(&f0[0..4], &[255, 0, 0, 255]);
        // 第二帧只在 (1,1)-(2,2) 画绿色，其余保持红
        let f1 = &doc.frames[1].rgba;
        let px = |x: usize, y: usize| {
            let o = (y * 4 + x) * 4;
            [f1[o], f1[o + 1], f1[o + 2], f1[o + 3]]
        };
        assert_eq!(px(0, 0), [255, 0, 0, 255], "角落应保持上一帧的红色");
        assert_eq!(px(1, 1), [0, 255, 0, 255], "局部区域应是绿色");
        assert_eq!(px(2, 2), [0, 255, 0, 255]);
    }

    #[test]
    fn roundtrip_encode_decode() {
        let doc = decode(&synth_gif()).unwrap();
        let bytes = encode(&doc, 10).unwrap();
        let doc2 = decode(&bytes).unwrap();
        assert_eq!(doc2.width, doc.width);
        assert_eq!(doc2.height, doc.height);
        assert_eq!(doc2.frames.len(), doc.frames.len());
    }

    #[test]
    fn edit_ops_behave() {
        let mut doc = decode(&synth_gif()).unwrap();
        let n = doc.frames.len();

        duplicate_frame(&mut doc, 0).unwrap();
        assert_eq!(doc.frames.len(), n + 1);
        delete_frame(&mut doc, 0).unwrap();
        assert_eq!(doc.frames.len(), n);

        set_fps(&mut doc, 25.0);
        assert!(doc.frames.iter().all(|f| f.delay_cs == 4));

        flip_horizontal(&mut doc);
        flip_vertical(&mut doc);
        assert_eq!((doc.width, doc.height), (4, 4));

        crop(&mut doc, 1, 1, 2, 2).unwrap();
        assert_eq!((doc.width, doc.height), (2, 2));
        assert!(doc.frames.iter().all(|f| f.rgba.len() == 2 * 2 * 4));

        resize_nearest(&mut doc, 8, 8).unwrap();
        assert_eq!((doc.width, doc.height), (8, 8));
        resize_bilinear(&mut doc, 3, 3).unwrap();
        assert_eq!((doc.width, doc.height), (3, 3));
    }

    #[test]
    fn single_frame_cannot_be_deleted() {
        let mut doc = decode(&synth_gif()).unwrap();
        delete_frame(&mut doc, 0).unwrap();
        assert!(delete_frame(&mut doc, 0).is_err());
    }

    #[test]
    fn thinning_respects_budget() {
        let mut doc = decode(&synth_gif()).unwrap();
        // 4x4 一帧 64 字节，预算 128 字节 => 只留 2 帧
        let dropped = thin_to_budget(&mut doc, 128);
        assert_eq!(doc.frames.len(), 2);
        assert_eq!(dropped, 0);
    }

    #[test]
    fn empty_input_is_an_error() {
        assert!(decode(&[]).is_err());
    }
}
