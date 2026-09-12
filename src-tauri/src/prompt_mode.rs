//! Spoken prompt-mode / verbatim cues for post-processing.
//!
//! Default dictation (including DJI Mic Link → `transcribe`) stays verbatim.
//! If the transcript starts with a configured prompt-mode cue, the cue is
//! stripped and the selected post-process prompt runs. A verbatim cue forces
//! the opposite, including clearing a sticky arm.

use crate::settings::{AppSettings, PostProcessProvider};
use log::debug;
use natural::phonetics::soundex;
use strsim::levenshtein;
use tauri::{AppHandle, Emitter};

/// Why post-processing was skipped after it was requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostProcessFallbackReason {
    NotConfigured,
    Failed,
}

impl PostProcessFallbackReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotConfigured => "not_configured",
            Self::Failed => "failed",
        }
    }
}

/// Which spoken cue (if any) was recognized at the start of a transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpokenModeCue {
    Prompt,
    Verbatim,
}

/// Result of applying spoken-mode cues (and optional sticky arm) to a transcript.
#[derive(Debug, Clone, PartialEq)]
pub struct SpokenModeResolution {
    /// Transcript with a leading cue removed, if one matched.
    pub text: String,
    /// Whether this utterance should run the selected post-process prompt.
    pub post_process: bool,
    pub cue: Option<SpokenModeCue>,
    pub sticky_armed: bool,
    pub sticky_changed: bool,
}

/// Decide post-processing from the binding flag, a leading spoken cue, and
/// the optional sticky arm.
pub fn resolve_spoken_mode(
    transcription: &str,
    binding_post_process: bool,
    settings: &AppSettings,
) -> SpokenModeResolution {
    let (cue, text) = match_leading_cue(transcription, settings);

    let mut sticky_armed = settings.prompt_mode_sticky_armed;
    let mut sticky_changed = false;

    if settings.prompt_mode_sticky_enabled {
        match cue {
            Some(SpokenModeCue::Prompt) if !sticky_armed => {
                sticky_armed = true;
                sticky_changed = true;
            }
            Some(SpokenModeCue::Verbatim) if sticky_armed => {
                sticky_armed = false;
                sticky_changed = true;
            }
            _ => {}
        }
    }

    let post_process = match cue {
        Some(SpokenModeCue::Prompt) => true,
        Some(SpokenModeCue::Verbatim) => false,
        None if settings.prompt_mode_sticky_enabled && settings.prompt_mode_sticky_armed => true,
        None => binding_post_process,
    };

    if cue.is_some() || sticky_changed {
        debug!(
            "Spoken mode cue={cue:?} post_process={post_process} sticky_armed={sticky_armed} sticky_changed={sticky_changed}"
        );
    }

    SpokenModeResolution {
        text,
        post_process,
        cue,
        sticky_armed,
        sticky_changed,
    }
}

/// Persist sticky-arm changes and notify the UI.
pub fn persist_sticky_if_changed(app: &AppHandle, resolution: &SpokenModeResolution) {
    if !resolution.sticky_changed {
        return;
    }

    let mut settings = crate::settings::get_settings(app);
    settings.prompt_mode_sticky_armed = resolution.sticky_armed;
    crate::settings::write_settings(app, settings);

    let _ = app.emit(
        "settings-changed",
        serde_json::json!({
            "setting": "prompt_mode_sticky_armed",
            "value": resolution.sticky_armed
        }),
    );
    let _ = app.emit(
        "prompt-mode-changed",
        serde_json::json!({
            "armed": resolution.sticky_armed
        }),
    );
}

pub fn emit_post_process_fallback(app: &AppHandle, reason: PostProcessFallbackReason) {
    let _ = app.emit("post-process-fallback", reason.as_str());
}

/// True when a selected provider, model, and prompt are ready for an LLM call.
/// An empty API key is allowed (local custom endpoints); the request itself
/// still fails closed to verbatim if the provider rejects it.
///
/// CLI harness providers (Grok / Claude / Codex) intentionally leave the
/// model empty to mean "CLI default" — same rule as `post_process_transcription`.
pub fn post_process_is_configured(settings: &AppSettings) -> bool {
    let Some(provider) = settings.active_post_process_provider() else {
        return false;
    };

    let model = settings
        .post_process_models
        .get(&provider.id)
        .map(|m| m.trim())
        .unwrap_or("");
    // API providers still need a model. CLI harnesses use the tool default.
    if model.is_empty() && !provider.is_cli() {
        return false;
    }

    let Some(prompt_id) = settings.post_process_selected_prompt_id.as_ref() else {
        return false;
    };
    settings
        .post_process_prompts
        .iter()
        .any(|prompt| &prompt.id == prompt_id && !prompt.prompt.trim().is_empty())
}

/// Same IDs as `cli_harness::is_cli_provider`. An inherent
/// `PostProcessProvider::is_cli` (CLI-harness PR) is preferred when both exist.
trait CliProvider {
    fn is_cli(&self) -> bool;
}

impl CliProvider for PostProcessProvider {
    fn is_cli(&self) -> bool {
        matches!(
            self.id.as_str(),
            "claude_code_cli" | "codex_cli" | "grok_cli"
        )
    }
}

fn match_leading_cue(
    transcription: &str,
    settings: &AppSettings,
) -> (Option<SpokenModeCue>, String) {
    let trimmed = transcription.trim();
    if trimmed.is_empty() {
        return (None, String::new());
    }

    let threshold = settings.word_correction_threshold;
    let prompt_match = best_cue_match(trimmed, &settings.prompt_mode_cues, threshold);
    let verbatim_match = best_cue_match(trimmed, &settings.verbatim_cues, threshold);

    match (prompt_match, verbatim_match) {
        (Some(prompt), Some(verbatim)) => {
            // Prefer the longer cue; on a tie, prefer verbatim so we do not
            // accidentally rewrite.
            if verbatim.matched_key_len > prompt.matched_key_len {
                (Some(SpokenModeCue::Verbatim), verbatim.remaining)
            } else if prompt.matched_key_len > verbatim.matched_key_len {
                (Some(SpokenModeCue::Prompt), prompt.remaining)
            } else {
                (Some(SpokenModeCue::Verbatim), verbatim.remaining)
            }
        }
        (Some(prompt), None) => (Some(SpokenModeCue::Prompt), prompt.remaining),
        (None, Some(verbatim)) => (Some(SpokenModeCue::Verbatim), verbatim.remaining),
        (None, None) => (None, trimmed.to_string()),
    }
}

struct CueMatch {
    remaining: String,
    matched_key_len: usize,
}

fn best_cue_match(text: &str, cues: &[String], threshold: f64) -> Option<CueMatch> {
    let mut best: Option<CueMatch> = None;

    for cue in cues {
        if let Some(remaining) = strip_leading_cue(text, cue, threshold) {
            let key_len = normalize_key(cue).chars().count();
            let is_better = best
                .as_ref()
                .is_none_or(|current| key_len > current.matched_key_len);
            if is_better {
                best = Some(CueMatch {
                    remaining,
                    matched_key_len: key_len,
                });
            }
        }
    }

    best
}

fn strip_leading_cue(text: &str, cue: &str, threshold: f64) -> Option<String> {
    let cue = cue.trim();
    if cue.is_empty() {
        return None;
    }

    let tokens: Vec<&str> = text.split_whitespace().collect();
    let cue_tokens: Vec<&str> = cue.split_whitespace().collect();
    if tokens.is_empty() || cue_tokens.is_empty() {
        return None;
    }

    if tokens.len() >= cue_tokens.len() {
        let candidate = tokens[..cue_tokens.len()].join(" ");
        if keys_match(&candidate, cue, threshold) {
            return Some(remaining_after_tokens(&tokens, cue_tokens.len()));
        }
    }

    // ASR sometimes mashes a multi-word cue into one token ("promptmode").
    if cue_tokens.len() > 1 && keys_match(tokens[0], cue, threshold) {
        return Some(remaining_after_tokens(&tokens, 1));
    }

    None
}

fn remaining_after_tokens(tokens: &[&str], skip: usize) -> String {
    if skip >= tokens.len() {
        return String::new();
    }

    tokens[skip..]
        .join(" ")
        .trim_start_matches(|c: char| {
            c.is_whitespace()
                || matches!(c, ',' | '.' | ':' | ';' | '-' | '—' | '–' | '!' | '?' | '"')
        })
        .trim()
        .to_string()
}

fn normalize_key(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn keys_match(candidate: &str, cue: &str, threshold: f64) -> bool {
    let candidate_key = normalize_key(candidate);
    let cue_key = normalize_key(cue);
    if candidate_key.is_empty() || cue_key.is_empty() {
        return false;
    }
    if candidate_key == cue_key {
        return true;
    }

    // Same ASCII-only guard as custom-word fuzzy matching.
    if !candidate_key.chars().all(|c| c.is_ascii_alphanumeric())
        || !cue_key.chars().all(|c| c.is_ascii_alphanumeric())
    {
        return false;
    }

    let candidate_len = candidate_key.chars().count();
    let cue_len = cue_key.chars().count();
    let max_len = candidate_len.max(cue_len) as f64;
    let len_diff = candidate_len.abs_diff(cue_len) as f64;
    if len_diff > (max_len * 0.25).max(2.0) {
        return false;
    }

    let levenshtein_score = levenshtein(&candidate_key, &cue_key) as f64 / max_len;
    // Soundex on long concatenated keys is coarse (e.g. "promptmodeclean"
    // vs "promptrewrite"). Only boost already-close misses, matching the
    // spirit of custom-word fuzzy matching without swallowing the next word.
    let phonetic_match = levenshtein_score < 0.35
        && candidate_key.chars().all(|c| c.is_ascii_alphabetic())
        && cue_key.chars().all(|c| c.is_ascii_alphabetic())
        && soundex(&candidate_key, &cue_key);
    let score = if phonetic_match {
        levenshtein_score * 0.3
    } else {
        levenshtein_score
    };
    score < threshold
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{AppSettings, LLMPrompt, PostProcessProvider};
    use std::collections::HashMap;

    fn provider(id: &str) -> PostProcessProvider {
        PostProcessProvider {
            id: id.to_string(),
            label: id.to_string(),
            base_url: String::new(),
            allow_base_url_edit: false,
            models_endpoint: None,
            supports_structured_output: false,
        }
    }

    fn settings_for_provider(provider_id: &str, model: &str) -> AppSettings {
        AppSettings {
            post_process_provider_id: provider_id.to_string(),
            post_process_providers: vec![provider(provider_id)],
            post_process_models: HashMap::from([(provider_id.to_string(), model.to_string())]),
            post_process_prompts: vec![LLMPrompt {
                id: "rewrite".into(),
                name: "Rewrite".into(),
                prompt: "Rewrite ${output}".into(),
            }],
            post_process_selected_prompt_id: Some("rewrite".into()),
            ..Default::default()
        }
    }

    fn settings_with_cues() -> AppSettings {
        AppSettings {
            prompt_mode_cues: vec!["prompt mode".into(), "prompt rewrite".into()],
            verbatim_cues: vec!["verbatim".into(), "plain mode".into()],
            prompt_mode_sticky_enabled: true,
            prompt_mode_sticky_armed: false,
            word_correction_threshold: 0.18,
            ..Default::default()
        }
    }

    #[test]
    fn no_cue_keeps_verbatim_binding() {
        let settings = settings_with_cues();
        let resolved = resolve_spoken_mode("hello from the mic", false, &settings);
        assert_eq!(resolved.text, "hello from the mic");
        assert!(!resolved.post_process);
        assert_eq!(resolved.cue, None);
        assert!(!resolved.sticky_changed);
    }

    #[test]
    fn no_cue_keeps_post_process_binding() {
        let settings = settings_with_cues();
        let resolved = resolve_spoken_mode("hello from the mic", true, &settings);
        assert!(resolved.post_process);
        assert_eq!(resolved.cue, None);
    }

    #[test]
    fn prompt_cue_strips_and_enables_post_process() {
        let settings = settings_with_cues();
        let resolved = resolve_spoken_mode("Prompt mode, rewrite this note", false, &settings);
        assert_eq!(resolved.text, "rewrite this note");
        assert!(resolved.post_process);
        assert_eq!(resolved.cue, Some(SpokenModeCue::Prompt));
        assert!(resolved.sticky_armed);
        assert!(resolved.sticky_changed);
    }

    #[test]
    fn prompt_rewrite_cue_is_recognized() {
        let settings = settings_with_cues();
        let resolved = resolve_spoken_mode("prompt rewrite please clean this", false, &settings);
        assert_eq!(resolved.text, "please clean this");
        assert_eq!(resolved.cue, Some(SpokenModeCue::Prompt));
    }

    #[test]
    fn verbatim_cue_forces_verbatim_on_post_process_binding() {
        let mut settings = settings_with_cues();
        settings.prompt_mode_sticky_armed = true;
        let resolved = resolve_spoken_mode("Verbatim keep this exact", true, &settings);
        assert_eq!(resolved.text, "keep this exact");
        assert!(!resolved.post_process);
        assert_eq!(resolved.cue, Some(SpokenModeCue::Verbatim));
        assert!(!resolved.sticky_armed);
        assert!(resolved.sticky_changed);
    }

    #[test]
    fn plain_mode_cue_clears_sticky() {
        let mut settings = settings_with_cues();
        settings.prompt_mode_sticky_armed = true;
        let resolved = resolve_spoken_mode("plain mode hello", false, &settings);
        assert_eq!(resolved.text, "hello");
        assert!(!resolved.post_process);
        assert!(!resolved.sticky_armed);
    }

    #[test]
    fn sticky_arm_applies_without_a_new_cue() {
        let mut settings = settings_with_cues();
        settings.prompt_mode_sticky_armed = true;
        let resolved = resolve_spoken_mode("next dictation only", false, &settings);
        assert_eq!(resolved.text, "next dictation only");
        assert!(resolved.post_process);
        assert_eq!(resolved.cue, None);
        assert!(!resolved.sticky_changed);
    }

    #[test]
    fn sticky_disabled_is_per_utterance_only() {
        let mut settings = settings_with_cues();
        settings.prompt_mode_sticky_enabled = false;
        let resolved = resolve_spoken_mode("prompt mode just this once", false, &settings);
        assert_eq!(resolved.text, "just this once");
        assert!(resolved.post_process);
        assert!(!resolved.sticky_changed);
        assert!(!resolved.sticky_armed);
    }

    #[test]
    fn cue_only_utterance_leaves_empty_text_and_arms() {
        let settings = settings_with_cues();
        let resolved = resolve_spoken_mode("prompt mode", false, &settings);
        assert_eq!(resolved.text, "");
        assert!(resolved.post_process);
        assert!(resolved.sticky_armed);
    }

    #[test]
    fn cue_in_the_middle_is_ignored() {
        let settings = settings_with_cues();
        let resolved = resolve_spoken_mode("please use prompt mode later", false, &settings);
        assert_eq!(resolved.text, "please use prompt mode later");
        assert!(!resolved.post_process);
        assert_eq!(resolved.cue, None);
    }

    #[test]
    fn fuzzy_prompt_cue_matches_light_asr_noise() {
        let settings = settings_with_cues();
        let resolved = resolve_spoken_mode("prompt moad clean this up", false, &settings);
        assert_eq!(resolved.text, "clean this up");
        assert_eq!(resolved.cue, Some(SpokenModeCue::Prompt));
    }

    #[test]
    fn mashed_single_token_cue_matches() {
        let settings = settings_with_cues();
        let resolved = resolve_spoken_mode("promptmode clean this up", false, &settings);
        assert_eq!(resolved.text, "clean this up");
        assert_eq!(resolved.cue, Some(SpokenModeCue::Prompt));
    }

    #[test]
    fn empty_cue_lists_never_match() {
        let settings = AppSettings {
            prompt_mode_cues: vec![],
            verbatim_cues: vec![],
            ..settings_with_cues()
        };
        let resolved = resolve_spoken_mode("prompt mode hello", false, &settings);
        assert_eq!(resolved.text, "prompt mode hello");
        assert!(!resolved.post_process);
    }

    #[test]
    fn longer_cue_wins_over_shorter_overlap() {
        let settings = AppSettings {
            prompt_mode_cues: vec!["prompt".into(), "prompt mode".into()],
            ..settings_with_cues()
        };
        let resolved = resolve_spoken_mode("prompt mode hello", false, &settings);
        assert_eq!(resolved.text, "hello");
        assert_eq!(resolved.cue, Some(SpokenModeCue::Prompt));
    }

    #[test]
    fn cli_provider_with_empty_model_is_configured() {
        let settings = settings_for_provider("grok_cli", "");
        assert!(
            settings.active_post_process_provider().unwrap().is_cli(),
            "Grok CLI must be treated as a CLI harness provider"
        );
        assert!(
            post_process_is_configured(&settings),
            "empty model is the CLI default and must not look unconfigured"
        );
    }

    #[test]
    fn api_provider_with_empty_model_is_not_configured() {
        let settings = settings_for_provider("openai", "");
        assert!(!settings.active_post_process_provider().unwrap().is_cli());
        assert!(!post_process_is_configured(&settings));
    }

    #[test]
    fn api_provider_with_model_and_prompt_is_configured() {
        let settings = settings_for_provider("openai", "gpt-4o-mini");
        assert!(post_process_is_configured(&settings));
    }

    #[test]
    fn cli_provider_still_requires_a_selected_prompt() {
        let mut settings = settings_for_provider("grok_cli", "");
        settings.post_process_selected_prompt_id = None;
        assert!(!post_process_is_configured(&settings));
    }
}
