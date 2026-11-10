//! DOM 事件 → `InputState` 桥接。
//!
//! wgpu-gameui 只认自己的 `InputState`，不碰 DOM。这里把浏览器的指针/滚轮事件
//! 翻译过去。键盘走 index.html → `sii::key()` 的短路，避免和 UI 的方向键导航打架。

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wgpu_gameui::InputState;

/// 事件监听器集合。`Closure::forget` 之后由页面生命周期兜底回收——
/// 单页应用里监听器本来就该活到关闭为止。
pub fn attach(canvas: &web_sys::HtmlCanvasElement, input: Rc<RefCell<InputState>>) {
    let win = web_sys::window().expect("no window");

    // ---- 指针移动 ----
    {
        let st = input.clone();
        let cb = Closure::wrap(Box::new(move |e: web_sys::MouseEvent| {
            let mut s = st.borrow_mut();
            s.mouse_x = e.offset_x() as f32;
            s.mouse_y = e.offset_y() as f32;
        }) as Box<dyn FnMut(_)>);
        let _ = canvas.add_event_listener_with_callback(
            "mousemove",
            cb.as_ref().unchecked_ref(),
        );
        cb.forget();
    }

    // ---- 按下 ----
    {
        let st = input.clone();
        let cb = Closure::wrap(Box::new(move |e: web_sys::MouseEvent| {
            let mut s = st.borrow_mut();
            s.mouse_x = e.offset_x() as f32;
            s.mouse_y = e.offset_y() as f32;
            if e.button() == 0 {
                s.mouse_down = true;
                s.mouse_clicked = true;
            } else if e.button() == 2 {
                s.mouse_right_down = true;
                s.mouse_right_clicked = true;
            } else if e.button() == 1 {
                s.mouse_middle_down = true;
                s.mouse_middle_clicked = true;
            }
        }) as Box<dyn FnMut(_)>);
        let _ = canvas.add_event_listener_with_callback(
            "mousedown",
            cb.as_ref().unchecked_ref(),
        );
        cb.forget();
    }

    // ---- 抬起 ----
    {
        let st = input.clone();
        let cb = Closure::wrap(Box::new(move |e: web_sys::MouseEvent| {
            let mut s = st.borrow_mut();
            s.mouse_x = e.offset_x() as f32;
            s.mouse_y = e.offset_y() as f32;
            if e.button() == 0 {
                s.mouse_down = false;
                s.mouse_released = true;
            } else if e.button() == 2 {
                s.mouse_right_down = false;
                s.mouse_right_released = true;
            } else if e.button() == 1 {
                s.mouse_middle_down = false;
                s.mouse_middle_released = true;
            }
        }) as Box<dyn FnMut(_)>);
        let _ = win.add_event_listener_with_callback("mouseup", cb.as_ref().unchecked_ref());
        cb.forget();
    }

    // ---- 滚轮 ----
    {
        let st = input.clone();
        let cb = Closure::wrap(Box::new(move |e: web_sys::WheelEvent| {
            // canvas 上的 wheel 默认不是 passive 的，preventDefault 有效
            e.prevent_default();
            let mut s = st.borrow_mut();
            s.scroll_delta -= e.delta_y() as f32;
        }) as Box<dyn FnMut(_)>);
        let _ = canvas.add_event_listener_with_callback("wheel", cb.as_ref().unchecked_ref());
        cb.forget();
    }

    // ---- 拖拽增量 ----
    {
        let st = input.clone();
        let cb = Closure::wrap(Box::new(move |e: web_sys::MouseEvent| {
            let mut s = st.borrow_mut();
            if s.mouse_down {
                s.drag_delta = [e.movement_x() as f32, e.movement_y() as f32];
                s.is_dragging = true;
            }
        }) as Box<dyn FnMut(_)>);
        let _ = canvas.add_event_listener_with_callback(
            "mousemove",
            cb.as_ref().unchecked_ref(),
        );
        cb.forget();
    }

    // 右键菜单会挡住画布交互
    {
        let cb = Closure::wrap(Box::new(move |e: web_sys::MouseEvent| {
            e.prevent_default();
        }) as Box<dyn FnMut(_)>);
        let _ = canvas.add_event_listener_with_callback(
            "contextmenu",
            cb.as_ref().unchecked_ref(),
        );
        cb.forget();
    }
}
