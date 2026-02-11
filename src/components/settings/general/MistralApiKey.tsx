import React, { useState, useEffect } from "react";
import { useTranslation } from "react-i18next";
import { Input } from "../../ui/Input";
import { commands } from "@/bindings";
import { useSettingsStore } from "@/stores/settingsStore";

export const MistralApiKey: React.FC = () => {
  const { t } = useTranslation();
  const settings = useSettingsStore((s) => s.settings);
  const [localValue, setLocalValue] = useState(
    settings?.mistral_api_key ?? "",
  );

  useEffect(() => {
    setLocalValue(settings?.mistral_api_key ?? "");
  }, [settings?.mistral_api_key]);

  const handleBlur = async () => {
    if (localValue !== (settings?.mistral_api_key ?? "")) {
      await commands.setMistralApiKey(localValue);
      useSettingsStore.getState().refreshSettings();
    }
  };

  return (
    <div className="flex flex-col gap-2 px-4 py-3">
      <label className="text-sm text-text/70">
        {t("settings.mistralApiKey")}
      </label>
      <Input
        type="password"
        value={localValue}
        onChange={(e) => setLocalValue(e.target.value)}
        onBlur={handleBlur}
        placeholder={t("settings.mistralApiKeyPlaceholder")}
        variant="compact"
        className="w-full"
      />
    </div>
  );
};
