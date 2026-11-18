// timeline.js — 推历 OshiCal 宣传片的唯一时间表。画面（index.html）与声音（score.mjs）都从这里 import。
// BPM 120 · 36 秒 = 18 小节 = 72 拍。

export const BPM = 120;
export const BEAT = 60 / BPM;   // 0.5s
export const BAR = BEAT * 4;    // 2s
export const b = n => n * BEAT;
export const revealOn = (hit, dur, reveal = 0) => hit - reveal * dur;

export const DUR = b(72);       // 36.0s

// 镜头表：每段是一个完整的"观众要看懂的变化"
export const SHOTS = [
  { id: 'hook',    t0: b(0),  t1: b(8),  note: '错过：21:00 开播，人已经在别的页面' },
  { id: 'reveal',  t0: b(8),  t1: b(15), note: '产品亮相：logo + 一句话定位' },
  { id: 'parse',   t0: b(15), t1: b(25), note: '卖点一：动态里的日程图 → 结构化日程，落进日历' },
  { id: 'views',   t0: b(25), t1: b(35), note: '卖点二：同一批日程，时间线 / 日历网格两种看法' },
  { id: 'library', t0: b(35), t1: b(45), note: '卖点三：主播库，一个人一个档案' },
  { id: 'alert',   t0: b(45), t1: b(55), note: '卖点四：开播 → 桌面通知 → 点一下进直播间' },
  { id: 'data',    t0: b(55), t1: b(66), note: '卖点五：粉丝与直播数据 / 服务端为唯一数据源' },
  { id: 'end',     t0: b(66), t1: b(72), note: '落版：logo + slogan + 平台信息' },
];

// 画面的关键事件 → 音效。画面读它做抖动 / 闪光，score.mjs 读它排音效。
export const CUES = [
  { t: b(1),    sfx: 'tick' },      // 21:00 亮起
  { t: b(2),    sfx: 'thud' },      // 第一张"已结束"卡落下
  { t: b(3),    sfx: 'thud' },
  { t: b(4),    sfx: 'thud' },
  { t: b(5),    sfx: 'swipe' },     // 大字进场
  { t: b(7.5),  sfx: 'whoosh' },    // → 亮相
  { t: b(8),    sfx: 'impact' },
  { t: b(9),    sfx: 'shimmer' },
  { t: b(11),   sfx: 'pop' },       // logo 落定
  { t: b(15),   sfx: 'whoosh' },    // → 解析
  { t: b(16.5), sfx: 'blip' },
  { t: b(18),   sfx: 'scan' },
  { t: b(20.5), sfx: 'pop' },       // 卡片落格 1
  { t: b(22),   sfx: 'pop' },
  { t: b(23.5), sfx: 'pop' },
  { t: b(25),   sfx: 'whoosh' },    // → 两种视图
  { t: b(27),   sfx: 'swipe' },     // 时间线 → 日历的擦除
  { t: b(28),   sfx: 'blip' },
  { t: b(30),   sfx: 'pop' },
  { t: b(32.5), sfx: 'tick' },
  { t: b(35),   sfx: 'whoosh' },    // → 主播库
  { t: b(36.5), sfx: 'blip' },
  { t: b(38),   sfx: 'pop' },       // 订阅日程
  { t: b(39.5), sfx: 'pop' },       // 开播通知
  { t: b(41),   sfx: 'pop' },       // 直播数据
  { t: b(43),   sfx: 'tick' },      // 粉丝数
  { t: b(45),   sfx: 'whoosh' },    // → 提醒
  { t: b(46.5), sfx: 'bell' },      // toast 滑入
  { t: b(50),   sfx: 'click' },     // 点击
  { t: b(50.5), sfx: 'whoosh' },    // 进直播间
  { t: b(55),   sfx: 'swipe' },     // → 数据
  { t: b(57),   sfx: 'count' },
  { t: b(61),   sfx: 'shimmer' },
  { t: b(66),   sfx: 'impact' },    // 落版
  { t: b(67),   sfx: 'shimmer' },
  { t: b(69),   sfx: 'bell' },
];

// 刻意停留（验收工具会把它们标成 intended hold，不算空拍）
export const HOLDS = [
  [b(66), DUR],          // 落版定格
  [b(23.5), b(25)],      // 三张卡落定后留一口气
];

// 大重拍：前面要有呼吸
export const DROPS = [b(8), b(66)];
