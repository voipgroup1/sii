//! 生成一份测试用的动图，方便在没有素材的时候跑通链路。
//! 用法：cargo run --bin mksample -- /tmp/sii-sample.gif

use std::io::Write;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let path = a.get(1).cloned().unwrap_or_else(|| "/tmp/sii-sample.gif".to_string());
    let w: u16 = a.get(2).and_then(|v| v.parse().ok()).unwrap_or(96);
    let h: u16 = a.get(3).and_then(|v| v.parse().ok()).unwrap_or(96);
    let frames: u16 = a.get(4).and_then(|v| v.parse().ok()).unwrap_or(24);

    let mut out: Vec<u8> = Vec::new();
    {
        // 16 色调色板：色相环
        let mut pal = Vec::with_capacity(48);
        for i in 0..16u8 {
            let a = i as f32 / 16.0 * std::f32::consts::TAU;
            pal.push(((a.cos() * 0.5 + 0.5) * 255.0) as u8);
            pal.push(((a.sin() * 0.5 + 0.5) * 255.0) as u8);
            pal.push((((i as f32 / 16.0) * 255.0) as u8).max(40));
        }
        let mut enc = gif::Encoder::new(&mut out, w, h, &pal).unwrap();
        enc.set_repeat(gif::Repeat::Infinite).unwrap();

        for f in 0..frames {
            let t = f as f32 / frames as f32 * std::f32::consts::TAU;
            let mut buf = vec![0u8; w as usize * h as usize];
            for y in 0..h as usize {
                for x in 0..w as usize {
                    let dx = x as f32 - w as f32 / 2.0;
                    let dy = y as f32 - h as f32 / 2.0;
                    let r = (dx * dx + dy * dy).sqrt();
                    let ang = dy.atan2(dx);
                    let v = ((ang + t).sin() * 3.0 + r / 6.0) as i32;
                    buf[y * w as usize + x] = (v.rem_euclid(16)) as u8;
                }
            }
            let mut gf = gif::Frame::default();
            gf.width = w;
            gf.height = h;
            gf.buffer = std::borrow::Cow::Owned(buf);
            gf.delay = 8;
            enc.write_frame(&gf).unwrap();
        }
    }

    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(&out).unwrap();
    println!("写出 {path}：{w}×{h} / {frames} 帧 / {} 字节", out.len());
}
