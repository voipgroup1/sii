// Node 侧验证：快照往返 + Worker 编码路径是否与主线程编码完全一致。
// 不碰 GPU，也不需要浏览器 —— 只验证"跨线程传过去的东西没变形"。
import init, {
  transcode,
  snapshot_of,
  encode_snapshot,
} from './pkg/sii.js';
import fs from 'node:fs';

// Node 里没有 file:// 的 fetch，直接把 wasm 字节喂进去
await init({ module_or_path: fs.readFileSync('./pkg/sii_bg.wasm') });

function fmt(n) {
  return n >= 1024 * 1024
    ? (n / 1024 / 1024).toFixed(2) + ' MB'
    : (n / 1024).toFixed(1) + ' KB';
}

function check(name, file) {
  if (!fs.existsSync(file)) {
    console.log(`\n[${name}] 跳过：${file} 不存在`);
    return;
  }
  const raw = new Uint8Array(fs.readFileSync(file));
  console.log(`\n[${name}] ${file}`);
  console.log(`  原始 GIF      : ${fmt(raw.length)}`);

  // 1) 快照
  const t0 = performance.now();
  const snap = snapshot_of(raw);
  const tSnap = performance.now() - t0;
  console.log(`  快照字节流    : ${fmt(snap.length)}  (${tSnap.toFixed(1)} ms)`);
  console.log(`  快照头        : ${new TextDecoder().decode(snap.slice(0, 4))}`);

  // 2) 快照 → 编码
  const t1 = performance.now();
  const fromSnap = encode_snapshot(snap, 10);
  const tEncode = performance.now() - t1;
  console.log(`  快照→编码     : ${fmt(fromSnap.length)}  (${tEncode.toFixed(1)} ms)`);

  // 3) 直接转码（主线程老路径）做对照
  const t2 = performance.now();
  const direct = transcode(raw, 10);
  const tDirect = performance.now() - t2;
  console.log(`  直接转码      : ${fmt(direct.length)}  (${tDirect.toFixed(1)} ms)`);

  // 4) 逐字节比对
  const same =
    fromSnap.length === direct.length &&
    fromSnap.every((b, i) => b === direct[i]);
  console.log(`  两条路径一致  : ${same ? '✅ 逐字节相同' : '❌ 有差异'}`);

  if (!same) process.exitCode = 1;
}

check('小样本', '/tmp/sample.gif');
check('高清', '/tmp/hd.gif');

// 5) 坏数据必须报错，不能静默产出垃圾
console.log('\n[错误路径]');
for (const [label, buf] of [
  ['空数组', new Uint8Array(0)],
  ['魔数不对', new Uint8Array([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12])],
  ['截断的快照', (() => {
    if (!fs.existsSync('/tmp/sample.gif')) return new Uint8Array(20);
    const s = snapshot_of(new Uint8Array(fs.readFileSync('/tmp/sample.gif')));
    return s.slice(0, s.length - 1);
  })()],
]) {
  try {
    encode_snapshot(buf, 10);
    console.log(`  ${label}: ❌ 居然没报错`);
    process.exitCode = 1;
  } catch (e) {
    console.log(`  ${label}: ✅ 报错 → ${e?.message ?? e}`);
  }
}

console.log('\n完成。');
