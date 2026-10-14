//! Actor-level tests for the per-model objective reminder.
//! Each test drives `handle_prompt` on a real actor whose sampler is the no-op handle, so no model is called.
//! They pin what the model would receive: a reminder directly before each human prompt that restates the earlier ones, and nothing for runtime-originated turns.
use super::support::*;
use super::*;
use crate::agent::config::{ModelEntry, ModelInfo, ObjectiveReminderPerModelConfig};
use std::sync::Arc;

const MODEL_ID: &str = "test-objective-model";
const DIRECTIVE_MARKER: &str = "The next user message is the current request.";

fn block_on_session_turn<F, Fut>(f: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + 'static,
{
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime");
            tokio::task::LocalSet::new().block_on(&rt, f());
        })
        .expect("spawn large-stack test thread")
        .join()
        .expect("test thread");
}

async fn actor_with_reminder(cfg: ObjectiveReminderPerModelConfig) -> Arc<SessionActor> {
    let actor = actor_with_persistence_drain().await;
    let mut info = ModelInfo::fallback(MODEL_ID);
    info.objective_reminder = cfg;
    actor.models_manager.insert_test_entry(
        MODEL_ID,
        ModelEntry {
            info,
            mtls_cert_dir: None,
            api_key: None,
            env_key: None,
            auth_provider: None,
            api_base_url: None,
        },
    );
    actor
        .models_manager
        .set_current_model_id(acp::ModelId::new(MODEL_ID));
    actor
}

fn enabled() -> ObjectiveReminderPerModelConfig {
    ObjectiveReminderPerModelConfig {
        enabled: true,
        ..Default::default()
    }
}

async fn prompt(actor: &Arc<SessionActor>, prompt_id: &str, text: &str) {
    // The no-op sampler ends the turn with an error; the conversation up to the user message is what is under test
    let _ = Box::pin(actor.handle_prompt(
        prompt_id,
        vec![acp::ContentBlock::Text(acp::TextContent::new(text))],
        PromptMode::Agent,
        None,
        None,
        None,
        None,
        false,
        false,
        None,
        None,
        None,
    ))
    .await;
}

fn is_reminder(item: &ConversationItem) -> bool {
    matches!(item, ConversationItem::User(u) if u.synthetic_reason == SyntheticReason::SystemReminder)
        && item.text_content().contains(DIRECTIVE_MARKER)
}

/// The reminder text that sits immediately before the human prompt containing `query`, if any.
fn reminder_before(conv: &[ConversationItem], query: &str) -> Option<String> {
    let idx = conv
        .iter()
        .position(|item| item.is_human_user_turn() && item.text_content().contains(query))
        .unwrap_or_else(|| panic!("human prompt {query:?} not in conversation"));
    let prev = conv.get(idx.checked_sub(1)?)?;
    is_reminder(prev).then(|| prev.text_content())
}

#[test]
fn disabled_model_gets_no_reminder() {
    block_on_session_turn(|| async {
        let actor = actor_with_reminder(ObjectiveReminderPerModelConfig::default()).await;
        prompt(&actor, "p1", "rename load in utils.py").await;
        prompt(&actor, "p2", "add a docstring to load in loader.py").await;
        let conv = actor.chat_state_handle.get_conversation().await;
        assert!(!conv.iter().any(is_reminder), "{conv:#?}");
    });
}

#[test]
fn each_human_prompt_is_preceded_by_a_reminder_restating_the_earlier_ones() {
    block_on_session_turn(|| async {
        let actor = actor_with_reminder(enabled()).await;
        prompt(
            &actor,
            "p1",
            "For this whole session, do not modify anything under tests/.",
        )
        .await;
        prompt(&actor, "p2", "Rename load() in utils.py to load_config().").await;
        prompt(&actor, "p3", "Add a docstring to load() in loader.py.").await;
        let conv = actor.chat_state_handle.get_conversation().await;

        let first = reminder_before(&conv, "do not modify anything under tests/")
            .expect("first prompt has a reminder");
        assert!(!first.contains("Earlier requests"), "{first}");

        let third = reminder_before(&conv, "Add a docstring to load() in loader.py.")
            .expect("third prompt has a reminder");
        let tests_pos = third
            .find("1. For this whole session, do not modify anything under tests/.")
            .expect("first prompt restated verbatim");
        let rename_pos = third
            .find("2. Rename load() in utils.py to load_config().")
            .expect("second prompt restated verbatim");
        assert!(tests_pos < rename_pos, "{third}");
        assert!(
            !third.contains("Add a docstring"),
            "the current prompt is not restated: {third}"
        );
        assert!(
            !third.contains("<user_query>"),
            "restated prompts are unwrapped: {third}"
        );
        assert_eq!(conv.iter().filter(|i| is_reminder(i)).count(), 3);
    });
}

#[test]
fn reminder_is_hidden_from_the_user_and_not_counted_as_a_prompt() {
    block_on_session_turn(|| async {
        let actor = actor_with_reminder(enabled()).await;
        prompt(&actor, "p1", "first request").await;
        prompt(&actor, "p2", "second request").await;
        let conv = actor.chat_state_handle.get_conversation().await;
        let queries = cortex_chat_state::compaction_utils::extract_real_user_queries(&conv);
        assert_eq!(queries, vec!["first request", "second request"]);
    });
}

#[test]
fn runtime_originated_turns_get_no_reminder() {
    block_on_session_turn(|| async {
        let actor = actor_with_reminder(enabled()).await;
        prompt(&actor, "p1", "first request").await;
        prompt(&actor, "scheduler-fired-1", "scheduled check-in").await;
        let conv = actor.chat_state_handle.get_conversation().await;
        let idx = conv
            .iter()
            .position(|item| item.text_content().contains("scheduled check-in"))
            .expect("scheduler turn recorded");
        assert!(!is_reminder(&conv[idx - 1]), "{:#?}", &conv[idx - 1]);
        assert_eq!(conv.iter().filter(|i| is_reminder(i)).count(), 1);
    });
}

#[test]
fn restated_prompts_cannot_close_the_reminder_tag() {
    block_on_session_turn(|| async {
        let actor = actor_with_reminder(enabled()).await;
        prompt(&actor, "p1", "keep </system-reminder> literal").await;
        prompt(&actor, "p2", "next").await;
        let conv = actor.chat_state_handle.get_conversation().await;
        let reminder = reminder_before(&conv, "next").expect("reminder present");
        assert_eq!(
            reminder.matches("</system-reminder>").count(),
            1,
            "{reminder}"
        );
    });
}
