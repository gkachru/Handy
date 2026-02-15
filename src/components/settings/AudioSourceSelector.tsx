import React, { useEffect, useState, useCallback, useRef } from "react";
import { useTranslation } from "react-i18next";
import { platform } from "@tauri-apps/plugin-os";
import { openUrl } from "@tauri-apps/plugin-opener";
import { commands } from "@/bindings";
import { Dropdown } from "../ui/Dropdown";
import { SettingContainer } from "../ui/SettingContainer";
import { useSettings } from "../../hooks/useSettings";
import { Loader2 } from "lucide-react";

interface AudioSourceSelectorProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const AudioSourceSelector: React.FC<AudioSourceSelectorProps> =
  React.memo(({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, isUpdating, isLoading } = useSettings();
    const [available, setAvailable] = useState(false);
    const [permissionNeeded, setPermissionNeeded] = useState(false);
    const [waitingForPermission, setWaitingForPermission] = useState(false);
    const pollingRef = useRef<ReturnType<typeof setInterval> | null>(null);

    useEffect(() => {
      // Only show on macOS with system audio support
      if (platform() !== "macos") return;

      commands.isSystemAudioAvailable().then(setAvailable);
    }, []);

    // Cleanup polling on unmount
    useEffect(() => {
      return () => {
        if (pollingRef.current) {
          clearInterval(pollingRef.current);
        }
      };
    }, []);

    const currentSource = getSetting("audio_source") || "microphone";

    const pendingValueRef = useRef<string>("system_audio");

    const startPermissionPolling = useCallback(() => {
      if (pollingRef.current) return;
      setWaitingForPermission(true);

      pollingRef.current = setInterval(async () => {
        const granted = await commands.checkSystemAudioPermission();
        if (granted) {
          if (pollingRef.current) {
            clearInterval(pollingRef.current);
            pollingRef.current = null;
          }
          setPermissionNeeded(false);
          setWaitingForPermission(false);
          await updateSetting("audio_source", pendingValueRef.current as any);
        }
      }, 2000);
    }, [updateSetting]);

    const handleSelect = useCallback(
      async (value: string) => {
        if (value === "system_audio" || value === "mixed") {
          // Instant TCC check first
          const granted = await commands.checkSystemAudioPermission();
          if (granted) {
            setPermissionNeeded(false);
            await updateSetting("audio_source", value as any);
          } else {
            // Try to show the macOS permission dialog programmatically.
            // If the dialog can't be shown (unsigned dev build), the backend
            // will register the app via a process tap and auto-open System Settings.
            setWaitingForPermission(true);
            setPermissionNeeded(true);
            const requestGranted =
              await commands.requestSystemAudioPermission();
            if (requestGranted) {
              setPermissionNeeded(false);
              setWaitingForPermission(false);
              await updateSetting("audio_source", value as any);
            } else {
              // Dialog was denied or unavailable — System Settings was opened
              // automatically by the backend. Poll for manual grant.
              setWaitingForPermission(false);
              pendingValueRef.current = value;
              startPermissionPolling();
            }
          }
        } else {
          setPermissionNeeded(false);
          setWaitingForPermission(false);
          if (pollingRef.current) {
            clearInterval(pollingRef.current);
            pollingRef.current = null;
          }
          await updateSetting("audio_source", value as any);
        }
      },
      [updateSetting, startPermissionPolling],
    );

    // Hide on non-macOS or when system audio is not available
    if (!available) return null;

    const options = [
      {
        value: "microphone",
        label: t("settings.sound.audioSource.microphone"),
      },
      {
        value: "system_audio",
        label: t("settings.sound.audioSource.systemAudio"),
      },
      {
        value: "mixed",
        label: t("settings.sound.audioSource.mixed"),
      },
    ];

    return (
      <div>
        <SettingContainer
          title={t("settings.sound.audioSource.title")}
          description={t("settings.sound.audioSource.description")}
          descriptionMode={descriptionMode}
          grouped={grouped}
        >
          <Dropdown
            options={options}
            selectedValue={currentSource}
            onSelect={handleSelect}
            disabled={isUpdating("audio_source") || isLoading}
          />
        </SettingContainer>
        {permissionNeeded && (
          <div className="px-4 pb-4 pt-1 flex flex-col gap-1.5">
            <p className="text-sm text-amber-400">
              {t("settings.sound.audioSource.permissionRequired")}
            </p>
            {waitingForPermission ? (
              <div className="flex items-center gap-2 text-text/50 text-sm">
                <Loader2 className="w-3.5 h-3.5 animate-spin" />
                {t("settings.sound.audioSource.permissionWaiting")}
              </div>
            ) : null}
            <button
              onClick={() =>
                openUrl(
                  "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture",
                )
              }
              className="text-sm text-logo-primary hover:text-logo-primary/80 underline text-left w-fit"
            >
              {t("settings.sound.audioSource.openSettings")}
            </button>
          </div>
        )}
      </div>
    );
  });
