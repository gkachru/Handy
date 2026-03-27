import { useEffect, useState, useRef } from "react";
import { useTranslation } from "react-i18next";
import { platform } from "@tauri-apps/plugin-os";
import {
  checkAccessibilityPermission,
  requestAccessibilityPermission,
} from "tauri-plugin-macos-permissions-api";
import { commands } from "@/bindings";
import { Loader2 } from "lucide-react";

interface AccessibilityPermissionPromptProps {
  featureContext: "pasteMethod" | "shortcuts";
  onGranted?: () => void;
}

export const AccessibilityPermissionPrompt: React.FC<
  AccessibilityPermissionPromptProps
> = ({ featureContext, onGranted }) => {
  const { t } = useTranslation();
  const [granted, setGranted] = useState<boolean | null>(null);
  const [waiting, setWaiting] = useState(false);
  const pollingRef = useRef<ReturnType<typeof setInterval> | null>(null);

  const isMacOS = platform() === "macos";

  useEffect(() => {
    if (!isMacOS) return;

    checkAccessibilityPermission().then((result) => {
      setGranted(result);
    });
  }, [isMacOS]);

  useEffect(() => {
    return () => {
      if (pollingRef.current) {
        clearInterval(pollingRef.current);
      }
    };
  }, []);

  // Don't render on non-macOS or if granted (or still checking)
  if (!isMacOS || granted === null || granted) {
    return null;
  }

  const handleGrant = async () => {
    await requestAccessibilityPermission();
    setWaiting(true);

    pollingRef.current = setInterval(async () => {
      const result = await checkAccessibilityPermission();
      if (result) {
        if (pollingRef.current) {
          clearInterval(pollingRef.current);
          pollingRef.current = null;
        }
        setGranted(true);
        setWaiting(false);

        // Initialize Enigo and shortcuts now that we have permission
        try {
          await Promise.all([
            commands.initializeEnigo(),
            commands.initializeShortcuts(),
          ]);
        } catch (e) {
          console.warn("Failed to initialize after permission grant:", e);
        }

        onGranted?.();
      }
    }, 1000);
  };

  const message = t(`accessibility.prompt.${featureContext}`);

  return (
    <div className="w-full p-3 rounded-lg border border-amber-500/30 bg-amber-500/10 flex items-center justify-between gap-3">
      <p className="text-sm text-text/80">{message}</p>
      {waiting ? (
        <Loader2 className="w-4 h-4 animate-spin text-text/50 shrink-0" />
      ) : (
        <button
          onClick={handleGrant}
          className="px-3 py-1.5 rounded-md bg-logo-primary hover:bg-logo-primary/90 text-white text-xs font-medium transition-colors whitespace-nowrap shrink-0"
        >
          {t("accessibility.prompt.grantPermission")}
        </button>
      )}
    </div>
  );
};
