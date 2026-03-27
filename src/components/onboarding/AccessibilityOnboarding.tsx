import { useEffect, useState, useCallback, useRef } from "react";
import { useTranslation } from "react-i18next";
import { platform } from "@tauri-apps/plugin-os";
import {
  checkAccessibilityPermission,
  requestAccessibilityPermission,
  checkMicrophonePermission,
  requestMicrophonePermission,
} from "tauri-plugin-macos-permissions-api";
import { toast } from "sonner";
import { commands } from "@/bindings";
import { useSettingsStore } from "@/stores/settingsStore";
import HandyTextLogo from "../icons/HandyTextLogo";
import { Keyboard, Mic, Volume2, Check, Loader2 } from "lucide-react";

interface AccessibilityOnboardingProps {
  onComplete: () => void;
}

type PermissionStatus = "checking" | "needed" | "waiting" | "granted";
type PermissionPlatform = "macos" | "windows" | "other";

interface PermissionsState {
  accessibility: PermissionStatus;
  microphone: PermissionStatus;
  systemAudio: PermissionStatus;
}

const AccessibilityOnboarding: React.FC<AccessibilityOnboardingProps> = ({
  onComplete,
}) => {
  const { t } = useTranslation();
  const refreshAudioDevices = useSettingsStore(
    (state) => state.refreshAudioDevices,
  );
  const refreshOutputDevices = useSettingsStore(
    (state) => state.refreshOutputDevices,
  );
  const [permissionPlatform, setPermissionPlatform] =
    useState<PermissionPlatform | null>(null);
  const [systemAudioAvailable, setSystemAudioAvailable] = useState(false);
  const [permissions, setPermissions] = useState<PermissionsState>({
    accessibility: "checking",
    microphone: "checking",
    systemAudio: "checking",
  });
  const pollingRef = useRef<ReturnType<typeof setInterval> | null>(null);
  const timeoutRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const errorCountRef = useRef<number>(0);
  const MAX_POLLING_ERRORS = 3;

  const isMacOS = permissionPlatform === "macos";
  const isWindows = permissionPlatform === "windows";
  const showMicrophonePermission = isMacOS || isWindows;
  const showAccessibilityPermission = isMacOS;

  const allGranted = isMacOS
    ? permissions.microphone === "granted" &&
      (!systemAudioAvailable || permissions.systemAudio === "granted")
    : isWindows
      ? permissions.microphone === "granted"
      : true;

  const completeOnboarding = useCallback(async () => {
    await Promise.all([refreshAudioDevices(), refreshOutputDevices()]);
    timeoutRef.current = setTimeout(() => onComplete(), 300);
  }, [onComplete, refreshAudioDevices, refreshOutputDevices]);

  const hasWindowsMicrophoneAccess = useCallback(async (): Promise<boolean> => {
    const microphoneStatus =
      await commands.getWindowsMicrophonePermissionStatus();

    if (!microphoneStatus.supported) {
      return true;
    }

    return microphoneStatus.overall_access !== "denied";
  }, []);

  // Check platform and permission status on mount
  useEffect(() => {
    const currentPlatform = platform();
    const nextPlatform: PermissionPlatform =
      currentPlatform === "macos"
        ? "macos"
        : currentPlatform === "windows"
          ? "windows"
          : "other";

    setPermissionPlatform(nextPlatform);

    // Skip immediately on unsupported platforms
    if (nextPlatform === "other") {
      onComplete();
      return;
    }

    const checkInitial = async () => {
      if (nextPlatform === "macos") {
        try {
          const [accessibilityGranted, microphoneGranted, sysAudioAvail] =
            await Promise.all([
              checkAccessibilityPermission(),
              checkMicrophonePermission(),
              commands.isSystemAudioAvailable(),
            ]);

          setSystemAudioAvailable(sysAudioAvail);

          // If accessibility is granted, initialize Enigo and shortcuts
          if (accessibilityGranted) {
            try {
              await Promise.all([
                commands.initializeEnigo(),
                commands.initializeShortcuts(),
              ]);
            } catch (e) {
              console.warn("Failed to initialize after permission grant:", e);
            }
          }

          // Check system audio permission if available
          let sysAudioStatus: PermissionStatus = "checking";
          if (sysAudioAvail) {
            const sysAudioGranted = await commands.checkSystemAudioPermission();
            sysAudioStatus = sysAudioGranted ? "granted" : "needed";
          }

          const newState: PermissionsState = {
            accessibility: accessibilityGranted ? "granted" : "needed",
            microphone: microphoneGranted ? "granted" : "needed",
            systemAudio: sysAudioStatus,
          };

          setPermissions(newState);

          if (microphoneGranted && (!sysAudioAvail || sysAudioStatus === "granted")) {
            await completeOnboarding();
          }
        } catch (error) {
          console.error("Failed to check macOS permissions:", error);
          toast.error(t("onboarding.permissions.errors.checkFailed"));
          setPermissions({
            accessibility: "needed",
            microphone: "needed",
            systemAudio: "needed",
          });
        }

        return;
      }

      try {
        const microphoneGranted = await hasWindowsMicrophoneAccess();

        setPermissions({
          accessibility: "granted",
          microphone: microphoneGranted ? "granted" : "needed",
          systemAudio: "checking",
        });

        if (microphoneGranted) {
          await completeOnboarding();
        }
      } catch (error) {
        console.warn("Failed to check Windows microphone permissions:", error);
        setPermissions({
          accessibility: "granted",
          microphone: "granted",
          systemAudio: "checking",
        });
        await completeOnboarding();
      }
    };

    checkInitial();
  }, [completeOnboarding, hasWindowsMicrophoneAccess, onComplete, t]);

  // Polling for permissions after user clicks a button
  const startPolling = useCallback(() => {
    if (pollingRef.current || permissionPlatform === null) return;

    pollingRef.current = setInterval(async () => {
      try {
        if (permissionPlatform === "windows") {
          const microphoneGranted = await hasWindowsMicrophoneAccess();

          if (microphoneGranted) {
            setPermissions((prev) => ({ ...prev, microphone: "granted" }));

            if (pollingRef.current) {
              clearInterval(pollingRef.current);
              pollingRef.current = null;
            }

            await completeOnboarding();
          }

          errorCountRef.current = 0;
          return;
        }

        const [accessibilityGranted, microphoneGranted] = await Promise.all([
          checkAccessibilityPermission(),
          checkMicrophonePermission(),
        ]);

        // Also check system audio if we're waiting for it
        let sysAudioGranted = false;
        if (systemAudioAvailable) {
          sysAudioGranted = await commands.checkSystemAudioPermission();
        }

        setPermissions((prev) => {
          const newState = { ...prev };

          if (accessibilityGranted && prev.accessibility !== "granted") {
            newState.accessibility = "granted";
            // Initialize Enigo and shortcuts when accessibility is granted
            Promise.all([
              commands.initializeEnigo(),
              commands.initializeShortcuts(),
            ]).catch((e) => {
              console.warn("Failed to initialize after permission grant:", e);
            });
          }

          if (microphoneGranted && prev.microphone !== "granted") {
            newState.microphone = "granted";
          }

          if (
            sysAudioGranted &&
            systemAudioAvailable &&
            prev.systemAudio !== "granted"
          ) {
            newState.systemAudio = "granted";
          }

          return newState;
        });

        // If required permissions granted, stop polling, refresh audio devices, and proceed
        if (microphoneGranted && (!systemAudioAvailable || sysAudioGranted)) {
          if (pollingRef.current) {
            clearInterval(pollingRef.current);
            pollingRef.current = null;
          }
          await completeOnboarding();
        }

        // Reset error count on success
        errorCountRef.current = 0;
      } catch (error) {
        console.error("Error checking permissions:", error);
        errorCountRef.current += 1;

        if (errorCountRef.current >= MAX_POLLING_ERRORS) {
          // Stop polling after too many consecutive errors
          if (pollingRef.current) {
            clearInterval(pollingRef.current);
            pollingRef.current = null;
          }
          toast.error(t("onboarding.permissions.errors.checkFailed"));
        }
      }
    }, 1000);
  }, [completeOnboarding, hasWindowsMicrophoneAccess, permissionPlatform, systemAudioAvailable, t]);

  // Cleanup polling and timeouts on unmount
  useEffect(() => {
    return () => {
      if (pollingRef.current) {
        clearInterval(pollingRef.current);
      }
      if (timeoutRef.current) {
        clearTimeout(timeoutRef.current);
      }
    };
  }, []);

  const handleGrantAccessibility = async () => {
    try {
      await requestAccessibilityPermission();
      setPermissions((prev) => ({ ...prev, accessibility: "waiting" }));
      startPolling();
    } catch (error) {
      console.error("Failed to request accessibility permission:", error);
      toast.error(t("onboarding.permissions.errors.requestFailed"));
    }
  };

  const handleGrantMicrophone = async () => {
    try {
      if (isWindows) {
        await commands.openMicrophonePrivacySettings();
      } else {
        await requestMicrophonePermission();
      }

      setPermissions((prev) => ({ ...prev, microphone: "waiting" }));
      startPolling();
    } catch (error) {
      console.error("Failed to request microphone permission:", error);
      toast.error(t("onboarding.permissions.errors.requestFailed"));
    }
  };

  const handleGrantSystemAudio = async () => {
    setPermissions((prev) => ({ ...prev, systemAudio: "waiting" }));
    // Try to show the macOS permission dialog programmatically
    const granted = await commands.requestSystemAudioPermission();
    if (granted) {
      setPermissions((prev) => ({ ...prev, systemAudio: "granted" }));
      return;
    }
    // Dialog was denied or TCC API unavailable -- fall back to opening System Settings
    await commands.registerScreenRecording();
    startPolling();
  };

  const isChecking =
    permissionPlatform === null ||
    (isMacOS &&
      permissions.accessibility === "checking" &&
      permissions.microphone === "checking") ||
    (isWindows && permissions.microphone === "checking");

  // Still checking platform/initial permissions
  if (isChecking) {
    return (
      <div className="h-screen w-screen flex items-center justify-center">
        <Loader2 className="w-8 h-8 animate-spin text-text/50" />
      </div>
    );
  }

  // All permissions granted - show success briefly
  if (allGranted) {
    return (
      <div className="h-screen w-screen flex flex-col items-center justify-center gap-4">
        <div className="p-4 rounded-full bg-emerald-500/20">
          <Check className="w-12 h-12 text-emerald-400" />
        </div>
        <p className="text-lg font-medium text-text">
          {t("onboarding.permissions.allGranted")}
        </p>
      </div>
    );
  }

  // Show permissions request screen
  return (
    <div className="h-screen w-screen flex flex-col p-6 gap-6 items-center justify-center">
      <div className="flex flex-col items-center gap-2">
        <HandyTextLogo width={200} />
      </div>

      <div className="max-w-md w-full flex flex-col items-center gap-4">
        <div className="text-center mb-2">
          <h2 className="text-xl font-semibold text-text mb-2">
            {t("onboarding.permissions.title")}
          </h2>
          <p className="text-text/70">
            {t("onboarding.permissions.description")}
          </p>
        </div>

        {/* Microphone Permission Card */}
        {showMicrophonePermission && (
          <div className="w-full p-4 rounded-lg bg-white/5 border border-mid-gray/20">
            <div className="flex items-center gap-4">
              <div className="p-2.5 rounded-full bg-logo-primary/20 shrink-0">
                <Mic className="w-5 h-5 text-logo-primary" />
              </div>
              <div className="flex-1 min-w-0">
                <h3 className="font-medium text-text text-sm">
                  {t("onboarding.permissions.microphone.title")}
                </h3>
                <p className="text-xs text-text/60">
                  {t("onboarding.permissions.microphone.description")}
                </p>
              </div>
              <div className="shrink-0">
                {permissions.microphone === "granted" ? (
                  <div className="flex items-center gap-1.5 text-emerald-400 text-sm">
                    <Check className="w-4 h-4" />
                    {t("onboarding.permissions.granted")}
                  </div>
                ) : permissions.microphone === "waiting" ? (
                  <Loader2 className="w-4 h-4 animate-spin text-text/50" />
                ) : (
                  <button
                    onClick={handleGrantMicrophone}
                    className="px-3 py-1.5 rounded-lg bg-logo-primary hover:bg-logo-primary/90 text-white text-xs font-medium transition-colors whitespace-nowrap"
                  >
                    {isWindows
                      ? t("accessibility.openSettings")
                      : t("onboarding.permissions.grant")}
                  </button>
                )}
              </div>
            </div>
          </div>
        )}

        {/* System Audio Permission Card (required, macOS 14.2+ only) */}
        {systemAudioAvailable && (
          <div className="w-full p-4 rounded-lg bg-white/5 border border-mid-gray/20">
            <div className="flex items-center gap-4">
              <div className="p-2.5 rounded-full bg-logo-primary/20 shrink-0">
                <Volume2 className="w-5 h-5 text-logo-primary" />
              </div>
              <div className="flex-1 min-w-0">
                <h3 className="font-medium text-text text-sm">
                  {t("onboarding.permissions.systemAudio.title")}
                </h3>
                <p className="text-xs text-text/60">
                  {t("onboarding.permissions.systemAudio.description")}
                </p>
              </div>
              <div className="shrink-0">
                {permissions.systemAudio === "granted" ? (
                  <div className="flex items-center gap-1.5 text-emerald-400 text-sm">
                    <Check className="w-4 h-4" />
                    {t("onboarding.permissions.granted")}
                  </div>
                ) : permissions.systemAudio === "waiting" ? (
                  <Loader2 className="w-4 h-4 animate-spin text-text/50" />
                ) : (
                  <button
                    onClick={handleGrantSystemAudio}
                    className="px-3 py-1.5 rounded-lg bg-logo-primary hover:bg-logo-primary/90 text-white text-xs font-medium transition-colors whitespace-nowrap"
                  >
                    {t("onboarding.permissions.grant")}
                  </button>
                )}
              </div>
            </div>
          </div>
        )}

        {/* Optional permissions */}
        {showAccessibilityPermission && (
          <div className="w-full">
            <p className="text-xs text-text/40 uppercase tracking-wider mb-2">
              {t("onboarding.permissions.optional", "Optional — can be enabled later in Settings")}
            </p>
            <div className="w-full p-4 rounded-lg border border-mid-gray/10">
              <div className="flex items-center gap-4">
                <div className="p-2.5 rounded-full bg-white/5 shrink-0">
                  <Keyboard className="w-5 h-5 text-text/40" />
                </div>
                <div className="flex-1 min-w-0">
                  <h3 className="font-medium text-text/70 text-sm">
                    {t("onboarding.permissions.accessibility.title")}
                  </h3>
                  <p className="text-xs text-text/40">
                    {t("onboarding.permissions.accessibility.description")}
                  </p>
                </div>
                <div className="shrink-0">
                  {permissions.accessibility === "granted" ? (
                    <div className="flex items-center gap-1.5 text-emerald-400 text-sm">
                      <Check className="w-4 h-4" />
                      {t("onboarding.permissions.granted")}
                    </div>
                  ) : permissions.accessibility === "waiting" ? (
                    <Loader2 className="w-4 h-4 animate-spin text-text/50" />
                  ) : (
                    <button
                      onClick={handleGrantAccessibility}
                      className="px-3 py-1.5 rounded-lg border border-text/20 hover:border-text/40 text-text/60 text-xs font-medium transition-colors whitespace-nowrap"
                    >
                      {t("onboarding.permissions.grant")}
                    </button>
                  )}
                </div>
              </div>
            </div>
          </div>
        )}
      </div>
    </div>
  );
};

export default AccessibilityOnboarding;
