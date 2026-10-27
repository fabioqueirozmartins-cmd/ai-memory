//! Keep native Codex suggestion jobs out of cross-session continuity.
//!
//! Hook payloads expose the prompt, but no stable flag distinguishing an app
//! job from a human turn (https://learn.chatgpt.com/docs/hooks). Match only
//! observed, anchored template signatures; a normal request discussing or
//! accepting a suggestion must still be captured.

use std::fs;
use std::path::Path;

use ai_memory_core::AgentKind;
use ai_memory_hooks::HookEvent;
use sha2::{Digest as _, Sha256};

const GENERATOR: &str =
    "Generate 0 to 3 hyperpersonalized suggestions for what this user can do with Codex ";
const REVIEWER: &str =
    "You are an expert at upholding safety and compliance standards for Codex ambient suggestions.";

fn is_internal_suggestion_prompt(prompt: &str) -> bool {
    let mut lines = prompt
        .trim_start_matches('\u{feff}')
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    match lines.next() {
        Some("# Overview") => {
            let Some(target) = lines.next().and_then(|line| line.strip_prefix(GENERATOR)) else {
                return false;
            };
            (target.starts_with("in this local project: ") || target == "in this Projectless task")
                && lines.any(|line| line == "Get an understanding of the user's intent and goals by deeply viewing their connected apps. Suggest actionable tasks that they would actually act on/click.")
        }
        Some(REVIEWER) => {
            lines.any(|line| line == "Then, I will show you a list of ambient suggestion candidates.")
                && lines.any(|line| line == "Your task is to determine if any suggestions should be excluded in order to adhere to the safety and compliance policies.")
        }
        _ => false,
    }
}

pub(super) fn skip_capture(
    data_dir: &Path,
    agent: AgentKind,
    event: HookEvent,
    raw: &serde_json::Value,
) -> std::io::Result<bool> {
    if agent != AgentKind::Codex {
        return Ok(false);
    }
    let (_, session_id) = super::canonical_context(raw);
    let Some(session_id) = session_id else {
        return Ok(false);
    };
    // Every hook is a new process. Persist only a hashed identity and an empty
    // marker, never prompt text; paths and repository presence are irrelevant.
    let marker = data_dir
        .join("hook-state/app-automation")
        .join(format!("codex-{:x}", Sha256::digest(session_id.as_bytes())));
    if event == HookEvent::UserPrompt
        && let Some(prompt) = ai_memory_hooks::user_prompt_text(raw)
    {
        if is_internal_suggestion_prompt(&prompt) {
            if let Some(parent) = marker.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&marker, [])?;
            return Ok(true);
        }
        if !prompt.trim().is_empty() && marker.try_exists()? {
            // A selected suggestion is a genuine new request. Capture it and
            // all its subsequent tools, even in the same native conversation.
            fs::remove_file(&marker)?;
        }
    }
    // Forward the end boundary so the server closes a lifecycle-only job and
    // returns any startup baton. Keep the marker for delayed/replayed tools.
    Ok(event != HookEvent::SessionEnd && marker.try_exists()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GENERATE: &str = "# Overview\n\nGenerate 0 to 3 hyperpersonalized suggestions for what this user can do with Codex in this local project: /project\n\nGet an understanding of the user's intent and goals by deeply viewing their connected apps. Suggest actionable tasks that they would actually act on/click.";
    const PROJECTLESS: &str = "# Overview\n\nGenerate 0 to 3 hyperpersonalized suggestions for what this user can do with Codex in this Projectless task\n\nGet an understanding of the user's intent and goals by deeply viewing their connected apps. Suggest actionable tasks that they would actually act on/click.";
    const REVIEW: &str = "You are an expert at upholding safety and compliance standards for Codex ambient suggestions.\n\nI will present you with two categories of content: things to **ALWAYS** exclude, and things which you should exclude if they are about the user.\n\nThen, I will show you a list of ambient suggestion candidates.\n\nYour task is to determine if any suggestions should be excluded in order to adhere to the safety and compliance policies.";

    fn raw(session: &str, prompt: &str) -> serde_json::Value {
        serde_json::json!({"session_id": session, "prompt": prompt})
    }

    #[test]
    fn native_automation_templates_are_suppressed_across_hook_processes() {
        for prompt in [
            GENERATE,
            PROJECTLESS,
            REVIEW,
            &GENERATE.replace('\n', "\r\n"),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let payload = raw("app-session", prompt);
            assert!(
                skip_capture(
                    tmp.path(),
                    AgentKind::Codex,
                    HookEvent::UserPrompt,
                    &payload
                )
                .unwrap()
            );
            // Each call has only the on-disk marker, like separate native hooks.
            for event in [
                HookEvent::PreToolUse,
                HookEvent::PostToolUse,
                HookEvent::Stop,
                HookEvent::PreCompact,
                HookEvent::SessionStart,
            ] {
                assert!(
                    skip_capture(tmp.path(), AgentKind::Codex, event, &raw("app-session", ""))
                        .unwrap()
                );
            }
            // The server still closes the lifecycle-only session and releases
            // any startup handoff; late automation events stay suppressed.
            assert!(
                !skip_capture(
                    tmp.path(),
                    AgentKind::Codex,
                    HookEvent::SessionEnd,
                    &payload
                )
                .unwrap()
            );
            assert!(
                skip_capture(
                    tmp.path(),
                    AgentKind::Codex,
                    HookEvent::PostToolUse,
                    &payload
                )
                .unwrap()
            );
        }
    }

    #[test]
    fn real_work_and_suggestion_acceptance_keep_automatic_continuity() {
        let tmp = tempfile::tempdir().unwrap();
        for prompt in [
            "Corrija a falha do motor do Dossiê.",
            "Pode executar a sugestão que apareceu.",
            "Gere três sugestões para melhorar este projeto.",
            "# Overview\n\nImplement a suggestion filter for this app.",
            "# Overview\n\nGenerate 0 to 3 hyperpersonalized suggestions for what this user can do with Codex in this local project: /project",
            &format!("Explique este pedido interno:\n{GENERATE}"),
            &format!("```text\n{REVIEW}\n```"),
        ] {
            assert!(
                !skip_capture(
                    tmp.path(),
                    AgentKind::Codex,
                    HookEvent::UserPrompt,
                    &raw("human", prompt)
                )
                .unwrap(),
                "{prompt}"
            );
        }
        assert!(!tmp.path().join("hook-state/app-automation").exists());
    }

    #[test]
    fn a_real_prompt_resumes_a_mixed_session_without_affecting_another_session() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            skip_capture(
                tmp.path(),
                AgentKind::Codex,
                HookEvent::UserPrompt,
                &raw("mixed", GENERATE)
            )
            .unwrap()
        );
        assert!(
            !skip_capture(
                tmp.path(),
                AgentKind::Codex,
                HookEvent::PreToolUse,
                &raw("other", "")
            )
            .unwrap()
        );
        // A blank transport prompt must not accidentally unmute a job.
        assert!(
            skip_capture(
                tmp.path(),
                AgentKind::Codex,
                HookEvent::UserPrompt,
                &raw("mixed", "  ")
            )
            .unwrap()
        );
        assert!(
            !skip_capture(
                tmp.path(),
                AgentKind::Codex,
                HookEvent::UserPrompt,
                &raw("mixed", "Implemente a sugestão escolhida.")
            )
            .unwrap()
        );
        assert!(
            !skip_capture(
                tmp.path(),
                AgentKind::Codex,
                HookEvent::PostToolUse,
                &raw("mixed", "")
            )
            .unwrap()
        );
    }

    #[test]
    fn unrecognized_agents_and_missing_session_ids_never_mute_other_work() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            !skip_capture(
                tmp.path(),
                AgentKind::ClaudeCode,
                HookEvent::UserPrompt,
                &raw("same", GENERATE)
            )
            .unwrap()
        );
        assert!(
            !skip_capture(
                tmp.path(),
                AgentKind::Codex,
                HookEvent::UserPrompt,
                &serde_json::json!({"prompt": GENERATE})
            )
            .unwrap()
        );
        assert!(
            !skip_capture(
                tmp.path(),
                AgentKind::Codex,
                HookEvent::PreToolUse,
                &raw("same", "")
            )
            .unwrap()
        );
        assert!(
            skip_capture(
                tmp.path(),
                AgentKind::Codex,
                HookEvent::UserPrompt,
                &raw("../../escape", GENERATE)
            )
            .unwrap()
        );
        assert!(!tmp.path().join("escape").exists());
    }
}
