export type DataMode = "local" | "server";
export type ViewMode = "timeline" | "grid";
export type Platform = "bilibili" | "youtube";
export type SourceKind = "text" | "reserve" | "image" | "manual" | "editor" | "weekly" | "live" | "youtube_community" | "youtube_live";

/** 思考程度：`auto` 表示请求体不发送 `reasoning_effort` 字段。 */
export type LlmReasoningEffort = "auto" | "low" | "medium" | "high";

export const LLM_REASONING_EFFORTS: readonly LlmReasoningEffort[] = ["auto", "low", "medium", "high"];

export const LLM_PARALLELISM_MIN = 1;

export const LLM_PARALLELISM_MAX = 8;

export interface AppSettings {
  dataMode: DataMode;
  serverUrl: string;
  livePollSeconds: number;
  dynamicPollSeconds: number;
  autoAnalyzeImages: boolean;
  notificationsEnabled: boolean;
  liveStatsEnabled: boolean;
  fertilityEnabled: boolean;
  /** YouTube 总开关；缺省表示关闭，旧设置不会触发任何 YouTube 请求。 */
  youtubeEnabled?: boolean;
  llmBaseUrl: string;
  llmModel: string;
  llmPromptVersion: string;
  /** 缺省 `"auto"`：不向模型发送 `reasoning_effort`。 */
  llmReasoningEffort?: LlmReasoningEffort;
  /** 并行个数，合法区间 1..8；表示同时向同一个模型端点发起的请求数。 */
  llmParallelism?: number;
  proxyPort: number | null;
}

export interface SecretStatus {
  llmConfigured: boolean;
  biliConfigured: boolean;
  serverTokenConfigured: boolean;
}

export interface YoutubeCredentialStatus {
  apiKeyConfigured: boolean;
  apiKeyValid?: boolean | null;
  oauthConfigured: boolean;
  /** Google OAuth 令牌已过期；与“未授权”分开显示，但不暴露令牌内容。 */
  oauthExpired?: boolean;
  oauthAccount?: string | null;
  webSessionConfigured: boolean;
  webSessionAccount?: string | null;
  checkedAt?: string | null;
  lastError?: string | null;
}

export interface YoutubeOAuthStart {
  authorizationUrl: string;
  state?: string | null;
  expiresInSeconds?: number;
}

export interface YoutubeWebLoginStart {
  loginUrl?: string | null;
  message?: string | null;
}

export interface BiliQrLoginStart {
  qrcodeKey: string;
  url: string;
  expiresInSeconds: number;
}

export type BiliQrStatus = "waiting" | "scanned" | "confirmed" | "expired";

export interface BiliQrLoginPoll {
  status: BiliQrStatus;
  message: string;
  cookieSaved: boolean;
}

export interface ServerBiliQrLoginStart {
  sessionId: string;
  url: string;
  expiresInSeconds: number;
}

export interface ServerBiliQrLoginPoll {
  status: BiliQrStatus;
  message: string;
  configured: boolean;
}

export interface ServerBiliStatus {
  configured: boolean;
}

export interface AccountSearchResult {
  platform?: Platform;
  mid: string;
  name: string;
  avatarUrl: string;
  sign: string;
  handle?: string | null;
  profileUrl?: string | null;
  uploadsPlaylistId?: string | null;
  roomId: string | null;
  fans: number;
  isLive: boolean;
  liveUrl?: string | null;
}

export interface LinkedAccount {
  id?: number;
  streamerId?: number;
  platform?: Platform;
  mid: string;
  name: string;
  avatarUrl: string;
  handle?: string | null;
  profileUrl?: string | null;
  revision?: number;
  createdAt: string;
}

export interface Streamer {
  id: number;
  platform?: Platform;
  mid: string;
  name: string;
  avatarUrl: string;
  handle?: string | null;
  profileUrl?: string | null;
  uploadsPlaylistId?: string | null;
  roomId: string | null;
  liveUrl: string | null;
  aliases: string[];
  scheduleEnabled: boolean;
  liveNotifyEnabled: boolean;
  liveStatsEnabled: boolean;
  fans: number | null;
  lastError: string | null;
  lastRefreshAt: string | null;
  isLive: boolean;
  liveTitle: string | null;
  liveCover: string | null;
  liveVideoId?: string | null;
  liveStartedAt: string | null;
  liveCheckedAt: string | null;
  linkedAccounts: LinkedAccount[];
  sortOrder: number;
  revision?: number;
  createdAt: string;
}

export interface Schedule {
  cachedLocalId?: number;
  remoteId?: number;
  remoteUpdatedAt?: string;
  id: number;
  streamerId: number;
  streamerName: string;
  streamerAvatar: string;
  participants: string[];
  title: string;
  startAt: string;
  endAt: string | null;
  roomUrl: string | null;
  roomOwnerMid: string | null;
  roomOwnerName: string | null;
  sourceKind: SourceKind;
  sourcePostId: string | null;
  sourceImageUrl: string | null;
  sourceAuthorMid: string | null;
  sourceFingerprint: string | null;
  sourceIdentity: string | null;
  sourcePublishedAt: string | null;
  weeklyRuleId: number | null;
  weeklyOccurrenceDate: string | null;
  confidence: number;
  manualLocked: boolean;
  sourceChanged: boolean;
  cancelled: boolean;
  createdAt: string;
  updatedAt: string;
  revision?: number;
}

export interface WeeklyScheduleRule {
  id: number;
  ruleKey: string;
  streamerId: number;
  title: string;
  weekdaysMask: number;
  startTime: string;
  endTime: string | null;
  timezone: string;
  roomUrl: string | null;
  effectiveFrom: string;
  effectiveUntil: string | null;
  enabled: boolean;
  revision?: number;
  createdAt: string;
  updatedAt: string;
}

export interface NewWeeklyScheduleRuleInput {
  ruleKey?: string;
  streamerId: number;
  title: string;
  weekdaysMask: number;
  startTime: string;
  endTime: string | null;
  timezone?: string;
  roomUrl: string | null;
  effectiveFrom: string;
  effectiveUntil: string | null;
  enabled: boolean;
}

export interface UpdateWeeklyScheduleRuleInput {
  title?: string;
  weekdaysMask?: number;
  startTime?: string;
  endTime?: string | null;
  clearEndTime?: boolean;
  roomUrl?: string | null;
  clearRoomUrl?: boolean;
  effectiveFrom?: string;
  effectiveUntil?: string | null;
  clearEffectiveUntil?: boolean;
  enabled?: boolean;
}

export interface LiveSession {
  scheduleId?: number | null;
  id: number;
  streamerId: number;
  streamerName: string;
  /** 后端显式提供时优先使用，缺失时才回退到 roomId 推断。 */
  platform?: Platform;
  roomId: string;
  title: string;
  coverUrl: string | null;
  startedAt: string;
  endedAt: string | null;
  durationSeconds: number;
  peakOnline: number;
  peakHighEnergy: number;
  avgOnline: number;
  onlineSamples: number;
  onlineTotal: number;
  revenueCents: number;
  giftRevenueCents: number;
  superChatRevenueCents: number;
  guardRevenueCents: number;
  paidUserCount: number;
  interactiveUserCount: number;
  danmakuCount: number;
  giftCount: number;
  superChatCount: number;
  guardCount: number;
  /** YouTube 会员事件数；null 表示 OAuth 统计从未可用，不代表 0。 */
  membershipCount?: number | null;
  /** YouTube 统计可用性；null 表示旧数据或尚不可知，false 表示已明确不可用。 */
  youtubeStatsAvailable?: boolean | null;
  reconnectCount: number;
  /** 本场由几段拼成：主播断流重开后并回同一场，段数递增。 */
  segmentCount: number;
  dataQuality: string;
  collectorState: string;
  createdAt: string;
  updatedAt: string;
}
export interface LiveOnlineSample {
  id: number;
  sessionId: number;
  sampledAt: string;
  observedAt: string | null;
  highEnergyCount: number | null;
  collectorState: string;
}

export type LiveOnlineMetric = "bilibili_high_energy_users" | "youtube_concurrent_viewers";

/** YouTube Super Chat / Super Sticker totals grouped by original currency. */
export interface LiveSuperChatTotal {
  currency: string | null;
  amountMicros: number;
  eventCount: number;
}

export interface LiveOnlineSamplesPage {
  sessionId: number;
  platform?: Platform;
  metric: LiveOnlineMetric;
  sampleIntervalSeconds: number;
  samples: LiveOnlineSample[];
  nextAfterId: number;
  hasMore: boolean;
  /** `null` means YouTube OAuth-backed collection was unavailable, not zero. */
  membershipCount?: number | null;
  /** `null` means the server did not report whether OAuth event collection worked. */
  statsAvailable?: boolean | null;
  superChatTotals?: LiveSuperChatTotal[];
}

export interface FanSnapshot {
  capturedAt: string;
  fans: number;
}

export interface FanHistory {
  streamerId: number;
  currentFans: number | null;
  checkedAt: string | null;
  snapshots: FanSnapshot[];
}

export interface LiveStatus {
  scheduleId?: number | null;
  scheduleKnown?: boolean;
  platform?: Platform;
  liveVideoId?: string | null;
  streamerId: number;
  mid: string;
  streamerName: string;
  avatarUrl: string;
  isLive: boolean;
  title: string;
  coverUrl: string | null;
  liveUrl: string | null;
  online: number;
  startedAt: string | null;
  checkedAt: string;
  liveNotifyEnabled: boolean;
  revision?: number;
}

export interface FertilityStatus {
  streamerId: number;
  mid: string;
  streamerName: string;
  streamerAvatar: string;
  status: "menstruating" | "fertile" | "ovulating" | "normal" | string;
  dayInCycle: number | null;
  ovulationDay: number | null;
  nextPeriod: string | null;
  cycleLength: number | null;
  periodLength: number | null;
  dataPoints: number;
  matchedMid: string;
}

export interface FertilitySnapshot {
  /** 「寻血猎犬」开关状态；false 时服务端不会抓取，也不会读取缓存。 */
  enabled: boolean;
  fetchedAt: string;
  source: string;
  stale: boolean;
  warning: string | null;
  totalAvailable: number;
  matched: number;
  statuses: FertilityStatus[];
}

export interface Bootstrap {
  settings: AppSettings;
  subscriptions: Streamer[];
  liveStatuses: LiveStatus[];
  secrets: SecretStatus;
}

export interface RefreshReport {
  refreshedStreamers: number;
  failedStreamers: string[];
  newSchedules: number;
  cancelledSchedules: number;
  llmCalls: number;
  llmCacheHits: number;
}

export interface RefreshProgress {
  streamerId: number;
  streamerName: string;
  stage: string;
  completed: number;
  total: number;
  percent: number;
  done: boolean;
  failed: boolean;
}

export interface LlmSecret {
  configured: boolean;
  revision: number;
  apiKey?: string | null;
  boundBaseUrl?: string | null;
}

export interface LlmTestInput {
  baseUrl: string;
  apiKey?: string;
  model: string;
  promptVersion: string;
  /** `auto` 时后端不发送 `reasoning_effort`。 */
  reasoningEffort?: LlmReasoningEffort;
}

export interface LlmDiscoveryInput {
  baseUrl: string;
  apiKey?: string;
}

export interface LlmTestReport {
  connected: boolean;
  latencyMs: number;
  eventCount: number;
  expectedCount: number;
  matchedCount: number;
  successRate: number;
  message: string;
}

export interface ProxyTestReport {
  connected: boolean;
  latencyMs: number;
  target: string;
  proxyUrl: string;
  statusCode: number | null;
  message: string;
}

export interface SyncMutation {
  mutationId: string;
  operation: "create" | "update" | "delete";
  entity: "subscription" | "schedule" | "link" | "weekly_rule";
  payload: unknown;
  baseUpdatedAt: string | null;
  status: string;
  lastError: string | null;
  createdAt: string;
  appliedAt: string | null;
  deviceId?: string | null;
  baseRevision?: number | null;
  resultJson?: unknown;
}

export interface LiveEventPayload {
  streamerId: number;
  mid: string;
  streamerName: string;
  previousIsLive: boolean;
  isLive: boolean;
  title: string;
  coverUrl?: string | null;
  liveUrl?: string | null;
  online?: number;
  startedAt?: string | null;
  checkedAt: string;
  liveNotifyEnabled: boolean;
  revision?: number;
}

export interface ServerEvent {
  replayed?: boolean;
  seq?: number;
  type: string;
  createdAt: string;
  payload: unknown;
}

export interface LiveBroadcast {
  id: string;
  streamerId: number;
  roomId: string;
  startedAt: string;
  lastSegmentStartedAt: string | null;
  lastSeenAt: string;
  offlineSince: string | null;
  scheduleId: number | null;
  scheduleDecided: boolean;
  segmentCount: number;
}

export interface ServerSnapshot {
  liveBroadcasts?: LiveBroadcast[];
  schemaVersion: number;
  subscriptions: Streamer[];
  schedules: Schedule[];
  weeklyRules?: WeeklyScheduleRule[];
  weeklyExceptions?: WeeklyScheduleException[];
  scheduleSources?: ScheduleSource[];
  sourceSuppressions?: SourceSuppression[];
  latestEventSeq: number;
  exportedAt: string;
}

export interface NewScheduleInput {
  streamerId: number;
  participants?: string[];
  title: string;
  startAt: string;
  endAt: string | null;
  roomUrl: string | null;
  roomOwnerMid?: string | null;
  roomOwnerName?: string | null;
  sourceKind: SourceKind;
  sourcePostId: string | null;
  sourceImageUrl: string | null;
  sourceAuthorMid: string | null;
  sourceFingerprint: string | null;
  sourceIdentity: string | null;
  sourcePublishedAt?: string | null;
  confidence: number;
}

export interface UpdateScheduleInput {
  title?: string;
  startAt?: string;
  endAt?: string | null;
  roomUrl?: string | null;
}

export interface UpdateSubscriptionInput {
  scheduleEnabled?: boolean;
  liveNotifyEnabled?: boolean;
  liveStatsEnabled?: boolean;
  aliases?: string[];
}


export interface ImportPreview {
  subscriptions: number;
  schedules: number;
  weeklyRules: number;
  serverStateDigest?: string;
  conflicts: { entity: string; label: string; localId: number; remoteId: number;
    local: unknown; remote: unknown }[];
}
export interface MigrationPreview extends ImportPreview {
  serverStateDigest: string;
}

export interface ImportReport {
  subscriptions: number;
  schedules: number;
  weeklyRules: number;
  skipped: number;
}








export type SyncOperation = "upsert" | "delete";

export interface SyncChange {
  seq: number;
  entityType: string;
  entityId: string;
  operation: SyncOperation;
  entityRevision: number;
  payload: unknown | null;
  createdAt: string;
}

export interface SyncBatch {
  fromSeq: number;
  toSeq: number;
  latestSeq: number;
  feedId?: string;
  changes: SyncChange[];
}

export interface SyncChangesPage {
  schemaVersion: number;
  feedId?: string;
  fromSeq: number;
  toSeq: number;
  latestSeq: number;
  minRetainedSeq: number;
  hasMore: boolean;
  resetRequired: boolean;
  changes: SyncChange[];
}

export interface WeeklyScheduleException {
  id: number;
  ruleId: number;
  occurrenceDate: string;
  exceptionKind: string;
  scheduleId: number | null;
  revision?: number;
  createdAt: string;
}

export interface ScheduleSource {
  id: number;
  scheduleId: number;
  platform?: Platform;
  sourceKind: SourceKind;
  sourcePostId: string | null;
  sourceImageUrl: string | null;
  sourceFingerprint: string | null;
  sourceIdentity: string;
  confidence: number;
  revision?: number;
  createdAt: string;
}

export interface SourceSuppression {
  id: number;
  platform?: Platform;
  streamerId: number;
  sourcePostId: string;
  sourceFingerprint: string;
  revision?: number;
  createdAt: string;
}

export interface ServerSettings {
  livePollSeconds?: number;
  dynamicPollSeconds?: number;
  autoAnalyzeImages?: boolean;
  liveStatsEnabled?: boolean;
  fertilityEnabled?: boolean;
  youtubeEnabled?: boolean;
  llmBaseUrl?: string;
  llmModel?: string;
  llmPromptVersion?: string;
  llmReasoningEffort?: LlmReasoningEffort;
  llmParallelism?: number;
  proxyPort?: number | null;
  revision?: number;
}

export interface SyncBootstrap {
  schemaVersion: number;
  feedId?: string;
  snapshotRevision: number;
  minRetainedSeq: number;
  serverTime: string;
  subscriptions: Streamer[];
  schedules: Schedule[];
  weeklyRules: WeeklyScheduleRule[];
  weeklyExceptions: WeeklyScheduleException[];
  scheduleSources: ScheduleSource[];
  sourceSuppressions: SourceSuppression[];
  serverSettings: ServerSettings;
  liveStatuses: LiveStatus[];
  liveSessions: LiveSession[];
}

export interface SyncMeta {
  serverKey: string;
  deviceId: string;
  feedId?: string;
  cursorSeq: number;
  latestSeq: number;
  lastSnapshotRevision: number;
  legacyCursorSeq?: number;
  lastAckSeq: number;
  lastSyncAt: string | null;
  status: string;
  initialized: boolean;
}

export interface ApplySyncBatchResult {
  applied: number;
  cursorSeq: number;
  latestSeq: number;
}

export interface ApplySyncBootstrapResult {
  applied: number;
  cursorSeq: number;
  snapshotRevision: number;
}

export interface DeviceRegistration {
  deviceId: string;
  name: string;
  platform: string;
  appVersion: string;
}

export interface DeviceInfo {
  deviceId: string;
  name: string;
  platform: string;
  appVersion: string;
  lastSeenAt: string | null;
  lastAckSeq: number;
  online: boolean;
}

export interface RefreshJob {
  jobId: string;
  kind: "refresh_all" | "refresh_streamer";
  streamerId: number | null;
  streamerName: string | null;
  status: "queued" | "running" | "completed" | "failed";
  progress: {
    stage: string;
    completed: number;
    total: number;
    percent: number;
    done: boolean;
    failed: boolean;
  };
  createdAt: string;
  startedAt: string | null;
  finishedAt: string | null;
  report: RefreshReport | null;
  error: string | null;
}

export interface RefreshJobAccepted {
  jobId: string;
  status: string;
  deduplicated?: boolean;
}

export interface SyncAckResult {
  deviceId: string;
  seq: number;
}

export type UpdateChannel = "stable" | "beta";

export interface ServerUpdateSettings {
  channel: UpdateChannel;
  autoCheckEnabled: boolean;
  autoUpdateEnabled: boolean;
  updateWindowStart: string;
  updateWindowEnd: string;
  githubMirrorUrl: string;
}

export interface ServerUpdateInfo extends ServerUpdateSettings {
  currentVersion: string;
  latestVersion: string | null;
  latestChannel: UpdateChannel | null;
  updateAvailable: boolean;
  status: string;
  phase: string;
  message: string;
  lastCheckedAt: string | null;
  lastUpdatedAt: string | null;
  agentOnline: boolean;
  targetVersion: string | null;
  error: string | null;
}
