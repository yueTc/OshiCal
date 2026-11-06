import { AlertTriangle, Check, Palette, RotateCcw } from "lucide-react";
import type { CSSProperties } from "react";
import {
  APPEARANCE_PRESETS,
  DEFAULT_APPEARANCE,
  inspectAppearanceContrast,
  resolveAppearancePalette,
  type AppearanceSettings,
} from "../lib/appearance";

interface AppearanceSettingsPanelProps {
  value: AppearanceSettings;
  onChange: (value: AppearanceSettings) => void;
}

export default function AppearanceSettingsPanel({ value, onChange }: AppearanceSettingsPanelProps) {
  const palette = resolveAppearancePalette(value);
  const contrast = inspectAppearanceContrast(value);

  function selectPreset(preset: AppearanceSettings["preset"]) {
    onChange({ ...value, preset });
  }

  function updateCustom(key: "primary" | "background", color: string) {
    onChange({
      preset: "custom",
      custom: { ...value.custom, [key]: color },
    });
  }

  return (
    <section className="settings-card span-2 appearance-card">
      <header>
        <h2>界面配色</h2>
        <span className="setting-scope local">仅当前设备</span>
        <span>选择常用配色，或自定义主色和背景色；修改后立即生效。</span>
      </header>

      <div className="appearance-presets" aria-label="常用配色">
        {APPEARANCE_PRESETS.map((preset) => {
          const active = value.preset === preset.id;
          const style = {
            "--preview-bg": preset.palette.bg,
            "--preview-soft": preset.palette.surfaceStrong,
            "--preview-primary": preset.palette.primary,
            "--preview-strong": preset.palette.primaryStrong,
          } as CSSProperties;
          return (
            <button
              type="button"
              key={preset.id}
              className={`appearance-preset ${active ? "active" : ""}`}
              onClick={() => selectPreset(preset.id)}
              aria-pressed={active}
            >
              <span className="appearance-swatch" style={style}>
                <i />
              </span>
              <span className="appearance-preset-copy">
                <strong>{preset.name}</strong>
                <small>{preset.description}</small>
              </span>
              {active ? <Check className="appearance-selected" size={15} /> : null}
            </button>
          );
        })}

        <button
          type="button"
          className={`appearance-preset custom-entry ${value.preset === "custom" ? "active" : ""}`}
          onClick={() => selectPreset("custom")}
          aria-pressed={value.preset === "custom"}
          style={{ "--preview-primary": palette.primary } as CSSProperties}
        >
          <span className="appearance-swatch custom-entry-swatch"><i /><i /><i /></span>
          <span className="appearance-preset-copy">
            <strong>自定义配色</strong>
            <small>自由选择颜色</small>
          </span>
          {value.preset === "custom" ? <Check className="appearance-selected" size={15} /> : null}
        </button>
      </div>

      <div className={`appearance-custom ${value.preset === "custom" ? "active" : ""}`}>
        <div className="appearance-custom-head">
          <Palette size={17} />
          <div>
            <strong>自定义配色</strong>
            <span>主色用于按钮、导航和强调元素；背景色用于页面底色与光晕。</span>
          </div>
        </div>
        <div className="appearance-color-grid">
          <label className="appearance-color-field">
            <span>主色</span>
            <span className="appearance-color-input">
              <input
                type="color"
                value={value.custom.primary}
                onChange={(event) => updateCustom("primary", event.target.value)}
                aria-label="自定义主色"
              />
              <code>{value.custom.primary.toUpperCase()}</code>
            </span>
          </label>
          <label className="appearance-color-field">
            <span>背景色</span>
            <span className="appearance-color-input">
              <input
                type="color"
                value={value.custom.background}
                onChange={(event) => updateCustom("background", event.target.value)}
                aria-label="自定义背景色"
              />
              <code>{value.custom.background.toUpperCase()}</code>
            </span>
          </label>
          <button
            type="button"
            className="button secondary appearance-reset"
            onClick={() => onChange({
              preset: DEFAULT_APPEARANCE.preset,
              custom: { ...DEFAULT_APPEARANCE.custom },
            })}
          >
            <RotateCcw size={15} />
            恢复默认
          </button>
        </div>
        {value.preset === "custom" && contrast.primaryAutoAdjusted ? (
          <div className="appearance-contrast-warning" role="status">
            <AlertTriangle size={16} />
            <span>
              主色与背景对比度仅 {contrast.rawPrimaryContrast.toFixed(1)}:1，已自动调整强调色到
              {" "}{contrast.effectivePrimaryContrast.toFixed(1)}:1，确保按钮和文字可读。
            </span>
          </div>
        ) : null}
      </div>
    </section>
  );
}