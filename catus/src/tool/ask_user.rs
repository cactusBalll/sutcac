//! The built-in `ask_user` tool.
//!
//! [`AskUserTool`] lets the model ask the user clarifying questions while a
//! turn is in progress. The tool itself never blocks: it validates the
//! arguments and returns a [`ToolResult`] carrying an [`InteractionRequest`].
//! The application (`app.rs`) detects that request, opens an interactive
//! overlay (`Overlay::Ask`), and resumes the paused tool turn once the user
//! answers. This module also hosts the shared question/answer data types used
//! by the overlay.

use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};

use super::{Tool, ToolCall, ToolDefinition, ToolResult};

/// Maximum number of questions in a single call.
pub const MAX_QUESTIONS: usize = 5;
/// Maximum length of a question's short title, in characters.
pub const MAX_TITLE_CHARS: usize = 12;
/// Minimum number of options per question.
pub const MIN_OPTIONS: usize = 2;
/// Maximum number of options per question.
pub const MAX_OPTIONS: usize = 5;

/// The built-in `ask_user` tool.
pub struct AskUserTool;

/// A single question shown to the user in the ask overlay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskQuestion {
    /// Full question text.
    pub prompt: String,
    /// Short label shown in the overlay title (max 12 characters).
    pub title: String,
    /// Selectable options (2–5).
    pub options: Vec<AskOption>,
    /// Whether multiple options may be selected. Accepted as `multiSelect`
    /// or `multi_select`.
    #[serde(rename = "multiSelect", alias = "multi_select")]
    pub multi_select: bool,
}

/// One selectable option of an [`AskQuestion`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskOption {
    /// Display text of the option.
    pub label: String,
    /// Optional longer explanation shown next to the label.
    pub description: Option<String>,
}

/// The user's answer to one question, sent back to the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AskAnswer {
    /// The question that was answered (the full prompt text).
    pub prompt: String,
    /// The selected option label(s). Custom "Other" input is included here.
    pub answer: Answer,
}

/// An answer value: a single label, or a list of labels for multi-select.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum Answer {
    One(String),
    Many(Vec<String>),
}

/// A request from a tool to collect answers from the user before the tool
/// result is final. Carried by [`ToolResult::interaction`]; the application
/// opens an overlay and completes the result after the user responds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractionRequest {
    pub questions: Vec<AskQuestion>,
}

/// JSON Schema for the ask_user tool.
const ASK_USER_TOOL_SCHEMA: &str = r#"{
  "type": "function",
  "function": {
    "name": "ask_user",
    "description": "Ask the user clarifying questions via an interactive dialog. Use when you need user input on preferences, implementation choices, or ambiguous instructions. The user always sees an extra \"Other\" option where they can type custom text. Waits until the user answers (or cancels), then returns a JSON array of {\"prompt\": ..., \"answer\": ...} objects: answer is a string for single-select questions and an array of strings for multiSelect questions.",
    "parameters": {
      "type": "object",
      "properties": {
        "questions": {
          "type": "array",
          "minItems": 1,
          "maxItems": 5,
          "description": "Questions to ask, shown one at a time in order.",
          "items": {
            "type": "object",
            "properties": {
              "prompt": {
                "type": "string",
                "description": "Full question text."
              },
              "title": {
                "type": "string",
                "description": "Short label for the question, max 12 characters."
              },
              "options": {
                "type": "array",
                "minItems": 2,
                "maxItems": 5,
                "description": "Options the user can choose from.",
                "items": {
                  "type": "object",
                  "properties": {
                    "label": {
                      "type": "string",
                      "description": "Display text of the option. Recommended options should be listed first with a '(Recommended)' suffix."
                    },
                    "description": {
                      "type": "string",
                      "description": "Optional short explanation shown next to the label."
                    }
                  },
                  "required": ["label"]
                }
              },
              "multiSelect": {
                "type": "boolean",
                "description": "Whether the user may select multiple options."
              }
            },
            "required": ["prompt", "title", "options", "multiSelect"]
          }
        }
      },
      "required": ["questions"]
    }
  }
}"#;

impl Tool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }

    fn definition(&self) -> ToolDefinition {
        serde_json::from_str(ASK_USER_TOOL_SCHEMA).expect("ask_user tool schema is valid JSON")
    }

    fn describe_call(&self, call: &ToolCall) -> String {
        parse_questions(&call.arguments)
            .filter(|questions| !questions.is_empty())
            .map(|questions| {
                let titles: Vec<&str> = questions.iter().map(|q| q.title.as_str()).collect();
                format!("ask_user: {}", titles.join(" / "))
            })
            .unwrap_or_else(|| call.arguments.clone())
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        _ctx: &'a mut super::ToolContext<'_>,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let Some(questions) = parse_questions(&call.arguments) else {
                log::warn!("malformed tool call: {}", call.arguments);
                return ToolResult {
                    call: call.clone(),
                    status: 2,
                    stdout: String::new(),
                    stderr: format!(
                        "catus: tool call format error: arguments must be a JSON object {{\"questions\": [...]}} with 1-5 question objects (prompt, title, options, multiSelect); got: {}",
                        call.arguments
                    ),
                    interaction: None,
                };
            };
            if let Err(err) = validate_questions(&questions) {
                return ToolResult {
                    call: call.clone(),
                    status: 2,
                    stdout: String::new(),
                    stderr: format!("catus: invalid questions: {}", err),
                    interaction: None,
                };
            }

            // The turn pauses here: the application opens the ask overlay and
            // completes this call once the user answers.
            ToolResult {
                call: call.clone(),
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
                interaction: Some(InteractionRequest { questions }),
            }
        })
    }
}

/// Arguments of an `ask_user` tool call.
#[derive(Debug, Deserialize)]
struct AskArguments {
    questions: Vec<AskQuestion>,
}

/// Extract the question list from a tool call's `arguments` string.
fn parse_questions(arguments: &str) -> Option<Vec<AskQuestion>> {
    serde_json::from_str::<AskArguments>(arguments)
        .ok()
        .map(|args| args.questions)
}

/// Validate questions against the interface constraints shared with the
/// overlay: 1–5 questions, non-empty prompt, title ≤ 12 chars, 2–5 unique
/// non-empty options per question.
fn validate_questions(questions: &[AskQuestion]) -> Result<(), String> {
    if questions.is_empty() || questions.len() > MAX_QUESTIONS {
        return Err(format!(
            "expected 1-{} questions, got {}",
            MAX_QUESTIONS,
            questions.len()
        ));
    }
    for (i, question) in questions.iter().enumerate() {
        let where_ = format!("questions[{}]", i);
        if question.prompt.trim().is_empty() {
            return Err(format!("{}.prompt must not be empty", where_));
        }
        if question.title.trim().is_empty() {
            return Err(format!("{}.title must not be empty", where_));
        }
        if question.title.chars().count() > MAX_TITLE_CHARS {
            return Err(format!(
                "{}.title must be at most {} characters",
                where_, MAX_TITLE_CHARS
            ));
        }
        if question.options.len() < MIN_OPTIONS || question.options.len() > MAX_OPTIONS {
            return Err(format!(
                "{}.options must have {}-{} entries, got {}",
                where_,
                MIN_OPTIONS,
                MAX_OPTIONS,
                question.options.len()
            ));
        }
        let mut seen = std::collections::HashSet::new();
        for (j, option) in question.options.iter().enumerate() {
            let label = option.label.trim();
            if label.is_empty() {
                return Err(format!("{}.options[{}].label must not be empty", where_, j));
            }
            if !seen.insert(label.to_string()) {
                return Err(format!(
                    "{}.options[{}].label is duplicated: {}",
                    where_, j, label
                ));
            }
        }
    }
    Ok(())
}

/// Build the answer for one question from the overlay state.
///
/// `focus` is the highlighted row: `0..options.len()` for real options and
/// `options.len()` for the pseudo-option "Other". For multi-select the answer
/// is every checked option plus the trimmed "Other" text (if any); for
/// single-select it is the focused option, or the "Other" text when the focus
/// is on that row. Returns `None` when nothing has been answered yet.
pub fn collect_answer(
    question: &AskQuestion,
    selections: &[bool],
    focus: usize,
    other: &str,
) -> Option<AskAnswer> {
    let answer = if question.multi_select {
        let mut picked: Vec<String> = question
            .options
            .iter()
            .zip(selections.iter())
            .filter(|(_, selected)| **selected)
            .map(|(option, _)| option.label.clone())
            .collect();
        let other = other.trim();
        if !other.is_empty() {
            picked.push(other.to_string());
        }
        if picked.is_empty() {
            return None;
        }
        Answer::Many(picked)
    } else if focus < question.options.len() {
        Answer::One(question.options[focus].label.clone())
    } else {
        let other = other.trim();
        if other.is_empty() {
            return None;
        }
        Answer::One(other.to_string())
    };
    Some(AskAnswer {
        prompt: question.prompt.clone(),
        answer,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::SkillRegistry;
    use crate::tool::ToolContext;
    use crate::tool::Toolbox;
    use sutcac_sh::exec::ShellState;

    fn ask_call(arguments: &str) -> ToolCall {
        ToolCall {
            id: "call_ask".to_string(),
            name: "ask_user".to_string(),
            arguments: arguments.to_string(),
        }
    }

    fn test_context<'a>(
        shell_state: &'a mut ShellState,
        skill_registry: &'a mut SkillRegistry,
        active_skills: &'a mut Vec<String>,
        messages: &'a mut Vec<crate::message::Message>,
    ) -> ToolContext<'a> {
        let toolbox: &'a Toolbox = Box::leak(Box::new(Toolbox::default()));
        ToolContext {
            shell_state,
            skill_registry,
            active_skills,
            messages,
            toolbox,
            agent_registry: None,
            subagents: None,
            current_agent: None,
            is_main_agent: true,
        }
    }

    fn valid_arguments() -> &'static str {
        r#"{"questions":[{"prompt":"Which auth method?","title":"Auth","options":[{"label":"JWT","description":"Stateless"},{"label":"Cookies"}],"multiSelect":false}]}"#
    }

    #[tokio::test]
    async fn ask_user_tool_requests_interaction() {
        let mut ctx_fields = (
            ShellState::new(),
            SkillRegistry::new(),
            Vec::new(),
            Vec::new(),
        );
        let (ref mut state, ref mut registry, ref mut active, ref mut messages) = ctx_fields;
        let mut ctx = test_context(state, registry, active, messages);

        let tool = AskUserTool;
        let result = tool.execute(&ask_call(valid_arguments()), &mut ctx).await;
        assert_eq!(result.status, 0);
        let request = result.interaction.expect("interaction request");
        assert_eq!(request.questions.len(), 1);
        let question = &request.questions[0];
        assert_eq!(question.prompt, "Which auth method?");
        assert_eq!(question.title, "Auth");
        assert_eq!(question.options.len(), 2);
        assert_eq!(
            question.options[0].description.as_deref(),
            Some("Stateless")
        );
        assert!(!question.multi_select);
    }

    #[tokio::test]
    async fn ask_user_tool_accepts_snake_case_multi_select() {
        let mut ctx_fields = (
            ShellState::new(),
            SkillRegistry::new(),
            Vec::new(),
            Vec::new(),
        );
        let (ref mut state, ref mut registry, ref mut active, ref mut messages) = ctx_fields;
        let mut ctx = test_context(state, registry, active, messages);

        let arguments = valid_arguments().replace("multiSelect", "multi_select");
        let tool = AskUserTool;
        let result = tool.execute(&ask_call(&arguments), &mut ctx).await;
        assert_eq!(result.status, 0);
        assert!(result.interaction.is_some());
    }

    #[tokio::test]
    async fn ask_user_tool_rejects_malformed_arguments() {
        let mut ctx_fields = (
            ShellState::new(),
            SkillRegistry::new(),
            Vec::new(),
            Vec::new(),
        );
        let (ref mut state, ref mut registry, ref mut active, ref mut messages) = ctx_fields;
        let mut ctx = test_context(state, registry, active, messages);

        let tool = AskUserTool;
        let result = tool
            .execute(&ask_call(r#"{"question":[]}"#), &mut ctx)
            .await;
        assert_eq!(result.status, 2);
        assert!(result.stderr.contains("format error"));
        assert!(result.interaction.is_none());
    }

    #[tokio::test]
    async fn ask_user_tool_rejects_invalid_questions() {
        let mut ctx_fields = (
            ShellState::new(),
            SkillRegistry::new(),
            Vec::new(),
            Vec::new(),
        );
        let (ref mut state, ref mut registry, ref mut active, ref mut messages) = ctx_fields;
        let mut ctx = test_context(state, registry, active, messages);

        let cases = [
            (r#"{"questions":[]}"#, "expected 1-5"),
            (
                r#"{"questions":[{"prompt":"","title":"T","options":[{"label":"a"},{"label":"b"}],"multiSelect":false}]}"#,
                "prompt must not be empty",
            ),
            (
                r#"{"questions":[{"prompt":"p","title":"this title is way too long","options":[{"label":"a"},{"label":"b"}],"multiSelect":false}]}"#,
                "at most 12 characters",
            ),
            (
                r#"{"questions":[{"prompt":"p","title":"T","options":[{"label":"a"}],"multiSelect":false}]}"#,
                "2-5 entries",
            ),
            (
                r#"{"questions":[{"prompt":"p","title":"T","options":[{"label":"a"},{"label":"a"}],"multiSelect":false}]}"#,
                "duplicated",
            ),
        ];
        let tool = AskUserTool;
        for (arguments, expected) in cases {
            let result = tool.execute(&ask_call(arguments), &mut ctx).await;
            assert_eq!(result.status, 2, "arguments: {}", arguments);
            assert!(
                result.stderr.contains(expected),
                "arguments: {}\nstderr: {}",
                arguments,
                result.stderr
            );
            assert!(result.interaction.is_none());
        }
    }

    #[test]
    fn collect_answer_single_select_uses_focused_option() {
        let question: AskQuestion = serde_json::from_str(
            r#"{"prompt":"p","title":"T","options":[{"label":"a"},{"label":"b"}],"multiSelect":false}"#,
        )
        .unwrap();
        assert_eq!(
            collect_answer(&question, &[false, false], 1, ""),
            Some(AskAnswer {
                prompt: "p".to_string(),
                answer: Answer::One("b".to_string()),
            })
        );
        // Focus on "Other" without text is not an answer yet.
        assert_eq!(collect_answer(&question, &[false, false], 2, "  "), None);
        // Focus on "Other" with text answers with the custom text.
        assert_eq!(
            collect_answer(&question, &[false, false], 2, " custom "),
            Some(AskAnswer {
                prompt: "p".to_string(),
                answer: Answer::One("custom".to_string()),
            })
        );
    }

    #[test]
    fn collect_answer_multi_select_collects_checked_and_other() {
        let question: AskQuestion = serde_json::from_str(
            r#"{"prompt":"p","title":"T","options":[{"label":"a"},{"label":"b"},{"label":"c"}],"multiSelect":true}"#,
        )
        .unwrap();
        assert_eq!(
            collect_answer(&question, &[false, false, false], 0, ""),
            None
        );
        assert_eq!(
            collect_answer(&question, &[true, false, true], 0, "extra"),
            Some(AskAnswer {
                prompt: "p".to_string(),
                answer: Answer::Many(vec!["a".to_string(), "c".to_string(), "extra".to_string()]),
            })
        );
    }

    #[test]
    fn ask_answer_serializes_to_json() {
        let answers = vec![
            AskAnswer {
                prompt: "Which auth?".to_string(),
                answer: Answer::One("JWT".to_string()),
            },
            AskAnswer {
                prompt: "Features?".to_string(),
                answer: Answer::Many(vec!["a".to_string(), "b".to_string()]),
            },
        ];
        let json = serde_json::to_string(&answers).unwrap();
        assert_eq!(
            json,
            r#"[{"prompt":"Which auth?","answer":"JWT"},{"prompt":"Features?","answer":["a","b"]}]"#
        );
    }

    #[test]
    fn definition_parses_and_advertises_ask_user() {
        let tool = AskUserTool;
        let definition = tool.definition();
        assert_eq!(tool.name(), "ask_user");
        assert_eq!(definition.function.name, "ask_user");
        assert_eq!(definition.tool_type, "function");
        assert!(definition.function.description.contains("Other"));
    }

    #[test]
    fn describe_call_lists_question_titles() {
        let tool = AskUserTool;
        let call = ask_call(valid_arguments());
        assert_eq!(tool.describe_call(&call), "ask_user: Auth");
    }
}
