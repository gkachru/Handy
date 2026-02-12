import React, { useEffect, useState, useCallback, useRef } from "react";
import { useTranslation } from "react-i18next";
import { platform } from "@tauri-apps/plugin-os";
import { checkScreenRecordingPermission } from "tauri-plugin-macos-permissions-api";
import { openUrl } from "@tauri-apps/plugin-opener";
import { commands } from "@/bindings";
import { Dropdown } from "../ui/Dropdown";
import { SettingContainer } from "../ui/SettingContainer";
import { useSettings } from "../../hooks/useSettings";

interface AudioSourceSelectorProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const AudioSourceSelector: React.FC<AudioSourceSelectorProps> =
  React.memo(({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, isUpdating, isLoading } = useSettings();
    const [available, setAvailable] = useState(false);
    const [waitingForPermission, setWaitingForPermission] = useState(false);
    const pendingSourceRef = useRef<string | null>(null);
    const pollingRef = useRef<ReturnType<typeof setInterval> | null>(null);

    // Cleanup polling on unmount
    useEffect(() => {
      return () => {
        if (pollingRef.current) {
          clearInterval(pollingRef.current);
        }
      };
    }, []);

    useEffect(() => {
      // Only show on macOS with system audio support
      if (platform() !== "macos") return;

      commands.isSystemAudioAvailable().then(setAvailable);
    }, []);

    const currentSource = getSetting("audio_source") || "microphone";

    const handleSelect = useCallback(
      async (value: string) => {
        // If selecting system audio or both, check Screen Recording permission
        if (value === "system_audio") {
          try {
            const granted = await checkScreenRecordingPermission();
            if (!granted) {
              // Register the app in the Screen Recording list via ScreenCaptureKit,
              // then show the permission hint with a link to System Settings
              pendingSourceRef.current = value;
              setWaitingForPermission(true);
              await commands.registerScreenRecording();

              // Start polling for permission grant
              if (pollingRef.current) clearInterval(pollingRef.current);
              pollingRef.current = setInterval(async () => {
                try {
                  const nowGranted = await checkScreenRecordingPermission();
                  if (nowGranted) {
                    if (pollingRef.current) {
                      clearInterval(pollingRef.current);
                      pollingRef.current = null;
                    }
                    setWaitingForPermission(false);
                    const source = pendingSourceRef.current;
                    pendingSourceRef.current = null;
                    if (source) {
                      await updateSetting("audio_source", source as any);
                    }
                  }
                } catch {
                  // keep polling
                }
              }, 1000);

              return;
            }
          } catch {
            // Permission check failed, stay on current source
            return;
          }
        }
        await updateSetting("audio_source", value as any);
      },
      [updateSetting],
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
    ];

    return (
      <div className="flex flex-col gap-1.5">
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
            disabled={
              isUpdating("audio_source") || isLoading || waitingForPermission
            }
          />
        </SettingContainer>
        {waitingForPermission && (
          <p className="text-xs text-text/50 px-4 pb-3">
            {t("settings.sound.audioSource.permissionRequired")}{" "}
            <button
              type="button"
              onClick={async () => {
                await commands.registerScreenRecording();
                openUrl(
                  "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture",
                ).catch(console.error);
              }}
              className="underline text-logo-primary hover:text-logo-primary/80 transition-colors"
            >
              {t("accessibility.openSettings")}
            </button>
          </p>
        )}
      </div>
    );
  });
