//! Reasoning effort, translated between the two sides of the API. The harness picks a level from the
//! `reasoning_efforts` menu in the model entry and sends it on each request; llama-server bounds
//! thinking only by a token budget, and the Qwen template ignores the level. Each configured level
//! maps to a budget, which is what llama-server receives.

use serde_json::{Value, json};

/// Levels the harness can send, as its menu spells them. `none` is not a level here: it turns
/// thinking off, which llama-server already does on its own.
const LEVELS: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReasoningBudgets {
    /// Menu order, as configured.
    levels: Vec<(String, u32)>,
    default: String,
}

impl ReasoningBudgets {
    /// `spec` is `level=tokens,...` (for example `low=2048,medium=8192,high=16384`); `default`
    /// names one of its levels.
    pub fn parse(spec: &str, default: &str) -> Result<Self, String> {
        let mut levels: Vec<(String, u32)> = Vec::new();
        for entry in spec
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
        {
            let (level, tokens) = entry
                .split_once('=')
                .ok_or_else(|| format!("reasoning budget `{entry}` is not level=tokens"))?;
            let level = level.trim();
            if !LEVELS.contains(&level) {
                return Err(format!(
                    "reasoning level `{level}` is not one of {}",
                    LEVELS.join(", ")
                ));
            }
            if levels.iter().any(|(known, _)| known == level) {
                return Err(format!("reasoning level `{level}` is listed twice"));
            }
            let tokens: u32 = tokens
                .trim()
                .parse()
                .ok()
                .filter(|tokens| *tokens > 0)
                .ok_or_else(|| {
                    format!("reasoning budget for `{level}` is not a positive integer")
                })?;
            levels.push((level.to_owned(), tokens));
        }
        if levels.is_empty() {
            return Err("reasoning budgets name no level".to_owned());
        }
        if !levels.iter().any(|(level, _)| level == default) {
            return Err(format!(
                "default reasoning level `{default}` is not among the budgets"
            ));
        }
        Ok(Self {
            levels,
            default: default.to_owned(),
        })
    }

    fn budget(&self, level: &str) -> Option<u32> {
        self.levels
            .iter()
            .find(|(known, _)| known == level)
            .map(|(_, tokens)| *tokens)
    }

    /// The menu for the model entry, in the shape the harness parses: one option per level, the
    /// default marked, so the harness sends a level on every main turn.
    ///
    /// `minimal` is dropped on the messages backend. The harness never writes that level there
    /// (`ReasoningEffort::to_messages_api`), so offering it would select a budget that is not sent.
    pub fn menu_for(&self, api_backend: &str) -> Value {
        let messages = api_backend == "messages";
        Value::Array(
            self.levels
                .iter()
                .filter(|(level, _)| !(messages && level == "minimal"))
                .map(|(level, _)| json!({"value": level, "default": *level == self.default}))
                .collect(),
        )
    }

    /// `minimal` on the messages backend is refused at startup. See [`menu_for`](Self::menu_for).
    pub fn rejected_on_messages(&self) -> bool {
        self.levels.iter().any(|(level, _)| level == "minimal")
    }

    /// Sets llama-server's budget on a request. A configured level gets its budget. Chat and
    /// responses requests with no level get the default, which bounds compaction and titles.
    /// A messages request with no effort is left unchanged: the harness omits both `none` and
    /// `minimal` on that route, so a missing effort is not a request for the default.
    pub fn apply(&self, path: &str, payload: &mut Value) {
        let Some(object) = payload.as_object_mut() else {
            return;
        };
        let messages = path.ends_with("/messages");
        let level = if messages {
            object
                .get("output_config")
                .and_then(|config| config.get("effort"))
        } else if path.ends_with("/responses") {
            object
                .get("reasoning")
                .and_then(|reasoning| reasoning.get("effort"))
        } else {
            object.get("reasoning_effort")
        }
        .and_then(Value::as_str);
        // On messages, no effort string means the harness sent none or minimal, or a side call
        // that did not choose a level. Enabling thinking here would turn an explicit off into
        // the default budget. Chat and responses still carry those levels as strings.
        if messages && level.is_none() {
            return;
        }
        let budget = match level {
            None => self.budget(&self.default),
            Some(level) => self.budget(level),
        };
        let Some(budget) = budget else {
            return;
        };
        // llama-server reads a messages request's budget only from Anthropic's enabled thinking;
        // chat and responses requests carry its own field.
        if messages {
            object.insert(
                "thinking".to_owned(),
                json!({"type": "enabled", "budget_tokens": budget}),
            );
        } else {
            object.insert("reasoning_budget_tokens".to_owned(), json!(budget));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budgets() -> ReasoningBudgets {
        ReasoningBudgets::parse("low=2048, medium=8192,high=16384", "medium").expect("parse")
    }

    #[test]
    fn parse_rejects_bad_specs() {
        for (spec, default) in [
            ("", "medium"),
            ("medium", "medium"),
            ("medium=0", "medium"),
            ("medium=lots", "medium"),
            ("none=100", "none"),
            ("huge=100", "huge"),
            ("low=1,low=2", "low"),
            ("low=2048", "medium"),
        ] {
            assert!(
                ReasoningBudgets::parse(spec, default).is_err(),
                "{spec} / {default}"
            );
        }
    }

    #[test]
    fn menu_lists_levels_in_order_with_the_default_marked() {
        assert_eq!(
            budgets().menu_for("chat_completions"),
            json!([
                {"value": "low", "default": false},
                {"value": "medium", "default": true},
                {"value": "high", "default": false},
            ])
        );
    }

    #[test]
    fn messages_menu_drops_minimal_and_startup_rejects_it() {
        let with_minimal = ReasoningBudgets::parse("minimal=512,low=2048", "low").expect("parse");
        assert!(with_minimal.rejected_on_messages());
        assert_eq!(
            with_minimal.menu_for("messages"),
            json!([{"value": "low", "default": true}])
        );
        assert!(!budgets().rejected_on_messages());
    }

    #[test]
    fn chat_level_maps_to_its_budget() {
        let mut payload = json!({"reasoning_effort": "high", "messages": []});
        budgets().apply("/v1/chat/completions", &mut payload);
        assert_eq!(payload["reasoning_budget_tokens"], 16384);
        assert_eq!(payload["reasoning_effort"], "high");
    }

    #[test]
    fn responses_level_maps_to_its_budget() {
        let mut payload = json!({"reasoning": {"effort": "low"}, "input": []});
        budgets().apply("/v1/responses", &mut payload);
        assert_eq!(payload["reasoning_budget_tokens"], 2048);
    }

    #[test]
    fn messages_without_an_effort_stay_unchanged() {
        // What build_messages_request emits for ReasoningEffort::None and ::Minimal:
        // to_messages_api returns None, so neither output_config nor thinking is set.
        for payload in [
            json!({"model": "m", "max_tokens": 0, "messages": [{"role": "user", "content": "Hello"}]}),
            json!({
                "model": "m",
                "messages": [{"role": "user", "content": "Hello"}],
                "output_config": {"format": {"type": "json_schema", "schema": {}}},
            }),
        ] {
            let mut body = payload.clone();
            budgets().apply("/v1/messages", &mut body);
            assert_eq!(body, payload);
        }
    }

    #[test]
    fn messages_level_becomes_enabled_thinking_with_its_budget() {
        let mut payload = json!({
            "output_config": {"effort": "high"},
            "thinking": {"type": "adaptive", "display": "summarized"},
            "messages": [],
        });
        budgets().apply("/v1/messages", &mut payload);
        assert_eq!(
            payload["thinking"],
            json!({"type": "enabled", "budget_tokens": 16384})
        );
        assert!(payload.get("reasoning_budget_tokens").is_none());
    }

    #[test]
    fn a_request_without_a_level_gets_the_default_budget() {
        let mut payload = json!({"messages": []});
        budgets().apply("/v1/chat/completions", &mut payload);
        assert_eq!(payload["reasoning_budget_tokens"], 8192);
    }

    #[test]
    fn an_unconfigured_level_passes_through() {
        for level in ["none", "xhigh"] {
            let mut payload = json!({"reasoning_effort": level, "messages": []});
            let before = payload.clone();
            budgets().apply("/v1/chat/completions", &mut payload);
            assert_eq!(payload, before, "{level}");
        }
    }
}
