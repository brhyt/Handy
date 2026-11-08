import React, { useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { useSettings } from "../../../hooks/useSettings";
import { Button } from "../../ui/Button";
import { Input } from "../../ui/Input";
import { ResetButton } from "../../ui/ResetButton";
import { SettingContainer } from "../../ui/SettingContainer";
import { ToggleSwitch } from "../../ui/ToggleSwitch";

const normalizeCue = (cue: string) =>
  cue
    .replace(/[<>"']/g, "")
    .replace(/\s+/g, " ")
    .trim();

interface CueListSettingProps {
  settingKey: "prompt_mode_cues" | "verbatim_cues";
  titleKey: string;
  descriptionKey: string;
  placeholderKey: string;
}

const CueListSetting: React.FC<CueListSettingProps> = ({
  settingKey,
  titleKey,
  descriptionKey,
  placeholderKey,
}) => {
  const { t } = useTranslation();
  const { getSetting, updateSetting, resetSetting, isUpdating } = useSettings();
  const [draft, setDraft] = useState("");
  const cues = getSetting(settingKey) || [];
  const normalized = normalizeCue(draft);

  const handleAdd = () => {
    if (!normalized || normalized.length > 50) {
      return;
    }
    if (cues.some((cue) => cue.toLowerCase() === normalized.toLowerCase())) {
      toast.error(
        t("settings.postProcessing.spokenMode.duplicate", {
          cue: normalized,
        }),
      );
      return;
    }
    updateSetting(settingKey, [...cues, normalized]);
    setDraft("");
  };

  const handleRemove = (cueToRemove: string) => {
    updateSetting(
      settingKey,
      cues.filter((cue) => cue !== cueToRemove),
    );
  };

  const handleKeyDown = (event: React.KeyboardEvent) => {
    if (event.key === "Enter") {
      event.preventDefault();
      handleAdd();
    }
  };

  return (
    <>
      <SettingContainer
        title={t(titleKey)}
        description={t(descriptionKey)}
        descriptionMode="tooltip"
        grouped={true}
        layout="stacked"
      >
        <div className="flex items-center gap-2">
          <Input
            type="text"
            className="max-w-48"
            value={draft}
            onChange={(event) => setDraft(event.target.value)}
            onKeyDown={handleKeyDown}
            placeholder={t(placeholderKey)}
            variant="compact"
            disabled={isUpdating(settingKey)}
          />
          <Button
            onClick={handleAdd}
            disabled={
              !normalized || normalized.length > 50 || isUpdating(settingKey)
            }
            variant="primary"
            size="md"
          >
            {t("settings.postProcessing.spokenMode.add")}
          </Button>
          <ResetButton
            onClick={() => resetSetting(settingKey)}
            disabled={isUpdating(settingKey)}
            ariaLabel={t("settings.postProcessing.spokenMode.reset")}
          />
        </div>
      </SettingContainer>
      {cues.length > 0 && (
        <div className="px-4 p-2 flex flex-wrap gap-1">
          {cues.map((cue) => (
            <Button
              key={cue}
              onClick={() => handleRemove(cue)}
              disabled={isUpdating(settingKey)}
              variant="secondary"
              size="sm"
              className="inline-flex items-center gap-1 cursor-pointer"
              aria-label={t("settings.postProcessing.spokenMode.remove", {
                cue,
              })}
            >
              <span>{cue}</span>
              <svg
                className="w-3 h-3"
                fill="none"
                stroke="currentColor"
                viewBox="0 0 24 24"
              >
                <path
                  strokeLinecap="round"
                  strokeLinejoin="round"
                  strokeWidth={2}
                  d="M6 18L18 6M6 6l12 12"
                />
              </svg>
            </Button>
          ))}
        </div>
      )}
    </>
  );
};

export const SpokenModeSettings: React.FC = () => {
  const { t } = useTranslation();
  const { getSetting, updateSetting, isUpdating, refreshSettings } =
    useSettings();
  const stickyEnabled = getSetting("prompt_mode_sticky_enabled") ?? true;
  const stickyArmed = getSetting("prompt_mode_sticky_armed") ?? false;

  return (
    <>
      <CueListSetting
        settingKey="prompt_mode_cues"
        titleKey="settings.postProcessing.spokenMode.promptCues.title"
        descriptionKey="settings.postProcessing.spokenMode.promptCues.description"
        placeholderKey="settings.postProcessing.spokenMode.promptCues.placeholder"
      />
      <CueListSetting
        settingKey="verbatim_cues"
        titleKey="settings.postProcessing.spokenMode.verbatimCues.title"
        descriptionKey="settings.postProcessing.spokenMode.verbatimCues.description"
        placeholderKey="settings.postProcessing.spokenMode.verbatimCues.placeholder"
      />
      <ToggleSwitch
        checked={stickyEnabled}
        onChange={async (enabled) => {
          await updateSetting("prompt_mode_sticky_enabled", enabled);
          if (!enabled) {
            await refreshSettings();
          }
        }}
        isUpdating={isUpdating("prompt_mode_sticky_enabled")}
        label={t("settings.postProcessing.spokenMode.sticky.title")}
        description={t("settings.postProcessing.spokenMode.sticky.description")}
        descriptionMode="tooltip"
        grouped={true}
      />
      {stickyEnabled && (
        <ToggleSwitch
          checked={stickyArmed}
          onChange={(armed) => updateSetting("prompt_mode_sticky_armed", armed)}
          isUpdating={isUpdating("prompt_mode_sticky_armed")}
          label={t("settings.postProcessing.spokenMode.armed.title")}
          description={t(
            "settings.postProcessing.spokenMode.armed.description",
          )}
          descriptionMode="tooltip"
          grouped={true}
        />
      )}
    </>
  );
};
