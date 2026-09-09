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
use crate::config::{AppConfig, ModelTier, TierModels};
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

    /// Parse a persisted state string; unknown values fall back to `Idle`.
    pub fn from_db(s: &str) -> Self {
        match s {
            "streaming" => SubagentState::Streaming,
            "running tool" => SubagentState::RunningTool,
            "completed" => SubagentState::Completed,
            "error" => SubagentState::Error,
            _ => SubagentState::Idle,
        }
    }

    /// Whether the subagent was still doing work (and therefore needs its
    /// runner restarted after a resume).
    pub fn is_running(&self) -> bool {
        matches!(
            self,
            SubagentState::Idle | SubagentState::Streaming | SubagentState::RunningTool
        )
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
    pub tier_models: TierModels,
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
    parent_tier_models: TierModels,
    parent_current_model: Model,
    parent_config: AppConfig,
    parent_mcp_manager: Option<Arc<McpManager>>,
    /// Channels waiting for a synchronous subagent result.
    pending_sync: HashMap<SubagentId, oneshot::Sender<Result<String, String>>>,
}

impl SubagentManager {
    pub fn new(
        parent_toolbox: Toolbox,
        parent_tier_models: TierModels,
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
            parent_tier_models,
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
            tier_models: self.parent_tier_models.clone(),
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

    /// Snapshot every subagent for persistence.
    pub fn snapshot(&self) -> Vec<crate::history::SubagentSnapshot> {
        self.subagents
            .iter()
            .map(|s| crate::history::SubagentSnapshot {
                record: crate::history::SubagentRecord {
                    id: s.id.clone(),
                    name: s.name.clone(),
                    task: s.task.clone(),
                    state: s.state.as_str().to_string(),
                    mode: match s.mode {
                        SubagentContextMode::Fork => "fork",
                        SubagentContextMode::Create => "create",
                    }
                    .to_string(),
                    parent_call_id: s.parent_call_id.clone(),
                    result: s.result.clone(),
                    error: s.error.clone(),
                },
                messages: s.messages.clone(),
            })
            .collect()
    }

    /// Restore a subagent from a persisted snapshot.
    ///
    /// Terminal subagents (completed/error) are restored as-is. Subagents
    /// that were still running get their turn loop restarted from the saved
    /// messages, so work that was interrupted by an exit continues after a
    /// resume. Returns `Err` when the agent definition no longer exists.
    pub fn restore(
        &mut self,
        snapshot: &crate::history::SubagentSnapshot,
        registry: &AgentRegistry,
        parent_shell_state: &ShellState,
        parent_active_skills: &[String],
        parent_skill_registry: &SkillRegistry,
    ) -> Result<(), String> {
        let record = &snapshot.record;
        // Never reuse restored ids for future spawns.
        if let Some(n) = record
            .id
            .strip_prefix("subagent-")
            .and_then(|rest| rest.split('-').next())
            .and_then(|n| n.parse::<usize>().ok())
        {
            self.next_id = self.next_id.max(n + 1);
        }

        let mode = record
            .mode
            .parse::<SubagentContextMode>()
            .unwrap_or(SubagentContextMode::Create);
        let saved_state = SubagentState::from_db(&record.state);
        let restart = saved_state.is_running();

        let subagent = Subagent {
            id: record.id.clone(),
            name: record.name.clone(),
            task: record.task.clone(),
            state: if restart {
                SubagentState::Streaming
            } else {
                saved_state
            },
            mode,
            // Keep the persisted conversation so later snapshots (incremental
            // persists) don't lose the restored history.
            messages: snapshot.messages.clone(),
            result: record.result.clone(),
            error: record.error.clone(),
            parent_call_id: record.parent_call_id.clone(),
        };
        self.subagents.push(subagent);

        if !restart {
            return Ok(());
        }

        let definition = registry.get(&record.name).cloned().ok_or_else(|| {
            format!(
                "agent definition '{}' not found; cannot resume subagent {}",
                record.name, record.id
            )
        })?;

        let parent = ParentSnapshot {
            messages: Vec::new(),
            shell_state: parent_shell_state.clone(),
            toolbox: self.parent_toolbox.clone(),
            active_skills: parent_active_skills.to_vec(),
            tier_models: self.parent_tier_models.clone(),
            current_model: self.parent_current_model.clone(),
            skill_registry: parent_skill_registry.clone(),
            config: self.parent_config.clone(),
            mcp_manager: self.parent_mcp_manager.clone(),
        };
        let runner = SubagentRunner::resume(
            record.id.clone(),
            definition,
            snapshot.messages.clone(),
            mode,
            parent,
            self.event_tx.clone(),
        );
        tokio::spawn(async move {
            runner.run().await;
        });
        Ok(())
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

    /// Insert a subagent without a runner. Only for tests in other modules
    /// that need managed subagent state without spawning a task.
    #[cfg(test)]
    pub(crate) fn insert_test(
        &mut self,
        name: &str,
        task: &str,
        state: SubagentState,
    ) -> SubagentId {
        let id = format!("subagent-{}-{}", self.next_id, name);
        self.next_id += 1;
        let definition = crate::agents::AgentDefinition {
            name: name.to_string(),
            description: format!("test agent {}", name),
            model_tier: None,
            allowed_tools: Vec::new(),
            permission: None,
            skills: Vec::new(),
            role: None,
            body: String::new(),
            source_path: std::path::PathBuf::new(),
        };
        let mut subagent = Subagent::new(
            id.clone(),
            &definition,
            task.to_string(),
            SubagentContextMode::Create,
            None,
        );
        subagent.state = state;
        self.subagents.push(subagent);
        id
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
    // Subagents never get the main agent's TODO list tool.
    allowed.retain(|n| n != "todo");
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
        let model = Self::resolve_model(&parent, definition.model_tier);
        let client = LlmClient::new(
            model.provider.clone(),
            model.clone(),
            &format!("{}-{}", id, chrono::Utc::now().timestamp_millis()),
        );

        let (shell_state, active_skills, skill_registry) =
            Self::build_runtime_context(definition, mode, &parent);
        let toolbox = build_subagent_toolbox(&parent.toolbox, definition);

        match mode {
            SubagentContextMode::Fork => {
                let mut messages = parent.messages.clone();
                messages.push(Message::user(format!(
                    "You are now acting as the '{}' subagent. Task: {}",
                    definition.name, task
                )));
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
                (
                    vec![system, task_msg],
                    shell_state,
                    active_skills,
                    toolbox,
                    skill_registry,
                    client,
                )
            }
        }
    }

    /// Rebuild the shell state, active skills, and skill registry for a
    /// (re)started subagent.
    fn build_runtime_context(
        definition: &AgentDefinition,
        mode: SubagentContextMode,
        parent: &ParentSnapshot,
    ) -> (ShellState, Vec<String>, SkillRegistry) {
        match mode {
            SubagentContextMode::Fork => {
                // Fork inherits the parent's environment, variables, and cwd,
                // but permissions always follow the subagent's own
                // declaration (or the config default) — never the parent's
                // session-adjusted policy or interactive grants.
                let mut shell_state = parent.shell_state.clone();
                let shell_config = {
                    let mut shell = parent.config.shell.clone().unwrap_or_default();
                    if let Some(perm) = &definition.permission {
                        shell.perm_mode = Some(perm.clone());
                    }
                    shell
                };
                shell_state.set_permission_policy(shell_config.permission_policy());
                (
                    shell_state,
                    if definition.inherits_skills() {
                        parent.active_skills.clone()
                    } else {
                        definition.explicit_skills()
                    },
                    parent.skill_registry.clone(),
                )
            }
            SubagentContextMode::Create => {
                let shell_config = {
                    let mut shell = parent.config.shell.clone().unwrap_or_default();
                    if let Some(perm) = &definition.permission {
                        shell.perm_mode = Some(perm.clone());
                    }
                    shell
                };
                (
                    ShellState::with_policy_and_logger(
                        shell_config.permission_policy(),
                        shell_config.audit_logger(),
                    ),
                    definition.explicit_skills(),
                    SkillRegistry::new(),
                )
            }
        }
    }

    /// Restart a subagent turn loop from persisted messages.
    ///
    /// Used by [`SubagentManager::restore`]: the conversation continues where
    /// it left off instead of re-running the task from scratch.
    fn resume(
        id: SubagentId,
        definition: AgentDefinition,
        messages: Vec<Message>,
        mode: SubagentContextMode,
        parent: ParentSnapshot,
        event_tx: mpsc::Sender<SubagentEvent>,
    ) -> Self {
        let model = Self::resolve_model(&parent, definition.model_tier);
        let client = LlmClient::new(
            model.provider.clone(),
            model.clone(),
            &format!("{}-{}", id, chrono::Utc::now().timestamp_millis()),
        );
        let (shell_state, active_skills, skill_registry) =
            Self::build_runtime_context(&definition, mode, &parent);
        let toolbox = build_subagent_toolbox(&parent.toolbox, &definition);
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

    fn resolve_model(parent: &ParentSnapshot, tier: Option<ModelTier>) -> Model {
        match tier {
            Some(tier) => parent.tier_models.get(tier).clone(),
            None => parent.current_model.clone(),
        }
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
                            Err(e) => {
                                // Leave the loop immediately: both channel
                                // senders are dropped once the stream task
                                // finishes, so re-polling the select would
                                // panic ("all branches are disabled").
                                stream_error = Some(e);
                                break;
                            }
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
                            TierModels::default(),
                            Model::default(),
                            AppConfig::default(),
                            None,
                        );
                        let mut dummy_todos = crate::tool::TodoList::new();
                        let mut ctx = ToolContext {
                            shell_state: &mut self.shell_state,
                            skill_registry: &mut self.skill_registry,
                            active_skills: &mut self.active_skills,
                            todos: &mut dummy_todos,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::AgentRegistry;
    use crate::llm::Provider;
    use std::time::Duration;

    fn test_registry(dir: &std::path::Path) -> AgentRegistry {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("coder.md"),
            "---\nname: coder\ndescription: test agent\n---\nDo things.\n",
        )
        .unwrap();
        AgentRegistry::discover(&[dir.to_path_buf()]).unwrap()
    }

    /// A model whose provider endpoint refuses connections immediately, so a
    /// restarted runner fails fast without touching the network.
    fn unreachable_model() -> Model {
        Model {
            id: "test-model".to_string(),
            name: String::new(),
            context_window: 0,
            provider: Provider {
                name: "test".to_string(),
                base_url: "http://127.0.0.1:9".to_string(),
                api_key: String::new(),
                session_header: None,
            },
        }
    }

    fn manager_with_unreachable_model() -> SubagentManager {
        SubagentManager::new(
            Toolbox::default(),
            TierModels::default(),
            unreachable_model(),
            AppConfig::default(),
            None,
        )
    }

    fn running_snapshot() -> crate::history::SubagentSnapshot {
        crate::history::SubagentSnapshot {
            record: crate::history::SubagentRecord {
                id: "subagent-0-coder".to_string(),
                name: "coder".to_string(),
                task: "do things".to_string(),
                state: "running tool".to_string(),
                mode: "create".to_string(),
                parent_call_id: Some("call-task".to_string()),
                result: None,
                error: None,
            },
            messages: vec![
                Message::user("You are the 'coder' subagent. Task: do things"),
                Message::tool("partial result", "call-2"),
            ],
        }
    }

    #[test]
    fn fork_context_permissions_follow_agent_declaration() {
        use sutcac_sh::permissions::{Permission, PermissionSet};

        fn custom_network() -> PermissionSet {
            let mut set = PermissionSet::empty();
            set.insert(Permission::Custom("NETWORK".to_string()));
            set
        }

        // The parent has an interactive session grant; it must not leak into
        // the subagent.
        let mut parent_shell = ShellState::new();
        parent_shell.permissions =
            sutcac_sh::permissions::PermissionPolicy::parse("allow:read").unwrap();
        parent_shell.permissions.grant_tag("network,write");
        parent_shell
            .vars
            .insert("from_parent".to_string(), "1".to_string());

        let dir = std::env::temp_dir().join(format!("catus_sub_fork_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("declared.md"),
            "---\nname: declared\ndescription: d\npermission: allow:read\n---\nBody.\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("undeclared.md"),
            "---\nname: undeclared\ndescription: u\n---\nBody.\n",
        )
        .unwrap();
        let registry = AgentRegistry::discover(&[dir.to_path_buf()]).unwrap();

        // Config default denies write; the parent's session state is ignored.
        let mut config = AppConfig::default();
        config.shell = Some(sutcac_sh::config::ShellConfig {
            perm_mode: Some("deny:write".to_string()),
            ..Default::default()
        });
        let parent = ParentSnapshot {
            messages: Vec::new(),
            shell_state: parent_shell,
            toolbox: Toolbox::default(),
            active_skills: Vec::new(),
            tier_models: TierModels::default(),
            current_model: unreachable_model(),
            skill_registry: SkillRegistry::new(),
            config,
            mcp_manager: None,
        };

        // Declared agent: its own permission string wins.
        let declared = registry.get("declared").unwrap();
        let (shell, _, _) =
            SubagentRunner::build_runtime_context(declared, SubagentContextMode::Fork, &parent);
        assert!(shell.permissions.check(&PermissionSet::read()).is_ok());
        assert!(shell.permissions.check(&PermissionSet::write()).is_err());
        assert!(shell.permissions.check(&custom_network()).is_err());
        // The fork still inherits the parent's environment.
        assert_eq!(shell.vars.get("from_parent").map(String::as_str), Some("1"));

        // Undeclared agent: falls back to the configured policy, again
        // without the parent's session grants.
        let undeclared = registry.get("undeclared").unwrap();
        let (shell, _, _) =
            SubagentRunner::build_runtime_context(undeclared, SubagentContextMode::Fork, &parent);
        assert!(shell.permissions.check(&PermissionSet::write()).is_err());
        assert!(shell.permissions.check(&custom_network()).is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn restore_restarts_running_subagent_turn_loop() {
        let dir = std::env::temp_dir().join(format!("catus_sub_restore_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let registry = test_registry(&dir);
        let mut manager = manager_with_unreachable_model();

        let shell = ShellState::new();
        manager
            .restore(
                &running_snapshot(),
                &registry,
                &shell,
                &[],
                &SkillRegistry::new(),
            )
            .unwrap();

        assert_eq!(manager.list().len(), 1);
        assert_eq!(
            manager.get("subagent-0-coder").unwrap().state,
            SubagentState::Streaming
        );

        // The restarted runner cannot reach the LLM endpoint; the resulting
        // error event proves the turn loop was relaunched from the saved
        // messages instead of being dropped. Skip the lifecycle events
        // (Started, StateChanged) emitted before the failed request.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let errored = loop {
            let event = tokio::time::timeout_at(deadline, manager.event_rx.recv())
                .await
                .expect("restart runner should emit an error event")
                .expect("event channel should stay open");
            match &event {
                SubagentEvent::Error { id, .. } => {
                    assert_eq!(id, "subagent-0-coder");
                    manager.handle_event(&event);
                    break true;
                }
                _ => manager.handle_event(&event),
            };
        };
        assert!(errored);
        assert_eq!(
            manager.get("subagent-0-coder").unwrap().state,
            SubagentState::Error
        );

        // A future spawn never reuses a restored id.
        assert!(manager.next_id >= 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn restore_completed_subagent_without_restart_and_snapshot_roundtrip() {
        let dir = std::env::temp_dir().join(format!("catus_sub_done_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let registry = test_registry(&dir);
        let mut manager = manager_with_unreachable_model();

        let snapshot = crate::history::SubagentSnapshot {
            record: crate::history::SubagentRecord {
                id: "subagent-3-coder".to_string(),
                name: "coder".to_string(),
                task: "done".to_string(),
                state: "completed".to_string(),
                mode: "create".to_string(),
                parent_call_id: Some("call-task".to_string()),
                result: Some("all done".to_string()),
                error: None,
            },
            messages: vec![Message::assistant("all done")],
        };
        let shell = ShellState::new();
        manager
            .restore(&snapshot, &registry, &shell, &[], &SkillRegistry::new())
            .unwrap();

        // Terminal record: restored as completed, no runner relaunch, and the
        // result survives for the parent-injection path.
        let sub = manager.get("subagent-3-coder").unwrap();
        assert_eq!(sub.state, SubagentState::Completed);
        assert_eq!(sub.result.as_deref(), Some("all done"));
        assert_eq!(sub.parent_call_id.as_deref(), Some("call-task"));

        // No restarted runner: the channel stays silent.
        assert!(
            tokio::time::timeout(Duration::from_millis(100), manager.event_rx.recv())
                .await
                .is_err()
        );

        // Snapshot round-trips through the store.
        let store = crate::history::SessionStore::open_in_memory().unwrap();
        let session = store.create_session("s", "sid", "m").unwrap();
        let snap = manager.snapshot();
        assert_eq!(snap.len(), 1);
        store.replace_subagents(session, &snap).unwrap();
        let loaded = store.load_session(session).unwrap().unwrap();
        assert_eq!(loaded.subagents.len(), 1);
        assert_eq!(loaded.subagents[0].record.id, "subagent-3-coder");
        assert_eq!(
            loaded.subagents[0].record.result.as_deref(),
            Some("all done")
        );
        assert_eq!(loaded.subagents[0].messages.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
