// =====================================================================
//  sii 导出 Worker
//
//  只干一件事：把主线程拍好的帧快照重新编码成 GIF 字节。
//  重编码里最贵的是 NeuQuant 量化（800×600×48 帧实测 1.5 秒），
//  放在这里跑，主线程的渲染和交互一秒都不会停。
//
//  刻意不碰 GPU：wgpu 的 wasm 在 Worker 里能正常加载，
//  但导出这条路一个 draw call 都不需要，没必要去初始化 adapter。
// =====================================================================

import init, { encode_snapshot } from './pkg/sii.js';

// 整个 Worker 生命周期只实例化一次 wasm，后续导出复用。
const ready = init().then(() => {
  self.postMessage({ phase: 'ready' });
});

self.onmessage = async (e) => {
  const { id, type, snapshot, speed } = e.data || {};
  if (type !== 'encode') return;

  try {
    await ready;
    const bytes = encode_snapshot(snapshot, speed | 0);
    // transfer 出去，避免把几 MB 的结果再拷一遍
    self.postMessage({ id, ok: true, bytes }, [bytes.buffer]);
  } catch (err) {
    self.postMessage({
      id,
      ok: false,
      error: String(err?.message ?? err ?? '未知错误'),
    });
  }
};
