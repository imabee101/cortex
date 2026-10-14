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
    pub fn menu(&self) -> Value {
        Value::Array(
            self.levels
                .iter()
                .map(|(level, _)| json!({"value": level, "default": *level == self.default}))
                .collect(),
        )
    }

    /// Sets llama-server's budget on a request. A configured level gets its budget; a request
    /// without a level (compaction, titles and other side calls) gets the default level's; any
    /// other level, such as `none`, is left to llama-server unchanged.
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
            budgets().menu(),
            json!([
                {"value": "low", "default": false},
                {"value": "medium", "default": true},
                {"value": "high", "default": false},
            ])
        );
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
