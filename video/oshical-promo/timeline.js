// timeline.js — 推历 OshiCal 宣传片的唯一时间表。画面（index.html）与声音（score.mjs）都从这里 import。
// BPM 120 · 30 秒 = 15 小节 = 60 拍。

export const BPM = 120;
export const BEAT = 60 / BPM;   // 0.5s
export const BAR = BEAT * 4;    // 2s
export const b = n => n * BEAT;
export const revealOn = (hit, dur, reveal = 0) => hit - reveal * dur;

export const DUR = b(60);       // 30.0s

// 镜头表：每段是一个完整的"观众要看懂的变化"
export const SHOTS = [
  { id: 'hook',   t0: b(0),  t1: b(8),  note: '错过：21:00 开播，人已经在别的页面' },
  { id: 'reveal', t0: b(8),  t1: b(16), note: '产品亮相：logo + 一句话定位' },
  { id: 'parse',  t0: b(16), t1: b(29), note: '卖点一：动态里的日程图 → 结构化日程，落进日历' },
  { id: 'alert',  t0: b(29), t1: b(41), note: '卖点二：开播 → 桌面通知 → 点一下进直播间' },
  { id: 'data',   t0: b(41), t1: b(52), note: '卖点三：粉丝与直播数据 / 服务端为唯一数据源' },
  { id: 'end',    t0: b(52), t1: b(60), note: '落版：logo + slogan + 平台信息' },
];

// 画面的关键事件 → 音效。画面读它做抖动 / 闪光，score.mjs 读它排音效。
export const CUES = [
  { t: b(1),    sfx: 'tick' },      // 21:00 时间戳亮起
  { t: b(2),    sfx: 'thud' },      // 第一张"已结束"卡落下
  { t: b(3.5),  sfx: 'thud' },
  { t: b(5),    sfx: 'thud' },
  { t: b(6),    sfx: 'swipe' },     // 大字「又一次，错过了」进场
  { t: b(7.5),  sfx: 'whoosh' },    // → 转场
  { t: b(8),    sfx: 'impact' },    // 粉色爆发 / 亮相
  { t: b(9),    sfx: 'shimmer' },
  { t: b(11),   sfx: 'pop' },       // logo 落定
  { t: b(16),   sfx: 'whoosh' },    // → 解析段
  { t: b(18),   sfx: 'blip' },      // 动态卡出现
  { t: b(19.5), sfx: 'scan' },      // 解析链路过一遍
  { t: b(22),   sfx: 'pop' },       // 卡片落格 1
  { t: b(24),   sfx: 'pop' },
  { t: b(26),   sfx: 'pop' },
  { t: b(29),   sfx: 'whoosh' },    // → 提醒段
  { t: b(30.5), sfx: 'bell' },      // Toast 滑入
  { t: b(34),   sfx: 'click' },     // 光标点击
  { t: b(34.5), sfx: 'whoosh' },    // 进直播间
  { t: b(41),   sfx: 'swipe' },     // → 数据段
  { t: b(43),   sfx: 'count' },     // 数字滚动起点
  { t: b(48),   sfx: 'shimmer' },   // 多端同步
  { t: b(52),   sfx: 'impact' },    // 落版
  { t: b(53),   sfx: 'shimmer' },
  { t: b(56),   sfx: 'bell' },
];

// 刻意停留（验收工具会把它们标成 intended hold，不算空拍）
export const HOLDS = [
  [b(52), DUR],          // 落版定格
  [b(26), b(28.5)],      // 三张日程卡落定后留一口气
];

// 大重拍：前面要有呼吸
export const DROPS = [b(8), b(52)];
