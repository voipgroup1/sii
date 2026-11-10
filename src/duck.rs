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
