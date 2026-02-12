import React from "react";
import { useTranslation } from "react-i18next";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { useSettings } from "../../hooks/useSettings";

interface MuteWhileRecordingToggleProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const MuteSystemAudioWhileRecording: React.FC<
  MuteWhileRecordingToggleProps
> = React.memo(({ descriptionMode = "tooltip", grouped = false }) => {
  const { t } = useTranslation();
  const { getSetting, updateSetting, isUpdating } = useSettings();

  const audioSource = getSetting("audio_source") || "microphone";

  // Only visible when audio source is "microphone" (system audio capture not active)
  if (audioSource !== "microphone") return null;

  const muteEnabled = getSetting("mute_while_recording") ?? false;

  return (
    <ToggleSwitch
      checked={muteEnabled}
      onChange={(enabled) => updateSetting("mute_while_recording", enabled)}
      isUpdating={isUpdating("mute_while_recording")}
      label={t("settings.debug.muteWhileRecording.label")}
      description={t("settings.debug.muteWhileRecording.description")}
      descriptionMode={descriptionMode}
      grouped={grouped}
    />
  );
});

export const MuteMicrophoneWhileRecording: React.FC<
  MuteWhileRecordingToggleProps
> = React.memo(({ descriptionMode = "tooltip", grouped = false }) => {
  const { t } = useTranslation();
  const { getSetting, updateSetting, isUpdating } = useSettings();

  const audioSource = getSetting("audio_source") || "microphone";

  // Only visible when audio source is "system_audio" (microphone not being used for transcription)
  if (audioSource !== "system_audio") return null;

  const muteEnabled =
    getSetting("mute_microphone_while_recording") ?? false;

  return (
    <ToggleSwitch
      checked={muteEnabled}
      onChange={(enabled) =>
        updateSetting("mute_microphone_while_recording", enabled)
      }
      isUpdating={isUpdating("mute_microphone_while_recording")}
      label={t("settings.debug.muteMicrophoneWhileRecording.label")}
      description={t(
        "settings.debug.muteMicrophoneWhileRecording.description",
      )}
      descriptionMode={descriptionMode}
      grouped={grouped}
    />
  );
});
