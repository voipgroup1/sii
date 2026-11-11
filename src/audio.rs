//! 音频播放桥。
//!
//! 真正的播放器在 JS 侧（`window.__sii.audio`）：`AudioContext` 只在主线程
//! 可用，解码（`decodeAudioData`）本身就是浏览器给的异步 API，搬到 wasm 里
//! 没有意义。这里只做四件事：调播放/停止、读当前播放位置、读进度。
//!
//! **为什么时间要问 JS 拿**：`AudioContext.currentTime` 是声卡的硬件时钟，
//! 和 `performance.now()` 之间会缓慢漂移（晶振误差，几十 ppm 量级）。跑一分钟
//! 差几毫秒，跑十分钟就能攒出一帧以上的错位。音频播放期间以它为唯一时间源、
//! 让画面跟着它走，才谈得上"完美同步"。

use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__sii", "audio"], js_name = "play")]
    fn js_play(start_ms: i32, end_ms: i32) -> bool;

    #[wasm_bindgen(js_namespace = ["window", "__sii", "audio"], js_name = "stop")]
    fn js_stop();

    #[wasm_bindgen(js_namespace = ["window", "__sii", "audio"], js_name = "timeMs")]
    fn js_time_ms() -> f64;

    #[wasm_bindgen(js_namespace = ["window", "__sii", "audio"], js_name = "progress")]
    fn js_progress() -> f64;
}

/// 播放 `[start_ms, end_ms)` 这一段；`end_ms == 0` 表示播到音频结尾。
///
/// 返回 false 表示没加载音频、区间非法，或者浏览器拒绝了播放（多半是
/// AudioContext 还没拿到用户手势）。
pub fn play(start_ms: u32, end_ms: u32) -> bool {
    js_play(start_ms as i32, end_ms as i32)
}

pub fn stop() {
    js_stop();
}

/// 当前播放到音频文件的第几毫秒；没在播返回 -1。
///
/// 这是音频文件的绝对位置，包含 `start_ms` 偏移 —— 调用方要减掉起始点才是
/// GIF 时间轴上的位置。
pub fn time_ms() -> f64 {
    js_time_ms()
}

/// 本次播放区间内的进度 0..1，给进度条用。
pub fn progress() -> f32 {
    js_progress() as f32
}
