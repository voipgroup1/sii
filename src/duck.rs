//! DuckDB-WASM 集成。
//!
//! 思路：**帧本体不上 DuckDB**。高清 GIF 的像素数据动辄几百 MB，塞进关系库既慢又
//! 没意义；真正值得用 SQL 查的是「帧元数据」——索引、延迟、尺寸、平均色、透明占比、
//! 内容哈希。这些数据放进去之后，"找出所有停顿超过 200ms 的帧""找出和前一帧几乎
//! 一样的重复帧""按平均色聚类做调色板分析"就都是一句 SQL 的事。
//!
//! 好处是将来接大模型很自然：模型只需要看到一张紧凑的表，而不是几千帧像素。

use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

use crate::gifdoc::Doc;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__sii", "duck"], js_name = "init")]
    fn duck_init_js() -> js_sys::Promise;

    #[wasm_bindgen(js_namespace = ["window", "__sii", "duck"], js_name = "exec")]
    fn duck_exec_js(sql: &str) -> js_sys::Promise;
}

async fn call(p: js_sys::Promise) -> Result<String, String> {
    match JsFuture::from(p).await {
        Ok(v) => Ok(v.as_string().unwrap_or_default()),
        Err(e) => Err(format!(
            "{}",
            e.dyn_ref::<js_sys::Error>()
                .map(|x| String::from(x.message()))
                .or_else(|| e.as_string())
                .unwrap_or_else(|| "未知错误".into())
        )),
    }
}

/// 初始化 DuckDB（首次会从 CDN 拉 wasm，之后走缓存）
pub async fn init() -> Result<String, String> {
    call(duck_init_js()).await
}

/// 每帧的元数据快照
struct FrameMeta {
    idx: usize,
    delay_cs: u16,
    bytes: usize,
    mean: (f32, f32, f32),
    alpha_pct: f32,
    hash: u64,
}

fn analyze(doc: &Doc) -> Vec<FrameMeta> {
    let total = doc.width as usize * doc.height as usize;
    // 采样步长：大图别一个像素一个像素地扫
    let step = ((total / 4096).max(1)) as usize;

    doc.frames
        .iter()
        .enumerate()
        .map(|(idx, f)| {
            let mut sr = 0u64;
            let mut sg = 0u64;
            let mut sb = 0u64;
            let mut opaque = 0u64;
            let mut n = 0u64;
            let mut hash = 0xcbf29ce484222325u64; // FNV-1a 基线

            for (pi, px) in f.rgba.chunks_exact(4).enumerate() {
                // 哈希要覆盖全量，不然重复帧检测会误判
                hash ^= px[0] as u64;
                hash = hash.wrapping_mul(0x100000001b3);
                hash ^= px[1] as u64;
                hash = hash.wrapping_mul(0x100000001b3);
                hash ^= px[2] as u64;
                hash = hash.wrapping_mul(0x100000001b3);

                if pi % step == 0 {
                    sr += px[0] as u64;
                    sg += px[1] as u64;
                    sb += px[2] as u64;
                    if px[3] > 8 {
                        opaque += 1;
                    }
                    n += 1;
                }
            }
            let n = n.max(1);
            FrameMeta {
                idx,
                delay_cs: f.delay_cs,
                bytes: f.rgba.len(),
                mean: (
                    sr as f32 / n as f32,
                    sg as f32 / n as f32,
                    sb as f32 / n as f32,
                ),
                alpha_pct: opaque as f32 * 100.0 / n as f32,
                hash,
            }
        })
        .collect()
}

/// 把整份文档的帧元数据写进 DuckDB，然后跑一条聚合查询把结果带回来。
pub async fn index_doc(doc: &Doc) -> Result<String, String> {
    init().await?;

    let metas = analyze(doc);

    let ddl = "CREATE OR REPLACE TABLE gif_frames (
        idx INTEGER,
        delay_cs INTEGER,
        delay_ms INTEGER,
        frame_bytes BIGINT,
        mean_r DOUBLE,
        mean_g DOUBLE,
        mean_b DOUBLE,
        alpha_pct DOUBLE,
        content_hash BIGINT
    )";
    call(duck_exec_js(ddl)).await?;

    // 分批 INSERT，单条语句别太长
    const CHUNK: usize = 200;
    for chunk in metas.chunks(CHUNK) {
        let mut sql = String::from(
            "INSERT INTO gif_frames VALUES ",
        );
        for (i, m) in chunk.iter().enumerate() {
            if i > 0 {
                sql.push(',');
            }
            sql.push_str(&format!(
                "({},{},{},{},{:.3},{:.3},{:.3},{:.3},{})",
                m.idx,
                m.delay_cs,
                m.delay_cs as u32 * 10,
                m.bytes,
                m.mean.0,
                m.mean.1,
                m.mean.2,
                m.alpha_pct,
                m.hash as i64
            ));
        }
        call(duck_exec_js(&sql)).await?;
    }

    // 顺手做几件"用 SQL 才划算"的事
    let report = call(duck_exec_js(
        "SELECT
            count(*) AS frames,
            sum(delay_ms) AS total_ms,
            round(avg(delay_ms), 1) AS avg_delay_ms,
            max(delay_ms) AS max_delay_ms,
            count(*) FILTER (WHERE delay_ms >= 200) AS long_holds,
            count(DISTINCT content_hash) AS unique_frames
         FROM gif_frames",
    ))
    .await?;

    Ok(format!("DuckDB 已索引 {} 帧 → {report}", metas.len()))
}

// ---------------------------------------------------------------- 音频元数据

//  localStorage 的四个键。用四个独立键而不是一个 JSON，是因为 wasm 侧没有
//  serde，手写 JSON 解析比多调四次 localStorage 贵得多，还容易出错。
const LS_NAME: &str = "sii.audio.name";
const LS_DUR: &str = "sii.audio.dur";
const LS_START: &str = "sii.audio.start";
const LS_END: &str = "sii.audio.end";

fn ls() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

/// SQL 字符串字面量里的单引号要写两遍。文件名带 `'` 完全可能
/// （"It's a gif.gif"），不转义就是一条语法错误。
fn esc(s: &str) -> String {
    s.replace('\'', "''")
}

fn save_audio_local(name: &str, dur: u32, start: u32, end: u32) {
    let Some(s) = ls() else { return };
    let _ = s.set_item(LS_NAME, name);
    let _ = s.set_item(LS_DUR, &dur.to_string());
    let _ = s.set_item(LS_START, &start.to_string());
    let _ = s.set_item(LS_END, &end.to_string());
}

/// 读回上次存下的音频对齐参数：`(文件名, 总时长 ms, 起始 ms, 结束 ms)`。
pub fn load_audio_local() -> Option<(String, u32, u32, u32)> {
    let s = ls()?;
    let name = s.get_item(LS_NAME).ok().flatten()?;
    let num = |k: &str| -> u32 {
        s.get_item(k)
            .ok()
            .flatten()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    Some((name, num(LS_DUR), num(LS_START), num(LS_END)))
}

pub fn clear_audio_local() {
    let Some(s) = ls() else { return };
    for k in [LS_NAME, LS_DUR, LS_START, LS_END] {
        let _ = s.remove_item(k);
    }
}

/// 把音频对齐参数写进 DuckDB，同时镜像一份到 localStorage。
///
/// 为什么两边都要写：DuckDB-WASM 跑的是内存实例，刷新就没了，而用户辛苦调出来的
/// 起止点不该丢；localStorage 则没法用 SQL 查（"哪段音频配哪个 GIF""平均截取多长"
/// 这类问题才是把它放进关系库的理由）。先写 localStorage —— 那一步不依赖网络，
/// DuckDB 的 wasm 还没下完也不会丢数据。
pub async fn save_audio_meta(
    gif_name: &str,
    audio_name: &str,
    duration_ms: u32,
    start_ms: u32,
    end_ms: u32,
) -> Result<String, String> {
    save_audio_local(audio_name, duration_ms, start_ms, end_ms);

    init().await?;

    let ddl = "CREATE TABLE IF NOT EXISTS gif_audio (
        gif_name VARCHAR,
        audio_name VARCHAR,
        duration_ms INTEGER,
        start_ms INTEGER,
        end_ms INTEGER,
        saved_at TIMESTAMP
    )";
    call(duck_exec_js(ddl)).await?;

    // 同一个 GIF 配同一段音频只保留最后一行。这张表记的是"当前的对齐方案"，
    // 不是操作日志 —— 不删的话调十次起止点就留十行垃圾，查起来还得自己挑最新。
    let del = format!(
        "DELETE FROM gif_audio WHERE gif_name='{}' AND audio_name='{}'",
        esc(gif_name),
        esc(audio_name)
    );
    call(duck_exec_js(&del)).await?;

    let sql = format!(
        "INSERT INTO gif_audio VALUES ('{}','{}',{},{},{},now())",
        esc(gif_name),
        esc(audio_name),
        duration_ms,
        start_ms,
        end_ms
    );
    call(duck_exec_js(&sql)).await?;

    let end_label = if end_ms == 0 { duration_ms } else { end_ms };
    Ok(format!(
        "音频对齐已入库：{audio_name} {}–{} ms（共 {} ms）",
        start_ms, end_label, duration_ms
    ))
}
