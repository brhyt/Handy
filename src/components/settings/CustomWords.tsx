import React, { useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import type { CustomWord } from "@/bindings";
import { useSettings } from "../../hooks/useSettings";
import { Input } from "../ui/Input";
import { Button } from "../ui/Button";
import { SettingContainer } from "../ui/SettingContainer";

interface CustomWordsProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

const normalizeCustomWordPart = (word: string) =>
  word
    .replace(/[<>"']/g, "")
    .replace(/\s+/g, " ")
    .trim();

const pairKey = (word: CustomWord) => {
  const written = word.written.trim().toLowerCase();
  const spoken = word.spoken.trim().toLowerCase() || written;
  return `${spoken}\0${written}`;
};

export const CustomWords: React.FC<CustomWordsProps> = React.memo(
  ({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, isUpdating } = useSettings();
    const [newSpoken, setNewSpoken] = useState("");
    const [newWritten, setNewWritten] = useState("");
    const customWords = getSetting("custom_words") || [];
    const spoken = normalizeCustomWordPart(newSpoken);
    const written = normalizeCustomWordPart(newWritten);
    const canAdd =
      Boolean(written) && written.length <= 50 && spoken.length <= 50;

    const formatPair = (word: CustomWord) => {
      const spokenLabel = word.spoken.trim();
      const writtenLabel = word.written.trim();
      if (
        !spokenLabel ||
        spokenLabel.toLowerCase() === writtenLabel.toLowerCase()
      ) {
        return writtenLabel;
      }
      return t("settings.advanced.customWords.pairLabel", {
        spoken: spokenLabel,
        written: writtenLabel,
      });
    };

    const handleAddWord = () => {
      if (!canAdd) {
        return;
      }

      const nextWord: CustomWord = { spoken, written };
      if (customWords.some((word) => pairKey(word) === pairKey(nextWord))) {
        toast.error(
          t("settings.advanced.customWords.duplicate", {
            word: formatPair(nextWord),
          }),
        );
        return;
      }

      updateSetting("custom_words", [...customWords, nextWord]);
      setNewSpoken("");
      setNewWritten("");
    };

    const handleRemoveWord = (indexToRemove: number) => {
      updateSetting(
        "custom_words",
        customWords.filter((_, index) => index !== indexToRemove),
      );
    };

    const handleKeyPress = (e: React.KeyboardEvent) => {
      if (e.key === "Enter") {
        e.preventDefault();
        handleAddWord();
      }
    };

    return (
      <>
        <SettingContainer
          title={t("settings.advanced.customWords.title")}
          description={t("settings.advanced.customWords.description")}
          descriptionMode={descriptionMode}
          grouped={grouped}
          layout="stacked"
        >
          <div className="flex flex-wrap items-center gap-2">
            <Input
              type="text"
              className="max-w-36"
              value={newSpoken}
              onChange={(e) => setNewSpoken(e.target.value)}
              onKeyDown={handleKeyPress}
              placeholder={t("settings.advanced.customWords.spokenPlaceholder")}
              variant="compact"
              disabled={isUpdating("custom_words")}
            />
            <Input
              type="text"
              className="max-w-36"
              value={newWritten}
              onChange={(e) => setNewWritten(e.target.value)}
              onKeyDown={handleKeyPress}
              placeholder={t(
                "settings.advanced.customWords.writtenPlaceholder",
              )}
              variant="compact"
              disabled={isUpdating("custom_words")}
            />
            <Button
              onClick={handleAddWord}
              disabled={!canAdd || isUpdating("custom_words")}
              variant="primary"
              size="md"
            >
              {t("settings.advanced.customWords.add")}
            </Button>
          </div>
        </SettingContainer>
        {customWords.length > 0 && (
          <div
            className={`px-4 p-2 ${grouped ? "" : "rounded-lg border border-mid-gray/20"} flex flex-wrap gap-1`}
          >
            {customWords.map((word, index) => {
              const label = formatPair(word);
              return (
                <Button
                  key={`${pairKey(word)}-${index}`}
                  onClick={() => handleRemoveWord(index)}
                  disabled={isUpdating("custom_words")}
                  variant="secondary"
                  size="sm"
                  className="inline-flex items-center gap-1 cursor-pointer"
                  aria-label={t("settings.advanced.customWords.remove", {
                    word: label,
                  })}
                >
                  <span>{label}</span>
                  <svg
                    className="w-3 h-3"
                    fill="none"
                    stroke="currentColor"
                    viewBox="0 0 24 24"
                  >
                    <path
                      strokeLinecap="round"
                      strokeLinejoin="round"
                      strokeWidth={2}
                      d="M6 18L18 6M6 6l12 12"
                    />
                  </svg>
                </Button>
              );
            })}
          </div>
        )}
      </>
    );
  },
);
