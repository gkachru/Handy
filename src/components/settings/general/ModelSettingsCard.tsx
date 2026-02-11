import React from "react";
import { useTranslation } from "react-i18next";
import { SettingsGroup } from "../../ui/SettingsGroup";
import { LanguageSelector } from "../LanguageSelector";
import { TranslateToEnglish } from "../TranslateToEnglish";
import { MistralApiKey } from "./MistralApiKey";
import { ToggleSwitch } from "../../ui/ToggleSwitch";
import { useModelStore } from "../../../stores/modelStore";
import { useSettings } from "../../../hooks/useSettings";
import { REALTIME_MODEL_IDS } from "../../onboarding/ModelCard";
import type { ModelInfo } from "@/bindings";

export const ModelSettingsCard: React.FC = () => {
  const { t } = useTranslation();
  const { currentModel, models } = useModelStore();
  const { getSetting, updateSetting, isUpdating } = useSettings();

  const currentModelInfo = models.find((m: ModelInfo) => m.id === currentModel);

  const isMistralApi = currentModelInfo?.engine_type === "MistralApi";
  const isRealtimeModel = currentModel
    ? REALTIME_MODEL_IDS.has(currentModel)
    : false;
  const supportsLanguageSelection =
    currentModelInfo?.engine_type === "Whisper" ||
    currentModelInfo?.engine_type === "SenseVoice";
  const supportsTranslation = currentModelInfo?.supports_translation ?? false;
  const hasAnySettings =
    supportsLanguageSelection ||
    supportsTranslation ||
    isMistralApi ||
    isRealtimeModel;

  // Don't render anything if no model is selected or no settings available
  if (!currentModel || !currentModelInfo || !hasAnySettings) {
    return null;
  }

  const realtimeTranscriptionEnabled =
    getSetting("realtime_transcription_enabled") ?? true;
  const streamingTranslationEnabled =
    getSetting("streaming_translation_enabled") || false;

  return (
    <SettingsGroup
      title={t("settings.modelSettings.title", {
        model: currentModelInfo.name,
      })}
    >
      {isMistralApi && <MistralApiKey />}
      {isRealtimeModel && (
        <ToggleSwitch
          checked={isMistralApi ? true : realtimeTranscriptionEnabled}
          onChange={(enabled) => {
            updateSetting("realtime_transcription_enabled", enabled);
            if (!enabled) {
              updateSetting("streaming_translation_enabled", false);
            }
          }}
          disabled={isMistralApi}
          isUpdating={isUpdating("realtime_transcription_enabled")}
          label={t("settings.realtimeTranscription.label")}
          description={
            isMistralApi
              ? t("settings.realtimeTranscription.requiredForCloud")
              : t("settings.realtimeTranscription.description")
          }
          descriptionMode="tooltip"
          grouped={true}
        />
      )}
      {isMistralApi && (
        <ToggleSwitch
          checked={streamingTranslationEnabled}
          onChange={(enabled) =>
            updateSetting("streaming_translation_enabled", enabled)
          }
          isUpdating={isUpdating("streaming_translation_enabled")}
          label={t("settings.streamingTranslation.label")}
          description={t("settings.streamingTranslation.description")}
          descriptionMode="tooltip"
          grouped={true}
        />
      )}
      {supportsLanguageSelection && (
        <LanguageSelector
          descriptionMode="tooltip"
          grouped={true}
          supportedLanguages={currentModelInfo.supported_languages}
        />
      )}
      {supportsTranslation && !isMistralApi && (
        <TranslateToEnglish descriptionMode="tooltip" grouped={true} />
      )}
    </SettingsGroup>
  );
};
