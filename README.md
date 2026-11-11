# sii

**纯前端、WebGPU 加速的 GIF 观看与编辑器。没有后端。**

打开一个高清 GIF，逐帧看，改延迟、裁剪缩放、翻转调色，配一段音频对齐节奏，
再导出成新的 GIF ——
解码、合成、编辑、编码全在 WebAssembly 里完成，上屏走 WebGPU。

UI 用 [`wgpu-gameui`](https://github.com/voipgroup1/wgpu-gameui) 手写即时模式界面，
渲染用 `wgpu 30` 的 `webgpu` 后端，两者共用同一个 `wgpu::Device` / `wgpu::Queue`。

---

## 它到底做了什么

GIF 播放器最容易做错的地方是**帧合成**。GIF 的每一帧可能只是画布上的一个局部矩形，
并且带一个 `disposal` 指令决定这帧显示完之后那块区域怎么处理：

| disposal | 含义 | 处理 |
|---|---|---|
| `Any` / `Keep` | 保留 | 什么都不做 |
| `Background` | 清成背景 | 把该矩形清成透明 |
| `Previous` | 回滚 | 整幅画布还原到本帧绘制之前的状态 |

`sii` 在解码时就把所有帧按这套规则合成成**完整画布**（`src/gifdoc.rs`），
所以后面所有编辑操作面对的都是"一整张图"，不用再操心局部矩形和透明叠加。

代价是内存：一帧 = `宽 × 高 × 4` 字节。800×600 的 48 帧就是 **87.9 MB**。
这是这个方案最硬的约束，后面会讲怎么绕。

## 架构

```
┌───────────────────────────── 浏览器 ─────────────────────────────┐
│                                                                  │
│  主线程                                                           │
│    index.html                                                    │
│      ├── File API 读字节  ─────────────► wasm: load_gif()         │
│      ├── 帧快照 ──transfer──► Worker    wasm: snapshot()          │
│      ├── 指针事件 → Float32Array ──────► wasm: input::sync()      │
│      ├── AudioContext ────────────────► wasm: audio::time_ms()   │
│      └── DuckDB-WASM (CDN, 懒加载) ◄─── wasm: duck::index_doc()   │
│                                                                  │
│    wasm (src/)                                                   │
│      ├── gifdoc.rs  解码 / 合成 / 编辑 / 编码   ← 不依赖 GPU      │
│      ├── canvas.rs  WebGPU 预览管线（纹理 + 棋盘格 + 采样器）      │
│      ├── duck.rs    帧元数据 / 音频对齐 → DuckDB 表                │
│      ├── input.rs   JS 事件数组 → wgpu-gameui InputState          │
│      ├── audio.rs   播放控制 + 声卡时钟                           │
│      └── lib.rs     App / rAF 主循环 / 界面                        │
│                                                                  │
│  ┌──────────── Worker 线程（worker.js）────────────┐              │
│  │  收快照 → wasm: encode_snapshot() → 回传字节    │              │
│  │  NeuQuant 量化 + GIF 编码，1.5 秒全在这儿跑     │              │
│  └────────────────────────────────────────────────┘              │
│                                                                  │
│  wgpu (WebGPU)  ── 一次 draw call：棋盘格 + 当前帧               │
│  wgpu-gameui    ── 工具栏 / 帧列表 / 时间轴 / 参数面板            │
└──────────────────────────────────────────────────────────────────┘
```

关键设计：

- **预览和 UI 是两条独立管线，同一个 render pass 序列。** 预览先画（`LoadOp::Clear`），
  UI 后画（`LoadOp::Load` 叠在上面）。这样大纹理不会污染 UI 的图集，UI 也不会挡住图像。
- **纹理尺寸 = GIF 原始分辨率，不做缩放。** 缩放交给采样器和 uniform 里的变换，
  放大缩小都是零成本。
- **棋盘格在 fragment shader 里算。** 透明区域用两档深灰交错显示，不需要额外纹理。
- **只在换帧或文档变更时上传纹理。** 播放时每帧一次 `queue.write_texture`，
  静止时一次都不传。
- **导出走 Worker，主线程只负责拍快照。** 帧像素压成一条定长头部的字节流
  （`SII1` + 宽高帧数 + 延迟表 + 连续 RGBA），`postMessage` 时 transfer 出去，
  Worker 里还原成 `Doc` 再编码。主线程不再被 1.5 秒的 NeuQuant 冻住。
- **指针事件不跨 wasm 边界。** 所有 `pointermove` / `wheel` 只往一个
  `Float32Array` 里写数，wasm 每帧读一次快照。1000 Hz 的游戏鼠标一秒上千次事件，
  跨语言调用从 O(事件数) 降到 O(帧数)，也顺带避开了高频回调撞上渲染帧时的借用冲突。
- **音频播放时，声卡时钟是唯一时间源。** `AudioContext.currentTime` 是硬件时钟，
  和 `performance.now()` 会缓慢漂移；画面按绝对时间查帧表，不做累加，误差不累积。

## 跑起来

```bash
# 依赖：rustup + wasm32 目标 + 与 Cargo.lock 版本一致的 wasm-bindgen-cli
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.130   # 或者下预编译包，快很多

./build.sh
python3 -m http.server 8080
# 打开 http://localhost:8080
```

**需要 WebGPU**：Chrome / Edge 113+ 桌面版开箱可用；Safari 18+ 要手动打开开关；
Firefox 还在 nightly 后面。`navigator.gpu` 不存在时页面会直接告诉你。

仓库里带了构建好的 `pkg/`，所以也可以不装 Rust，直接起个静态服务器就能试。

### 生成测试素材

```bash
cargo run --bin mksample -- /tmp/sample.gif          # 96×96 / 24 帧
cargo run --bin mksample -- /tmp/hd.gif 800 600 48   # 800×600 / 48 帧
```

### 快捷键

`空格` 播放/暂停（有音频时走音频时钟） · `,` `.` 上一帧/下一帧 · `-` `=` 缩放 · `0` 适应窗口 · `Home` `End` 首/末帧

## 实测数据

在 1 核 / 1.9 GB 的 Linux 沙箱里编译，Node 22 里跑真实 wasm（`wasm32-unknown-unknown`
release，`opt-level="s"` + thin LTO）：

| 样本 | 文件 | 解码后内存 | 解码 | 重编码 |
|---|---|---|---|---|
| 96×96 / 24 帧 | 35 KB | 0.8 MB | 3.7 ms | 26 ms |
| 800×600 / 48 帧 | 2.9 MB | **87.9 MB** | **296 ms** | **1483 ms** |

导出拆成两段之后（`verify.mjs` 实测，Node 22）：

| 阶段 | 96×96 / 24 帧 | 800×600 / 48 帧 |
|---|---|---|
| 主线程拍快照（含解码） | 11.8 ms | 514.8 ms |
| └ 其中纯快照拷贝 | ≈0 ms | **≈218 ms** |
| Worker 编码 | 20.5 ms | 1481 ms |
| **主线程阻塞** | **≈0 ms** | **≈218 ms** |

也就是说点下"导出"后主线程只停顿两百来毫秒，剩下的时间页面照常播放、照常拖动帧。
快照路径和直接编码路径产出的 GIF **逐字节相同**，这一点有单元测试和 Node 双向比对守着。

产物大小：3.5 MB wasm（wgpu + naga 占大头）。

`cargo test` 有 12 个单元测试，覆盖解码合成、往返编解码、编辑操作、快照往返、
快照截断拒绝，以及"绕一圈快照再编码字节必须一致"。全部跑在原生环境，不需要浏览器。

## DuckDB 怎么进来的

**帧本体不进 DuckDB，帧元数据进。**

把 87.9 MB 的像素塞进关系库既慢又没意义。真正值得用 SQL 查的是每帧的
`idx / delay_cs / delay_ms / frame_bytes / mean_rgb / alpha_pct / content_hash`。
这张表进去之后：

```sql
-- 哪些帧停顿超过 200ms（节奏分析）
SELECT idx, delay_ms FROM gif_frames WHERE delay_ms >= 200 ORDER BY idx;

-- 有没有和前一帧一模一样的重复帧（可以去重瘦身）
SELECT count(DISTINCT content_hash) AS unique_frames, count(*) AS total FROM gif_frames;

-- 按平均色粗聚类，看整份 GIF 的色调分布
SELECT round(mean_r/32)*32 AS r_bucket, count(*) FROM gif_frames GROUP BY 1 ORDER BY 2 DESC;
```

`index_doc()` 会建表、分批 INSERT，最后跑一条聚合把 `帧数 / 总时长 / 平均延迟 /
最长停顿 / 长停顿帧数 / 唯一帧数` 带回来显示在状态栏。

DuckDB-WASM 走 CDN 懒加载，**不点按钮就不会下载那 30 MB**。

## 音频：和画面严丝合缝

可以给 GIF 配一段音频，只取其中 `[start_ms, end_ms)` 这一段来播。播放时画面跟着
音频走 —— 不是"大概对齐"，是每一帧都按音频的绝对播放位置查表。

三件事决定了它准：

1. **时间源用 `AudioContext.currentTime`。** 那是声卡的硬件时钟，和
   `performance.now()` 之间会缓慢漂移（晶振误差，几十 ppm 量级）。跑十分钟能攒出
   一帧以上的错位，所以播放期间不能让两边各走各的。
2. **查帧是纯函数，没有累加状态。** `Doc::frame_at_ms(t)` 里同一个 `t` 永远返回同一帧。
   `play_acc += dt` 那种写法每一帧的舍入误差都会留在累加器里，越跑越偏。
3. **截取交给浏览器。** `AudioBufferSourceNode.start(0, offset, duration)` 由音频线程
   自己截尾，比在 JS 里 `setTimeout` 调 `stop()` 准得多。

对齐参数（音频文件名、总长、起止毫秒）存进 DuckDB：

```sql
CREATE TABLE IF NOT EXISTS gif_audio (
    gif_name VARCHAR, audio_name VARCHAR,
    duration_ms INTEGER, start_ms INTEGER, end_ms INTEGER,
    saved_at TIMESTAMP
);
```

`end_ms = 0` 表示播到结尾。同一个 GIF + 同一段音频只保留最后一行 —— 这张表记的是
"当前的对齐方案"，不是操作日志。

DuckDB-WASM 跑的是内存实例，刷新就没了，所以同一份参数还会镜像到 localStorage。
音频二进制本身不存：几 MB 直接撞 localStorage 的 5 MB 配额，只存"怎么配"，
文件让用户重选一次。

## 关于 OffscreenCanvas

评估过，没做。渲染一帧不到 2 ms，而解码一次要 296 ms —— 把渲染搬进 Worker 的收益
可以忽略，反而会让输入多一帧延迟（UI 布局和文档状态都在主线程，每次点击都要
`postMessage` 过去）。真正该搬的是 `gifdoc::decode`：纯 CPU、输入字节输出像素、
没有 UI 依赖，天然适合 Worker。要优化就往那个方向走。

## 已知边界

- **内存是主要瓶颈。** 全画布 RGBA 解码在 1080p / 100 帧的量级会摸到 wasm32 的 4 GB 天花板。
  真要做"高清长 GIF"，需要改成按需解码 + 只保留缩略图 + LRU 淘汰帧缓存。
  现在的"抽帧到内存预算"是个粗暴兜底，不是正解。
- **拍快照还有 200 ms 的停顿。** 88 MB 像素从 wasm 内存搬到 JS 堆，
  实测约 400 MB/s。要彻底消掉，得把 `Doc` 的像素从"每帧一个 `Vec`"改成
  一块连续的 `Vec<u8>`，让快照变成单次 `copy_from`。属于架构级改动，还没做。
- **Worker 里也加载了整份 3.5 MB 的 wasm**（含 wgpu），但一个 draw call 都不用。
  把 `gifdoc` 拆成独立 crate 能给 Worker 单独编一份 ~250 KB 的产物，
  首次导出能再快 200 ms 左右。
- **编辑操作会重建全部帧的像素**（翻转、调色、缩放都是全量遍历）。
  对 87.9 MB 的文档，一次操作就是几百毫秒，目前是同步做的。
- 还没有 undo/redo。编辑是直接改 `Doc`，没有历史栈。

## 下一步：大模型生成和修改 GIF

这条路是通的，而且比想象中短：

1. **元数据先给模型看。** DuckDB 里那张表就是模型的"索引"，几十行 JSON 就能让它理解
   整份动图的节奏和结构，不用喂像素。
2. **让模型输出结构化指令，而不是像素。** 比如
   `{"op":"set_delay","frames":[3,4,5],"cs":50}` 或
   `{"op":"crop","x":120,"y":0,"w":560,"h":600}` —— 这些正好对应 `gifdoc.rs` 里已有的函数，
   加一层 JSON → 编辑操作的映射就能跑。
3. **真要生成新帧，走图生图。** 把某一帧的 RGBA 编码成 PNG，交给多模态模型改，
   拿回来贴进 `Doc.frames`。不需要模型理解 GIF 格式，它只要会改单张图。
4. **wasm 里没有网络和密钥。** 调模型必须回到 JS 侧（`fetch`），
   Rust 只负责"把指令翻译成编辑操作"。这个边界要保持干净。

## License

MIT
