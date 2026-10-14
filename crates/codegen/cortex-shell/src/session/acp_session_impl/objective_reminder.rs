//! Per-model objective reminder for `SessionActor`: before each human prompt, restate the user's earlier prompts and frame the coming message as the current request.
//! It compensates for models that latch onto one word of a request, or lose earlier constraints as the context grows.
use super::*;
use cortex_config_types::ObjectiveReminderPerModelConfig;

const DEFAULT_MAX_PRIOR_INSTRUCTIONS: usize = 8;
const DEFAULT_MAX_CHARS: usize = 4000;

const PRIOR_REQUESTS_HEADER: &str = "Earlier requests from the user in this session, oldest first. \
Their constraints still apply unless a later request changed them:";

const CURRENT_REQUEST_DIRECTIVE: &str = "The next user message is the current request. \
Work out what it asks for as a whole, reading the earlier requests only as context and constraints; \
when two instructions conflict, the later one wins. \
Do not act on a single word or name that also fits something else: \
settle which file, symbol, or setting the request means before changing anything, \
and change only what it asks for.";

/// Render the reminder pushed ahead of a human prompt, from the real user queries already in the conversation.
/// The most recent `max_prior_instructions` queries are kept, then the oldest are dropped until their text fits in `max_chars`.
pub(crate) fn objective_reminder_text(
    prior_queries: &[String],
    cfg: &ObjectiveReminderPerModelConfig,
) -> String {
    let max_prior = cfg
        .max_prior_instructions
        .map_or(DEFAULT_MAX_PRIOR_INSTRUCTIONS, |n| n as usize);
    let max_chars = cfg.max_chars.map_or(DEFAULT_MAX_CHARS, |n| n as usize);
    let prior: Vec<&str> = prior_queries
        .iter()
        .map(|q| q.trim())
        .filter(|q| !q.is_empty())
        .collect();
    let mut kept = &prior[prior.len().saturating_sub(max_prior)..];
    while !kept.is_empty() && kept.iter().map(|q| q.chars().count()).sum::<usize>() > max_chars {
        kept = &kept[1..];
    }
    if kept.is_empty() {
        return CURRENT_REQUEST_DIRECTIVE.to_owned();
    }
    let omitted = prior.len() - kept.len();
    let mut text = String::from(PRIOR_REQUESTS_HEADER);
    if omitted > 0 {
        text.push_str(&format!("\n({omitted} older requests omitted.)"));
    }
    for (i, query) in kept.iter().enumerate() {
        text.push_str(&format!("\n{}. {query}", omitted + i + 1));
    }
    text.push_str("\n\n");
    text.push_str(CURRENT_REQUEST_DIRECTIVE);
    text
}

impl SessionActor {
    /// Called from `handle_prompt()` for human prompts only, after every other per-turn reminder, so this one sits directly before the user's message.
    /// The restated queries are user-authored, so both reminder tags are escaped inside them.
    pub(super) async fn inject_objective_reminder(&self) {
        let model_id = self.models_manager.current_model_id().0.to_string();
        let cfg = self.models_manager.objective_reminder_for(&model_id);
        if !cfg.enabled {
            return;
        }
        let conversation = self.chat_state_handle.get_conversation().await;
        let prior_queries =
            cortex_chat_state::compaction_utils::extract_real_user_queries(&conversation);
        let text = objective_reminder_text(&prior_queries, &cfg);
        self.chat_state_handle
            .push_user_message(wrap_untrusted_in_reminder_tag(
                &text,
                self.reminder_wrapper_tag(),
            ));
        tracing::debug!(
            model_id,
            prior_queries = prior_queries.len(),
            "Injected objective reminder"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(max_prior: Option<u32>, max_chars: Option<u32>) -> ObjectiveReminderPerModelConfig {
        ObjectiveReminderPerModelConfig {
            enabled: true,
            max_prior_instructions: max_prior,
            max_chars,
        }
    }

    fn queries(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn first_prompt_gets_only_the_directive() {
        assert_eq!(
            objective_reminder_text(&[], &cfg(None, None)),
            CURRENT_REQUEST_DIRECTIVE
        );
        assert_eq!(
            objective_reminder_text(&queries(&["  ", ""]), &cfg(None, None)),
            CURRENT_REQUEST_DIRECTIVE
        );
    }

    #[test]
    fn earlier_prompts_are_restated_in_order_before_the_directive() {
        let text = objective_reminder_text(
            &queries(&[
                "Do not modify anything under tests/.",
                "Rename load() in utils.py.",
            ]),
            &cfg(None, None),
        );
        assert_eq!(
            text,
            format!(
                "{PRIOR_REQUESTS_HEADER}\n1. Do not modify anything under tests/.\n2. Rename load() in utils.py.\n\n{CURRENT_REQUEST_DIRECTIVE}"
            )
        );
    }

    #[test]
    fn count_cap_keeps_the_most_recent_and_numbers_them_by_position() {
        let text = objective_reminder_text(&queries(&["a", "b", "c", "d"]), &cfg(Some(2), None));
        assert!(
            text.contains("(2 older requests omitted.)\n3. c\n4. d\n\n"),
            "{text}"
        );
        assert!(!text.contains("1. a") && !text.contains("2. b"), "{text}");
    }

    #[test]
    fn char_cap_drops_the_oldest_first() {
        let long = "x".repeat(30);
        let text = objective_reminder_text(
            &queries(&[&long, "keep me", "and me"]),
            &cfg(None, Some(20)),
        );
        assert!(!text.contains(&long), "{text}");
        assert!(
            text.contains("(1 older requests omitted.)\n2. keep me\n3. and me"),
            "{text}"
        );
    }

    #[test]
    fn char_cap_smaller_than_the_latest_prompt_falls_back_to_the_directive() {
        let text = objective_reminder_text(&queries(&["a very long request"]), &cfg(None, Some(3)));
        assert_eq!(text, CURRENT_REQUEST_DIRECTIVE);
    }
}
