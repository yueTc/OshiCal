#!/usr/bin/env node
import crypto from 'node:crypto';
import zlib from 'node:zlib';
import { execFileSync } from 'node:child_process';
import { performance } from 'node:perf_hooks';

const DEFAULT_ROOM_ID = '22625027';
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
}

function getWbiKeys(navData) {
  const imgUrl = navData?.data?.wbi_img?.img_url;
  const subUrl = navData?.data?.wbi_img?.sub_url;
  if (!imgUrl || !subUrl) throw new Error('B 站 nav 响应缺少 WBI key');
  const raw = imgUrl.split('/').pop().split('.')[0] + subUrl.split('/').pop().split('.')[0];
  return MIXIN_KEY_TAB.map((index) => raw[index]).join('').slice(0, 32);
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
    superChatRevenueCents: 0, guardRevenueCents: 0, interactiveUsers: new Set(),
    paidUsers: new Set(), danmakuUsers: new Set(), eventCounts: new Map(),
    eventDedup: new Map(), appRxBytes: 0, appTxBytes: 0, httpRxBytes: 0,
    wsRxBytes: 0, wsTxBytes: 0, wsFrames: 0, packets: 0, reconnects: 0,
    socket: null, heartbeatTimer: null, reconnectTimer: null,
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
  if (data.code !== 0 || !data.data?.token || !data.data?.host_list?.length) {
    throw new Error(`获取弹幕服务器失败: ${data.code} ${data.message ?? ''}`);
  }
  return data.data;
}

function decodeVarint(buffer, start) {
  let value = 0n;
  let shift = 0n;
  let offset = start;
  while (offset < buffer.length && shift < 70n) {
    const byte = buffer[offset];
    offset += 1;
    value |= BigInt(byte & 0x7f) << shift;
    if ((byte & 0x80) === 0) return { value, offset };
    shift += 7n;
  }
  throw new Error('非法 protobuf varint');
}

function decodeProtobufFields(buffer) {
  const fields = new Map();
  let offset = 0;
  while (offset < buffer.length) {
    const tag = decodeVarint(buffer, offset);
    offset = tag.offset;
    const fieldNumber = Number(tag.value >> 3n);
    const wireType = Number(tag.value & 7n);
    let value;
    if (wireType === 0) {
      const decoded = decodeVarint(buffer, offset);
      value = decoded.value;
      offset = decoded.offset;
    } else if (wireType === 1) {
      value = buffer.subarray(offset, offset + 8);
      offset += 8;
    } else if (wireType === 2) {
      const length = decodeVarint(buffer, offset);
      offset = length.offset;
      const end = offset + Number(length.value);
      value = buffer.subarray(offset, end);
      offset = end;
    } else if (wireType === 5) {
      value = buffer.subarray(offset, offset + 4);
      offset += 4;
    } else {
      throw new Error(`不支持的 protobuf wire type: ${wireType}`);
    }
    if (!fields.has(fieldNumber)) fields.set(fieldNumber, []);
    fields.get(fieldNumber).push(value);
  }
  return fields;
}

function fieldNumber(fields, id) {
  const value = fields.get(id)?.[0];
  return typeof value === 'bigint' ? Number(value) : 0;
}

function fieldString(fields, id) {
  const value = fields.get(id)?.[0];
  return Buffer.isBuffer(value) ? value.toString('utf8') : '';
}

function decodeSendGiftV2(base64) {
  if (!base64) return [];
  const root = decodeProtobufFields(Buffer.from(base64, 'base64'));
  const uid = fieldNumber(root, 1);
  return (root.get(10) ?? []).map((buffer) => {
    const gift = decodeProtobufFields(buffer);
    return {
      uid,
      giftId: fieldNumber(gift, 1),
      giftName: fieldString(gift, 2),
      num: fieldNumber(gift, 3),
      price: fieldNumber(gift, 5),
      totalCoin: fieldNumber(gift, 7),
      coinType: fieldString(gift, 8),
      tid: fieldString(gift, 9),
      timestamp: fieldNumber(gift, 10),
      comboTotalCoin: fieldNumber(gift, 14),
    };
  });
}

function recordGift(state, gift) {
  state.giftCount += 1;
  addInteractiveUser(state, gift.uid);
  const coins = gift.totalCoin > 0 ? gift.totalCoin : gift.price * gift.num;
  const cents = Number.isFinite(coins) && coins > 0 && gift.coinType !== 'silver'
    ? Math.round(coins / 10)
    : 0;
  const key = gift.tid
    ? `gift:${gift.tid}`
    : `gift:${gift.uid}:${gift.giftId}:${gift.num}:${gift.timestamp}:${gift.price}`;
  if (cents > 0 && markUniqueEvent(state, key)) {
    state.revenueCents += cents;
    state.giftRevenueCents += cents;
    addPaidUser(state, gift.uid);
  }
}
function noteEvent(state, cmd) {
  state.eventCounts.set(cmd, (state.eventCounts.get(cmd) ?? 0) + 1);
}

function markUniqueEvent(state, key) {
  const now = Date.now();
  const existing = state.eventDedup.get(key);
  if (existing !== undefined && now - existing < 10 * 60 * 1000) return false;
  state.eventDedup.set(key, now);
  if (state.eventDedup.size > 25000) {
    for (const [oldKey, timestamp] of state.eventDedup) {
      if (now - timestamp > 10 * 60 * 1000) state.eventDedup.delete(oldKey);
      if (state.eventDedup.size <= 20000) break;
    }
  }
  return true;
}

function addInteractiveUser(state, uid) {
  if (uid != null && uid !== 0) state.interactiveUsers.add(String(uid));
}

function addPaidUser(state, uid) {
  if (uid != null && uid !== 0) {
    state.interactiveUsers.add(String(uid));
    state.paidUsers.add(String(uid));
  }
}

function parseMessage(state, message) {
  const cmd = String(message?.cmd ?? '').split(':')[0];
  if (!cmd) return;
  noteEvent(state, cmd);
  const data = message.data ?? {};
  if (cmd === 'ONLINE_RANK_COUNT') {
    state.highEnergy = Math.max(0, Number(data.online_count ?? data.count ?? 0) || 0);
    state.peakHighEnergy = Math.max(state.peakHighEnergy, state.highEnergy);
  } else if (cmd === 'DANMU_MSG') {
    state.danmakuCount += 1;
    const uid = message.info?.[2]?.[0];
    if (uid != null) {
      state.danmakuUsers.add(String(uid));
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
      num: Number(data.num || 1),
      price: Number(data.price || 0),
      totalCoin: Number(data.total_coin || 0),
      coinType: data.coin_type,
      tid: data.tid,
      timestamp: Number(data.timestamp || 0),
    });
  } else if (cmd === 'SEND_GIFT_V2') {
    let gifts = [];
    try {
      gifts = decodeSendGiftV2(data.pb);
    } catch (error) {
      if (state.options.verbose) console.warn('SEND_GIFT_V2 protobuf 解析失败:', error.message);
    }
    for (const gift of gifts) {
      if (state.options.verbose) {
        console.log('[gift-v2]', JSON.stringify({ giftName: gift.giftName, num: gift.num, price: gift.price, totalCoin: gift.totalCoin, coinType: gift.coinType }));
      }
      recordGift(state, gift);
    }  } else if (cmd === 'SUPER_CHAT_MESSAGE') {
    state.superChatCount += 1;
    addInteractiveUser(state, data.uid);
    const cents = Math.round(Number(data.price || 0) * 100);
    const key = data.id ? `sc:${data.id}` : `sc:${data.uid}:${data.message}:${data.time}`;
    if (cents > 0 && markUniqueEvent(state, key)) {
      state.revenueCents += cents;
      state.superChatRevenueCents += cents;
      addPaidUser(state, data.uid);
    }
  } else if (cmd === 'GUARD_BUY') {
    state.guardCount += 1;
    addInteractiveUser(state, data.uid);
    const rawPrice = Number(data.price || 0);
    const cents = rawPrice > 0 ? Math.round(rawPrice / 10) : 0;
    const key = `guard:${data.uid}:${data.guard_level}:${data.start_time ?? data.timestamp}`;
    if (cents > 0 && markUniqueEvent(state, key)) {
      state.revenueCents += cents;
      state.guardRevenueCents += cents;
      addPaidUser(state, data.uid);
    }
  } else if (cmd.startsWith('INTERACT_WORD') || cmd === 'ENTRY_EFFECT') {
    state.enterCount += 1;
  }
}

function parsePackets(state, buffer) {
  let offset = 0;
  while (offset + 16 <= buffer.length) {
    const packetLength = buffer.readUInt32BE(offset);
    if (packetLength < 16 || offset + packetLength > buffer.length) break;
    const headerLength = buffer.readUInt16BE(offset + 4);
    const version = buffer.readUInt16BE(offset + 6);
    const operation = buffer.readUInt32BE(offset + 8);
    const body = buffer.subarray(offset + headerLength, offset + packetLength);
    state.packets += 1;
    if (operation === 5) {
      if (version === 2) {
        try { parsePackets(state, zlib.inflateSync(body)); } catch (error) {
          if (state.options.verbose) console.warn('zlib 解压失败:', error.message);
        }
      } else if (version === 3) {
        try { parsePackets(state, zlib.brotliDecompressSync(body)); } catch (error) {
          if (state.options.verbose) console.warn('brotli 解压失败:', error.message);
        }
      } else {
        try { parseMessage(state, JSON.parse(body.toString('utf8'))); } catch (error) {
          if (state.options.verbose) console.warn('JSON 解析失败:', error.message);
        }
      }
    } else if (operation === 8 && state.options.verbose) {
      console.log(`[ws] 认证响应: ${body.toString('utf8').slice(0, 160)}`);
    }
    offset += packetLength;
  }
}

async function handleSocketData(state, data) {
  state.wsFrames += 1;
  if (typeof data === 'string') {
    const bytes = Buffer.byteLength(data);
    state.wsRxBytes += bytes;
    state.appRxBytes += bytes;
    try { parseMessage(state, JSON.parse(data)); } catch (error) {
      if (state.options.verbose) console.warn('文本消息解析失败:', error.message);
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
  if (!state.socket || state.socket.readyState !== WebSocket.OPEN) return;
  const packet = makePacket(operation, payload);
  state.socket.send(packet);
  state.wsTxBytes += packet.length;
  state.appTxBytes += packet.length;
}

function openSocket(state, url, token) {
  return new Promise((resolve, reject) => {
    const socket = new WebSocket(url);
    state.socket = socket;
    let settled = false;
    const timeout = setTimeout(() => {
      if (!settled) {
        settled = true;
        state.socket = null;
        try { socket.close(); } catch { /* 连接尚未完成时关闭可能失败。 */ }
        reject(new Error('连接超时'));
      }
    }, 12000);
    socket.binaryType = 'arraybuffer';
    socket.addEventListener('open', () => {
      if (settled) return;
      settled = true;
      clearTimeout(timeout);
      sendSocketPacket(state, 7, JSON.stringify({
        uid: 0,
        roomid: Number(state.options.roomId),
        protover: 2,
        platform: 'web',
        type: 2,
        key: token,
      }));
      resolve(socket);
    }, { once: true });
    socket.addEventListener('message', (event) => {
      handleSocketData(state, event.data).catch((error) => {
        if (state.options.verbose) console.warn('消息处理失败:', error.message);
      });
    });
    socket.addEventListener('error', () => {
      if (!settled) {
        settled = true;
        clearTimeout(timeout);
        state.socket = null;
        reject(new Error('WebSocket 错误'));
      }
    }, { once: true });
    socket.addEventListener('close', () => {
      clearTimeout(timeout);
      if (state.socket === socket) state.socket = null;
      if (!state.finishing) {
        state.reconnects += 1;
        scheduleReconnect(state);
      }
    });
  });
}

async function connectSocket(state, danmuInfo) {
  for (const hostInfo of danmuInfo.host_list) {
    if (state.finishing) return null;
    const url = `wss://${hostInfo.host}:${hostInfo.wss_port || 2245}/sub`;
    try {
      if (state.options.verbose) console.log(`[ws] 尝试连接 ${url}`);
      const socket = await openSocket(state, url, danmuInfo.token);
      console.log(`[ws] 已连接 ${hostInfo.host}`);
      clearInterval(state.heartbeatTimer);
      state.heartbeatTimer = setInterval(() => sendSocketPacket(state, 2), 30000);
      return socket;
    } catch (error) {
      if (state.options.verbose) console.warn(`[ws] 连接失败 ${hostInfo.host}: ${error.message}`);
    }
  }
  throw new Error('所有弹幕服务器均连接失败');
}

function scheduleReconnect(state) {
  if (state.finishing || state.reconnectTimer) return;
  state.reconnectTimer = setTimeout(async () => {
    state.reconnectTimer = null;
    if (state.finishing) return;
    try {
      await connectSocket(state, await getDanmuInfo(state));
    } catch (error) {
      console.warn(`[ws] 重连失败: ${error.message}`);
      scheduleReconnect(state);
    }
  }, 3000);
}

function printReport(state) {
  const elapsed = (performance.now() - state.startedAt) / 1000;
  console.log(
    `[${formatDuration(elapsed)}] 弹幕 ${state.danmakuCount} | 营收(估) ¥${(state.revenueCents / 100).toFixed(2)} | ` +
    `付费 ${state.paidUsers.size} | 互动 ${state.interactiveUsers.size} | 同接 ${state.highEnergy}`,
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
  console.log(`付费人数(估): ${state.paidUsers.size}`);
  console.log(`互动人数(估): ${state.interactiveUsers.size}`);
  console.log(`弹幕用户数: ${state.danmakuUsers.size}`);
  console.log(`最高同时在线: ${state.peakHighEnergy}`);
  console.log(`入场/进场事件: ${state.enterCount}`);
  console.log(`WebSocket 断线次数: ${state.reconnects}`);
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
  clearInterval(state.reportTimer);
  clearInterval(state.heartbeatTimer);
  clearTimeout(state.reconnectTimer);
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











