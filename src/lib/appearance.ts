export type AppearancePresetId =
  | "sakura"
  | "ocean"
  | "lavender"
  | "mint"
  | "amber"
  | "peach"
  | "graphite"
  | "custom";

export interface AppearancePalette {
  bg: string;
  surface: string;
  surfaceSoft: string;
  surfaceStrong: string;
  primary: string;
  primaryStrong: string;
  primarySoft: string;
  text: string;
  muted: string;
  faint: string;
  border: string;
  borderStrong: string;
}

export interface CustomAppearance {
  primary: string;
  background: string;
}

export interface AppearanceSettings {
  preset: AppearancePresetId;
  custom: CustomAppearance;
}

export interface AppearancePreset {
  id: Exclude<AppearancePresetId, "custom">;
  name: string;
  description: string;
  palette: AppearancePalette;
}

const STORAGE_KEY = "oshical.appearance";

const LIGHT_TEXT = "#241820";
const LIGHT_MUTED = "#75636d";
const LIGHT_FAINT = "#a3909a";

export const APPEARANCE_PRESETS: AppearancePreset[] = [
  {
    id: "sakura",
    name: "樱花粉",
    description: "柔和、轻盈",
    palette: {
      bg: "#fff8fb",
      surface: "#ffffff",
      surfaceSoft: "#fff2f7",
      surfaceStrong: "#fde9f1",
      primary: "#e94f88",
      primaryStrong: "#b83263",
      primarySoft: "#ffe3ee",
      text: LIGHT_TEXT,
      muted: LIGHT_MUTED,
      faint: LIGHT_FAINT,
      border: "#eedde5",
      borderStrong: "#e4c8d5",
    },
  },
  {
    id: "ocean",
    name: "海盐蓝",
    description: "清爽、专注",
    palette: {
      bg: "#f4f9ff",
      surface: "#ffffff",
      surfaceSoft: "#edf6ff",
      surfaceStrong: "#dceeff",
      primary: "#2f7fd3",
      primaryStrong: "#1d5d9e",
      primarySoft: "#e1f0ff",
      text: "#172435",
      muted: "#607184",
      faint: "#93a2b2",
      border: "#dbe9f6",
      borderStrong: "#c5dcf1",
    },
  },
  {
    id: "lavender",
    name: "薰衣草",
    description: "安静、雅致",
    palette: {
      bg: "#faf7ff",
      surface: "#ffffff",
      surfaceSoft: "#f5efff",
      surfaceStrong: "#e9dcff",
      primary: "#8b5cf6",
      primaryStrong: "#6840c7",
      primarySoft: "#eee5ff",
      text: "#251c35",
      muted: "#6f6382",
      faint: "#9d92ae",
      border: "#e8def6",
      borderStrong: "#d8c8ee",
    },
  },
  {
    id: "mint",
    name: "薄荷绿",
    description: "清新、自然",
    palette: {
      bg: "#f4fcf8",
      surface: "#ffffff",
      surfaceSoft: "#ebfaf3",
      surfaceStrong: "#d7f4e8",
      primary: "#1f9f79",
      primaryStrong: "#167357",
      primarySoft: "#ddf7ed",
      text: "#17261f",
      muted: "#5f746a",
      faint: "#91a59a",
      border: "#d9eee5",
      borderStrong: "#c3e3d6",
    },
  },
  {
    id: "amber",
    name: "暖杏橙",
    description: "温暖、明快",
    palette: {
      bg: "#fffaf1",
      surface: "#ffffff",
      surfaceSoft: "#fff5e5",
      surfaceStrong: "#ffebc9",
      primary: "#d97716",
      primaryStrong: "#a65308",
      primarySoft: "#ffedd3",
      text: "#2b2117",
      muted: "#78685b",
      faint: "#aa9a8c",
      border: "#f1e1cf",
      borderStrong: "#e8cfb3",
    },
  },
  {
    id: "peach",
    name: "蜜桃红",
    description: "活泼、鲜明",
    palette: {
      bg: "#fff7f7",
      surface: "#ffffff",
      surfaceSoft: "#fff0f1",
      surfaceStrong: "#ffe0e3",
      primary: "#d94a64",
      primaryStrong: "#a62d44",
      primarySoft: "#ffe4e8",
      text: "#2b1b20",
      muted: "#78636a",
      faint: "#aa929a",
      border: "#f0dde1",
      borderStrong: "#e6c7ce",
    },
  },
  {
    id: "graphite",
    name: "石墨灰",
    description: "克制、专业",
    palette: {
      bg: "#f7f8fa",
      surface: "#ffffff",
      surfaceSoft: "#f0f2f5",
      surfaceStrong: "#e4e8ee",
      primary: "#536579",
      primaryStrong: "#36485c",
      primarySoft: "#e8edf2",
      text: "#1f252c",
      muted: "#68727e",
      faint: "#9aa3ad",
      border: "#dde2e8",
      borderStrong: "#cbd3dc",
    },
  },
];

export const DEFAULT_APPEARANCE: AppearanceSettings = {
  preset: "sakura",
  custom: {
    primary: "#e94f88",
    background: "#fff8fb",
  },
};

const PRESET_IDS = new Set<AppearancePresetId>([
  ...APPEARANCE_PRESETS.map((preset) => preset.id),
  "custom",
]);

function normalizeHex(value: string, fallback: string): string {
  const match = /^#?([0-9a-f]{3}|[0-9a-f]{6})$/i.exec(value.trim());
  if (!match) {
    return fallback;
  }
  const raw = match[1].length === 3
    ? match[1].split("").map((part) => part + part).join("")
    : match[1];
  return `#${raw.toLowerCase()}`;
}

function parseHex(value: string): [number, number, number] {
  const normalized = normalizeHex(value, "#000000").slice(1);
  return [
    Number.parseInt(normalized.slice(0, 2), 16),
    Number.parseInt(normalized.slice(2, 4), 16),
    Number.parseInt(normalized.slice(4, 6), 16),
  ];
}

function channelToHex(value: number): string {
  return Math.round(Math.min(255, Math.max(0, value))).toString(16).padStart(2, "0");
}

function mix(left: string, right: string, rightWeight: number): string {
  const leftRgb = parseHex(left);
  const rightRgb = parseHex(right);
  const weight = Math.min(1, Math.max(0, rightWeight));
  return `#${leftRgb.map((channel, index) => channelToHex(channel * (1 - weight) + rightRgb[index] * weight)).join("")}`;
}

function rgbChannels(value: string): string {
  return parseHex(value).join(", ");
}

function customPalette(custom: CustomAppearance): AppearancePalette {
  const primary = normalizeHex(custom.primary, DEFAULT_APPEARANCE.custom.primary);
  const background = normalizeHex(custom.background, DEFAULT_APPEARANCE.custom.background);
  return {
    bg: background,
    surface: mix(background, "#ffffff", 0.92),
    surfaceSoft: mix(primary, "#ffffff", 0.9),
    surfaceStrong: mix(primary, "#ffffff", 0.78),
    primary,
    primaryStrong: mix(primary, "#241820", 0.28),
    primarySoft: mix(primary, "#ffffff", 0.84),
    text: LIGHT_TEXT,
    muted: LIGHT_MUTED,
    faint: LIGHT_FAINT,
    border: mix(primary, "#ffffff", 0.82),
    borderStrong: mix(primary, "#ffffff", 0.7),
  };
}

export function resolveAppearancePalette(settings: AppearanceSettings): AppearancePalette {
  if (settings.preset === "custom") {
    return customPalette(settings.custom);
  }
  return APPEARANCE_PRESETS.find((preset) => preset.id === settings.preset)?.palette
    ?? APPEARANCE_PRESETS[0].palette;
}

export function readAppearanceSettings(): AppearanceSettings {
  try {
    const raw = window.localStorage.getItem(STORAGE_KEY);
    if (!raw) {
      return {
        preset: DEFAULT_APPEARANCE.preset,
        custom: { ...DEFAULT_APPEARANCE.custom },
      };
    }
    const parsed = JSON.parse(raw) as Partial<AppearanceSettings>;
    const preset = typeof parsed.preset === "string" && PRESET_IDS.has(parsed.preset as AppearancePresetId)
      ? parsed.preset as AppearancePresetId
      : DEFAULT_APPEARANCE.preset;
    return {
      preset,
      custom: {
        primary: normalizeHex(parsed.custom?.primary ?? "", DEFAULT_APPEARANCE.custom.primary),
        background: normalizeHex(parsed.custom?.background ?? "", DEFAULT_APPEARANCE.custom.background),
      },
    };
  } catch {
    return {
      preset: DEFAULT_APPEARANCE.preset,
      custom: { ...DEFAULT_APPEARANCE.custom },
    };
  }
}

export function saveAppearanceSettings(settings: AppearanceSettings): void {
  try {
    window.localStorage.setItem(STORAGE_KEY, JSON.stringify(settings));
  } catch {
    // The active palette still works for this session when storage is unavailable.
  }
}

export function applyAppearance(settings: AppearanceSettings): void {
  if (typeof document === "undefined") {
    return;
  }
  const palette = resolveAppearancePalette(settings);
  const root = document.documentElement;
  const primaryRgb = rgbChannels(palette.primary);
  const shadowRgb = rgbChannels(mix(palette.primary, "#241820", 0.36));
  const values: Record<string, string> = {
    "--bg": palette.bg,
    "--surface": palette.surface,
    "--surface-soft": palette.surfaceSoft,
    "--surface-strong": palette.surfaceStrong,
    "--primary": palette.primary,
    "--primary-strong": palette.primaryStrong,
    "--primary-soft": palette.primarySoft,
    "--text": palette.text,
    "--muted": palette.muted,
    "--faint": palette.faint,
    "--border": palette.border,
    "--border-strong": palette.borderStrong,
    "--primary-rgb": primaryRgb,
    "--shadow-rgb": shadowRgb,
    "--shadow": `0 16px 50px rgba(${shadowRgb}, 0.12)`,
    "--shadow-soft": `0 8px 24px rgba(${shadowRgb}, 0.08)`,
  };
  for (const [name, value] of Object.entries(values)) {
    root.style.setProperty(name, value);
  }
  root.dataset.appearance = settings.preset;
  document.querySelector('meta[name="theme-color"]')?.setAttribute("content", palette.bg);
}