import React, { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { type } from "@tauri-apps/plugin-os";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { useSettings } from "../../hooks/useSettings";
import { commands, type AirpodsMaxTriggerStatus } from "@/bindings";

interface AirpodsMaxTriggerProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const AirpodsMaxTrigger: React.FC<AirpodsMaxTriggerProps> = React.memo(
  ({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, isUpdating } = useSettings();
    const isMacos = type() === "macos";
    const enabled = getSetting("airpods_max_trigger_enabled") ?? false;
    const [status, setStatus] = useState<AirpodsMaxTriggerStatus | null>(null);

    useEffect(() => {
      if (!isMacos) {
        return;
      }
      let cancelled = false;
      const tick = async () => {
        const data = await commands.getAirpodsMaxTriggerStatus();
        if (!cancelled) {
          setStatus(data);
        }
      };
      void tick();
      const id = window.setInterval(() => {
        void tick();
      }, 2000);
      return () => {
        cancelled = true;
        window.clearInterval(id);
      };
    }, [isMacos, enabled]);

    if (!isMacos) {
      return null;
    }

    let statusText: string | null = null;
    if (enabled && status) {
      const device =
        status.last_device_name ||
        t("settings.general.airpodsMaxTrigger.defaultDeviceName");
      if (!status.supported) {
        statusText = t("settings.general.airpodsMaxTrigger.statusUnsupported");
      } else {
        // Media-key tap disabled for stability — stem cannot fire.
        statusText = t("settings.general.airpodsMaxTrigger.statusTapDisabled", {
          device,
        });
      }
    }

    return (
      <div className="space-y-1">
        <ToggleSwitch
          checked={enabled}
          onChange={(value) =>
            updateSetting("airpods_max_trigger_enabled", value)
          }
          isUpdating={isUpdating("airpods_max_trigger_enabled")}
          label={t("settings.general.airpodsMaxTrigger.label")}
          description={t("settings.general.airpodsMaxTrigger.description")}
          descriptionMode={descriptionMode}
          grouped={grouped}
        />
        {statusText && (
          <p className="text-xs text-mid-gray px-3 pb-2">{statusText}</p>
        )}
      </div>
    );
  },
);
