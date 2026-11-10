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

`cargo test` 有 7 个单元测试，跑在原生环境不需要浏览器，覆盖：
局部帧合成、disposal 语义、往返编解码、增删/翻转/裁剪/缩放、单帧保护、抽帧预算。

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

### 5.3 重编码慢到必须进 Worker

NeuQuant 量化是纯 CPU 的，800×600 × 48 帧要 1483 ms。这个量级如果放在主线程，
会直接卡住 rAF，用户看到的是一秒半的白屏。

正确做法：导出时把 `Doc` 的帧数据 `postMessage` 给 Web Worker（transferable），
Worker 里跑 wasm 的 `encode()`，进度用消息回传。

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

1. 导出丢进 Web Worker（解决 1.5 秒卡顿，半天）
2. 缩略图时间轴 + 按需解码（解决内存，两三天）
3. undo/redo 历史栈（现在编辑不可逆，属于可用性硬伤，一天）
4. JSON 编辑指令层（为接模型铺路，一天）
5. tile 分块解码（真·高清长 GIF 的唯一解，一周以上）

## License

MIT
