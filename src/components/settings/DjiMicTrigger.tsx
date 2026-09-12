import React, { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { type } from "@tauri-apps/plugin-os";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { useSettings } from "../../hooks/useSettings";
import { commands, type DjiMicTriggerStatus } from "@/bindings";

interface DjiMicTriggerProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const DjiMicTrigger: React.FC<DjiMicTriggerProps> = React.memo(
  ({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, isUpdating } = useSettings();
    const isMacos = type() === "macos";
    const enabled = getSetting("dji_mic_trigger_enabled") ?? false;
    const [status, setStatus] = useState<DjiMicTriggerStatus | null>(null);

    useEffect(() => {
      if (!isMacos) {
        return;
      }
      let cancelled = false;
      const tick = async () => {
        const data = await commands.getDjiMicTriggerStatus();
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
        t("settings.general.djiMicTrigger.defaultDeviceName");
      const usage = status.last_hid_usage ? ` (${status.last_hid_usage})` : "";
      if (!status.supported) {
        statusText = t("settings.general.djiMicTrigger.statusUnsupported");
      } else if (status.button_seen) {
        statusText = t("settings.general.djiMicTrigger.statusButtonSeen", {
          device,
          usage,
        });
      } else if (status.receiver_present) {
        statusText = t("settings.general.djiMicTrigger.statusReceiverReady", {
          device,
        });
        if (status.listener_running && !status.volume_swallow_active) {
          statusText = `${statusText} ${t("settings.general.djiMicTrigger.statusNoSwallow")}`;
        }
      } else if (status.listener_running && !status.volume_swallow_active) {
        statusText = t("settings.general.djiMicTrigger.statusNoSwallow");
      } else {
        statusText = t("settings.general.djiMicTrigger.statusWaitingReceiver");
      }
    }

    return (
      <div className="space-y-1">
        <ToggleSwitch
          checked={enabled}
          onChange={(value) => updateSetting("dji_mic_trigger_enabled", value)}
          isUpdating={isUpdating("dji_mic_trigger_enabled")}
          label={t("settings.general.djiMicTrigger.label")}
          description={t("settings.general.djiMicTrigger.description")}
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
