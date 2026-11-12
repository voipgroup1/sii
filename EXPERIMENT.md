# 实验报告：wasm + WebGPU + DuckDB 做纯前端 GIF 编辑器

> 目标：不用任何后端，只用前端 + WebGPU，实现"观看 + 编辑高清 GIF"，
> 并验证能不能把 DuckDB 集成进来，为将来接大模型改 GIF 铺路。

## 1. 结论先说

**可行，但有一个硬边界。**

- 观看、编辑、导出这条链路完全跑得通，全程零后端、零网络请求。
- WebGPU 只负责上屏，它**不是**瓶颈 —— 一次 draw call，几百帧的 800×600 图也毫无压力。
- 真正的瓶颈是 **CPU 侧的全画布 RGBA 解码**：800×600 × 48 帧 = 87.9 MB 常驻内存，
  解码 296 ms。按这个比例，1920×1080 × 100 帧要 829 MB，直接顶到 wasm32 的 4 GB 天花板。
- DuckDB 集成**有意义但必须分层**：像素进库是自杀，元数据进库很好用。

## 2. 环境

| 项 | 值 |
|---|---|
| 沙箱 | 1 核 / 1.9 GB RAM / 9.8 GB 磁盘，无 swap |
| Rust | 1.99.1，`wasm32-unknown-unknown` |
| wasm-bindgen | 0.2.130（CLI 用预编译包，`cargo install` 在单核上太慢） |
| wgpu | 30.0.1，只开 `webgpu` feature |
| wgpu-gameui | git 依赖，`default-features = false` + `bundled-font` + `phosphor-icons` |
| 产物 | 3.5 MB wasm（`opt-level="s"` + thin LTO） |

## 3. 一路上踩的坑

### 3.1 中文路径会让 rust-lld 直接失败

在 `/Coze/Drive/九丫头/所有对话/...` 下跑 `cargo check --target wasm32-unknown-unknown`：

```
error: linking with `cc` failed
  = note: rust-lld: error: failed to write output
          '.../libbytemuck_derive-74b1732a00514937.so': No such file or directory
```

不是权限问题，是 rust-lld 处理不了非 ASCII 路径。换到 `/root/work/` 纯 ASCII 目录后
`cargo check` 68 秒通过。**所有 wasm 构建都必须放在纯 ASCII 路径下。**

### 3.2 国内拉 Rust 工具链

`static.rust-lang.org` 实测 41 KB/s，193.6 MB 工具链要 80 分钟，第一次尝试 900 秒超时。

换成 `rsproxy.cn`（实测 24 MB/s）后几十秒装完。crates.io 同理，走 sparse 索引：

```toml
[source.crates-io]
replace-with = 'rsproxy-sparse'
[source.rsproxy-sparse]
registry = "sparse+https://rsproxy.cn/index/"
```

### 3.3 wgpu-gameui 的 wasm 构建必须关掉默认 feature

默认 `["bundled-font", "phosphor-icons", "headless"]`，其中 `headless` 拉 `pollster` +
`axum` + `tokio`，在 wasm 上编不过。正确姿势：

```toml
wgpu-gameui = { git = "...", default-features = false, features = ["bundled-font", "phosphor-icons"] }
```

### 3.4 wgpu 30 的 API 和网上教程对不上

| 变化 | wgpu 30 的写法 |
|---|---|
| `PipelineLayoutDescriptor` | `bind_group_layouts: &[Option<&BindGroupLayout>]`，`immediate_size` 取代 `push_constant_ranges` |
| `RenderPipelineDescriptor` | `multiview` → `multiview_mask: Option<NonZeroU32>` |
| `RenderPassDescriptor` | 新增必填 `multiview_mask` |
| `ImageCopyTexture` | 改名 `TexelCopyTextureInfo` |
| `SamplerDescriptor.mipmap_filter` | 类型变成 `MipmapFilterMode` |
| `request_device` | 收 `&DeviceDescriptor`，不再需要 `.into()`，也不再传 error callback |
| `get_current_texture()` | 返回 `CurrentSurfaceTexture` 枚举，不是 `Result` |
| `SurfaceTexture::present()` | **没了**，drop 即呈现 |

### 3.5 UI 库没有 `UiState::frame()` 这个方法

文档示例写的是 `state.frame(...)`，实际要用 `Frame::new(&mut state, &mut input, &theme, &nav)`。
另外 `DrawList` 要用 `with_font_system(shared_font_system())` 创建，
不然文字测量和渲染会用两套字体系统，宽度对不上。

### 3.6 借用一个 `&mut self` 的 App 去跑闭包 UI

`Frame::run(&mut list, |ui| self.build_ui(ui))` 直接编译不过：闭包要 `&mut self`，
`list` 又是 `self` 的字段。

解法是把 `draw_list` 存成 `Option<DrawList>` 临时 take 出来，再把要读的字段逐个
绑成局部变量，让编译器看见"这些是不同字段"。最后 UI 里不直接改状态，
只往 `Actions` 结构里记标记，帧末统一 `apply_actions()`。

### 3.7 `textureSample` 不能放在非 uniform 的分支里

预览 shader 第一版是这样写的：

```wgsl
if (uv.x >= 0.0 && uv.x <= 1.0 && uv.y >= 0.0 && uv.y <= 1.0) {
    let t = textureSample(frame_tex, frame_samp, uv);
    ...
}
```

`cargo check` 过、`cargo test` 过、wasm 也构建出来了，但一打开页面整块预览区全黑，
console 里是：

```text
error: 'textureSample' must only be called from uniform control flow
note: control flow depends on possibly non-uniform value
note: builtin 'frag' of 'fs' may be non-uniform
[Invalid ShaderModule "sii preview shader"] is invalid due to a previous error.
[Invalid RenderPipeline "sii preview pipeline"] is invalid due to a previous error.
```

原因：`textureSample` 要隐式求导来选 mip level，WGSL 规范要求它必须处于
uniform control flow。而"这个像素在不在图像内"依赖 `frag` 坐标，天生非 uniform。

**修法**：换成显式指定 level 的 `textureSampleLevel(tex, samp, uv, 0.0)`。
本项目所有纹理都只有一级 mip（`mip_level_count: 1`），采样结果与 `textureSample`
完全一致，但不受控制流约束。顺便把 uv clamp 到 `[0,1]`，用 `select` 决定 alpha：

```wgsl
let inside = uv.x >= 0.0 && uv.x <= 1.0 && uv.y >= 0.0 && uv.y <= 1.0;
let t = textureSampleLevel(frame_tex, frame_samp, clamp(uv, vec2<f32>(0.0), vec2<f32>(1.0)), 0.0);
let a = select(0.0, t.a, inside);
```

**更值得记的是这个坑怎么才不会再犯。** 一开始想用 naga（wgpu 自己的 WGSL 前端）
在 `cargo test` 里校验 shader，结果发现它拦不住 —— naga 30 的 `valid/analyzer.rs` 里有

```rust
const DISABLE_UNIFORMITY_REQ_FOR_FRAGMENT_STAGE: bool = true;
```

主动关掉了 fragment 阶段的 uniformity 检查。拿旧 shader 去跑 naga 的 validator，
一声不吭 —— **浏览器报错的东西，naga 不报**。

最后落成一个源码级 lint 测试：shader 里不允许出现 `textureSample(` /
`textureSampleBias(` / `textureSampleCompare(`，必须写显式 level 的版本。
反向验证过 —— 把旧写法放回去，测试会准确报出第 48 行。

教训：**"编译通过"和"能跑"之间隔着 GPU 驱动那一层。** 纯 CPU 侧的测试覆盖不到
shader 语义，这类错误只能靠规范知识 + 源码级约束兜。

### 3.8 漏调 `UiRenderer::begin_frame` —— 画面只闪一帧

现象：预览区第一帧有内容，然后整个画布冻住不动。没有报错，没有 warning。

原因：`wgpu-gameui` 里有两个同名方法，很容易只调一个就以为齐了。

- `UiState::begin_frame` —— 由 `Frame::run` 内部自动调用，管布局和交互状态。
- `UiRenderer::begin_frame` —— **必须自己每帧调一次**，管 GPU 缓冲区。

`UiRenderer` 用 bump 分配器管理 uniform、顶点、实例这些 per-frame 缓冲区。
`begin_frame` 把游标归零，让每一帧从缓冲区头部重新写。漏掉它，第二帧就在
第一帧的游标位置上继续往后追加，缓冲区一路涨；涨到 `ARENA_SANITY_CAP`
之后渲染器直接拒绝出图 —— 表现就是"闪一下就没了"。

还有一层更隐蔽的：`Queue::write_buffer` 不是立即生效的，它在下次 `submit`
开头才执行。同一次 submit 里的多个 pass 会全部读到**最后一个** pass 写的字节，
除非每个 pass 各自持有一段独立的字节范围。`begin_frame` 就是划分这个边界的。

修法就一行，位置有讲究 —— 必须在本次 `submit` 的第一个 pass 之前：

```rust
let mut encoder = self.device.create_command_encoder(&desc);
self.ui.begin_frame();   // ← 在这里，不是构造函数里，也不是帧末
self.preview.draw(...);
self.ui.render(...);
self.queue.submit(Some(encoder.finish()));
```

教训：**库里有同名方法时，先搞清楚哪个是自动的、哪个要手动调。** 这类
"沉默的 API 契约"不会报错，只会让画面静静地停住。

### 3.9 中文字体：自带字体里没有汉字

现象：UI 里的中文全是豆腐块（□□□），英文数字正常。

原因：`wgpu-gameui` 的 `bundled-font` feature 加载的是 Noto Sans 的
regular / bold / italic / bold-italic 四个字重，**只有拉丁子集，没有汉字字形**。
`register_bundled_fonts` 会把 Noto Sans 设成默认 sans-serif，于是所有文本
都往这个没有汉字的字体上落。

修法：注册一份中文字体并抢过默认 sans-serif 的位置。

```rust
let font_system = shared_font_system();      // 内部已加载 Noto Sans
let handle = load_font_bytes(&font_system, &bytes)?;
font_system.lock().unwrap()
    .db_mut()
    .set_sans_serif_family(handle.family().to_string());
```

用的是**阿里巴巴普惠体 2.0**（55 Regular），8.45 MB，29030 个字形，覆盖
GB2312 / GBK 和 CJK 统一汉字基本区。免费商用，可分发可嵌入。

**为什么运行时 fetch 而不是 `include_bytes!`**：8.45 MB 编进 wasm 会让产物
从 3.4 MB 涨到 12 MB，而且整个 wasm 得先下完才能启动 —— 等于把字体的下载
时间直接叠加到白屏时间上。单独 fetch 的话 wasm 先起来、界面先出现，字体
随后补上。`index.html` 里再加一条 `<link rel="preload" as="fetch">`，让它和
wasm 并行下载，wasm 里那次 fetch 就命中缓存了。

**注意 `FontHandle` 只是族名字符串**。cosmic-text 按族名选字体，所以"加载
字体"和"让 UI 用上它"是两件事：前者是 `load_font_bytes`，后者是
`set_sans_serif_family`。只做前者，中文照样是豆腐块。

### 3.10 输入事件：从「每次事件一次 wasm 调用」到「每帧读一次数组」

最初的做法是每个 DOM 事件注册一个 `Closure`，事件一来就调进 wasm：

```rust
canvas.add_event_listener_with_callback("mousemove", cb.as_ref().unchecked_ref())?;
```

问题不在功能，在量级。1000 Hz 的游戏鼠标一秒能打出上千次 `mousemove`，
每次都是一次跨语言调用：参数封送、栈切换、`RefCell::borrow_mut()`。60 fps
下真正需要的信息只有 60 份，剩下 940 次全是白烧。更糟的是 `borrow_mut()`
撞上正在渲染的帧会 panic —— 高频事件下这是真实存在的风险。

改成「事件写数组、每帧读一次」：

```js
// index.html
const IN = new Float32Array(16);
window.__siiInput = IN;
canvasEl.addEventListener('pointermove', e => {
  const r = canvasEl.getBoundingClientRect();
  IN[0] = e.clientX - r.left;   // x
  IN[1] = e.clientY - r.top;    // y
});
```

```rust
// input.rs
pub fn sync(st: &mut InputState) {
    let v = arr.to_vec();          // 一次 64 字节，拿到一致快照
    st.mouse_x = v[I_X];
    st.mouse_y = v[I_Y];
    // ...
    for i in I_CLICK..=I_DRAGGING { arr.set_index(i, 0.0); }  // 边沿清零
}
```

三个容易踩的点：

1. **`to_vec()` 而不是逐个 `Reflect::get`。** 前者一次读完，后者要 16 次属性
   查找，还可能在读到一半时被事件改掉，拿到的不是一个时刻的状态。
2. **边沿和增量必须读完清零。** `clicked` / `released` / `scroll_delta` 表达的
   是「这一帧发生的事」，不清零下一帧会重复触发 —— 表现是点一下按钮触发两次。
   而 `mouse_down` 是持续状态，不能清。
3. **坐标必须是 CSS 逻辑像素，不能乘 `devicePixelRatio`。** UI 布局用的是逻辑
   像素，喂物理像素进去所有点击都会偏移到右下角。

顺带解决了指针跑出画布的问题：`setPointerCapture(e.pointerId)` 之后指针划出
canvas 也继续收事件，否则松手收不到，`mouse_down` 永远卡在 1，界面像粘住了。

### 3.11 要不要上 OffscreenCanvas：评估过，不做

`OffscreenCanvas` 能把渲染搬到 Worker，主线程彻底解放。对游戏、对可视化大屏
都是好选择，但对这个编辑器不合适：

1. **瓶颈根本不在渲染。** 实测 800×600 / 48 帧的 GIF，渲染一帧不到 2 ms，
   而解码一次要 296 ms。把 2 ms 搬到 Worker，收益可以忽略。
2. **搬过去会让输入多一帧延迟。** UI 布局（`Frame::run` + `build_ui`）依赖
   `InputState` 和文档状态，全在主线程。搬进 Worker 后每次点击都要
   `postMessage` 过去，编辑器的手感会明显变钝 —— 这是倒退。
3. **`wgpu-gameui` 的 `UiRenderer` 不是为跨线程设计的。** 它持有 GPU 资源句柄和
   `FontSystemHandle`（`Arc<Mutex<...>>`），跨 Worker 共享要么复制状态、要么
   全程加锁，复杂度远超收益。
4. **真正该搬的是解码。** `gifdoc::decode` 是纯 CPU 计算，输入是字节、输出是
   像素，没有 UI 依赖，天然适合放进 Worker。要做优化，方向在这里。

结论：OffscreenCanvas 留给「渲染本身变重」的那天（比如加了滤镜链、多了实时
合成层）。现在做，是把复杂度花在了不痛的地方。

### 3.12 音频和 GIF 的时间同步

难点不是"放声音"，是"放声音的同时让画面跟得上，而且跑十分钟不跑偏"。

**时间源只有一个：`AudioContext.currentTime`。**

它是声卡的硬件时钟，和 `performance.now()` 之间会缓慢漂移（晶振误差，几十 ppm
量级）。跑一分钟差几毫秒，跑十分钟就能攒出一帧以上的错位。所以播放期间不能
两边各走各的，必须让画面跟着音频走。

**查帧用绝对时间，不做累加。**

```rust
// 音频文件内的绝对位置 - 截取起点 = GIF 时间轴上的位置
let rel = audio::time_ms() - self.audio_start_ms as f64;
self.frame_idx = doc.frame_at_ms(rel);
```

`frame_at_ms` 是个纯函数：同一个 `t` 永远返回同一帧，内部没有任何状态。这正是
不漂移的关键 —— 主循环里 `play_acc += dt` 的写法，每一帧的舍入误差都会留在
累加器里，越跑越偏；直接查表则误差不累积，第 1 秒和第 10 分钟一样准。

帧延迟用 `max(10)` 兜底，和自由播放一致。GIF 允许写 0 延迟，但真按 0 算会
一瞬间跑完一整轮。`rem_euclid` 而不是 `%`：音频刚起步时 `rel` 可能是负数，
`%` 会返回负值。

**截取区间交给浏览器。**

```js
src.start(0, s / 1000, (e - s) / 1000);   // 立即播、从第 s 秒起、播 e-s 秒
```

让 `AudioBufferSourceNode` 自己截尾，比在 JS 里 `setTimeout` 调 `stop()` 准得多
—— 后者要跟渲染抢主线程，晚个几十毫秒很正常。

**元数据进 DuckDB，同时镜像一份到 localStorage。**

```sql
CREATE OR REPLACE TABLE gif_audio (
    gif_name VARCHAR, audio_name VARCHAR,
    duration_ms INTEGER, start_ms INTEGER, end_ms INTEGER,
    saved_at TIMESTAMP
);
```

DuckDB-WASM 跑的是内存实例，刷新就没了；而用户辛苦调出来的起止点不该丢。
localStorage 那份负责持久化，DuckDB 那份负责可查询（"哪段音频配哪个 GIF"
"平均截取多长"这类问题才是把它放进关系库的理由）。音频二进制本身不存 ——
几 MB 直接撞 localStorage 的 5 MB 配额，只存"怎么配"，文件让用户重选一次。

`end_ms = 0` 表示播到结尾，比存一个等于 `duration_ms` 的绝对值更能表达"没截尾"。

### 3.13 画布尺寸：为什么不能每帧量

症状：拖动窗口改变大小之后，文字和图形糊在一起互相叠着，播放也肉眼可见地变慢。

三处叠加在一起：

**一、rAF 里每帧读 `clientWidth`。**

```rust
let nw = c.client_width().max(1) as f32;   // ← 每帧一次 forced reflow
```

`clientWidth` / `getBoundingClientRect()` 是布局属性。读它们的时候浏览器必须
确保布局是最新的 —— 如果有攒着的样式变更，就当场同步算一遍。一秒六十次，等于
六十次同步布局，每一次都可能打断渲染管线。

**二、rAF 里每帧给 `canvas.width` 赋值。**

```rust
c.set_width((nw * dpr) as u32);            // ← 每帧重置一次绘图缓冲
c.set_height((nh * dpr) as u32);
```

这是最狠的一刀。`canvas.width = x` 会重置绘图缓冲，对 WebGPU canvas 来说就是让
浏览器重建 swapchain。即使新旧值一样，这个 setter 也不保证是空操作 —— 各浏览器
实现不一样，赌不起。

更糟的是时序：`set_width` 在 rAF 回调里、`get_current_texture()` 之前跑。canvas
的 swapchain 在这一刻被换掉了，而 wgpu 手里的 `SurfaceConfiguration` 还是旧的，
这一帧拿到的纹理可能已经作废。画上去的结果就是**上一帧的残留叠着新内容** ——
正是"文字图形重叠"。

**三、两边算尺寸的取整方式不一样。**

```rust
// rAF 里：截断
c.set_width((nw * dpr) as u32);
// sync_surface 里：四舍五入
let w = (vw * scale).round().max(1.0) as u32;
```

`1599.6` 一个得 1599、一个得 1600。只要差一个像素，`sync_surface` 就每帧都判定
"尺寸变了"，每帧 `configure` 一次 —— 又是每帧一次 swapchain 重建。

**修法：尺寸变成"变了才动"的事件，而不是"每帧确认"的轮询。**

JS 侧用 `ResizeObserver`：

```js
const SIZE = new Float32Array(4);   // [cssW, cssH, dpr, gen]
window.__siiSize = SIZE;

function applySize() {
  const r = canvasEl.getBoundingClientRect();
  const pw = Math.max(1, Math.round(r.width  * dpr));
  const ph = Math.max(1, Math.round(r.height * dpr));
  if (canvasEl.width  !== pw) canvasEl.width  = pw;   // ← 变了才碰
  if (canvasEl.height !== ph) canvasEl.height = ph;
  if (SIZE[0] !== r.width || SIZE[1] !== r.height || SIZE[2] !== dpr) {
    SIZE[0] = r.width; SIZE[1] = r.height; SIZE[2] = dpr;
    SIZE[3] += 1;                                     // 版本号
  }
}
new ResizeObserver(applySize).observe(canvasEl);
window.addEventListener('resize', applySize);
```

wasm 侧（`src/viewport.rs`）每帧只读这四个数，一个 DOM 查询都不做：

```rust
if let Some(s) = viewport::read() {
    self.size = s;
}
if self.sync_surface() {   // 尺寸变了就重配 surface，返回 true
    return;                // ← 这一帧丢掉
}
```

**为什么必须丢掉一帧**：从 `configure` 到第一张新纹理能稳定拿到之间有一次交接。
这期间画上去的内容可能落在已经被丢弃的 buffer 上。16 ms 的静止画面换绝不花屏，
划算。

**顺带修掉的**：`physical()` 现在只有一处实现，`config.width/height` 和传给
`UiRenderer::render` 的尺寸都从它来，再也不可能对不上。

还有个隐性的坑：原来 UI 布局对 `vw/vh` 做了 `max(320.0)` / `max(240.0)` 兜底，
但 `config` 用的是没兜底的值。窗口小于 320 时两者不一致，`rect_clip` 算出来会
超出屏幕。现在不兜底了，小窗口由 `pw/ph` 各自兜底。

### 3.14 resize 叠影的真正原因：`DrawList` 没有每帧清空

3.13 那套 swapchain / ResizeObserver 的修法是必要的，但它**不是叠影的原因**。
修完 resize 依然叠影，说明问题不在交换链，而在 UI 几何本身。

根因只有一行：wgpu-gameui 的 `Frame::run` / `UiContext` **只往 `DrawList` 里追加
几何，不会替你清空**。而这个列表是跨帧复用的 —— 字形光栅化和文本测量缓存都挂在
它上面，每帧重建就白丢了。于是每帧都在上一帧的几何后面再追加一整份 UI：

- 界面静止时看不出来：同一批图形画在同一个位置，叠多少层都是同一个像素；
- 一旦 resize、布局变了，旧尺寸的 36 个文本块和 104 个矩形还留在列表里，和新
  布局的一起被渲染 —— 屏幕上就是旧文字、旧图形和新的一起重影；
- 列表只涨不缩，拖得越久每帧要画的几何越多，还会越来越卡。

**修法**：`Frame::run` 之前调一次 `list.clear()`。`clear()` 只清几何，保留缓存和
列表的 identity，正是渲染器文档里要求的用法。

**实测证据**（临时把列表各缓冲的长度打到 `window`，headless Chrome 里读）：

| 构建 | 帧号 → 文本块 / chrome 矩形 |
|---|---|
| 漏 `clear()` | 81 → 2916 / 8424；116 → 4176 / 12064（每帧 +36 / +104） |
| 有 `clear()` | 90 → 36 / 104；150 → 36 / 104；resize 后 167/227 → 36 / 104 |

漏清时每帧精确增加 36 个文本块、104 个 chrome 矩形，和单帧 UI 的几何量完全一致；
resize 后继续累加。加上 `clear()` 后，无论跑多少帧、无论 1280×720 → 900×600
resize，列表长度都恒定。

> 顺带记一个环境坑：headless Chrome + SwiftShader 下 WebGPU 画布既不能用
> `Page.captureScreenshot` 也不能用 `canvas.toDataURL()` 取到内容（一个最小
> WebGPU 红色清屏页面同样只能截到黑图）。所以这里用几何计数验证，而不是截图。

## 4. 实测数据

Node 22 里加载真实 wasm（不是 mock），`performance.now()` 在外部计时：

| 样本 | 文件 | 解码后内存 | 解码 | 重编码 (speed=10) |
|---|---|---|---|---|
| 96×96 / 24 帧 | 35 KB | 0.8 MB | 3.7 ms | 26 ms |
| 800×600 / 48 帧 | 2.9 MB | **87.9 MB** | **296 ms** | **1483 ms** |

按这个比例外推：

| 目标 | 解码后内存 | 判断 |
|---|---|---|
| 480×360 / 30 帧 | 20.7 MB | 轻松 |
| 800×600 / 48 帧 | 87.9 MB | 实测可行 |
| 1280×720 / 60 帧 | 221 MB | 可行，但要小心 |
| 1920×1080 / 100 帧 | **829 MB** | 顶到天花板，必须换架构 |

导出拆成两段之后（`verify.mjs` 实测）：

| 阶段 | 96×96 / 24 帧 | 800×600 / 48 帧 |
|---|---|---|
| 主线程拍快照（含解码） | 11.8 ms | 514.8 ms |
| └ 其中纯快照拷贝 | ≈0 ms | **≈218 ms** |
| Worker 编码 | 20.5 ms | 1481 ms |
| **主线程实际阻塞** | **≈0 ms** | **≈218 ms** |

`cargo test` 有 12 个单元测试，跑在原生环境不需要浏览器，覆盖：
局部帧合成、disposal 语义、往返编解码、增删/翻转/裁剪/缩放、单帧保护、抽帧预算、
快照往返、快照截断拒绝、"绕一圈快照再编码字节必须一致"，以及两项 shader 检查
（naga 解析校验 + 禁止隐式 LOD 采样，见 3.7）。

## 5. 三个关键判断

### 5.1 WebGPU 不是瓶颈，别把它当卖点

一次 draw call 画棋盘格 + 图像，纹理就是 GIF 原始分辨率，缩放靠采样器。
GPU 侧的开销可以忽略。**"只使用 WebGPU 加速"这件事本身带来的收益很小**，
真正的加速来自"用 Rust/wasm 做解码"而不是 JS 逐像素循环。

要说 WebGPU 的价值，在于：能和 UI 共用同一个 device/queue、零拷贝上屏、
以及为将来的实时滤镜/合成留了位置。而不是"算得快"。

### 5.2 内存是唯一的硬约束

`宽 × 高 × 4 × 帧数`，没有商量余地。要突破，只有三条路：

1. **按需解码 + LRU**：只保留当前帧 ±N 帧，其余丢索引回原文件。需要 GIF 支持随机访问
   （`gif` crate 的 `DecodeOptions` 可以跳帧，但要自己重放 disposal 链）。
2. **缩略图 + 全尺寸按需**：时间轴上用 64px 缩略图（48 帧只要 786 KB），
   预览时才解全尺寸。
3. **分块（tile）**：把画布切成 256×256 块，只解码视口内的块。复杂度最高，收益最大。

现在的实现是"全解码 + 抽帧兜底"，属于能演示但不能上生产。

### 5.3 重编码必须进 Worker —— 已实现

NeuQuant 量化是纯 CPU 的，800×600 × 48 帧要 1483 ms。放在主线程会直接冻住 rAF，
用户看到的就是一秒半白屏。

**难点不在 Worker 本身，在怎么把像素送过去。** `Doc` 的帧数据躺在 wasm 线性内存里，
Worker 有自己的 wasm 实例，两边不共享地址空间。三条路：

| 方案 | 传输量 | 主线程开销 | 结论 |
|---|---|---|---|
| 原始 GIF 字节 + 编辑操作序列 | 2.8 MB | ≈0 | 最优，但要先建 op 历史栈 |
| `Doc` 帧快照 | 88 MB | 218 ms | **已实现**，改动最小 |
| 序列化成 JSON | 120 MB+ | 慢到没法看 | 排除 |

选了快照，因为不用动 `Doc` 的存储方式，也不用先做 undo 历史。

快照格式是定长头 + 连续像素，不含任何字段名：

```text
[0..4]    b"SII1"
[4..6]    width      u16 LE
[6..8]    height     u16 LE
[8..12]   frame_cnt  u32 LE
[12..]    delays     frame_cnt × u16 LE
[..]      pixels     frame_cnt × width × height × 4
```

两个细节值得记一笔：

**第一，别让 wasm-bindgen 拷两遍。** 如果 `snapshot()` 返回 `Vec<u8>`，
wasm-bindgen 会先在 Rust 侧建 Vec（一次 88 MB 拷贝），再 `.slice()` 到 JS 堆
（又一次 88 MB）。改成直接 `Uint8Array::new_with_length()` 分配 JS 堆、
逐帧 `copy_from`，就只剩一次跨边界拷贝。

**第二，transfer 而不是 clone。** `postMessage({snapshot}, [snapshot.buffer])`
把整个 ArrayBuffer 的所有权移交给 Worker，主线程这边立刻 detach，零拷贝。

Worker 侧收到后 `from_snapshot()` 还原成 `Doc`，调同一个 `encode()`。
**产出的 GIF 与主线程直接编码逐字节相同**——这条有单元测试
（`snapshot_encode_matches_direct_encode`）和 Node 双向比对守着，
不是"看起来一样"。

剩下的 218 ms 是 88 MB 从 wasm 内存搬到 JS 堆的 memcpy，约 400 MB/s。
要消掉它只有一条路：把 `Doc` 的像素从"每帧一个 `Vec<u8>`"改成一块连续 buffer，
快照就能单次 `copy_from`。那是架构级改动，先记在下一步里。

## 6. DuckDB 集成：能做，但要分层

### 做了什么

`src/duck.rs` 把每帧的元数据算出来写进 DuckDB-WASM 的 `gif_frames` 表：

```
idx / delay_cs / delay_ms / frame_bytes / mean_r / mean_g / mean_b / alpha_pct / content_hash
```

然后跑一条聚合把结果带回状态栏：

```sql
SELECT count(*) AS frames,
       sum(delay_ms) AS total_ms,
       round(avg(delay_ms),1) AS avg_delay_ms,
       max(delay_ms) AS max_delay_ms,
       count(*) FILTER (WHERE delay_ms >= 200) AS long_holds,
       count(DISTINCT content_hash) AS unique_frames
FROM gif_frames
```

### 为什么这么分层

87.9 MB 的像素进关系库，DuckDB 会先做列式压缩，然后你为了显示又得读回来 ——
一进一出，延迟翻倍，内存翻倍，换来的只是"能在 SQL 里看到像素值"，没有任何分析价值。

而元数据表小到可以全量常驻（几百行），却回答了真正的问题：

- 节奏：哪几帧停顿超过 200 ms（GIF 的"表演"全在延迟里）
- 冗余：`count(DISTINCT content_hash)` 和总帧数的差就是可去重的帧
- 色调：`mean_r/g/b` 聚类能看出整份动图的色彩走向，这对调色板优化直接有用
- 定位：改完之后按 `idx` 反查，指哪打哪

### 成本

DuckDB-WASM 从 CDN 拉大约 30 MB（`duckdb-mvp.wasm` + worker）。
所以做成**懒加载**：不点"索引到 DuckDB"按钮，一个字节都不下载。

## 7. 接大模型的路径

这条路比想象中短，而且不需要模型理解 GIF 格式：

1. **喂元数据，不喂像素。** DuckDB 那张表 JSON 化之后只有几十行，
   模型能完整理解整份动图的节奏和结构。
2. **模型输出结构化编辑指令。** 例如
   ```json
   {"op": "set_delay", "frames": [3,4,5], "cs": 50}
   {"op": "crop", "x": 120, "y": 0, "w": 560, "h": 600}
   ```
   这些正好一一对应 `gifdoc.rs` 里已有的函数，加一层 JSON → 编辑操作的映射就能跑。
   **这是最短路径，一两天的工作量。**
3. **真要生成新画面，走图生图。** 把某一帧编码成 PNG 丢给多模态模型，
   拿回来贴进 `Doc.frames`。模型只要会改单张图就行。
4. **边界要干净：wasm 里不放密钥、不发网络请求。** 调模型回 JS 侧 `fetch`，
   Rust 只做"指令 → 编辑操作"的翻译。这样 wasm 模块本身还是纯离线的。

## 8. 下一步该做什么（按性价比排序）

1. ~~导出丢进 Web Worker~~ ✅ 已完成，主线程阻塞从 1483 ms 降到 218 ms
2. **把 `Doc` 像素改成连续 buffer**（消掉剩下 218 ms，同时让编辑操作少一层间接，两三天）
3. **undo/redo 历史栈**（现在编辑不可逆，属于可用性硬伤，一天）
4. **缩略图时间轴 + 按需解码**（解决内存，两三天）
5. **JSON 编辑指令层**（为接模型铺路，同时让"传 op 而不是传像素"成为可能，一天）
6. **tile 分块解码**（真·高清长 GIF 的唯一解，一周以上）

## License

MIT
