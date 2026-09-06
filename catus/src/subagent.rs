//! Subagent runtime management.
//!
//! A subagent is a separate LLM conversation launched by the main agent (via
//! the `task` or `taskSync` tools). It has its own messages, toolbox, and shell
//! state, and reports back through the `completeTask` tool.

use std::collections::HashMap;
use std::sync::Arc;

use sutcac_sh::exec::ShellState;
use tokio::sync::{mpsc, oneshot};

use crate::agents::{AgentDefinition, AgentRegistry};
use crate::config::AppConfig;
use crate::llm::{LlmClient, LlmError, Model, StreamEvent};
use crate::mcp::McpManager;
use crate::message::Message;
use crate::skills::SkillRegistry;
use crate::tool::{CompleteTaskTool, ToolContext, Toolbox};

pub type SubagentId = String;

/// Runtime state of a subagent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentState {
    Idle,
    Streaming,
    RunningTool,
    Completed,
    Error,
}

impl SubagentState {
    pub fn as_str(&self) -> &'static str {
        match self {
            SubagentState::Idle => "idle",
            SubagentState::Streaming => "streaming",
            SubagentState::RunningTool => "running tool",
            SubagentState::Completed => "completed",
            SubagentState::Error => "error",
        }
    }
}

/// How a subagent should be initialized relative to its parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentContextMode {
    /// Inherit a snapshot of the parent's context.
    Fork,
    /// Start with an independent context defined by the subagent.
    Create,
}

impl std::str::FromStr for SubagentContextMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "fork" => Ok(SubagentContextMode::Fork),
            "create" => Ok(SubagentContextMode::Create),
            _ => Err(format!(
                "unknown context mode '{}'; expected 'fork' or 'create'",
                s
            )),
        }
    }
}

/// Snapshot of parent state used to spawn a subagent.
pub struct ParentSnapshot {
    pub messages: Vec<Message>,
    pub shell_state: ShellState,
    pub toolbox: Toolbox,
    pub active_skills: Vec<String>,
    pub models: Vec<Model>,
    pub current_model: Model,
    pub skill_registry: SkillRegistry,
    pub config: AppConfig,
    pub mcp_manager: Option<Arc<McpManager>>,
}

/// Event emitted by a running subagent back to the application.
#[derive(Debug, Clone)]
pub enum SubagentEvent {
    Started {
        id: SubagentId,
    },
    StateChanged {
        id: SubagentId,
        state: SubagentState,
    },
    Message {
        id: SubagentId,
        message: Message,
    },
    Completed {
        id: SubagentId,
        result: String,
    },
    Error {
        id: SubagentId,
        error: String,
    },
}

/// A running subagent instance.
#[derive(Debug, Clone)]
pub struct Subagent {
    pub id: SubagentId,
    pub name: String,
    pub task: String,
    pub state: SubagentState,
    pub mode: SubagentContextMode,
    pub messages: Vec<Message>,
    pub result: Option<String>,
    pub error: Option<String>,
    pub parent_call_id: Option<String>,
}

impl Subagent {
    fn new(
        id: SubagentId,
        definition: &AgentDefinition,
        task: String,
        mode: SubagentContextMode,
        parent_call_id: Option<String>,
    ) -> Self {
        Self {
            id,
            name: definition.name.clone(),
            task,
            state: SubagentState::Idle,
            mode,
            messages: Vec::new(),
            result: None,
            error: None,
            parent_call_id,
        }
    }
}

/// Manages all running subagents.
pub struct SubagentManager {
    subagents: Vec<Subagent>,
    event_tx: mpsc::Sender<SubagentEvent>,
    pub event_rx: mpsc::Receiver<SubagentEvent>,
    next_id: usize,
    parent_toolbox: Toolbox,
    parent_models: Vec<Model>,
    parent_current_model: Model,
    parent_config: AppConfig,
    parent_mcp_manager: Option<Arc<McpManager>>,
    /// Channels waiting for a synchronous subagent result.
    pending_sync: HashMap<SubagentId, oneshot::Sender<Result<String, String>>>,
}

impl SubagentManager {
    pub fn new(
        parent_toolbox: Toolbox,
        parent_models: Vec<Model>,
        parent_current_model: Model,
        parent_config: AppConfig,
        parent_mcp_manager: Option<Arc<McpManager>>,
    ) -> Self {
        let (event_tx, event_rx) = mpsc::channel(128);
        Self {
            subagents: Vec::new(),
            event_tx,
            event_rx,
            next_id: 0,
            parent_toolbox,
            parent_models,
            parent_current_model,
            parent_config,
            parent_mcp_manager,
            pending_sync: HashMap::new(),
        }
    }

    /// Spawn a subagent asynchronously.
    ///
    /// Returns the subagent id immediately; completion is reported via
    /// `SubagentEvent::Completed` on `event_rx`.
    pub fn spawn(
        &mut self,
        definition: &AgentDefinition,
        task: String,
        mode: SubagentContextMode,
        parent_messages: Vec<Message>,
        parent_shell_state: ShellState,
        parent_active_skills: Vec<String>,
        parent_skill_registry: SkillRegistry,
        parent_call_id: Option<String>,
    ) -> SubagentId {
        let id = format!("subagent-{}-{}", self.next_id, definition.name);
        self.next_id += 1;

        let mut subagent =
            Subagent::new(id.clone(), definition, task.clone(), mode, parent_call_id);
        let parent = ParentSnapshot {
            messages: parent_messages,
            shell_state: parent_shell_state,
            toolbox: self.parent_toolbox.clone(),
            active_skills: parent_active_skills,
            models: self.parent_models.clone(),
            current_model: self.parent_current_model.clone(),
            skill_registry: parent_skill_registry,
            config: self.parent_config.clone(),
            mcp_manager: self.parent_mcp_manager.clone(),
        };
        let runner = SubagentRunner::new(
            id.clone(),
            definition.clone(),
            task,
            mode,
            parent,
            self.event_tx.clone(),
        );

        subagent.state = SubagentState::Streaming;
        self.subagents.push(subagent);

        tokio::spawn(async move {
            runner.run().await;
        });

        id
    }

    /// Spawn a subagent and return a future that resolves when it completes.
    ///
    /// Used by the synchronous `taskSync` tool.
    pub fn spawn_sync(
        &mut self,
        definition: &AgentDefinition,
        task: String,
        mode: SubagentContextMode,
        parent_messages: Vec<Message>,
        parent_shell_state: ShellState,
        parent_active_skills: Vec<String>,
        parent_skill_registry: SkillRegistry,
        parent_call_id: Option<String>,
    ) -> (SubagentId, oneshot::Receiver<Result<String, String>>) {
        let (tx, rx) = oneshot::channel();
        let id = self.spawn(
            definition,
            task,
            mode,
            parent_messages,
            parent_shell_state,
            parent_active_skills,
            parent_skill_registry,
            parent_call_id,
        );
        self.pending_sync.insert(id.clone(), tx);
        (id, rx)
    }

    /// Return a reference to a subagent by id.
    pub fn get(&self, id: &str) -> Option<&Subagent> {
        self.subagents.iter().find(|s| s.id == id)
    }

    /// Return a mutable reference to a subagent by id.
    pub fn get_mut(&mut self, id: &str) -> Option<&mut Subagent> {
        self.subagents.iter_mut().find(|s| s.id == id)
    }

    /// Remove a completed or errored subagent.
    pub fn remove(&mut self, id: &str) -> bool {
        let len = self.subagents.len();
        self.subagents.retain(|s| s.id != id);
        self.subagents.len() < len
    }

    /// List all subagents.
    pub fn list(&self) -> &[Subagent] {
        &self.subagents
    }

    /// Number of subagents still running.
    pub fn running_count(&self) -> usize {
        self.subagents
            .iter()
            .filter(|s| {
                matches!(
                    s.state,
                    SubagentState::Idle | SubagentState::Streaming | SubagentState::RunningTool
                )
            })
            .count()
    }

    /// Update the parent toolbox snapshot used for future subagents.
    pub fn update_parent_toolbox(
        &mut self,
        toolbox: Toolbox,
        mcp_manager: Option<Arc<McpManager>>,
    ) {
        self.parent_toolbox = toolbox;
        self.parent_mcp_manager = mcp_manager;
    }

    /// Apply a subagent event to the managed state.
    ///
    /// Returns `Some(result)` if a synchronous caller was waiting for this
    /// subagent's completion.
    pub fn handle_event(&mut self, event: &SubagentEvent) -> Option<Result<String, String>> {
        match event {
            SubagentEvent::Started { id } => {
                if let Some(s) = self.get_mut(id) {
                    s.state = SubagentState::Streaming;
                }
            }
            SubagentEvent::StateChanged { id, state } => {
                if let Some(s) = self.get_mut(id) {
                    s.state = *state;
                }
            }
            SubagentEvent::Message { id, message } => {
                if let Some(s) = self.get_mut(id) {
                    s.messages.push(message.clone());
                }
            }
            SubagentEvent::Completed { id, result } => {
                if let Some(s) = self.get_mut(id) {
                    s.state = SubagentState::Completed;
                    s.result = Some(result.clone());
                }
                if let Some(tx) = self.pending_sync.remove(id) {
                    let _ = tx.send(Ok(result.clone()));
                }
            }
            SubagentEvent::Error { id, error } => {
                if let Some(s) = self.get_mut(id) {
                    s.state = SubagentState::Error;
                    s.error = Some(error.clone());
                }
                if let Some(tx) = self.pending_sync.remove(id) {
                    let _ = tx.send(Err(error.clone()));
                }
            }
        }
        None
    }
}

/// Internal runner that executes a subagent turn loop in a background task.
struct SubagentRunner {
    id: SubagentId,
    definition: AgentDefinition,
    messages: Vec<Message>,
    toolbox: Toolbox,
    shell_state: ShellState,
    active_skills: Vec<String>,
    skill_registry: SkillRegistry,
    client: LlmClient,
    max_rounds: usize,
    event_tx: mpsc::Sender<SubagentEvent>,
}

fn build_subagent_toolbox(parent_toolbox: &Toolbox, definition: &AgentDefinition) -> Toolbox {
    let mut allowed: Vec<String> = if definition.allowed_tools.is_empty() {
        parent_toolbox
            .definitions()
            .into_iter()
            .map(|d| d.function.name)
            .collect()
    } else {
        let mut names = definition.explicit_tools();
        if definition.inherits_tools() {
            names.extend(
                parent_toolbox
                    .definitions()
                    .into_iter()
                    .map(|d| d.function.name),
            );
        }
        names
    };

    // Subagents never get task-dispatch tools.
    allowed.retain(|n| n != "task" && n != "taskSync");
    // Subagents always get completeTask.
    if !allowed.iter().any(|n| n == "completeTask") {
        allowed.push("completeTask".to_string());
    }

    let mut toolbox = parent_toolbox.filter(&allowed);
    toolbox.register(Arc::new(CompleteTaskTool));
    toolbox
}

impl SubagentRunner {
    fn new(
        id: SubagentId,
        definition: AgentDefinition,
        task: String,
        mode: SubagentContextMode,
        parent: ParentSnapshot,
        event_tx: mpsc::Sender<SubagentEvent>,
    ) -> Self {
        let (messages, shell_state, active_skills, toolbox, skill_registry, client) =
            Self::initialize_context(&id, &definition, &task, mode, parent);

        Self {
            id,
            definition,
            messages,
            toolbox,
            shell_state,
            active_skills,
            skill_registry,
            client,
            max_rounds: 30,
            event_tx,
        }
    }

    fn initialize_context(
        id: &SubagentId,
        definition: &AgentDefinition,
        task: &str,
        mode: SubagentContextMode,
        parent: ParentSnapshot,
    ) -> (
        Vec<Message>,
        ShellState,
        Vec<String>,
        Toolbox,
        SkillRegistry,
        LlmClient,
    ) {
        let model = Self::resolve_model(&parent, definition.model_tier.as_deref());
        let client = LlmClient::new(
            model.provider.clone(),
            model.clone(),
            &format!("{}-{}", id, chrono::Utc::now().timestamp_millis()),
        );

        match mode {
            SubagentContextMode::Fork => {
                let mut messages = parent.messages.clone();
                messages.push(Message::user(format!(
                    "You are now acting as the '{}' subagent. Task: {}",
                    definition.name, task
                )));

                let toolbox = build_subagent_toolbox(&parent.toolbox, definition);

                let shell_state = parent.shell_state.clone();
                let active_skills = if definition.inherits_skills() {
                    parent.active_skills.clone()
                } else {
                    definition.explicit_skills()
                };
                let skill_registry = parent.skill_registry.clone();

                (
                    messages,
                    shell_state,
                    active_skills,
                    toolbox,
                    skill_registry,
                    client,
                )
            }
            SubagentContextMode::Create => {
                let system = Message::system(definition.body.clone());
                let task_msg = Message::user(format!(
                    "You are the '{}' subagent. Task: {}",
                    definition.name, task
                ));
                let messages = vec![system, task_msg];

                let shell_config = {
                    let mut shell = parent.config.shell.clone().unwrap_or_default();
                    if let Some(perm) = &definition.permission {
                        shell.perm_mode = Some(perm.clone());
                    }
                    shell
                };
                let shell_state = ShellState::with_policy_and_logger(
                    shell_config.permission_policy(),
                    shell_config.audit_logger(),
                );

                let toolbox = build_subagent_toolbox(&parent.toolbox, definition);

                let active_skills = definition.explicit_skills();
                let skill_registry = SkillRegistry::new();

                (
                    messages,
                    shell_state,
                    active_skills,
                    toolbox,
                    skill_registry,
                    client,
                )
            }
        }
    }

    fn resolve_model(parent: &ParentSnapshot, tier: Option<&str>) -> Model {
        if let Some(tier) = tier {
            if let Some(model) = parent
                .models
                .iter()
                .find(|m| m.tier.as_deref() == Some(tier))
                .cloned()
            {
                return model;
            }
            log::warn!(
                "no model with tier '{}' configured for subagent; falling back to parent model",
                tier
            );
        }
        parent.current_model.clone()
    }

    async fn run(mut self) {
        let _ = self
            .event_tx
            .send(SubagentEvent::Started {
                id: self.id.clone(),
            })
            .await;

        for _round in 0..self.max_rounds {
            let (event_tx, mut event_rx) = mpsc::channel::<StreamEvent>(128);
            let (done_tx, mut done_rx) = mpsc::channel::<Result<(), LlmError>>(1);

            let messages = self.messages.clone();
            let tools = self.toolbox.definitions();
            let client = self.client.clone();

            tokio::spawn(async move {
                let result = client.stream_chat(&messages, &tools, event_tx).await;
                let _ = done_tx.send(result).await;
            });

            self.send_state(SubagentState::Streaming);

            let mut assistant = Message::assistant(String::new());
            let mut pending_tool_calls = Vec::new();
            let mut stream_error: Option<LlmError> = None;

            loop {
                tokio::select! {
                    Some(event) = event_rx.recv() => {
                        match event {
                            StreamEvent::Text(text) => assistant.content.push_str(&text),
                            StreamEvent::Reasoning(text) => assistant.reasoning_content.push_str(&text),
                            StreamEvent::ToolCall(call) => {
                                assistant.had_tool_calls = true;
                                assistant.tool_calls.push(call.clone());
                                pending_tool_calls.push(call);
                            }
                            StreamEvent::Usage(_) => {}
                        }
                    }
                    Some(result) = done_rx.recv() => {
                        while let Ok(event) = event_rx.try_recv() {
                            match event {
                                StreamEvent::Text(text) => assistant.content.push_str(&text),
                                StreamEvent::Reasoning(text) => assistant.reasoning_content.push_str(&text),
                                StreamEvent::ToolCall(call) => {
                                    assistant.had_tool_calls = true;
                                    assistant.tool_calls.push(call.clone());
                                    pending_tool_calls.push(call);
                                }
                                StreamEvent::Usage(_) => {}
                            }
                        }
                        match result {
                            Ok(()) => break,
                            Err(e) => stream_error = Some(e),
                        }
                    }
                }
            }

            if let Some(e) = stream_error {
                self.send_error(e.to_string());
                return;
            }

            self.messages.push(assistant.clone());
            self.send_message(assistant);

            if pending_tool_calls.is_empty() {
                let result = self
                    .messages
                    .last()
                    .map(|m| m.content.clone())
                    .unwrap_or_default();
                self.complete(result);
                return;
            }

            for call in pending_tool_calls {
                if call.name == "completeTask" {
                    let result = parse_complete_task_result(&call.arguments);
                    self.complete(result);
                    return;
                }

                self.send_state(SubagentState::RunningTool);

                let result = match self.toolbox.get(&call.name) {
                    Some(tool) => {
                        let mut dummy_agent_registry = AgentRegistry::new();
                        let mut dummy_subagents = SubagentManager::new(
                            Toolbox::default(),
                            Vec::new(),
                            Model::default(),
                            AppConfig::default(),
                            None,
                        );
                        let mut ctx = ToolContext {
                            shell_state: &mut self.shell_state,
                            skill_registry: &mut self.skill_registry,
                            active_skills: &mut self.active_skills,
                            messages: &mut self.messages,
                            toolbox: &self.toolbox,
                            agent_registry: Some(&mut dummy_agent_registry),
                            subagents: Some(&mut dummy_subagents),
                            current_agent: Some(&self.definition),
                            is_main_agent: false,
                        };
                        tool.execute(&call, &mut ctx).await
                    }
                    None => crate::tool::ToolResult {
                        call: call.clone(),
                        status: 1,
                        stdout: String::new(),
                        stderr: format!("catus: unknown tool '{}'", call.name),
                        interaction: None,
                    },
                };

                if result.interaction.is_some() {
                    // ask_user is not supported inside subagents.
                    let result = crate::tool::ToolResult {
                        call: call.clone(),
                        status: 1,
                        stdout: String::new(),
                        stderr: "catus: interactive questions are not supported inside subagents"
                            .to_string(),
                        interaction: None,
                    };
                    let msg = result.to_message();
                    self.messages.push(Message::tool(msg, call.id.clone()));
                    self.send_message(self.messages.last().unwrap().clone());
                } else {
                    let msg = result.to_message();
                    self.messages.push(Message::tool(msg, call.id.clone()));
                    self.send_message(self.messages.last().unwrap().clone());
                }
            }
        }

        // Reached max rounds without completing.
        let result = self
            .messages
            .last()
            .filter(|m| m.is_assistant())
            .map(|m| m.content.clone())
            .unwrap_or_else(|| "(subagent reached max tool rounds without a result)".to_string());
        self.complete(result);
    }

    fn send_state(&self, state: SubagentState) {
        let _ = self.event_tx.try_send(SubagentEvent::StateChanged {
            id: self.id.clone(),
            state,
        });
    }

    fn send_message(&self, message: Message) {
        let _ = self.event_tx.try_send(SubagentEvent::Message {
            id: self.id.clone(),
            message,
        });
    }

    fn complete(&self, result: String) {
        let _ = self.event_tx.try_send(SubagentEvent::Completed {
            id: self.id.clone(),
            result,
        });
    }

    fn send_error(&self, error: String) {
        let _ = self.event_tx.try_send(SubagentEvent::Error {
            id: self.id.clone(),
            error,
        });
    }
}

fn parse_complete_task_result(arguments: &str) -> String {
    #[derive(serde::Deserialize)]
    struct Args {
        result: String,
    }
    serde_json::from_str::<Args>(arguments)
        .map(|a| a.result)
        .unwrap_or_else(|_| arguments.to_string())
}
