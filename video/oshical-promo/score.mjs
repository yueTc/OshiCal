// score.mjs — 推历 OshiCal 宣传片的配乐与音效（第 1 档：内置合成，零依赖、完全确定）
// 构建：node <skill>/scripts/audio.mjs build score.mjs  →  audio/music.wav（−14 LUFS）
//
// 声音方向（brief 的 sound 一栏）：
//   benchmark  日系 UI 小品的"木琴 + 拨弦 + 软鼓"语汇；学它的干净、留白、一问一答，不学任何具体旋律
//   groove     120 BPM 直拍，但不四踩：kick 落在 1 与 3&，rim/clap 在 2、4，shaker 走八分，留出空气
//   harmony    D 大调五声为主，和声走 Dmaj7–Bm7–Gmaj7–A7sus4，色彩靠 7 音而不是靠加厚
//   palette    kalimba 主奏动机 D–F#–A / pluck 琶音 / 软 bass / pad
//   arc        稀疏(0–4s) → 落地(4s) → 建起来(4–22.5s) → 最满(22.5–32s) → 抽空(32–33s) → 收成一句(33–36s)
//   mix        小房间、暖、几乎不侧链；音效在前，音乐退到 0.3–0.4
import { DUR, BPM, CUES, b } from './timeline.js';
export { DUR };

export default function score(S) {
  S.tempo(BPM);
  S.space({ room: 0.72, damp: 0.42, width: 1 });

  const music = S.bus('music', { gain: 0.46, reverb: 0.28, duck: 0.45, highpass: 160 });
  const keys  = S.bus('keys',  { gain: 0.40, reverb: 0.34, duck: 0.5, highpass: 220, width: 0.9 });
  const bass  = S.bus('bass',  { gain: 0.52, duck: 0.55 });
  const drums = S.bus('drums', { gain: 0.58, duck: 0.18 });
  const sfx   = S.bus('sfx',   { gain: 0.86, reverb: 0.16 });

  const BAR = S.bar(1);                 // 2s
  const BEAT = S.beat(1);               // 0.5s
  const bars = Math.round(DUR / BAR);   // 18
  const prog = [['D3', 'maj7'], ['B2', 'min7'], ['G2', 'maj7'], ['A2', '7sus4']];
  const PENTA = S.scale('D4', 'penta'); // 五声：D E F# A B

  // ── 铺底：pad 从头铺到尾，前段更暗，落版更亮；33s 前留白
  for (let bar = 0; bar < bars; bar++) {
    const [root, q] = prog[bar % prog.length];
    const t = S.bar(bar);
    const bright = bar < 2 ? 760 : bar < 16 ? 1600 : 2200;
    const pDur = bar === 16 ? BAR * 0.5 : BAR * 0.98;      // 32–33s 留一口气给落版
    music.add(S.pad(S.chord(root, q), pDur, { cutoff: bright, attack: bar < 2 ? 0.6 : 0.35, release: bar === 16 ? 0.45 : 1.4, voices: 5 }), t, { gain: bar < 2 ? 0.5 : bar === 16 ? 0.42 : 0.62 });
  }

  // ── 动机：kalimba 的 D–F#–A，只换节奏和音区
  const motif = [0, 2, 3];
  const playMotif = (t0, oct = 0, gain = 0.5, step = BEAT * 0.5, dur = 0.9) => {
    motif.forEach((deg, i) => {
      keys.add(S.mallet(PENTA(deg) + 12 + oct * 12, dur, { material: 'kalimba', decay: 0.55, bright: 0.5 }),
               t0 + i * step, { gain, pan: -0.18 + i * 0.18 });
    });
  };
  playMotif(b(1), 0, 0.5);
  playMotif(b(9), 1, 0.42);
  playMotif(b(17), 0, 0.46);
  playMotif(b(26), 1, 0.5, BEAT * 0.25);
  playMotif(b(36), 1, 0.44);
  playMotif(b(46.5), 1, 0.5, BEAT * 0.25);
  playMotif(b(56), 0, 0.46);
  // 落版：动机倒过来（A–F#–D）
  [3, 2, 0].forEach((deg, i) => {
    keys.add(S.mallet(PENTA(deg) + 12, 1.7, { material: 'kalimba', decay: 0.7, bright: 0.55 }), b(68) + i * BEAT * 0.5, { gain: 0.5 });
  });

  // ── 拨弦琶音：从 4s 的落地开始
  const arp = [0, 2, 3, 4, 3, 2, 1, 2];
  for (let bar = 2; bar < 16; bar++) {
    const [root, q] = prog[bar % prog.length];
    const chord = S.chord(root, q).map(n => n + 12);
    for (let i = 0; i < 8; i++) {
      if (bar > 13 && i % 2 === 1) continue;
      const n = chord[arp[i] % chord.length];
      music.add(S.pluck(n, 0.5, { bright: 0.5, damp: 0.994 }), S.bar(bar) + i * (BAR / 8),
               { gain: 0.30, pan: i % 2 ? 0.3 : -0.3 });
    }
  }

  // ── 低音
  for (let bar = 2; bar < 16; bar++) {
    const [root] = prog[bar % prog.length];
    const t = S.bar(bar);
    bass.add(S.bass(root, BEAT * 0.85, { cutoff: 420, sub: 0.5, drive: 0.08 }), t, { gain: 0.8 });
    bass.add(S.bass(root, BEAT * 0.55, { cutoff: 380, sub: 0.45 }), t + BEAT * 2.5, { gain: 0.6 });
    bass.add(S.bass(root, BEAT * 0.55, { cutoff: 380, sub: 0.45 }), t + BEAT * 3.5, { gain: 0.45 });
  }

  // ── 鼓：不四踩；11 小节起加一层；16 小节（32–34s）只留呼吸
  for (let bar = 0; bar < bars; bar++) {
    const t = S.bar(bar);
    const soft = bar < 2 || (bar >= 16 && bar < 17);
    const full = bar >= 2 && bar < 16;
    if (soft && bar < 2) {
      drums.add(S.kick({ decay: 0.28, drive: 0.2 }), t, { gain: bar === 0 ? 0.5 : 0.42 });
      S.duck(t, 0.35);
      drums.add(S.shaker({ decay: 0.05 }), t + BEAT * 2, { gain: 0.20, pan: 0.25 });
      drums.add(S.rim({ tone: 1.1 }), t + BEAT * 3, { gain: 0.22, pan: -0.2 });
    }
    if (full) {
      drums.add(S.kick({ decay: 0.32, drive: 0.3 }), t, { gain: 0.62 }); S.duck(t, 0.5);
      drums.add(S.kick({ decay: 0.24, drive: 0.28 }), t + BEAT * 2.5, { gain: 0.42 }); S.duck(t + BEAT * 2.5, 0.4);
      drums.add(S.kick({ decay: 0.22, drive: 0.28 }), t + BEAT * 3, { gain: 0.34 });
      drums.add(S.rim({ tone: 1 }), t + BEAT, { gain: 0.3, pan: 0.15 });
      drums.add(S.clap({ decay: 0.2 }), t + BEAT * 3, { gain: 0.34, pan: -0.12 });
      for (let i = 0; i < 8; i++) {
        drums.add(S.shaker({ decay: 0.04 }), t + i * (BAR / 8), { gain: i % 2 ? 0.13 : 0.2, pan: i % 2 ? 0.35 : -0.25 });
      }
      if (bar === 3 || bar === 7 || bar === 11) drums.add(S.crash({ dur: 1.8 }), t, { gain: 0.15 });
    }
  }

  // ── 呼吸：两个大重拍前各留一口气
  sfx.add(S.reverseCymbal(0.8), b(7), { gain: 0.4 });
  sfx.add(S.riser(1.0, { from: 220, to: 2600 }), b(7), { gain: 0.28 });
  sfx.add(S.reverseCymbal(0.7), b(65), { gain: 0.34 });
  sfx.add(S.riser(0.9, { from: 260, to: 2200 }), b(65.2), { gain: 0.24 });

  // ── 事件音效（时间点全部来自 timeline.js）
  for (const c of CUES) {
    switch (c.sfx) {
      case 'tick':   sfx.add(S.tick(), c.t, { gain: 0.24, pan: 0.1 }); break;
      case 'thud':   sfx.add(S.boom({ size: 0.8 }), c.t, { gain: 0.55 }); sfx.add(S.rim({ tone: 0.8 }), c.t, { gain: 0.2 }); break;
      case 'swipe':  sfx.add(S.swipe(0.3, { up: true }), c.t, { gain: 0.34 }); break;
      case 'whoosh': sfx.add(S.whoosh(0.55, { peak: 0.72 }), c.t - 0.55 * 0.72, { gain: 0.42 }); break;
      case 'impact': sfx.add(S.impact({ size: 1.05 }), c.t, { gain: 0.7 }); drums.add(S.crash({ dur: 1.6 }), c.t, { gain: 0.18 }); S.duck(c.t, 0.8); break;
      case 'shimmer': sfx.add(S.shimmer(1.5, { root: 'A5' }), c.t, { gain: 0.30 }); break;
      case 'pop':    sfx.add(S.pop('A5'), c.t, { gain: 0.32, pan: 0.2 }); break;
      case 'bell':   sfx.add(S.bell('D6', 1.8, { ratio: 3.2, index: 3.4 }), c.t, { gain: 0.30 }); break;
      case 'click':  sfx.add(S.click(), c.t, { gain: 0.34 }); break;
      case 'blip':   sfx.add(S.blip('D6', 0.07), c.t, { gain: 0.20 }); break;
      case 'scan':   for (let i = 0; i < 7; i++) sfx.add(S.blip(['D6', 'F#6', 'A6'][i % 3], 0.05), c.t + i * 0.055, { gain: 0.10 + i * 0.004, pan: -0.5 + i * 0.17 }); break;
      case 'count':  for (let i = 0; i < 12; i++) sfx.add(S.tick(), c.t + i * 0.075, { gain: 0.10 }); break;
      default: break;
    }
  }
}
