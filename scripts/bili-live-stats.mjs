#!/usr/bin/env node
import crypto from 'node:crypto';
import zlib from 'node:zlib';
import { execFileSync } from 'node:child_process';
import { performance } from 'node:perf_hooks';
import { TextDecoder } from 'node:util';

const DEFAULT_ROOM_ID = '22625027';
const MAX_PACKET_NESTING_DEPTH = 4;
const MAX_DECOMPRESSED_BYTES = 8 * 1024 * 1024;
const MAX_MESSAGE_BYTES = 1024 * 1024;
const MAX_GIFT_V2_ITEMS = 1024;
const MAX_TRACKED_USERS = 100_000;
const USER_TRACKING_TTL_MS = 12 * 60 * 60 * 1000;
const MAX_DEDUP_EVENTS = 50_000;
const EVENT_DEDUP_TTL_MS = 30 * 60 * 1000;
const MAX_WEBSOCKET_HOSTS = 16;
const LIVE_AUTH_TIMEOUT_MS = 15_000;
const LIVE_SOCKET_IDLE_TIMEOUT_MS = 95_000;
const RECONNECT_DELAY_MS = 3_000;
const MAX_RECONNECT_DELAY_MS = 60_000;
const MAX_PROTOBUF_FIELD_NUMBER = 0x1fffffff;
const MAX_SAFE_BIGINT = BigInt(Number.MAX_SAFE_INTEGER);
const UTF8_DECODER = new TextDecoder('utf-8', { fatal: true });
const DANMU_INFO_URL = 'https://api.live.bilibili.com/xlive/web-room/v1/index/getDanmuInfo';
const ROOM_INFO_URL = 'https://api.live.bilibili.com/room/v1/Room/get_info';
const NAV_URL = 'https://api.bilibili.com/x/web-interface/nav';
const FINGER_URL = 'https://api.bilibili.com/x/frontend/finger/spi';
const MIXIN_KEY_TAB = [
  46, 47, 18, 2, 53, 8, 23, 32, 15, 50, 10, 31, 58, 3, 45, 35,
  27, 43, 5, 49, 33, 9, 42, 19, 29, 28, 14, 39, 12, 38, 41, 13,
  37, 48, 7, 16, 24, 55, 40, 61, 26, 17, 0, 1, 60, 51, 30, 4,
  22, 25, 54, 21, 56, 59, 6, 63, 57, 62, 11, 36, 20, 34, 44, 52,
];

let activeState = null;

function parseArgs(argv) {
  const options = {
    roomId: DEFAULT_ROOM_ID,
    durationSeconds: 60,
    reportSeconds: 5,
    systemNetwork: false,
    verbose: false,
  };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === '--room') options.roomId = argv[++index] ?? options.roomId;
    else if (arg === '--duration') options.durationSeconds = Number(argv[++index] ?? options.durationSeconds);
    else if (arg === '--interval') options.reportSeconds = Number(argv[++index] ?? options.reportSeconds);
    else if (arg === '--system-network') options.systemNetwork = true;
    else if (arg === '--no-system-network') options.systemNetwork = false;
    else if (arg === '--verbose') options.verbose = true;
    else if (arg === '--help' || arg === '-h') {
      printHelp();
      process.exit(0);
    } else throw new Error(`未知参数: ${arg}`);
  }
  if (!/^\d+$/.test(options.roomId)) throw new Error('--room 必须是数字直播间号');
  if (!Number.isFinite(options.durationSeconds) || options.durationSeconds <= 0) throw new Error('--duration 必须大于 0 秒');
  if (!Number.isFinite(options.reportSeconds) || options.reportSeconds <= 0) throw new Error('--interval 必须大于 0 秒');
  return options;
}

function printHelp() {
  console.log(`用法: node scripts/bili-live-stats.mjs [选项]

  --room <id>              B 站直播间号（默认 ${DEFAULT_ROOM_ID}）
  --duration <seconds>     采样时长，必须大于 0（默认 60）
  --interval <seconds>     控制台报告间隔，必须大于 0（默认 5）
  --system-network         尝试读取系统网卡流量
  --no-system-network      不读取系统网卡流量
  --verbose                输出更多解析和连接诊断
  -h, --help               显示本帮助
`);
}

function getWbiKeys(navData) {
  const imgUrl = navData?.data?.wbi_img?.img_url;
  const subUrl = navData?.data?.wbi_img?.sub_url;
  if (typeof imgUrl !== 'string' || typeof subUrl !== 'string' || !imgUrl || !subUrl) {
    throw new Error('B 站 nav 响应缺少 WBI key');
  }
  const raw = imgUrl.split('/').pop().split('.')[0] + subUrl.split('/').pop().split('.')[0];
  if (raw.length < 64) throw new Error('B 站 WBI key 长度不足');
  const mixinKey = MIXIN_KEY_TAB.map((index) => raw[index]).join('').slice(0, 32);
  if (mixinKey.length !== 32) throw new Error('B 站 WBI key 格式非法');
  return mixinKey;
}

function normalizeDanmuHost(host) {
  if (typeof host !== 'string') return null;
  const normalized = host.trim().replace(/\.+$/, '').toLowerCase();
  if (!normalized || normalized.length > 253 || !normalized.includes('.')) return null;
  if (/^\d{1,3}(?:\.\d{1,3}){3}$/.test(normalized) || normalized.startsWith('[')) return null;
  const labels = normalized.split('.');
  const tld = labels.at(-1);
  if (!tld || tld.length < 2 || !/^[a-z]+$/.test(tld)) return null;
  if (labels.some((label) => !/^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/.test(label))) return null;
  if (['local', 'localhost', 'internal', 'lan', 'home'].includes(tld)) return null;
  return normalized;
}

function encodeWbiValue(value) {
  return encodeURIComponent(String(value)).replace(/[!'()*]/g, '');
}

function signWbi(params, mixinKey) {
  const query = Object.keys(params).sort().map((key) => `${encodeWbiValue(key)}=${encodeWbiValue(params[key])}`).join('&');
  const wRid = crypto.createHash('md5').update(query + mixinKey).digest('hex');
  return `${query}&w_rid=${wRid}`;
}

function round(value, digits = 2) {
  const factor = 10 ** digits;
  return Math.round(value * factor) / factor;
}

function formatBytes(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${round(bytes / 1024)} KB`;
  return `${round(bytes / 1024 / 1024)} MB`;
}

function formatRate(bytes, seconds) {
  if (seconds <= 0) return '0 kbps';
  return `${round((bytes * 8) / seconds / 1024)} kbps`;
}

function formatDuration(seconds) {
  const total = Math.max(0, Math.floor(seconds));
  const hours = Math.floor(total / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  return `${String(hours).padStart(2, '0')}:${String(minutes).padStart(2, '0')}:${String(total % 60).padStart(2, '0')}`;
}

function readNetworkCounters() {
  if (process.platform !== 'win32') return null;
  try {
    const script = [
      "$route = Get-NetRoute -DestinationPrefix '0.0.0.0/0' -ErrorAction Stop |",
      '  Sort-Object RouteMetric | Select-Object -First 1;',
      '$alias = $route.InterfaceAlias;',
      '$stats = Get-NetAdapterStatistics -Name $alias -ErrorAction Stop;',
      '[pscustomobject]@{ alias = $alias; rx = [double]$stats.ReceivedBytes; tx = [double]$stats.SentBytes } |',
      '  ConvertTo-Json -Compress',
    ].join(' ');
    const output = execFileSync('powershell.exe', ['-NoProfile', '-Command', script], {
      encoding: 'utf8', windowsHide: true, timeout: 5000,
    });
    return JSON.parse(output.trim());
  } catch {
    return null;
  }
}

function makePacket(operation, payload = Buffer.alloc(0)) {
  const body = Buffer.isBuffer(payload) ? payload : Buffer.from(payload);
  const header = Buffer.alloc(16);
  header.writeUInt32BE(16 + body.length, 0);
  header.writeUInt16BE(16, 4);
  header.writeUInt16BE(1, 6);
  header.writeUInt32BE(operation, 8);
  header.writeUInt32BE(1, 12);
  return Buffer.concat([header, body]);
}

function createState(options) {
  return {
    options, startedAt: performance.now(), roomInfo: null, roomStartText: null,
    highEnergy: 0, peakHighEnergy: 0, danmakuCount: 0, giftCount: 0, superChatCount: 0,
    guardCount: 0, enterCount: 0, revenueCents: 0, giftRevenueCents: 0,
    superChatRevenueCents: 0, guardRevenueCents: 0, interactiveUsers: new Map(),
    paidUsers: new Map(), danmakuUsers: new Map(), interactiveUserCount: 0,
    paidUserCount: 0, danmakuUserCount: 0, eventCounts: new Map(),
    eventDedup: new Map(), dataQuality: 'complete', dataQualityReasons: new Set(),
    malformedPackets: 0, appRxBytes: 0, appTxBytes: 0, httpRxBytes: 0,
    wsRxBytes: 0, wsTxBytes: 0, wsFrames: 0, packets: 0, reconnects: 0,
    socket: null, socketGeneration: 0, heartbeatTimer: null, reconnectTimer: null,
    reconnectGeneration: 0, reconnectDelayMs: RECONNECT_DELAY_MS, connecting: false,
    socketOpenedAt: 0, lastSocketActivityAt: 0, authConfirmed: false, authenticatedOnce: false,
    reportTimer: null, finishTimer: null, finishing: false, systemStart: null, systemEnd: null,
  };
}

async function fetchJson(state, url, headers = {}) {
  const response = await fetch(url, {
    headers: {
      'User-Agent': 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) OshiCal-Live-Stats-POC/1.0',
      Referer: `https://live.bilibili.com/${state.options.roomId}`,
      ...headers,
    },
    signal: AbortSignal.timeout(15000),
  });
  const text = await response.text();
  const bytes = Buffer.byteLength(text);
  state.appRxBytes += bytes;
  state.httpRxBytes += bytes;
  if (!response.ok) throw new Error(`HTTP ${response.status}: ${text.slice(0, 200)}`);
  return JSON.parse(text);
}

async function loadRoomInfo(state) {
  const data = await fetchJson(state, `${ROOM_INFO_URL}?room_id=${state.options.roomId}`);
  if (data.code !== 0 || !data.data) throw new Error(`获取直播间信息失败: ${data.code} ${data.message ?? ''}`);
  state.roomInfo = data.data;
  state.roomStartText = data.data.live_status === 1 && data.data.live_time && !data.data.live_time.startsWith('0000-00-00')
    ? data.data.live_time
    : null;
  return data.data;
}

async function getDanmuInfo(state) {
  const finger = await fetchJson(state, FINGER_URL);
  const b3 = finger?.data?.b_3;
  const b4 = finger?.data?.b_4;
  const cookie = [b3 ? `buvid3=${b3}` : null, b4 ? `buvid4=${b4}` : null].filter(Boolean).join('; ');
  const nav = await fetchJson(state, NAV_URL, cookie ? { Cookie: cookie } : {});
  const mixinKey = getWbiKeys(nav);
  const query = signWbi({
    id: state.options.roomId,
    type: '0',
    wts: String(Math.floor(Date.now() / 1000)),
  }, mixinKey);
  const data = await fetchJson(state, `${DANMU_INFO_URL}?${query}`, {
    ...(cookie ? { Cookie: cookie } : {}),
    Origin: 'https://live.bilibili.com',
  });
  if (
    data.code !== 0
    || typeof data.data?.token !== 'string'
    || !data.data.token
    || !Array.isArray(data.data.host_list)
    || data.data.host_list.length === 0
  ) {
    throw new Error(`获取弹幕服务器失败: ${data.code} ${data.message ?? ''}`);
  }
  return data.data;
}

function decodeVarint(buffer, start) {
  if (!Buffer.isBuffer(buffer) || !Number.isSafeInteger(start) || start < 0 || start >= buffer.length) {
    throw new Error('非法 protobuf varint 起点');
  }
  let value = 0n;
  let offset = start;
  for (let index = 0; index < 10; index += 1) {
    if (offset >= buffer.length) throw new Error('protobuf varint 越界');
    const byte = buffer[offset];
    offset += 1;
    if (index === 9 && byte > 1) throw new Error('protobuf varint 溢出');
    value |= BigInt(byte & 0x7f) << BigInt(index * 7);
    if ((byte & 0x80) === 0) return { value, offset };
  }
  throw new Error('非法 protobuf varint');
}

function decodeProtobufFields(buffer) {
  if (!Buffer.isBuffer(buffer) || buffer.length > MAX_MESSAGE_BYTES) {
    throw new Error('protobuf 数据为空或过大');
  }
  const fields = new Map();
  let offset = 0;
  while (offset < buffer.length) {
    const tag = decodeVarint(buffer, offset);
    offset = tag.offset;
    const fieldNumber = Number(tag.value >> 3n);
    const wireType = Number(tag.value & 7n);
    if (!Number.isSafeInteger(fieldNumber) || fieldNumber <= 0 || fieldNumber > MAX_PROTOBUF_FIELD_NUMBER) {
      throw new Error('protobuf 字段编号非法');
    }
    let value;
    if (wireType === 0) {
      const decoded = decodeVarint(buffer, offset);
      value = decoded.value;
      offset = decoded.offset;
    } else if (wireType === 1) {
      if (offset + 8 > buffer.length) throw new Error('protobuf fixed64 越界');
      value = buffer.subarray(offset, offset + 8);
      offset += 8;
    } else if (wireType === 2) {
      const length = decodeVarint(buffer, offset);
      offset = length.offset;
      if (length.value > MAX_SAFE_BIGINT) throw new Error('protobuf 长度超出安全范围');
      const end = offset + Number(length.value);
      if (!Number.isSafeInteger(end) || end > buffer.length) throw new Error('protobuf 长度越界');
      value = buffer.subarray(offset, end);
      offset = end;
    } else if (wireType === 5) {
      if (offset + 4 > buffer.length) throw new Error('protobuf fixed32 越界');
      value = buffer.subarray(offset, offset + 4);
      offset += 4;
    } else {
      throw new Error(`不支持的 protobuf wire type: ${wireType}`);
    }
    if (!fields.has(fieldNumber)) fields.set(fieldNumber, []);
    fields.get(fieldNumber).push({ wireType, value });
  }
  return fields;
}

function fieldNumber(fields, id, options = {}) {
  const { required = false, min = 0, max = Number.MAX_SAFE_INTEGER } = options;
  const values = fields.get(id);
  if (values === undefined) {
    if (required) throw new Error(`protobuf 缺少字段 ${id}`);
    return null;
  }
  let first = null;
  for (const entry of values) {
    if (entry?.wireType !== 0 || typeof entry.value !== 'bigint') {
      throw new Error(`protobuf 字段 ${id} 的 wire type 错误`);
    }
    if (first === null) first = entry.value;
  }
  if (first < 0n || first > MAX_SAFE_BIGINT) throw new Error(`protobuf 字段 ${id} 超出安全范围`);
  const number = Number(first);
  if (number < min || number > max) throw new Error(`protobuf 字段 ${id} 数值越界`);
  return number;
}

function fieldString(fields, id, options = {}) {
  const { required = false } = options;
  const values = fields.get(id);
  if (values === undefined) {
    if (required) throw new Error(`protobuf 缺少字段 ${id}`);
    return '';
  }
  let first = null;
  for (const entry of values) {
    if (entry?.wireType !== 2 || !Buffer.isBuffer(entry.value)) {
      throw new Error(`protobuf 字段 ${id} 的 wire type 错误`);
    }
    const text = UTF8_DECODER.decode(entry.value);
    if (first === null) first = text;
  }
  return first ?? '';
}

function decodeBase64Strict(base64) {
  if (typeof base64 !== 'string' || base64.length === 0 || base64.length > MAX_MESSAGE_BYTES) {
    throw new Error('SEND_GIFT_V2 base64 数据为空或过大');
  }
  if (!/^[A-Za-z0-9+/]*={0,2}$/.test(base64) || base64.length % 4 === 1) {
    throw new Error('SEND_GIFT_V2 base64 格式错误');
  }
  if (base64.includes('=') && base64.length % 4 !== 0) {
    throw new Error('SEND_GIFT_V2 base64 padding 非法');
  }
  const buffer = Buffer.from(base64, 'base64');
  const normalized = base64.replace(/=+$/, '');
  if (buffer.toString('base64').replace(/=+$/, '') !== normalized) {
    throw new Error('SEND_GIFT_V2 base64 编码非规范');
  }
  if (buffer.length === 0 || buffer.length > MAX_MESSAGE_BYTES) {
    throw new Error('SEND_GIFT_V2 解码后数据为空或过大');
  }
  return buffer;
}

function decodeSendGiftV2(base64) {
  const root = decodeProtobufFields(decodeBase64Strict(base64));
  const uid = fieldNumber(root, 1) ?? 0;
  const items = root.get(10);
  if (!items || items.length === 0 || items.length > MAX_GIFT_V2_ITEMS) {
    throw new Error('SEND_GIFT_V2 礼物流数量非法');
  }
  return items.map((entry) => {
    if (entry?.wireType !== 2 || !Buffer.isBuffer(entry.value)) {
      throw new Error('SEND_GIFT_V2 礼物流字段类型错误');
    }
    const gift = decodeProtobufFields(entry.value);
    fieldNumber(gift, 14);
    return {
      uid,
      giftId: fieldNumber(gift, 1, { required: true, min: 1 }),
      giftName: fieldString(gift, 2),
      num: fieldNumber(gift, 3, { required: true, min: 1 }),
      price: fieldNumber(gift, 5, { min: 0 }) ?? 0,
      totalCoin: fieldNumber(gift, 7, { min: 0 }) ?? 0,
      coinType: fieldString(gift, 8),
      tid: fieldString(gift, 9),
      timestamp: fieldNumber(gift, 10, { min: 0 }) ?? 0,
      comboTotalCoin: fieldNumber(gift, 14, { min: 0 }) ?? 0,
    };
  });
}

function markDegraded(state, reason) {
  state.dataQuality = 'partial';
  state.dataQualityReasons.add(reason);
}

function incrementCounter(state, field, amount = 1) {
  const current = Number(state[field] ?? 0);
  const next = current + amount;
  if (!Number.isSafeInteger(current) || !Number.isSafeInteger(next)) {
    markDegraded(state, `${field}-overflow`);
    state[field] = Number.MAX_SAFE_INTEGER;
  } else {
    state[field] = next;
  }
}

function addCents(state, field, amount) {
  const current = Number(state[field] ?? 0);
  const next = current + amount;
  if (!Number.isSafeInteger(current) || !Number.isSafeInteger(amount) || !Number.isSafeInteger(next)) {
    markDegraded(state, `${field}-overflow`);
    state[field] = Number.MAX_SAFE_INTEGER;
  } else {
    state[field] = next;
  }
}

function pruneTrackedMap(state, values, capacity, ttlMs, reason) {
  const now = Date.now();
  for (const [key, insertedAt] of values) {
    if (now - insertedAt < ttlMs) break;
    values.delete(key);
    markDegraded(state, reason);
  }
  while (values.size >= capacity) {
    const oldest = values.keys().next().value;
    if (oldest === undefined) break;
    values.delete(oldest);
    markDegraded(state, reason);
  }
}

function boundedTrackingKey(state, prefix, value, maxBytes, reason) {
  const text = String(value);
  if (Buffer.byteLength(text, 'utf8') <= maxBytes) return text;
  markDegraded(state, reason);
  const digest = crypto.createHash('sha256').update(text).digest('hex');
  return `${prefix}-hash:${digest}`;
}

function addTrackedUser(state, values, uid, reason) {
  if (uid == null) return false;
  const rawKey = String(uid).trim();
  if (!rawKey || rawKey === '0') return false;
  const key = boundedTrackingKey(state, 'user', rawKey, 128, `${reason}-key-too-long`);
  pruneTrackedMap(state, values, MAX_TRACKED_USERS, USER_TRACKING_TTL_MS, reason);
  if (values.has(key)) return false;
  values.set(key, Date.now());
  return true;
}

function addInteractiveUser(state, uid) {
  if (addTrackedUser(state, state.interactiveUsers, uid, 'interactive-users-cap-or-ttl')) {
    incrementCounter(state, 'interactiveUserCount');
  }
}

function addPaidUser(state, uid) {
  addInteractiveUser(state, uid);
  if (addTrackedUser(state, state.paidUsers, uid, 'paid-users-cap-or-ttl')) {
    incrementCounter(state, 'paidUserCount');
  }
}

function addDanmakuUser(state, uid) {
  if (addTrackedUser(state, state.danmakuUsers, uid, 'danmaku-users-cap-or-ttl')) {
    incrementCounter(state, 'danmakuUserCount');
  }
}

function markUniqueEvent(state, key) {
  const boundedKey = boundedTrackingKey(state, 'event', key, 512, 'event-key-too-long');
  const now = Date.now();
  for (const [oldKey, timestamp] of state.eventDedup) {
    if (now - timestamp < EVENT_DEDUP_TTL_MS) break;
    state.eventDedup.delete(oldKey);
    markDegraded(state, 'event-dedup-ttl');
  }
  if (state.eventDedup.has(boundedKey)) return false;
  while (state.eventDedup.size >= MAX_DEDUP_EVENTS) {
    const oldest = state.eventDedup.keys().next().value;
    if (oldest === undefined) break;
    state.eventDedup.delete(oldest);
    markDegraded(state, 'event-dedup-capacity');
  }
  state.eventDedup.set(boundedKey, now);
  return true;
}

function nonNegativeSafeInteger(state, value, fallback, reason) {
  const number = Number(value ?? fallback);
  if (!Number.isSafeInteger(number) || number < 0) {
    markDegraded(state, reason);
    return fallback;
  }
  return number;
}

function recordGift(state, gift) {
  const uidKey = gift.uid == null ? 'anonymous' : String(gift.uid).trim() || 'anonymous';
  const tid = String(gift.tid ?? '').trim();
  const key = tid
    ? `gift:${tid}`
    : `gift:${uidKey}:${gift.giftId}:${gift.num}:${gift.timestamp}:${gift.price}:${gift.giftName ?? ''}`;
  if (!markUniqueEvent(state, key)) return;

  incrementCounter(state, 'giftCount');
  addInteractiveUser(state, gift.uid);

  let num = nonNegativeSafeInteger(state, gift.num, 1, 'gift-num-invalid');
  if (num === 0) {
    markDegraded(state, 'gift-num-zero');
    num = 1;
  }
  const price = nonNegativeSafeInteger(state, gift.price, 0, 'gift-price-invalid');
  const totalCoin = nonNegativeSafeInteger(state, gift.totalCoin, 0, 'gift-total-invalid');
  let coins = totalCoin;
  if (coins === 0) {
    coins = price * num;
    if (!Number.isSafeInteger(coins)) {
      markDegraded(state, 'gift-product-overflow');
      coins = 0;
    }
  }
  const cents = String(gift.coinType ?? '').toLowerCase() === 'silver'
    ? 0
    : Math.floor(coins / 10);
  if (cents > 0) {
    addCents(state, 'revenueCents', cents);
    addCents(state, 'giftRevenueCents', cents);
    addPaidUser(state, gift.uid);
  }
}

function noteEvent(state, cmd) {
  const key = boundedTrackingKey(state, 'cmd', cmd, 128, 'event-type-key-too-long');
  if (!state.eventCounts.has(key) && state.eventCounts.size >= 256) {
    markDegraded(state, 'event-type-capacity');
    return;
  }
  state.eventCounts.set(key, (state.eventCounts.get(key) ?? 0) + 1);
}

function parseMessage(state, message) {
  const cmd = String(message?.cmd ?? '').split(':')[0];
  if (!cmd) return;
  noteEvent(state, cmd);
  const data = message.data ?? {};
  if (cmd === 'ONLINE_RANK_COUNT') {
    state.highEnergy = nonNegativeSafeInteger(
      state,
      data.online_count ?? data.count ?? 0,
      0,
      'high-energy-invalid',
    );
    state.peakHighEnergy = Math.max(state.peakHighEnergy, state.highEnergy);
  } else if (cmd === 'DANMU_MSG') {
    incrementCounter(state, 'danmakuCount');
    const uid = message.info?.[2]?.[0];
    if (uid != null) {
      addDanmakuUser(state, uid);
      addInteractiveUser(state, uid);
    }
  } else if (cmd === 'SEND_GIFT') {
    if (state.options.verbose) {
      console.log('[gift-v1]', JSON.stringify({ giftName: data.giftName, num: data.num, price: data.price, totalCoin: data.total_coin, coinType: data.coin_type }));
    }
    recordGift(state, {
      uid: data.uid,
      giftId: data.giftId,
      giftName: data.giftName,
      num: data.num ?? 1,
      price: data.price ?? 0,
      totalCoin: data.total_coin ?? 0,
      coinType: data.coin_type,
      tid: data.tid,
      timestamp: data.timestamp ?? 0,
    });
  } else if (cmd === 'SEND_GIFT_V2') {
    let gifts = [];
    try {
      gifts = decodeSendGiftV2(data.pb);
    } catch (error) {
      markDegraded(state, 'gift-v2-protobuf-invalid');
      if (state.options.verbose) console.warn('SEND_GIFT_V2 protobuf 解析失败:', error.message);
    }
    for (const gift of gifts) {
      if (state.options.verbose) {
        console.log('[gift-v2]', JSON.stringify({ giftName: gift.giftName, num: gift.num, price: gift.price, totalCoin: gift.totalCoin, coinType: gift.coinType }));
      }
      recordGift(state, gift);
    }
  } else if (cmd === 'SUPER_CHAT_MESSAGE') {
    const eventId = data.id == null || String(data.id) === '' ? null : String(data.id);
    const key = eventId
      ? `sc:${eventId}`
      : `sc:${data.uid ?? 'anonymous'}:${data.price ?? 0}:${data.time ?? ''}:${data.message ?? ''}`;
    if (!markUniqueEvent(state, key)) return;
    incrementCounter(state, 'superChatCount');
    addInteractiveUser(state, data.uid);
    const amount = Number(data.price ?? 0);
    let cents = 0;
    if (Number.isFinite(amount) && amount > 0 && amount <= Number.MAX_SAFE_INTEGER / 100) {
      cents = Math.round(amount * 100);
    } else if (data.price != null && (!Number.isFinite(amount) || amount < 0 || amount > Number.MAX_SAFE_INTEGER / 100)) {
      markDegraded(state, 'super-chat-price-invalid');
    }
    if (cents > 0) {
      addCents(state, 'revenueCents', cents);
      addCents(state, 'superChatRevenueCents', cents);
      addPaidUser(state, data.uid);
    }
  } else if (cmd === 'GUARD_BUY') {
    const key = `guard:${data.uid ?? 'anonymous'}:${data.guard_level ?? ''}:${data.start_time ?? data.timestamp ?? ''}:${data.price ?? 0}`;
    if (!markUniqueEvent(state, key)) return;
    incrementCounter(state, 'guardCount');
    addInteractiveUser(state, data.uid);
    const rawPrice = Number(data.price ?? 0);
    let cents = 0;
    if (Number.isSafeInteger(rawPrice) && rawPrice > 0) {
      cents = Math.floor(rawPrice / 10);
    } else if (data.price != null && (!Number.isSafeInteger(rawPrice) || rawPrice < 0)) {
      markDegraded(state, 'guard-price-invalid');
    }
    if (cents > 0) {
      addCents(state, 'revenueCents', cents);
      addCents(state, 'guardRevenueCents', cents);
      addPaidUser(state, data.uid);
    }
  } else if (cmd.startsWith('INTERACT_WORD') || cmd === 'ENTRY_EFFECT') {
    incrementCounter(state, 'enterCount');
  }
}

function markMalformedPacket(state, reason, error = null) {
  state.malformedPackets += 1;
  markDegraded(state, reason);
  if (state.options.verbose) {
    console.warn(`数据包已丢弃 (${reason}):`, error?.message ?? '格式非法');
  }
}

function parsePackets(state, buffer, depth = 0, budget = null) {
  if (!Buffer.isBuffer(buffer)) {
    markMalformedPacket(state, 'packet-not-buffer');
    return;
  }
  if (depth > MAX_PACKET_NESTING_DEPTH) {
    markMalformedPacket(state, 'packet-nesting-depth');
    return;
  }
  if (buffer.length === 0) {
    markMalformedPacket(state, 'packet-frame-empty');
    return;
  }
  if (buffer.length > MAX_DECOMPRESSED_BYTES) {
    markMalformedPacket(state, 'packet-frame-too-large');
    return;
  }
  const decompressionBudget = budget ?? { remaining: MAX_DECOMPRESSED_BYTES };
  let offset = 0;
  while (offset < buffer.length) {
    if (buffer.length - offset < 16) {
      markMalformedPacket(state, 'packet-truncated-header');
      return;
    }
    const packetLength = buffer.readUInt32BE(offset);
    if (packetLength < 16) {
      markMalformedPacket(state, 'packet-length-too-small');
      return;
    }
    const packetEnd = offset + packetLength;
    if (!Number.isSafeInteger(packetEnd) || packetEnd > buffer.length) {
      markMalformedPacket(state, 'packet-length-out-of-bounds');
      return;
    }
    const headerLength = buffer.readUInt16BE(offset + 4);
    if (headerLength < 16 || headerLength > packetLength) {
      markMalformedPacket(state, 'packet-header-length-invalid');
      return;
    }
    const version = buffer.readUInt16BE(offset + 6);
    const operation = buffer.readUInt32BE(offset + 8);
    const body = buffer.subarray(offset + headerLength, packetEnd);
    state.packets += 1;
    if (operation === 5) {
      if (version === 2 || version === 3) {
        if (decompressionBudget.remaining <= 0 || body.length > MAX_DECOMPRESSED_BYTES) {
          markMalformedPacket(state, 'decompressed-data-budget');
          offset = packetEnd;
          continue;
        }
        try {
          const options = { maxOutputLength: Math.min(MAX_DECOMPRESSED_BYTES, decompressionBudget.remaining) };
          const decoded = version === 2
            ? zlib.inflateSync(body, options)
            : zlib.brotliDecompressSync(body, options);
          if (decoded.length > decompressionBudget.remaining) {
            markMalformedPacket(state, 'decompressed-data-budget');
          } else {
            decompressionBudget.remaining -= decoded.length;
            parsePackets(state, decoded, depth + 1, decompressionBudget);
          }
        } catch (error) {
          markMalformedPacket(state, version === 2 ? 'zlib-inflate-failed' : 'brotli-inflate-failed', error);
        }
      } else {
        if (body.length > MAX_MESSAGE_BYTES) {
          markMalformedPacket(state, 'json-message-too-large');
        } else {
          try {
            const text = UTF8_DECODER.decode(body);
            parseMessage(state, JSON.parse(text));
          } catch (error) {
            markMalformedPacket(state, 'json-message-invalid', error);
          }
        }
      }
    } else if (operation === 8) {
      if (body.length > MAX_MESSAGE_BYTES) {
        markMalformedPacket(state, 'auth-message-too-large');
      } else {
        try {
          const text = UTF8_DECODER.decode(body);
          const response = JSON.parse(text);
          const code = String(response?.code ?? '');
          if (code === '0') {
            state.authConfirmed = true;
            state.authenticatedOnce = true;
            state.reconnectDelayMs = RECONNECT_DELAY_MS;
          } else {
            markDegraded(state, 'ws-auth-rejected');
            if (state.socket?.readyState === WebSocket.OPEN) {
              state.socket.close(4001, '认证失败');
            }
          }
          if (state.options.verbose) console.log(`[ws] 认证响应 code=${code || 'missing'}`);
        } catch (error) {
          markMalformedPacket(state, 'auth-message-invalid', error);
        }
      }
    }
    offset = packetEnd;
  }
}

async function handleSocketData(state, data) {
  state.wsFrames += 1;
  if (typeof data === 'string') {
    const bytes = Buffer.byteLength(data);
    state.wsRxBytes += bytes;
    state.appRxBytes += bytes;
    if (bytes > MAX_MESSAGE_BYTES) {
      markMalformedPacket(state, 'text-message-too-large');
      return;
    }
    try {
      parseMessage(state, JSON.parse(data));
    } catch (error) {
      markMalformedPacket(state, 'text-message-invalid', error);
    }
    return;
  }
  let buffer;
  if (data instanceof ArrayBuffer) buffer = Buffer.from(data);
  else if (ArrayBuffer.isView(data)) buffer = Buffer.from(data.buffer, data.byteOffset, data.byteLength);
  else if (typeof data?.arrayBuffer === 'function') buffer = Buffer.from(await data.arrayBuffer());
  else return;
  state.wsRxBytes += buffer.length;
  state.appRxBytes += buffer.length;
  parsePackets(state, buffer);
}

function sendSocketPacket(state, operation, payload = Buffer.alloc(0)) {
  if (!state.socket || state.socket.readyState !== WebSocket.OPEN) return false;
  const packet = makePacket(operation, payload);
  try {
    state.socket.send(packet);
  } catch (error) {
    markDegraded(state, 'ws-send-failed');
    if (state.options.verbose) console.warn('WebSocket 发送失败:', error.message);
    try { state.socket.close(4004, '发送失败'); } catch { /* 已关闭时忽略。 */ }
    return false;
  }
  state.wsTxBytes += packet.length;
  state.appTxBytes += packet.length;
  return true;
}

function hasOpenSocket(state) {
  return state.socket?.readyState === WebSocket.OPEN;
}

function clearReconnectTimer(state) {
  if (state.reconnectTimer !== null) {
    clearTimeout(state.reconnectTimer);
    state.reconnectTimer = null;
  }
  state.reconnectGeneration += 1;
}

function openSocket(state, url, token) {
  return new Promise((resolve, reject) => {
    const socket = new WebSocket(url);
    const generation = ++state.socketGeneration;
    state.socket = socket;
    state.authConfirmed = false;
    state.socketOpenedAt = 0;
    state.lastSocketActivityAt = Date.now();
    let settled = false;
    const isCurrent = () => state.socket === socket && state.socketGeneration === generation;
    const timeout = setTimeout(() => {
      if (!settled) {
        settled = true;
        if (isCurrent()) state.socket = null;
        try { socket.close(); } catch { /* 连接尚未完成时关闭可能失败。 */ }
        reject(new Error('连接超时'));
      }
    }, 12000);
    socket.binaryType = 'arraybuffer';
    socket.addEventListener('open', () => {
      if (settled || state.finishing || !isCurrent()) {
        if (!settled) {
          settled = true;
          clearTimeout(timeout);
          reject(new Error('连接已被新请求替代'));
        }
        try { socket.close(); } catch { /* 已被替代的连接无需继续。 */ }
        return;
      }
      settled = true;
      clearTimeout(timeout);
      clearReconnectTimer(state);
      state.socketOpenedAt = Date.now();
      state.lastSocketActivityAt = state.socketOpenedAt;
      const sent = sendSocketPacket(state, 7, JSON.stringify({
        uid: 0,
        roomid: Number(state.options.roomId),
        protover: 2,
        platform: 'web',
        type: 2,
        key: token,
      }));
      if (!sent) {
        if (isCurrent()) state.socket = null;
        reject(new Error('发送认证消息失败'));
        return;
      }
      resolve(socket);
    }, { once: true });
    socket.addEventListener('message', (event) => {
      if (!isCurrent()) return;
      state.lastSocketActivityAt = Date.now();
      handleSocketData(state, event.data).catch((error) => {
        markMalformedPacket(state, 'message-handling-failed', error);
        if (state.options.verbose) console.warn('消息处理失败:', error.message);
      });
    });
    socket.addEventListener('error', () => {
      if (!settled) {
        settled = true;
        clearTimeout(timeout);
        if (isCurrent()) state.socket = null;
        reject(new Error('WebSocket 错误'));
      }
    }, { once: true });
    socket.addEventListener('close', () => {
      clearTimeout(timeout);
      const current = isCurrent();
      if (current) {
        state.socket = null;
        state.authConfirmed = false;
        clearInterval(state.heartbeatTimer);
        state.heartbeatTimer = null;
      }
      if (!settled) {
        settled = true;
        reject(new Error('WebSocket 关闭'));
      }
      if (!state.finishing && current) {
        state.reconnects += 1;
        scheduleReconnect(state);
      }
    });
  });
}

async function connectSocket(state, danmuInfo) {
  if (state.finishing) return null;
  if (state.connecting) return state.socket;
  if (hasOpenSocket(state)) return state.socket;
  state.connecting = true;
  try {
    for (const hostInfo of danmuInfo.host_list.slice(0, MAX_WEBSOCKET_HOSTS)) {
      if (state.finishing) return null;
      const host = normalizeDanmuHost(hostInfo?.host);
      if (!host) continue;
      const requestedPort = Number(hostInfo?.wss_port ?? 2245);
      const port = Number.isInteger(requestedPort) && requestedPort > 0 && requestedPort <= 65535
        ? requestedPort
        : 2245;
      const url = port === 443 ? `wss://${host}/sub` : `wss://${host}:${port}/sub`;
      try {
        if (state.options.verbose) console.log(`[ws] 尝试连接 ${url}`);
        const socket = await openSocket(state, url, danmuInfo.token);
        if (state.finishing) {
          try { socket.close(); } catch { /* 采样结束，忽略关闭异常。 */ }
          return null;
        }
        clearReconnectTimer(state);
        console.log(`[ws] 已连接 ${host}`);
        clearInterval(state.heartbeatTimer);
        state.heartbeatTimer = setInterval(() => {
          if (!hasOpenSocket(state)) return;
          const now = Date.now();
          if (!state.authConfirmed && now - state.socketOpenedAt >= LIVE_AUTH_TIMEOUT_MS) {
            markDegraded(state, 'ws-auth-timeout');
            try { state.socket.close(4002, '认证超时'); } catch { /* 已关闭时忽略。 */ }
            return;
          }
          if (now - state.lastSocketActivityAt >= LIVE_SOCKET_IDLE_TIMEOUT_MS) {
            markDegraded(state, 'ws-idle-timeout');
            try { state.socket.close(4003, '连接失活'); } catch { /* 已关闭时忽略。 */ }
            return;
          }
          sendSocketPacket(state, 2);
        }, 30000);
        return socket;
      } catch (error) {
        if (state.options.verbose) console.warn(`[ws] 连接失败 ${host}: ${error.message}`);
      }
    }
    throw new Error('所有弹幕服务器均连接失败');
  } finally {
    state.connecting = false;
  }
}

function scheduleReconnect(state) {
  if (state.finishing || state.reconnectTimer !== null || hasOpenSocket(state)) return;
  const generation = ++state.reconnectGeneration;
  const delayMs = state.reconnectDelayMs;
  state.reconnectDelayMs = Math.min(state.reconnectDelayMs * 2, MAX_RECONNECT_DELAY_MS);
  state.reconnectTimer = setTimeout(async () => {
    if (generation !== state.reconnectGeneration) return;
    state.reconnectTimer = null;
    if (state.finishing || state.connecting || hasOpenSocket(state)) return;
    try {
      const danmuInfo = await getDanmuInfo(state);
      if (generation !== state.reconnectGeneration || state.finishing || state.connecting || hasOpenSocket(state)) return;
      await connectSocket(state, danmuInfo);
    } catch (error) {
      console.warn(`[ws] 重连失败: ${error.message}`);
      if (generation === state.reconnectGeneration) scheduleReconnect(state);
    }
  }, delayMs);
}

function printReport(state) {
  const elapsed = (performance.now() - state.startedAt) / 1000;
  console.log(
    `[${formatDuration(elapsed)}] 弹幕 ${state.danmakuCount} | 营收(估) ¥${(state.revenueCents / 100).toFixed(2)} | ` +
    `付费 ${state.paidUserCount} | 互动 ${state.interactiveUserCount} | 同接 ${state.highEnergy} | 质量 ${state.dataQuality}`,
  );
  console.log(
    `         应用层接收 ${formatBytes(state.appRxBytes)} (${formatRate(state.appRxBytes, elapsed)}) | ` +
    `WS ${formatBytes(state.wsRxBytes)} | HTTP ${formatBytes(state.httpRxBytes)} | 断线 ${state.reconnects}`,
  );
}

function printSummary(state) {
  const elapsed = (performance.now() - state.startedAt) / 1000;
  const systemRx = state.systemStart && state.systemEnd ? Math.max(0, state.systemEnd.rx - state.systemStart.rx) : null;
  const systemTx = state.systemStart && state.systemEnd ? Math.max(0, state.systemEnd.tx - state.systemStart.tx) : null;
  console.log('\n=== 直播统计 POC 结果 ===');
  console.log(`直播间: ${state.roomInfo?.title ?? '-'} (${state.options.roomId})`);
  console.log(`开播时间: ${state.roomStartText ?? '-'}`);
  console.log(`本次采样: ${formatDuration(elapsed)}`);
  console.log(`弹幕数: ${state.danmakuCount}`);
  console.log(`营收(估): ¥${(state.revenueCents / 100).toFixed(2)}`);
  console.log(`  礼物: ¥${(state.giftRevenueCents / 100).toFixed(2)} (${state.giftCount} 次事件)`);
  console.log(`  SC:   ¥${(state.superChatRevenueCents / 100).toFixed(2)} (${state.superChatCount} 次事件)`);
  console.log(`  舰长: ¥${(state.guardRevenueCents / 100).toFixed(2)} (${state.guardCount} 次事件)`);
  console.log(`付费人数(估): ${state.paidUserCount}`);
  console.log(`互动人数(估): ${state.interactiveUserCount}`);
  console.log(`弹幕用户数: ${state.danmakuUserCount}`);
  console.log(`最高同时在线: ${state.peakHighEnergy}`);
  console.log(`入场/进场事件: ${state.enterCount}`);
  console.log(`WebSocket 断线次数: ${state.reconnects}`);
  console.log(`数据质量: ${state.dataQuality === 'complete' ? '完整' : '部分缺失'}`);
  if (state.dataQualityReasons.size > 0) {
    console.log(`数据质量降级原因: ${[...state.dataQualityReasons].join(', ')}`);
  }
  if (state.malformedPackets > 0) console.log(`已丢弃异常数据包: ${state.malformedPackets}`);
  console.log('\n=== 网络带宽 ===');
  console.log(`应用层接收: ${formatBytes(state.appRxBytes)} | 平均 ${formatRate(state.appRxBytes, elapsed)}`);
  console.log(`  其中 WebSocket: ${formatBytes(state.wsRxBytes)}`);
  console.log(`  其中 HTTP:       ${formatBytes(state.httpRxBytes)}`);
  console.log(`应用层发送: ${formatBytes(state.appTxBytes)} | 平均 ${formatRate(state.appTxBytes, elapsed)}`);
  if (systemRx != null) {
    console.log(`系统网卡接收: ${formatBytes(systemRx)} | 平均 ${formatRate(systemRx, elapsed)}`);
    console.log(`系统网卡发送: ${formatBytes(systemTx)} | 平均 ${formatRate(systemTx, elapsed)}`);
    console.log(`网卡: ${state.systemStart.alias}`);
    console.log('注意: 系统网卡数据包含脚本之外的其他后台流量。');
  } else if (state.options.systemNetwork) {
    console.log('系统网卡接收: 未取得（仅显示应用层流量）');
  } else {
    console.log('系统网卡接收: 未启用（应用层流量为脚本自身）');
  }
  console.log('\n原始事件落盘: 否');
  console.log('\n事件类型计数:');
  const sorted = [...state.eventCounts.entries()].sort((a, b) => b[1] - a[1]);
  for (const [name, count] of sorted.slice(0, 15)) console.log(`  ${name}: ${count}`);
}

function finish(state, code = 0) {
  if (state.finishing) return;
  state.finishing = true;
  if (!state.authenticatedOnce) markDegraded(state, 'ws-never-authenticated');
  clearInterval(state.reportTimer);
  clearInterval(state.heartbeatTimer);
  clearReconnectTimer(state);
  clearTimeout(state.finishTimer);
  if (state.socket) {
    try { state.socket.close(); } catch { /* 已断开的 socket 无需处理。 */ }
  }
  state.systemEnd = state.options.systemNetwork ? readNetworkCounters() : null;
  printSummary(state);
  process.exitCode = code;
  setTimeout(() => process.exit(code), 150);
}

async function main() {
  if (typeof WebSocket !== 'function') throw new Error('需要 Node.js 22+（当前缺少全局 WebSocket）');
  const options = parseArgs(process.argv.slice(2));
  const state = createState(options);
  activeState = state;
  state.systemStart = options.systemNetwork ? readNetworkCounters() : null;
  if (options.systemNetwork && !state.systemStart) console.warn('无法读取 Windows 网卡统计，将只显示应用层流量。');
  console.log('=== B 站直播统计 POC ===');
  console.log(`直播间: ${options.roomId}`);
  console.log(`采样时长: ${options.durationSeconds}s | 上报间隔: ${options.reportSeconds}s`);
  console.log('原始事件只保存在内存，不写磁盘。\n');
  const room = await loadRoomInfo(state);
  console.log(`房间标题: ${room.title}`);
  console.log(`直播状态: ${room.live_status === 1 ? '直播中' : '未开播'}`);
  if (room.live_status !== 1) console.warn('当前未开播，WebSocket 可能收不到互动事件。');
  await connectSocket(state, await getDanmuInfo(state));
  state.reportTimer = setInterval(() => printReport(state), options.reportSeconds * 1000);
  state.finishTimer = setTimeout(() => finish(state), options.durationSeconds * 1000);
  printReport(state);
}

process.on('SIGINT', () => {
  if (activeState) finish(activeState, 0);
});

main().catch((error) => {
  console.error(`启动失败: ${error.message}`);
  if (activeState) activeState.systemEnd = activeState.options.systemNetwork ? readNetworkCounters() : null;
  process.exitCode = 1;
});











