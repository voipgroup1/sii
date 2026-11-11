//! 画布尺寸桥。
//!
//! 尺寸由 JS 侧的 `ResizeObserver` 维护在 `window.__siiSize` 里，这里每帧只
//! 读一次缓存值，一个 DOM 查询都不做。
//!
//! **为什么不能每帧自己量**：
//!
//! - `clientWidth` / `getBoundingClientRect()` 是布局属性，读它会触发
//!   forced reflow —— 浏览器必须把攒着的样式和布局全部算完才肯回答，
//!   一秒六十次就是六十次同步布局。
//! - `canvas.width = x` 更狠：赋值会重置绘图缓冲、让浏览器重建 canvas 的
//!   swapchain。那是几十毫秒的活，而且上一帧 wgpu 已经拿到的 surface
//!   texture 会直接作废 —— 画上去就是旧帧残留和新内容叠在一起，看起来
//!   就是"图形文字全糊成一团"。
//!
//! 所以尺寸这件事必须"变了才动"，而不是"每帧确认一遍"。

use std::cell::RefCell;

use wasm_bindgen::JsCast;

/// JS 侧尺寸缓冲区挂在这个全局变量上（`window.__siiSize`）。
const SIZE_GLOBAL: &str = "__siiSize";

/// 槽位数：[cssW, cssH, dpr, gen]
const SLOTS: u32 = 4;

thread_local! {
    static BUF: RefCell<Option<js_sys::Float32Array>> = const { RefCell::new(None) };
}

/// 一份画布尺寸快照。
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Size {
    /// CSS 逻辑像素宽。UI 布局全用它。
    pub css_w: f32,
    /// CSS 逻辑像素高。
    pub css_h: f32,
    /// devicePixelRatio。窗口拖到另一块缩放比例不同的屏幕时会变。
    pub dpr: f32,
    /// 版本号。变了说明尺寸或 dpr 动过，可以用来做"只在变化时重建"的判据。
    pub gen: u32,
}

impl Default for Size {
    fn default() -> Self {
        Self {
            css_w: 1.0,
            css_h: 1.0,
            dpr: 1.0,
            gen: 0,
        }
    }
}

impl Size {
    /// 帧缓冲的像素尺寸。
    ///
    /// 必须和 JS 侧 `canvas.width/height` 用同一套算法（都是 `round`）：差一个
    /// 像素就会让 `sync_surface` 每帧都判定"尺寸变了"，白烧一次 swapchain
    /// 重建 —— 那正是这次要修的病。
    pub fn physical(&self) -> (u32, u32) {
        (
            (self.css_w * self.dpr).round().max(1.0) as u32,
            (self.css_h * self.dpr).round().max(1.0) as u32,
        )
    }
}

/// 抓住 JS 侧的尺寸缓冲区。启动时调一次。
///
/// 拿不到就退回 1×1 —— 画面会很难看，但不至于 panic：一个量不到窗口大小的
/// 编辑器不该把用户拦在启动页上。
pub fn attach() {
    let Some(win) = web_sys::window() else { return };
    let Ok(v) = js_sys::Reflect::get(&win, &wasm_bindgen::JsValue::from_str(SIZE_GLOBAL)) else {
        log::warn!("{SIZE_GLOBAL} 不存在，画布尺寸不会跟随窗口变化");
        return;
    };
    let Ok(arr) = v.dyn_into::<js_sys::Float32Array>() else {
        log::warn!("{SIZE_GLOBAL} 不是 Float32Array，画布尺寸不会跟随窗口变化");
        return;
    };
    if arr.length() != SLOTS {
        log::warn!(
            "{SIZE_GLOBAL} 长度是 {}，期望 {SLOTS} —— index.html 和 viewport.rs 版本对不上",
            arr.length()
        );
        return;
    }
    BUF.with(|b| *b.borrow_mut() = Some(arr));
}

/// 读当前尺寸。没接上 JS 侧就返回 `None`。
pub fn read() -> Option<Size> {
    BUF.with(|b| {
        // 借用要绑到变量上：`b.borrow().as_ref()` 里的临时 Ref 在语句结束时就
        // drop 了，后面的 arr 会悬空。
        let binding = b.borrow();
        let arr = binding.as_ref()?;
        let v = arr.to_vec();
        if v.len() < SLOTS as usize {
            return None;
        }
        Some(Size {
            css_w: v[0].max(1.0),
            css_h: v[1].max(1.0),
            // dpr 理论上不可能是 0，但真出现了会让 physical() 变成 1×1，
            // 画面直接消失，不如兜个底。
            dpr: if v[2] > 0.0 { v[2] } else { 1.0 },
            gen: v[3].max(0.0) as u32,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::Size;

    /// 物理尺寸必须和 JS 侧 `Math.round(cssW * dpr)` 完全一致 —— 差一个像素
    /// 就会让 `sync_surface` 每帧都判定"尺寸变了"，白烧一次 swapchain 重建。
    #[test]
    fn physical_matches_js_round() {
        let s = Size { css_w: 1280.0, css_h: 720.0, dpr: 1.25, gen: 0 };
        assert_eq!(s.physical(), (1600, 900));

        let s = Size { css_w: 1512.0, css_h: 982.0, dpr: 2.0, gen: 0 };
        assert_eq!(s.physical(), (3024, 1964));
    }

    /// 窗口最小化、元素 `display: none` 时量出来是 0。不能让它变成 0×0 ——
    /// wgpu 的 surface 不接受零尺寸，configure 会直接失败。
    #[test]
    fn physical_never_zero() {
        let s = Size { css_w: 0.0, css_h: 0.0, dpr: 0.0, gen: 0 };
        assert_eq!(s.physical(), (1, 1));
    }
}
