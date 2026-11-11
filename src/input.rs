//! DOM 事件 → `InputState` 桥接。
//!
//! 事件处理全部下沉到 JS 侧：index.html 里的一组监听器只往一个
//! `Float32Array` 里写数字，一个 wasm 边界都不过。wasm 每帧读一次快照，
//! 翻译成 wgpu-gameui 的 `InputState`。
//!
//! 为什么值得这么绕：以前每个 `mousemove` 都调一次 wasm 闭包，1000 Hz 的
//! 游戏鼠标一秒能打上千次跨语言调用，主线程光在参数封送和边界切换上就烧掉
//! 一大截。改成「事件写数组、每帧读一次」之后，事件处理退化成几次数组赋值，
//! 跨语言调用从 O(事件数) 降到 O(帧数)，60 Hz 下就是每秒 60 次。
//!
//! 还有一层好处：事件永远不丢。以前 wasm 闭包里的 `borrow_mut()` 撞上正在
//! 渲染的帧会 panic，高频事件下这是真实存在的风险；现在 JS 侧只碰自己的
//! 数组，和 wasm 的借用检查完全解耦。
//!
//! 键盘不走这条路：它走 index.html → `sii::key()` 的短路，避免和 UI 的
//! 方向键导航抢事件。

use std::cell::RefCell;

use wasm_bindgen::JsCast;
use wgpu_gameui::InputState;

/// JS 侧事件缓冲区挂在这个全局变量上（`window.__siiInput`）。
const INPUT_GLOBAL: &str = "__siiInput";

/// 缓冲区槽位数。末尾留了空位，以后加字段不用改 JS 侧的分配。
const SLOTS: u32 = 16;

// 字段偏移。改这里必须同步改 index.html 里的 I_* 常量。
const I_X: usize = 0;
const I_Y: usize = 1;
const I_DOWN: usize = 2;
const I_RDOWN: usize = 3;
const I_MDOWN: usize = 4;
const I_CLICK: usize = 5;
const I_RCLICK: usize = 6;
const I_MCLICK: usize = 7;
const I_RELEASE: usize = 8;
const I_RRELEASE: usize = 9;
const I_MRELEASE: usize = 10;
const I_SCROLL: usize = 11;
const I_DX: usize = 12;
const I_DY: usize = 13;
const I_DRAGGING: usize = 14;

thread_local! {
    static BUF: RefCell<Option<js_sys::Float32Array>> = const { RefCell::new(None) };
}

/// 抓住 JS 侧的事件缓冲区。启动时调一次。
///
/// 拿不到也不致命 —— 只是所有指针交互失效，画面照常渲染。所以这里只记日志、
/// 不 panic：一个拿不到缓冲区的输入层不该把整个编辑器拦在启动页上。
pub fn attach() {
    let Some(win) = web_sys::window() else { return };
    let Ok(v) = js_sys::Reflect::get(&win, &wasm_bindgen::JsValue::from_str(INPUT_GLOBAL))
    else {
        return;
    };
    let Ok(arr) = v.dyn_into::<js_sys::Float32Array>() else {
        log::warn!("{INPUT_GLOBAL} 不是 Float32Array，指针交互不会生效");
        return;
    };
    if arr.length() != SLOTS {
        log::warn!(
            "{INPUT_GLOBAL} 长度是 {}，期望 {SLOTS} —— index.html 和 input.rs 版本对不上，\
             指针交互不会生效",
            arr.length()
        );
        return;
    }
    BUF.with(|b| *b.borrow_mut() = Some(arr));
}

/// 把 JS 侧攒下的事件搬进 `InputState`，并把边沿和增量清零。
///
/// 每帧调一次，必须在 `Frame::run` 之前 —— 否则 UI 拿到的是上一帧的输入，
/// 点按钮会感觉慢半拍。
pub fn sync(st: &mut InputState) {
    BUF.with(|b| {
        // 借用要绑到一个变量上：`b.borrow().as_ref()` 里的临时 Ref 在语句
        // 结束时就 drop 了，后面的 arr 会悬空。
        let binding = b.borrow();
        let Some(arr) = binding.as_ref() else { return };

        // 一次读 16 个 f32（64 字节）。比逐个 Reflect::get 快一个数量级，
        // 而且拿到的是同一时刻的一致快照 —— 逐个读可能读到事件改了一半的状态。
        let v = arr.to_vec();
        if v.len() < SLOTS as usize {
            return;
        }

        st.mouse_x = v[I_X];
        st.mouse_y = v[I_Y];
        st.mouse_down = v[I_DOWN] != 0.0;
        st.mouse_right_down = v[I_RDOWN] != 0.0;
        st.mouse_middle_down = v[I_MDOWN] != 0.0;
        st.mouse_clicked = v[I_CLICK] != 0.0;
        st.mouse_right_clicked = v[I_RCLICK] != 0.0;
        st.mouse_middle_clicked = v[I_MCLICK] != 0.0;
        st.mouse_released = v[I_RELEASE] != 0.0;
        st.mouse_right_released = v[I_RRELEASE] != 0.0;
        st.mouse_middle_released = v[I_MRELEASE] != 0.0;
        st.scroll_delta = v[I_SCROLL];
        st.drag_delta = [v[I_DX], v[I_DY]];
        st.is_dragging = v[I_DRAGGING] != 0.0;

        // 边沿和增量属于"这一帧发生的事"，消费完必须清零，否则下一帧会重复
        // 触发 —— 表现是点一下按钮触发两次、滚一格滚两格。
        // 按住状态（I_DOWN / I_RDOWN / I_MDOWN）是持续状态，不能清。
        for i in I_CLICK..=I_DRAGGING {
            arr.set_index(i as u32, 0.0);
        }
    });
}
