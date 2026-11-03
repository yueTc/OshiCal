export type DataMode = "local" | "server";
export type ViewMode = "timeline" | "grid";
export type SourceKind = "text" | "reserve" | "image" | "manual" | "editor" | "weekly";

export interface AppSettings {
  dataMode: DataMode;
  serverUrl: string;
  livePollSeconds: number;
  dynamicPollSeconds: number;
  autoAnalyzeImages: boolean;
  notificationsEnabled: boolean;
  liveStatsEnabled: boolean;
  llmBaseUrl: string;
  llmModel: string;
  llmPromptVersion: string;
  proxyPort: number | null;
}

export interface SecretStatus {
  llmConfigured: boolean;
  biliConfigured: boolean;
  serverTokenConfigured: boolean;
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
  mid: string;
  name: string;
  avatarUrl: string;
  sign: string;
  roomId: string | null;
  fans: number;
  isLive: boolean;
}

export interface LinkedAccount {
  mid: string;
  name: string;
  avatarUrl: string;
  createdAt: string;
}

export interface Streamer {
  id: number;
  mid: string;
  name: string;
  avatarUrl: string;
  roomId: string | null;
  liveUrl: string | null;
  aliases: string[];
  scheduleEnabled: boolean;
  liveNotifyEnabled: boolean;
  liveStatsEnabled: boolean;
  lastError: string | null;
  lastRefreshAt: string | null;
  isLive: boolean;
  liveTitle: string | null;
  liveCover: string | null;
  liveStartedAt: string | null;
  liveCheckedAt: string | null;
  linkedAccounts: LinkedAccount[];
  sortOrder: number;
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
  id: number;
  streamerId: number;
  streamerName: string;
  roomId: string;
  title: string;
  coverUrl: string | null;
  startedAt: string;
  endedAt: string | null;
  durationSeconds: number;
  peakOnline: number;
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
  reconnectCount: number;
  dataQuality: string;
  collectorState: string;
  createdAt: string;
  updatedAt: string;
}
export interface LiveStatus {
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
}

export interface FertilitySnapshot {
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
}

export interface LlmTestInput {
  baseUrl: string;
  apiKey?: string;
  model: string;
  promptVersion: string;
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
}

export interface ServerEvent {
  replayed?: boolean;
  seq?: number;
  type: string;
  createdAt: string;
  payload: unknown;
}

export interface ServerSnapshot {
  subscriptions: Streamer[];
  schedules: Schedule[];
  weeklyRules?: WeeklyScheduleRule[];
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
  conflicts: { entity: string; label: string; localId: number; remoteId: number;
    local: unknown; remote: unknown }[];
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
  changes: SyncChange[];
}

export interface SyncChangesPage {
  schemaVersion: number;
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
  createdAt: string;
}

export interface ScheduleSource {
  id: number;
  scheduleId: number;
  sourceKind: SourceKind;
  sourcePostId: string | null;
  sourceImageUrl: string | null;
  sourceFingerprint: string | null;
  sourceIdentity: string;
  confidence: number;
  createdAt: string;
}

export interface SourceSuppression {
  id: number;
  streamerId: number;
  sourcePostId: string;
  sourceFingerprint: string;
  createdAt: string;
}

export interface ServerSettings {
  livePollSeconds?: number;
  dynamicPollSeconds?: number;
  autoAnalyzeImages?: boolean;
  liveStatsEnabled?: boolean;
  llmBaseUrl?: string;
  llmModel?: string;
  llmPromptVersion?: string;
  proxyPort?: number | null;
}

export interface SyncBootstrap {
  schemaVersion: number;
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
  cursorSeq: number;
  latestSeq: number;
  lastSnapshotRevision: number;
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
}

export interface SyncAckResult {
  deviceId: string;
  seq: number;
}
