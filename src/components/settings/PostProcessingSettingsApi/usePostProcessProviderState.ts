import { useCallback, useMemo, useState } from "react";
import { useSettings } from "../../../hooks/useSettings";
import {
  commands,
  type CliHarnessStatus,
  type PostProcessProvider,
} from "@/bindings";
import type { ModelOption } from "./types";
import type { DropdownOption } from "../../ui/Dropdown";

const APPLE_PROVIDER_ID = "apple_intelligence";
const CLI_PROVIDER_IDS = ["claude_code_cli", "codex_cli", "grok_cli"];

const isCliProviderId = (providerId: string | undefined): boolean => {
  if (!providerId) return false;
  return CLI_PROVIDER_IDS.includes(providerId);
};

type PostProcessProviderState = {
  providerOptions: DropdownOption[];
  selectedProviderId: string;
  selectedProvider: PostProcessProvider | undefined;
  isCustomProvider: boolean;
  isAppleProvider: boolean;
  isCliProvider: boolean;
  appleIntelligenceUnavailable: boolean;
  baseUrl: string;
  handleBaseUrlChange: (value: string) => void;
  isBaseUrlUpdating: boolean;
  apiKey: string;
  handleApiKeyChange: (value: string) => void;
  isApiKeyUpdating: boolean;
  model: string;
  handleModelChange: (value: string) => void;
  modelOptions: ModelOption[];
  isModelUpdating: boolean;
  isFetchingModels: boolean;
  cliBinaryPath: string;
  cliConfigDir: string;
  cliTimeoutSecs: number;
  handleCliBinaryPathChange: (value: string) => void;
  handleCliConfigDirChange: (value: string) => void;
  handleCliTimeoutChange: (value: number) => void;
  isCliBinaryUpdating: boolean;
  isCliConfigDirUpdating: boolean;
  isCliTimeoutUpdating: boolean;
  cliStatus: CliHarnessStatus | null;
  isCliStatusChecking: boolean;
  handleCheckCliStatus: () => void;
  handleProviderSelect: (providerId: string) => void;
  handleModelSelect: (value: string) => void;
  handleModelCreate: (value: string) => void;
  handleRefreshModels: () => void;
};

export const usePostProcessProviderState = (): PostProcessProviderState => {
  const {
    settings,
    isUpdating,
    setPostProcessProvider,
    updatePostProcessBaseUrl,
    updatePostProcessApiKey,
    updatePostProcessModel,
    updatePostProcessCliBinary,
    updatePostProcessCliConfigDir,
    updatePostProcessCliTimeout,
    fetchPostProcessModels,
    postProcessModelOptions,
  } = useSettings();

  // Settings are guaranteed to have providers after migration
  const providers = settings?.post_process_providers || [];

  const selectedProviderId = useMemo(() => {
    return settings?.post_process_provider_id || providers[0]?.id || "openai";
  }, [providers, settings?.post_process_provider_id]);

  const selectedProvider = useMemo(() => {
    return (
      providers.find((provider) => provider.id === selectedProviderId) ||
      providers[0]
    );
  }, [providers, selectedProviderId]);

  const isAppleProvider = selectedProvider?.id === APPLE_PROVIDER_ID;
  const isCliProvider =
    selectedProvider?.kind === "cli" || isCliProviderId(selectedProvider?.id);
  const [appleIntelligenceUnavailable, setAppleIntelligenceUnavailable] =
    useState(false);
  const [cliStatus, setCliStatus] = useState<CliHarnessStatus | null>(null);
  const [isCliStatusChecking, setIsCliStatusChecking] = useState(false);

  // Use settings directly as single source of truth
  const baseUrl = selectedProvider?.base_url ?? "";
  const apiKey = settings?.post_process_api_keys?.[selectedProviderId] ?? "";
  const model = settings?.post_process_models?.[selectedProviderId] ?? "";
  const cliSettings = settings?.post_process_cli?.[selectedProviderId];
  const cliBinaryPath = cliSettings?.binary_path ?? "";
  const cliConfigDir = cliSettings?.config_dir ?? "";
  const cliTimeoutSecs = cliSettings?.timeout_secs ?? 90;

  const providerOptions = useMemo<DropdownOption[]>(() => {
    return providers.map((provider) => ({
      value: provider.id,
      label: provider.label,
    }));
  }, [providers]);

  const probeCliProvider = useCallback(async (providerId: string) => {
    setIsCliStatusChecking(true);
    try {
      const result = await commands.probePostProcessCli(providerId);
      if (result.status === "ok") {
        setCliStatus(result.data);
      } else {
        setCliStatus({
          provider_id: providerId,
          binary_name: "",
          resolved_binary: null,
          binary_found: false,
          logged_in: null,
          message: result.error,
          login_hint: "",
        });
      }
    } catch (error) {
      setCliStatus({
        provider_id: providerId,
        binary_name: "",
        resolved_binary: null,
        binary_found: false,
        logged_in: null,
        message:
          error instanceof Error
            ? error.message
            : "Failed to check CLI status.",
        login_hint: "",
      });
    } finally {
      setIsCliStatusChecking(false);
    }
  }, []);

  const handleProviderSelect = useCallback(
    async (providerId: string) => {
      // Clear error state on any selection attempt (allows dismissing the error)
      setAppleIntelligenceUnavailable(false);
      setCliStatus(null);

      if (providerId === selectedProviderId) return;

      // Check Apple Intelligence availability before selecting
      if (providerId === APPLE_PROVIDER_ID) {
        const available = await commands.checkAppleIntelligenceAvailable();
        if (!available) {
          setAppleIntelligenceUnavailable(true);
          // Don't return - still set the provider so dropdown shows the selection
          // The backend gracefully handles unavailable Apple Intelligence
        }
      }

      await setPostProcessProvider(providerId);

      if (isCliProviderId(providerId)) {
        void probeCliProvider(providerId);
        return;
      }

      // Auto-fetch available models for the new provider so the model dropdown
      // reflects what's actually valid. Without this, a stale model value from
      // a previous provider/base_url can persist and silently 404 at runtime.
      // Skip when the provider isn't configured yet (no API key / empty base URL)
      // to avoid unnecessary backend errors.
      if (providerId !== APPLE_PROVIDER_ID) {
        const provider = providers.find((p) => p.id === providerId);
        const apiKey = settings?.post_process_api_keys?.[providerId] ?? "";
        const hasBaseUrl = (provider?.base_url ?? "").trim() !== "";
        const hasApiKey = apiKey.trim() !== "";

        if (provider?.id === "custom" ? hasBaseUrl : hasApiKey) {
          void fetchPostProcessModels(providerId);
        }
      }
    },
    [
      selectedProviderId,
      setPostProcessProvider,
      fetchPostProcessModels,
      probeCliProvider,
      providers,
      settings,
    ],
  );

  const handleBaseUrlChange = useCallback(
    (value: string) => {
      if (!selectedProvider || selectedProvider.id !== "custom") {
        return;
      }
      const trimmed = value.trim();
      if (trimmed && trimmed !== baseUrl) {
        void updatePostProcessBaseUrl(selectedProvider.id, trimmed);
      }
    },
    [selectedProvider, baseUrl, updatePostProcessBaseUrl],
  );

  const handleApiKeyChange = useCallback(
    (value: string) => {
      const trimmed = value.trim();
      if (trimmed !== apiKey) {
        void updatePostProcessApiKey(selectedProviderId, trimmed);
      }
    },
    [apiKey, selectedProviderId, updatePostProcessApiKey],
  );

  const handleModelChange = useCallback(
    (value: string) => {
      const trimmed = value.trim();
      if (trimmed !== model) {
        void updatePostProcessModel(selectedProviderId, trimmed);
      }
    },
    [model, selectedProviderId, updatePostProcessModel],
  );

  const handleModelSelect = useCallback(
    (value: string) => {
      void updatePostProcessModel(selectedProviderId, value.trim());
    },
    [selectedProviderId, updatePostProcessModel],
  );

  const handleModelCreate = useCallback(
    (value: string) => {
      void updatePostProcessModel(selectedProviderId, value);
    },
    [selectedProviderId, updatePostProcessModel],
  );

  const handleRefreshModels = useCallback(() => {
    if (isAppleProvider || isCliProvider) return;
    void fetchPostProcessModels(selectedProviderId);
  }, [
    fetchPostProcessModels,
    isAppleProvider,
    isCliProvider,
    selectedProviderId,
  ]);

  const handleCliBinaryPathChange = useCallback(
    (value: string) => {
      const trimmed = value.trim();
      if (trimmed !== cliBinaryPath) {
        void updatePostProcessCliBinary(selectedProviderId, trimmed);
      }
    },
    [cliBinaryPath, selectedProviderId, updatePostProcessCliBinary],
  );

  const handleCliConfigDirChange = useCallback(
    (value: string) => {
      const trimmed = value.trim();
      if (trimmed !== cliConfigDir) {
        void updatePostProcessCliConfigDir(selectedProviderId, trimmed);
      }
    },
    [cliConfigDir, selectedProviderId, updatePostProcessCliConfigDir],
  );

  const handleCliTimeoutChange = useCallback(
    (value: number) => {
      if (!Number.isFinite(value) || value === cliTimeoutSecs) {
        return;
      }
      void updatePostProcessCliTimeout(selectedProviderId, value);
    },
    [cliTimeoutSecs, selectedProviderId, updatePostProcessCliTimeout],
  );

  const handleCheckCliStatus = useCallback(() => {
    if (!isCliProvider) return;
    void probeCliProvider(selectedProviderId);
  }, [isCliProvider, probeCliProvider, selectedProviderId]);

  const availableModelsRaw = postProcessModelOptions[selectedProviderId] || [];

  const modelOptions = useMemo<ModelOption[]>(() => {
    const seen = new Set<string>();
    const options: ModelOption[] = [];

    const upsert = (value: string | null | undefined) => {
      const trimmed = value?.trim();
      if (!trimmed || seen.has(trimmed)) return;
      seen.add(trimmed);
      options.push({ value: trimmed, label: trimmed });
    };

    // Add available models from API
    for (const candidate of availableModelsRaw) {
      upsert(candidate);
    }

    // Ensure current model is in the list
    upsert(model);

    return options;
  }, [availableModelsRaw, model]);

  const isBaseUrlUpdating = isUpdating(
    `post_process_base_url:${selectedProviderId}`,
  );
  const isApiKeyUpdating = isUpdating(
    `post_process_api_key:${selectedProviderId}`,
  );
  const isModelUpdating = isUpdating(
    `post_process_model:${selectedProviderId}`,
  );
  const isFetchingModels = isUpdating(
    `post_process_models_fetch:${selectedProviderId}`,
  );
  const isCliBinaryUpdating = isUpdating(
    `post_process_cli_binary:${selectedProviderId}`,
  );
  const isCliConfigDirUpdating = isUpdating(
    `post_process_cli_config_dir:${selectedProviderId}`,
  );
  const isCliTimeoutUpdating = isUpdating(
    `post_process_cli_timeout:${selectedProviderId}`,
  );

  const isCustomProvider = selectedProvider?.id === "custom";

  // No automatic fetching - user must click refresh button

  return {
    providerOptions,
    selectedProviderId,
    selectedProvider,
    isCustomProvider,
    isAppleProvider,
    isCliProvider,
    appleIntelligenceUnavailable,
    baseUrl,
    handleBaseUrlChange,
    isBaseUrlUpdating,
    apiKey,
    handleApiKeyChange,
    isApiKeyUpdating,
    model,
    handleModelChange,
    modelOptions,
    isModelUpdating,
    isFetchingModels,
    cliBinaryPath,
    cliConfigDir,
    cliTimeoutSecs,
    handleCliBinaryPathChange,
    handleCliConfigDirChange,
    handleCliTimeoutChange,
    isCliBinaryUpdating,
    isCliConfigDirUpdating,
    isCliTimeoutUpdating,
    cliStatus,
    isCliStatusChecking,
    handleCheckCliStatus,
    handleProviderSelect,
    handleModelSelect,
    handleModelCreate,
    handleRefreshModels,
  };
};
