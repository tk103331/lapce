//! A small client for the [Agent Client Protocol](https://agentclientprotocol.com/).
//!
//! Lapce starts an ACP compatible agent as a child process and exchanges
//! newline-delimited JSON-RPC 2.0 messages over its stdin/stdout. The agent
//! handles `session/update` notifications, permission requests and file
//! requests while the UI only has to talk to [`AgentData`].

use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap, VecDeque},
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
};

use anyhow::{Context, Result, anyhow, bail};
use crossbeam_channel::{Receiver, Sender, bounded, select, unbounded};
use floem::{
    ext_event::create_signal_from_channel,
    keyboard::Modifiers,
    reactive::{ReadSignal, RwSignal, Scope, SignalGet, SignalUpdate, SignalWith},
};
use lapce_core::{command::EditCommand, mode::Mode};
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::{
    command::{CommandExecuted, CommandKind, LapceCommand},
    config::{
        LapceConfig,
        agent::{AgentConfig, AgentProfile},
    },
    editor::EditorData,
    keypress::{KeyPressFocus, condition::Condition},
    main_split::Editors,
    window_tab::CommonData,
};

/// An ACP session is a single conversation with an agent.
///
/// The client speaks JSON-RPC 2.0 over the agent's stdin/stdout,
/// which is the transport defined by the Agent Client Protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentRole {
    User,
    Assistant,
    Thought,
    Tool,
    Plan,
    Notice,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentToolCall {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub status: String,
    pub content: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentMessage {
    pub id: u64,
    pub role: AgentRole,
    pub content: String,
    pub tool: Option<AgentToolCall>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionOption {
    pub option_id: String,
    pub name: String,
    pub kind: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingPermission {
    pub title: String,
    pub options: Vec<PermissionOption>,
}

/// A model or session mode offered by an ACP agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentChoice {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct AgentChoiceState {
    pub available: Vec<AgentChoice>,
    pub current: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentConfigValueOption {
    pub value: String,
    pub name: String,
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentConfigOptionKind {
    Select {
        current: String,
        options: Vec<AgentConfigValueOption>,
    },
    Boolean {
        current: bool,
    },
}

/// A session configuration option advertised by an ACP agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentConfigOption {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub category: Option<String>,
    pub kind: AgentConfigOptionKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentConfigValue {
    Select(String),
    Boolean(bool),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentQuestionOption {
    pub value: String,
    pub title: String,
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AgentQuestionKind {
    Text {
        default: Option<String>,
    },
    Integer {
        default: Option<i64>,
        minimum: Option<f64>,
        maximum: Option<f64>,
    },
    Number {
        default: Option<f64>,
        minimum: Option<f64>,
        maximum: Option<f64>,
    },
    Boolean {
        default: bool,
    },
    Select {
        default: Option<String>,
        options: Vec<AgentQuestionOption>,
    },
    MultiSelect {
        default: Vec<String>,
        options: Vec<AgentQuestionOption>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentQuestionField {
    pub name: String,
    pub title: String,
    pub description: Option<String>,
    pub required: bool,
    pub kind: AgentQuestionKind,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentQuestion {
    pub id: u64,
    pub message: String,
    pub fields: Vec<AgentQuestionField>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentQuestionValue {
    Select(String),
    Boolean(bool),
    Multi(Vec<String>),
}

#[derive(Clone, Debug)]
pub enum AgentQuestionAnswer {
    Accept(BTreeMap<String, Value>),
    Decline,
    Cancel,
}

/// Summary of a conversation shown in the session history.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentSessionInfo {
    pub id: u64,
    pub agent: String,
    pub title: String,
    pub messages: usize,
    pub created: u64,
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentTask {
    pub content: String,
    pub status: String,
    pub priority: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentUsage {
    pub total_tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub thought_tokens: Option<u64>,
    pub cached_read_tokens: Option<u64>,
    pub cached_write_tokens: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentContextUsage {
    pub used: u64,
    pub size: u64,
    pub cost: Option<AgentCost>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentCost {
    pub amount: f64,
    pub currency: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum AgentStatus {
    #[default]
    Idle,
    Starting,
    Ready,
    Thinking,
    WaitingForPermission,
    WaitingForAnswer,
    Stopping,
    Failed(String),
    Stopped,
}

impl AgentStatus {
    pub fn is_busy(&self) -> bool {
        matches!(
            self,
            AgentStatus::Starting
                | AgentStatus::Thinking
                | AgentStatus::WaitingForPermission
                | AgentStatus::WaitingForAnswer
                | AgentStatus::Stopping
        )
    }

    pub fn label(&self) -> String {
        match self {
            AgentStatus::Idle => "Not started".to_string(),
            AgentStatus::Starting => "Starting agent…".to_string(),
            AgentStatus::Ready => "Ready".to_string(),
            AgentStatus::Thinking => "Thinking…".to_string(),
            AgentStatus::WaitingForPermission => {
                "Waiting for permission".to_string()
            }
            AgentStatus::WaitingForAnswer => "Waiting for your answer".to_string(),
            AgentStatus::Stopping => "Stopping…".to_string(),
            AgentStatus::Failed(_) => "Error".to_string(),
            AgentStatus::Stopped => "Stopped".to_string(),
        }
    }
}

#[derive(Clone, Debug)]
enum AgentCommand {
    Prompt(String),
    Cancel,
    NewSession,
    UseSession(Option<String>),
    Shutdown,
    SetModel(String),
    SetMode(String),
    SetConfigOption {
        config_id: String,
        value: AgentConfigValue,
    },
}

struct Notifier {
    sequence: AtomicU64,
    sender: std::sync::mpsc::SyncSender<u64>,
}

impl Notifier {
    fn notify(&self) {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        // Only one notification needs to be queued: the UI always reads the
        // latest state from the store.
        let _ = self.sender.try_send(sequence);
    }
}

#[derive(Default)]
struct Store {
    messages: Vec<AgentMessage>,
    next_message_id: u64,
    status: AgentStatus,
    agent_name: Option<String>,
    permission: Option<PendingPermission>,
    permission_tx: Option<Sender<Option<String>>>,
    assistant_message: Option<u64>,
    thought_message: Option<u64>,
    tool_messages: HashMap<String, u64>,
    restart_requested: bool,
    models: Option<AgentChoiceState>,
    modes: Option<AgentChoiceState>,
    config_options: Vec<AgentConfigOption>,
    tasks: Vec<AgentTask>,
    question: Option<AgentQuestion>,
    question_tx: Option<Sender<AgentQuestionAnswer>>,
    next_question_id: u64,
    usage: Option<AgentUsage>,
    context_usage: Option<AgentContextUsage>,
    sessions: Vec<SessionArchive>,
    current_session: u64,
    next_session_id: u64,
    session_created: u64,
    acp_session_id: Option<String>,
}

#[derive(Clone)]
struct SessionArchive {
    id: u64,
    agent: String,
    title: String,
    created: u64,
    acp_session_id: Option<String>,
    messages: Vec<AgentMessage>,
    usage: Option<AgentUsage>,
    context_usage: Option<AgentContextUsage>,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn message_preview(text: &str) -> String {
    let first = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim();
    let mut chars = first.chars();
    let mut preview: String = chars.by_ref().take(80).collect();
    if chars.next().is_some() {
        preview.push('…');
    }
    if preview.is_empty() {
        "New session".to_string()
    } else {
        preview
    }
}

impl Store {
    fn push_message(&mut self, role: AgentRole, content: String) -> u64 {
        self.next_message_id += 1;
        let id = self.next_message_id;
        self.messages.push(AgentMessage {
            id,
            role,
            content,
            tool: None,
        });
        id
    }

    fn append_stream(&mut self, role: AgentRole, text: &str) {
        if text.is_empty() {
            return;
        }

        let message_id = match role {
            AgentRole::Assistant => self.assistant_message,
            AgentRole::Thought => self.thought_message,
            _ => None,
        };

        if let Some(message_id) = message_id {
            if let Some(message) =
                self.messages.iter_mut().find(|m| m.id == message_id)
            {
                if role == AgentRole::Thought && message.content.len() > 16_000 {
                    return;
                }
                message.content.push_str(text);
                return;
            }
        }

        let message_id = self.push_message(role, text.to_string());
        match role {
            AgentRole::Assistant => self.assistant_message = Some(message_id),
            AgentRole::Thought => self.thought_message = Some(message_id),
            _ => {}
        }
    }

    fn begin_turn(&mut self) {
        self.assistant_message = None;
        self.thought_message = None;
        self.tool_messages.clear();
        self.permission = None;
        self.permission_tx = None;
        self.question = None;
        self.question_tx = None;
        self.tasks.clear();
        self.status = AgentStatus::Thinking;
    }

    fn archive_current(&mut self) {
        if self.messages.is_empty() {
            return;
        }

        let title = self
            .messages
            .iter()
            .find(|message| message.role == AgentRole::User)
            .map(|message| message_preview(&message.content))
            .unwrap_or_else(|| "New session".to_string());
        let archive = SessionArchive {
            id: self.current_session,
            agent: self.agent_name.clone().unwrap_or_default(),
            title,
            created: self.session_created,
            acp_session_id: self.acp_session_id.clone(),
            messages: self.messages.clone(),
            usage: self.usage.clone(),
            context_usage: self.context_usage.clone(),
        };

        if let Some(existing) = self
            .sessions
            .iter_mut()
            .find(|session| session.id == archive.id)
        {
            *existing = archive;
        } else {
            self.sessions.push(archive);
        }
    }

    fn begin_session(&mut self) {
        self.archive_current();
        if self.next_session_id < self.current_session {
            self.next_session_id = self.current_session;
        }
        self.next_session_id += 1;
        self.current_session = self.next_session_id;
        self.session_created = now_secs();
        self.reset_transcript();
        self.status = AgentStatus::Starting;
    }

    fn reset_transcript(&mut self) {
        self.messages.clear();
        self.tool_messages.clear();
        self.assistant_message = None;
        self.thought_message = None;
        self.permission = None;
        self.permission_tx = None;
        self.question = None;
        self.question_tx = None;
        self.tasks.clear();
        self.usage = None;
        self.context_usage = None;
        self.acp_session_id = None;
    }

    fn session_agent(&self, id: u64) -> Option<String> {
        if id == self.current_session {
            return Some(self.agent_name.clone().unwrap_or_default());
        }
        self.sessions
            .iter()
            .find(|session| session.id == id)
            .map(|session| session.agent.clone())
    }

    fn load_session(&mut self, id: u64) -> Option<Option<String>> {
        if id == self.current_session {
            return Some(self.acp_session_id.clone());
        }

        self.archive_current();
        let index = self.sessions.iter().position(|session| session.id == id)?;
        let archive = self.sessions.remove(index);
        self.current_session = archive.id;
        self.session_created = archive.created;
        self.messages = archive.messages;
        self.usage = archive.usage;
        self.context_usage = archive.context_usage;
        self.acp_session_id = archive.acp_session_id.clone();
        self.tool_messages.clear();
        self.assistant_message = None;
        self.thought_message = None;
        self.permission = None;
        self.permission_tx = None;
        self.question = None;
        self.question_tx = None;
        self.tasks.clear();
        self.status = AgentStatus::Ready;
        Some(archive.acp_session_id)
    }

    fn session_infos(&self) -> Vec<AgentSessionInfo> {
        let mut infos = Vec::new();
        if !self.messages.is_empty() {
            let title = self
                .messages
                .iter()
                .find(|message| message.role == AgentRole::User)
                .map(|message| message_preview(&message.content))
                .unwrap_or_else(|| "New session".to_string());
            infos.push(AgentSessionInfo {
                id: self.current_session,
                agent: self.agent_name.clone().unwrap_or_default(),
                title,
                messages: self.messages.len(),
                created: self.session_created,
                active: true,
            });
        }

        for session in self.sessions.iter() {
            infos.push(AgentSessionInfo {
                id: session.id,
                agent: session.agent.clone(),
                title: session.title.clone(),
                messages: session.messages.len(),
                created: session.created,
                active: session.id == self.current_session,
            });
        }

        infos.sort_by(|a, b| b.created.cmp(&a.created).then(b.id.cmp(&a.id)));
        infos
    }

    fn clear(&mut self) {
        self.messages.clear();
        self.tool_messages.clear();
        self.assistant_message = None;
        self.thought_message = None;
        self.permission = None;
        self.permission_tx = None;
        self.question = None;
        self.question_tx = None;
        self.tasks.clear();
        self.status = AgentStatus::Idle;
        self.models = None;
        self.modes = None;
        self.config_options.clear();
        self.usage = None;
        self.context_usage = None;
    }

    fn tool_call(
        &mut self,
        tool_call_id: String,
        title: String,
        kind: String,
        status: String,
        content: String,
    ) {
        // Text after a tool call belongs to a new assistant message.
        self.assistant_message = None;
        self.thought_message = None;

        let content = truncate_tool_output(content);

        if let Some(&message_id) = self.tool_messages.get(&tool_call_id) {
            if let Some(message) =
                self.messages.iter_mut().find(|m| m.id == message_id)
            {
                if let Some(tool) = message.tool.as_mut() {
                    tool.title = title;
                    tool.kind = kind;
                    tool.status = status;
                    if !content.is_empty() {
                        tool.content = content;
                    }
                }
                return;
            }
        }

        let message_id = self.push_message(AgentRole::Tool, String::new());
        if let Some(message) = self.messages.last_mut() {
            message.tool = Some(AgentToolCall {
                id: tool_call_id.clone(),
                title,
                kind,
                status,
                content,
            });
        }
        self.tool_messages.insert(tool_call_id, message_id);
    }

    fn update_tool(
        &mut self,
        tool_call_id: &str,
        title: Option<String>,
        kind: Option<String>,
        status: Option<String>,
        content: Option<String>,
    ) {
        let Some(&message_id) = self.tool_messages.get(tool_call_id) else {
            return;
        };
        let Some(message) = self.messages.iter_mut().find(|m| m.id == message_id)
        else {
            return;
        };
        let Some(tool) = message.tool.as_mut() else {
            return;
        };
        if let Some(title) = title {
            tool.title = title;
        }
        if let Some(kind) = kind {
            tool.kind = kind;
        }
        if let Some(status) = status {
            tool.status = status;
        }
        if let Some(content) = content {
            let content = truncate_tool_output(content);
            if !content.is_empty() {
                tool.content = content;
            }
        }
    }

    fn set_plan(&mut self, entries: &[Value]) {
        self.tasks = entries
            .iter()
            .filter_map(|entry| {
                Some(AgentTask {
                    content: entry["content"].as_str()?.to_string(),
                    status: entry["status"]
                        .as_str()
                        .unwrap_or("pending")
                        .to_string(),
                    priority: entry["priority"]
                        .as_str()
                        .unwrap_or("medium")
                        .to_string(),
                })
            })
            .collect();
    }

    fn error(&mut self, message: impl Into<String>) {
        let message = message.into();
        self.push_message(AgentRole::Error, message.clone());
        self.status = AgentStatus::Failed(message);
    }
}

#[derive(Clone)]
pub struct AgentData {
    pub common: Rc<CommonData>,
    pub input: EditorData,
    pub messages: RwSignal<im::Vector<RwSignal<AgentMessage>>>,
    pub status: RwSignal<AgentStatus>,
    pub agent_name: RwSignal<Option<String>>,
    pub permission: RwSignal<Option<PendingPermission>>,
    pub revision: ReadSignal<Option<u64>>,
    pub auto_scroll: RwSignal<bool>,
    pub models: RwSignal<Option<AgentChoiceState>>,
    pub modes: RwSignal<Option<AgentChoiceState>>,
    pub config_options: RwSignal<im::Vector<AgentConfigOption>>,
    pub tasks: RwSignal<im::Vector<AgentTask>>,
    pub question: RwSignal<Option<AgentQuestion>>,
    pub question_values: RwSignal<im::HashMap<String, AgentQuestionValue>>,
    pub usage: RwSignal<Option<AgentUsage>>,
    pub context_usage: RwSignal<Option<AgentContextUsage>>,
    pub expanded_thoughts: RwSignal<im::HashSet<u64>>,
    pub expanded_tools: RwSignal<im::HashSet<u64>>,
    pub profiles: RwSignal<im::Vector<AgentProfile>>,
    pub active_profile: RwSignal<String>,
    pub sessions: RwSignal<im::Vector<AgentSessionInfo>>,
    pub sessions_open: RwSignal<bool>,
    store: Arc<Mutex<Store>>,
    notifier: Arc<Notifier>,
    sender: Arc<Mutex<Option<Sender<AgentCommand>>>>,
    running: Arc<AtomicBool>,
    epoch: Arc<AtomicU64>,
    profile: Arc<Mutex<Option<AgentProfile>>>,
    editors: Editors,
    question_inputs: Rc<RefCell<HashMap<String, EditorData>>>,
}

impl std::fmt::Debug for AgentData {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("AgentData").finish()
    }
}

impl KeyPressFocus for AgentData {
    fn get_mode(&self) -> Mode {
        self.input.get_mode()
    }

    fn check_condition(&self, condition: Condition) -> bool {
        self.input.check_condition(condition)
    }

    fn run_command(
        &self,
        command: &LapceCommand,
        count: Option<usize>,
        mods: Modifiers,
    ) -> CommandExecuted {
        if matches!(&command.kind, CommandKind::Edit(EditCommand::InsertNewLine))
            && !mods.contains(Modifiers::SHIFT)
            && !mods.contains(Modifiers::ALT)
        {
            self.submit_input();
            return CommandExecuted::Yes;
        }

        self.input.run_command(command, count, mods)
    }

    fn expect_char(&self) -> bool {
        self.input.expect_char()
    }

    fn receive_char(&self, c: &str) {
        self.input.receive_char(c);
    }
}

impl AgentData {
    pub fn new(cx: Scope, editors: Editors, common: Rc<CommonData>) -> Self {
        let input = editors.make_local(cx, common.clone());
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let revision = create_signal_from_channel(receiver);
        let notifier = Arc::new(Notifier {
            sequence: AtomicU64::new(0),
            sender,
        });
        let store = Arc::new(Mutex::new(Store::default()));

        let agent_config = common.config.get_untracked().agent.clone();
        let profiles: im::Vector<AgentProfile> =
            agent_config.profiles().into_iter().collect();
        let active_profile = agent_config.active_profile();

        let data = Self {
            common,
            input,
            messages: cx.create_rw_signal(im::Vector::new()),
            status: cx.create_rw_signal(AgentStatus::Idle),
            agent_name: cx.create_rw_signal(None),
            permission: cx.create_rw_signal(None),
            revision,
            auto_scroll: cx.create_rw_signal(true),
            models: cx.create_rw_signal(None),
            modes: cx.create_rw_signal(None),
            config_options: cx.create_rw_signal(im::Vector::new()),
            tasks: cx.create_rw_signal(im::Vector::new()),
            question: cx.create_rw_signal(None),
            question_values: cx.create_rw_signal(im::HashMap::new()),
            usage: cx.create_rw_signal(None),
            context_usage: cx.create_rw_signal(None),
            expanded_thoughts: cx.create_rw_signal(im::HashSet::new()),
            expanded_tools: cx.create_rw_signal(im::HashSet::new()),
            profiles: cx.create_rw_signal(profiles),
            active_profile: cx.create_rw_signal(active_profile.name.clone()),
            sessions: cx.create_rw_signal(im::Vector::new()),
            sessions_open: cx.create_rw_signal(false),
            store,
            notifier,
            sender: Arc::new(Mutex::new(None)),
            running: Arc::new(AtomicBool::new(false)),
            epoch: Arc::new(AtomicU64::new(0)),
            profile: Arc::new(Mutex::new(Some(active_profile))),
            editors,
            question_inputs: Rc::new(RefCell::new(HashMap::new())),
        };

        let data_for_effect = data.clone();
        cx.create_effect(move |_| {
            if data_for_effect.revision.get().is_some() {
                data_for_effect.sync();
            }
        });

        // Keep the panel in sync with profiles edited in the settings page.
        let data_for_profiles = data.clone();
        cx.create_effect(move |_| {
            let agent_config = data_for_profiles.common.config.get().agent.clone();
            let profiles: im::Vector<AgentProfile> =
                agent_config.profiles().into_iter().collect();
            if data_for_profiles.profiles.get_untracked() != profiles {
                data_for_profiles.profiles.set(profiles);
            }
            let active = agent_config.active_profile();
            if !active.name.is_empty()
                && data_for_profiles.active_profile.get_untracked() != active.name
            {
                data_for_profiles.active_profile.set(active.name.clone());
                *data_for_profiles.profile.lock() = Some(active);
            }
        });

        data
    }

    fn sync(&self) {
        let (
            messages,
            status,
            agent_name,
            permission,
            models,
            modes,
            config_options,
            tasks,
            question,
            usage,
            context_usage,
            sessions,
            restart,
        ) = {
            let mut store = self.store.lock();
            let restart =
                store.restart_requested && !self.running.load(Ordering::SeqCst);
            if restart {
                store.restart_requested = false;
            }
            (
                store.messages.clone(),
                store.status.clone(),
                store.agent_name.clone(),
                store.permission.clone(),
                store.models.clone(),
                store.modes.clone(),
                store
                    .config_options
                    .iter()
                    .cloned()
                    .collect::<im::Vector<_>>(),
                store.tasks.iter().cloned().collect::<im::Vector<_>>(),
                store.question.clone(),
                store.usage.clone(),
                store.context_usage.clone(),
                store.session_infos().into_iter().collect::<im::Vector<_>>(),
                restart,
            )
        };

        if self.status.get_untracked() != status {
            self.status.set(status);
        }
        if self.agent_name.get_untracked() != agent_name {
            self.agent_name.set(agent_name);
        }
        if self.permission.get_untracked() != permission {
            self.permission.set(permission);
        }
        if self.models.get_untracked() != models {
            self.models.set(models);
        }
        if self.modes.get_untracked() != modes {
            self.modes.set(modes);
        }
        if self.config_options.get_untracked() != config_options {
            self.config_options.set(config_options);
        }
        if self.tasks.get_untracked() != tasks {
            self.tasks.set(tasks);
        }
        if self.question.get_untracked() != question {
            self.prepare_question(question.as_ref());
            self.question.set(question);
        }
        if self.usage.get_untracked() != usage {
            self.usage.set(usage);
        }
        if self.context_usage.get_untracked() != context_usage {
            self.context_usage.set(context_usage);
        }
        if self.sessions.get_untracked() != sessions {
            self.sessions.set(sessions);
        }

        self.messages.update(|existing| {
            let mut updated = im::Vector::new();
            for message in messages {
                if let Some(signal) = existing
                    .iter()
                    .find(|signal| signal.get_untracked().id == message.id)
                    .cloned()
                {
                    if signal.get_untracked() != message {
                        signal.set(message);
                    }
                    updated.push_back(signal);
                } else {
                    updated.push_back(self.common.scope.create_rw_signal(message));
                }
            }
            *existing = updated;
        });

        if restart {
            self.ensure_started();
        }
    }

    /// Send the content of the composer to the agent.
    pub fn submit_input(&self) {
        if self.status.get_untracked().is_busy() {
            return;
        }

        let text = self.input.doc().buffer.with(|buffer| buffer.to_string());
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }

        if self.send_prompt(text) {
            self.input.reset();
        }
    }

    pub fn send_prompt(&self, text: String) -> bool {
        if self.status.get_untracked().is_busy() {
            return false;
        }

        self.ensure_started();
        let sender = self.sender.lock().clone();
        let Some(sender) = sender else {
            return false;
        };

        {
            let mut store = self.store.lock();
            store.push_message(AgentRole::User, text.clone());
            store.begin_turn();
        }
        self.auto_scroll.set(true);
        self.notifier.notify();

        if sender.send(AgentCommand::Prompt(text)).is_err() {
            let mut store = self.store.lock();
            store.error("The agent is no longer running");
            drop(store);
            self.notifier.notify();
        }

        true
    }

    /// Ask the agent to stop the current turn.
    pub fn cancel(&self) {
        if !self.status.get_untracked().is_busy() {
            return;
        }

        {
            let mut store = self.store.lock();
            if store.status != AgentStatus::WaitingForPermission
                && store.status != AgentStatus::WaitingForAnswer
            {
                store.status = AgentStatus::Stopping;
            }
        }
        self.notifier.notify();

        if let Some(sender) = self.sender.lock().as_ref() {
            let _ = sender.send(AgentCommand::Cancel);
        }
    }

    /// Start a fresh ACP session with the current agent, keeping the
    /// previous conversation in the history.
    pub fn new_session(&self) {
        self.start_session(None);
    }

    /// Start a fresh ACP session with the given agent profile. The profile
    /// becomes the active agent and its name is shown in the panel header.
    pub fn new_session_with_profile(&self, name: String) {
        self.start_session(Some(name));
    }

    fn start_session(&self, profile_name: Option<String>) {
        if self.status.get_untracked().is_busy() {
            return;
        }

        let mut switched = false;
        if let Some(name) = profile_name.as_deref() {
            if self.active_profile.get_untracked() != name {
                // Keep the current conversation before the restart clears it.
                self.store.lock().archive_current();
            }
            switched = self.apply_profile(name);
        }

        {
            let mut store = self.store.lock();
            store.begin_session();
            store.agent_name = Some(self.active_profile.get_untracked());
        }
        self.sessions_open.set(false);
        self.auto_scroll.set(true);
        self.notifier.notify();

        if switched {
            // `apply_profile` restarts the process; `sync` starts it again
            // once the old process has exited.
            return;
        }

        if self.running.load(Ordering::SeqCst) {
            if let Some(sender) = self.sender.lock().as_ref() {
                let _ = sender.send(AgentCommand::NewSession);
            }
        } else {
            self.ensure_started();
        }
    }

    /// Load a conversation from the session history.
    pub fn switch_session(&self, id: u64) {
        if self.status.get_untracked().is_busy() {
            return;
        }

        let Some(agent) = self.store.lock().session_agent(id) else {
            return;
        };

        let same_profile =
            agent.is_empty() || self.active_profile.get_untracked() == agent;
        if !same_profile {
            self.store.lock().archive_current();
            self.apply_profile(&agent);
        }

        let resume = {
            let mut store = self.store.lock();
            let resume = store.load_session(id);
            if !same_profile {
                // A restarted process creates a brand new ACP session.
                store.acp_session_id = None;
            }
            resume
        };
        let Some(resume) = resume else {
            return;
        };
        let resume = if same_profile { resume } else { None };

        self.sessions_open.set(false);
        self.auto_scroll.set(true);
        self.notifier.notify();

        if self.running.load(Ordering::SeqCst) {
            if let Some(sender) = self.sender.lock().as_ref() {
                let _ = sender.send(AgentCommand::UseSession(resume));
            }
        } else {
            self.ensure_started();
        }
    }

    pub fn toggle_sessions(&self) {
        self.sessions_open.update(|open| *open = !*open);
    }

    /// Apply an agent profile, restarting the process when it changed.
    /// Returns whether a restart was requested.
    fn apply_profile(&self, name: &str) -> bool {
        if self.active_profile.get_untracked() == name {
            return false;
        }

        let profiles = self.profiles.get_untracked();
        let Some(profile) = profiles
            .iter()
            .find(|profile| profile.name == name)
            .cloned()
        else {
            return false;
        };

        *self.profile.lock() = Some(profile.clone());
        self.active_profile.set(profile.name);
        self.persist_agent_profiles();
        self.restart();
        true
    }

    /// Stop the current process and start a new one with the current config.
    pub fn restart(&self) {
        if self.running.load(Ordering::SeqCst) {
            {
                let mut store = self.store.lock();
                store.clear();
                store.status = AgentStatus::Stopping;
                store.restart_requested = true;
            }
            self.auto_scroll.set(true);
            self.notifier.notify();
            if let Some(sender) = self.sender.lock().as_ref() {
                let _ = sender.send(AgentCommand::Shutdown);
            }
        } else {
            {
                let mut store = self.store.lock();
                store.clear();
            }
            self.auto_scroll.set(true);
            self.notifier.notify();
            self.ensure_started();
        }
    }

    /// Resolve the currently displayed permission request.
    pub fn resolve_permission(&self, option_id: Option<String>) {
        let mut store = self.store.lock();
        if let Some(sender) = store.permission_tx.take() {
            let _ = sender.send(option_id);
        }
        store.permission = None;
        store.status = AgentStatus::Thinking;
        drop(store);
        self.notifier.notify();
    }

    fn persist_agent_profiles(&self) {
        let profiles: Vec<AgentProfile> =
            self.profiles.get_untracked().iter().cloned().collect();
        let active = self.active_profile.get_untracked();
        LapceConfig::update_agent_profiles(&profiles, &active);
    }

    pub fn select_model(&self, model_id: String) {
        {
            let mut store = self.store.lock();
            if let Some(models) = store.models.as_mut() {
                models.current = model_id.clone();
            }
        }
        self.notifier.notify();
        self.send_command(AgentCommand::SetModel(model_id));
    }

    pub fn select_mode(&self, mode_id: String) {
        {
            let mut store = self.store.lock();
            if let Some(modes) = store.modes.as_mut() {
                modes.current = mode_id.clone();
            }
        }
        self.notifier.notify();
        self.send_command(AgentCommand::SetMode(mode_id));
    }

    pub fn set_config_option(&self, config_id: &str, value: AgentConfigValue) {
        {
            let mut store = self.store.lock();
            if let Some(option) = store
                .config_options
                .iter_mut()
                .find(|option| option.id == config_id)
            {
                match (&mut option.kind, &value) {
                    (
                        AgentConfigOptionKind::Select { current, .. },
                        AgentConfigValue::Select(value),
                    ) => *current = value.clone(),
                    (
                        AgentConfigOptionKind::Boolean { current },
                        AgentConfigValue::Boolean(value),
                    ) => *current = *value,
                    _ => {}
                }
            }
        }
        self.notifier.notify();
        self.send_command(AgentCommand::SetConfigOption {
            config_id: config_id.to_string(),
            value,
        });
    }

    fn prepare_question(&self, question: Option<&AgentQuestion>) {
        self.clear_question_ui();
        let Some(question) = question else {
            return;
        };

        let mut values = im::HashMap::new();
        {
            let mut inputs = self.question_inputs.borrow_mut();
            for field in &question.fields {
                match &field.kind {
                    AgentQuestionKind::Text { default } => {
                        let editor = self
                            .editors
                            .make_local(self.common.scope, self.common.clone());
                        if let Some(default) = default {
                            editor.doc().reload(
                                lapce_xi_rope::Rope::from(default.clone()),
                                true,
                            );
                        }
                        inputs.insert(field.name.clone(), editor);
                    }
                    AgentQuestionKind::Integer { default, .. } => {
                        let editor = self
                            .editors
                            .make_local(self.common.scope, self.common.clone());
                        if let Some(default) = default {
                            editor.doc().reload(
                                lapce_xi_rope::Rope::from(default.to_string()),
                                true,
                            );
                        }
                        inputs.insert(field.name.clone(), editor);
                    }
                    AgentQuestionKind::Number { default, .. } => {
                        let editor = self
                            .editors
                            .make_local(self.common.scope, self.common.clone());
                        if let Some(default) = default {
                            editor.doc().reload(
                                lapce_xi_rope::Rope::from(default.to_string()),
                                true,
                            );
                        }
                        inputs.insert(field.name.clone(), editor);
                    }
                    AgentQuestionKind::Boolean { default } => {
                        values.insert(
                            field.name.clone(),
                            AgentQuestionValue::Boolean(*default),
                        );
                    }
                    AgentQuestionKind::Select { default, options } => {
                        let value = default
                            .clone()
                            .or_else(|| {
                                options.first().map(|option| option.value.clone())
                            })
                            .unwrap_or_default();
                        values.insert(
                            field.name.clone(),
                            AgentQuestionValue::Select(value),
                        );
                    }
                    AgentQuestionKind::MultiSelect { default, .. } => {
                        values.insert(
                            field.name.clone(),
                            AgentQuestionValue::Multi(default.clone()),
                        );
                    }
                }
            }
        }
        self.question_values.set(values);
    }

    fn clear_question_ui(&self) {
        let mut inputs = self.question_inputs.borrow_mut();
        for (_, editor) in inputs.drain() {
            self.editors.remove(editor.id());
        }
        self.question_values.set(im::HashMap::new());
    }

    pub fn question_input(&self, name: &str) -> Option<EditorData> {
        self.question_inputs.borrow().get(name).cloned()
    }

    pub fn set_question_select(&self, name: &str, value: String) {
        self.question_values.update(|values| {
            values.insert(name.to_string(), AgentQuestionValue::Select(value));
        });
    }

    pub fn toggle_question_bool(&self, name: &str) {
        self.question_values.update(|values| {
            let current =
                matches!(values.get(name), Some(AgentQuestionValue::Boolean(true)));
            values.insert(name.to_string(), AgentQuestionValue::Boolean(!current));
        });
    }

    pub fn toggle_question_multi(&self, name: &str, value: String) {
        self.question_values.update(|values| {
            let current = match values.get(name) {
                Some(AgentQuestionValue::Multi(values)) => values.clone(),
                _ => Vec::new(),
            };
            let mut next = current;
            if let Some(index) = next.iter().position(|item| item == &value) {
                next.remove(index);
            } else {
                next.push(value);
            }
            values.insert(name.to_string(), AgentQuestionValue::Multi(next));
        });
    }

    pub fn submit_question_answer(&self) {
        let Some(question) = self.question.get_untracked() else {
            return;
        };
        let values = self.question_values.get_untracked();
        let inputs = self.question_inputs.borrow();
        let mut content = BTreeMap::new();

        for field in &question.fields {
            let text = inputs.get(&field.name).map(|editor| {
                editor
                    .doc()
                    .buffer
                    .with(|buffer| buffer.to_string())
                    .trim()
                    .to_string()
            });

            match &field.kind {
                AgentQuestionKind::Text { .. } => {
                    if let Some(text) = text {
                        if !text.is_empty() || field.required {
                            content.insert(field.name.clone(), Value::String(text));
                        }
                    }
                }
                AgentQuestionKind::Integer { .. } => {
                    if let Some(text) = text {
                        if let Ok(value) = text.parse::<i64>() {
                            content.insert(field.name.clone(), Value::from(value));
                        }
                    }
                }
                AgentQuestionKind::Number { .. } => {
                    if let Some(text) = text {
                        if let Ok(value) = text.parse::<f64>() {
                            content.insert(field.name.clone(), Value::from(value));
                        }
                    }
                }
                AgentQuestionKind::Boolean { .. } => {
                    let value = matches!(
                        values.get(&field.name),
                        Some(AgentQuestionValue::Boolean(true))
                    );
                    content.insert(field.name.clone(), Value::Bool(value));
                }
                AgentQuestionKind::Select { .. } => {
                    if let Some(AgentQuestionValue::Select(value)) =
                        values.get(&field.name)
                    {
                        if !value.is_empty() || field.required {
                            content.insert(
                                field.name.clone(),
                                Value::String(value.clone()),
                            );
                        }
                    }
                }
                AgentQuestionKind::MultiSelect { .. } => {
                    if let Some(AgentQuestionValue::Multi(values)) =
                        values.get(&field.name)
                    {
                        content.insert(
                            field.name.clone(),
                            Value::Array(
                                values.iter().cloned().map(Value::String).collect(),
                            ),
                        );
                    }
                }
            }
        }
        drop(inputs);

        self.answer_question(AgentQuestionAnswer::Accept(content));
    }

    pub fn answer_question(&self, answer: AgentQuestionAnswer) {
        {
            let mut store = self.store.lock();
            if let Some(sender) = store.question_tx.take() {
                let _ = sender.send(answer);
            }
            store.question = None;
            store.status = AgentStatus::Thinking;
        }
        self.clear_question_ui();
        self.notifier.notify();
    }

    pub fn toggle_thought(&self, id: u64) {
        self.expanded_thoughts.update(|expanded| {
            if expanded.remove(&id).is_none() {
                expanded.insert(id);
            }
        });
    }

    pub fn toggle_tool(&self, id: u64) {
        self.expanded_tools.update(|expanded| {
            if expanded.remove(&id).is_none() {
                expanded.insert(id);
            }
        });
    }

    fn send_command(&self, command: AgentCommand) {
        if let Some(sender) = self.sender.lock().as_ref() {
            let _ = sender.send(command);
        }
    }

    fn ensure_started(&self) {
        if self.running.load(Ordering::SeqCst) {
            return;
        }

        if self.common.workspace.kind.is_remote() {
            let mut store = self.store.lock();
            store.error("The agent chat is only available for local workspaces.");
            drop(store);
            self.notifier.notify();
            return;
        }

        let mut config = self.common.config.get_untracked().agent.clone();
        let mut env: Vec<(String, String)> = Vec::new();
        if let Some(profile) = self.profile.lock().clone() {
            if !profile.command.trim().is_empty() {
                config.command = profile.command;
            }
            env = profile
                .env
                .iter()
                .filter_map(|entry| {
                    let (key, value) = entry.split_once('=')?;
                    let key = key.trim();
                    if key.is_empty() {
                        return None;
                    }
                    Some((key.to_string(), value.to_string()))
                })
                .collect();
        }
        let cwd = self
            .common
            .workspace
            .path
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));

        let (sender, receiver) = unbounded();
        let epoch = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        self.running.store(true, Ordering::SeqCst);
        *self.sender.lock() = Some(sender);

        {
            let mut store = self.store.lock();
            store.status = AgentStatus::Starting;
            store.agent_name = None;
            store.models = None;
            store.modes = None;
            store.config_options.clear();
        }
        self.notifier.notify();

        let context = AgentThread {
            store: self.store.clone(),
            notifier: self.notifier.clone(),
            running: self.running.clone(),
            epoch: self.epoch.clone(),
        };

        let spawn_result = thread::Builder::new()
            .name("lapce-agent".to_string())
            .spawn(move || run_agent(context, receiver, config, cwd, env, epoch));

        if let Err(error) = spawn_result {
            self.running.store(false, Ordering::SeqCst);
            *self.sender.lock() = None;
            let mut store = self.store.lock();
            store.error(format!("Failed to start the agent thread: {error}"));
            drop(store);
            self.notifier.notify();
        }
    }
}

struct AgentThread {
    store: Arc<Mutex<Store>>,
    notifier: Arc<Notifier>,
    running: Arc<AtomicBool>,
    epoch: Arc<AtomicU64>,
}

impl AgentThread {
    fn error(&self, message: impl Into<String>) {
        self.store.lock().error(message);
        self.notifier.notify();
    }
}

fn run_agent(
    context: AgentThread,
    commands: Receiver<AgentCommand>,
    config: AgentConfig,
    cwd: PathBuf,
    env: Vec<(String, String)>,
    epoch: u64,
) {
    let result = run_agent_inner(&context, &commands, config, cwd, env);
    if context.epoch.load(Ordering::SeqCst) != epoch {
        // Another agent was started while this one was shutting down.
        return;
    }

    if let Err(error) = result {
        context.error(format!("{error:#}"));
    }

    context.running.store(false, Ordering::SeqCst);
    {
        let mut store = context.store.lock();
        if !matches!(store.status, AgentStatus::Failed(_)) {
            store.status = AgentStatus::Stopped;
        }
        store.permission = None;
        store.permission_tx = None;
    }
    context.notifier.notify();
}

fn run_agent_inner(
    context: &AgentThread,
    commands: &Receiver<AgentCommand>,
    config: AgentConfig,
    cwd: PathBuf,
    env: Vec<(String, String)>,
) -> Result<()> {
    let mut connection = ProtocolConnection::spawn(context, config, cwd, env)?;
    if !connection.initialize(commands)? {
        return Ok(());
    }
    if !connection.new_session(commands)? {
        return Ok(());
    }
    connection.run(commands)
}

struct ProtocolConnection {
    child: Child,
    stdin: Option<BufWriter<ChildStdin>>,
    incoming: Receiver<Incoming>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    next_id: u64,
    session_id: Option<String>,
    cwd: PathBuf,
    store: Arc<Mutex<Store>>,
    notifier: Arc<Notifier>,
    auto_approve: bool,
    pending_commands: VecDeque<AgentCommand>,
}

enum Incoming {
    Message(Value),
    Eof,
    Error(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Flow {
    Continue,
    Shutdown,
}

impl ProtocolConnection {
    fn spawn(
        context: &AgentThread,
        config: AgentConfig,
        cwd: PathBuf,
        env: Vec<(String, String)>,
    ) -> Result<Self> {
        let mut command = if cfg!(windows) {
            let mut command = Command::new("cmd");
            command.arg("/C").arg(&config.command);
            command
        } else {
            let mut command = Command::new("sh");
            command.arg("-c").arg(&config.command);
            command
        };

        let mut child = command
            .current_dir(&cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("NO_COLOR", "1")
            .envs(env)
            .spawn()
            .with_context(|| {
                format!("Failed to start ACP agent `{}`", config.command)
            })?;

        let stdin = BufWriter::new(
            child
                .stdin
                .take()
                .ok_or_else(|| anyhow!("Agent stdin is not available"))?,
        );
        let stdout = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| anyhow!("Agent stdout is not available"))?,
        );
        let stderr = BufReader::new(
            child
                .stderr
                .take()
                .ok_or_else(|| anyhow!("Agent stderr is not available"))?,
        );

        let (sender, receiver) = unbounded();
        thread::Builder::new()
            .name("lapce-agent-reader".to_string())
            .spawn(move || read_agent_output(stdout, sender))?;

        let stderr_tail = Arc::new(Mutex::new(VecDeque::new()));
        let tail = stderr_tail.clone();
        thread::Builder::new()
            .name("lapce-agent-stderr".to_string())
            .spawn(move || read_agent_stderr(stderr, tail))?;

        Ok(Self {
            child,
            stdin: Some(stdin),
            incoming: receiver,
            stderr_tail,
            next_id: 0,
            session_id: None,
            cwd,
            store: context.store.clone(),
            notifier: context.notifier.clone(),
            auto_approve: config.auto_approve,
            pending_commands: VecDeque::new(),
        })
    }

    fn initialize(&mut self, commands: &Receiver<AgentCommand>) -> Result<bool> {
        self.set_status(AgentStatus::Starting);

        let id = self.request(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {
                    "fs": {
                        "readTextFile": true,
                        "writeTextFile": true,
                    },
                    "terminal": false,
                    "elicitation": {
                        "form": {},
                    },
                },
                "clientInfo": {
                    "name": "Lapce",
                    "title": "Lapce",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            }),
        )?;

        let Some(response) = self.wait_for_response(id, Some(commands))? else {
            return Ok(false);
        };
        let result = response_result(response)?;
        let agent_name = result
            .get("agentInfo")
            .and_then(|info| info.get("title").or_else(|| info.get("name")))
            .and_then(Value::as_str)
            .map(str::to_string);

        let mut store = self.store.lock();
        if store.agent_name.as_deref().unwrap_or("").is_empty() {
            store.agent_name = agent_name;
        }
        drop(store);
        self.notifier.notify();
        Ok(true)
    }

    fn new_session(&mut self, commands: &Receiver<AgentCommand>) -> Result<bool> {
        let id = self.request(
            "session/new",
            json!({
                "cwd": self.cwd,
                "mcpServers": [],
            }),
        )?;

        let Some(response) = self.wait_for_response(id, Some(commands))? else {
            return Ok(false);
        };
        let result = response_result(response)?;
        let session_id = result
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Agent did not return a session id"))?
            .to_string();

        self.session_id = Some(session_id.clone());
        {
            let mut store = self.store.lock();
            store.acp_session_id = Some(session_id);
            store.models = parse_models(&result["models"]);
            store.modes = parse_modes(&result["modes"]);
            store.config_options = parse_config_options(&result["configOptions"]);
        }
        self.set_status(AgentStatus::Ready);
        Ok(true)
    }

    fn run(&mut self, commands: &Receiver<AgentCommand>) -> Result<()> {
        self.set_status(AgentStatus::Ready);
        loop {
            let command = match self.pending_commands.pop_front() {
                Some(command) => Some(command),
                None => commands.recv().ok(),
            };

            match command {
                Some(AgentCommand::Prompt(text)) => {
                    self.prompt(&text, commands)?;
                }
                Some(AgentCommand::Cancel) => {}
                Some(AgentCommand::NewSession) => {
                    if !self.new_session(commands)? {
                        return Ok(());
                    }
                }
                Some(AgentCommand::UseSession(session_id)) => {
                    self.session_id = session_id;
                }
                Some(AgentCommand::SetModel(model_id)) => {
                    self.send_set_model(&model_id)?;
                }
                Some(AgentCommand::SetMode(mode_id)) => {
                    self.send_set_mode(&mode_id)?;
                }
                Some(AgentCommand::SetConfigOption { config_id, value }) => {
                    self.send_set_config_option(&config_id, value)?;
                }
                Some(AgentCommand::Shutdown) | None => return Ok(()),
            }
        }
    }

    fn prompt(
        &mut self,
        text: &str,
        commands: &Receiver<AgentCommand>,
    ) -> Result<()> {
        let Some(session_id) = self.session_id.clone() else {
            self.store.lock().error("There is no active ACP session");
            self.notifier.notify();
            return Ok(());
        };

        {
            let mut store = self.store.lock();
            store.begin_turn();
        }
        self.notifier.notify();

        let id = self.request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{
                    "type": "text",
                    "text": text,
                }],
            }),
        )?;

        let incoming_rx = self.incoming.clone();
        let commands_rx = commands.clone();
        loop {
            select! {
                recv(incoming_rx) -> incoming => {
                    match incoming {
                        Ok(Incoming::Message(value)) => {
                            if is_response_for(&value, id) {
                                return self.finish_prompt(value);
                            }
                            if let Flow::Shutdown =
                                self.handle_incoming(value, Some(commands))?
                            {
                                return Ok(());
                            }
                        }
                        Ok(Incoming::Eof) => return Err(self.closed_error()),
                        Ok(Incoming::Error(error)) => bail!(error),
                        Err(_) => bail!("The agent connection closed"),
                    }
                }
                recv(commands_rx) -> command => {
                    match command {
                        Ok(AgentCommand::Cancel) => {
                            self.cancel_turn()?;
                        }
                        Ok(AgentCommand::SetModel(model_id)) => {
                            self.send_set_model(&model_id)?;
                        }
                        Ok(AgentCommand::SetMode(mode_id)) => {
                            self.send_set_mode(&mode_id)?;
                        }
                        Ok(AgentCommand::SetConfigOption { config_id, value }) => {
                            self.send_set_config_option(&config_id, value)?;
                        }
                        Ok(AgentCommand::Shutdown) | Err(_) => return Ok(()),
                        Ok(command) => {
                            // The UI guards against this, but keep any command
                            // that arrives during a turn for the next loop.
                            self.pending_commands.push_back(command);
                        }
                    }
                }
            }
        }
    }

    fn finish_prompt(&mut self, response: Value) -> Result<()> {
        match response_result(response) {
            Ok(result) => {
                if let Some(usage) = parse_usage(&result["usage"]) {
                    self.store.lock().usage = Some(usage);
                }
                let stop_reason = result
                    .get("stopReason")
                    .and_then(Value::as_str)
                    .unwrap_or("end_turn");
                if matches!(
                    stop_reason,
                    "refusal" | "max_tokens" | "max_turn_requests"
                ) {
                    let mut store = self.store.lock();
                    store.push_message(
                        AgentRole::Notice,
                        format!("The agent stopped: {stop_reason}"),
                    );
                }
            }
            Err(error) => {
                self.store.lock().error(format!("{error:#}"));
                self.notifier.notify();
                return Ok(());
            }
        }

        self.set_status(AgentStatus::Ready);
        Ok(())
    }

    fn cancel_turn(&mut self) -> Result<()> {
        self.set_status(AgentStatus::Stopping);
        if let Some(session_id) = self.session_id.clone() {
            self.write(json!({
                "jsonrpc": "2.0",
                "method": "session/cancel",
                "params": {
                    "sessionId": session_id,
                },
            }))?;
        }
        Ok(())
    }

    fn request(&mut self, method: &str, params: Value) -> Result<u64> {
        self.next_id += 1;
        let id = self.next_id;
        self.write(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))?;
        Ok(id)
    }

    fn send_set_model(&mut self, model_id: &str) -> Result<()> {
        let Some(session_id) = self.session_id.clone() else {
            return Ok(());
        };
        self.send_request_without_response(
            "session/set_model",
            json!({
                "sessionId": session_id,
                "modelId": model_id,
            }),
        )
    }

    fn send_set_mode(&mut self, mode_id: &str) -> Result<()> {
        let Some(session_id) = self.session_id.clone() else {
            return Ok(());
        };
        self.send_request_without_response(
            "session/set_mode",
            json!({
                "sessionId": session_id,
                "modeId": mode_id,
            }),
        )
    }

    fn send_set_config_option(
        &mut self,
        config_id: &str,
        value: AgentConfigValue,
    ) -> Result<()> {
        let Some(session_id) = self.session_id.clone() else {
            return Ok(());
        };

        let mut params = json!({
            "sessionId": session_id,
            "configId": config_id,
        });
        match value {
            AgentConfigValue::Select(value) => {
                params["value"] = Value::String(value);
            }
            AgentConfigValue::Boolean(value) => {
                params["type"] = Value::String("boolean".to_string());
                params["value"] = Value::Bool(value);
            }
        }
        self.send_request_without_response("session/set_config_option", params)
    }

    fn send_request_without_response(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<()> {
        self.next_id += 1;
        self.write(json!({
            "jsonrpc": "2.0",
            "id": self.next_id,
            "method": method,
            "params": params,
        }))
    }

    fn wait_for_response(
        &mut self,
        id: u64,
        commands: Option<&Receiver<AgentCommand>>,
    ) -> Result<Option<Value>> {
        let incoming_rx = self.incoming.clone();
        loop {
            let incoming = match commands {
                Some(commands) => {
                    let commands_rx = commands.clone();
                    select! {
                        recv(incoming_rx) -> incoming => incoming,
                        recv(commands_rx) -> command => {
                            match command {
                                Ok(AgentCommand::Shutdown) | Err(_) => {
                                    return Ok(None);
                                }
                                Ok(AgentCommand::Cancel) => {
                                    self.cancel_turn()?;
                                    continue;
                                }
                                Ok(command) => {
                                    self.pending_commands.push_back(command);
                                    continue;
                                }
                            }
                        }
                    }
                }
                None => incoming_rx.recv(),
            };

            match incoming {
                Ok(Incoming::Message(value)) => {
                    if is_response_for(&value, id) {
                        return Ok(Some(value));
                    }
                    if let Flow::Shutdown = self.handle_incoming(value, commands)? {
                        return Ok(None);
                    }
                }
                Ok(Incoming::Eof) => {
                    return Err(self.closed_error());
                }
                Ok(Incoming::Error(error)) => bail!(error),
                Err(_) => bail!("The agent connection closed"),
            }
        }
    }

    fn handle_incoming(
        &mut self,
        value: Value,
        commands: Option<&Receiver<AgentCommand>>,
    ) -> Result<Flow> {
        let Some(method) = value.get("method").and_then(Value::as_str) else {
            return Ok(Flow::Continue);
        };
        let method = method.to_string();

        if let Some(id) = value.get("id").cloned() {
            let params = value.get("params").cloned().unwrap_or(Value::Null);
            return self.handle_agent_request(id, &method, params, commands);
        }

        if method == "session/update" {
            let params = value.get("params").cloned().unwrap_or(Value::Null);
            self.handle_session_update(&params);
        }

        Ok(Flow::Continue)
    }

    fn handle_agent_request(
        &mut self,
        id: Value,
        method: &str,
        params: Value,
        commands: Option<&Receiver<AgentCommand>>,
    ) -> Result<Flow> {
        match method {
            "session/request_permission" => {
                self.handle_permission_request(id, &params, commands)
            }
            "elicitation/create" => {
                self.handle_elicitation_request(id, &params, commands)
            }
            "fs/read_text_file" => {
                self.handle_read_file(id, &params);
                Ok(Flow::Continue)
            }
            "fs/write_text_file" => {
                self.handle_write_file(id, &params);
                Ok(Flow::Continue)
            }
            _ => {
                self.write_error(
                    id,
                    -32601,
                    format!("Unsupported client method: {method}"),
                )?;
                Ok(Flow::Continue)
            }
        }
    }

    fn handle_read_file(&mut self, id: Value, params: &Value) {
        let path = params["path"].as_str().unwrap_or_default();
        let line = params["line"].as_u64();
        let limit = params["limit"].as_u64();

        let result = read_text_file(Path::new(path), line, limit);
        let response = match result {
            Ok(content) => self.write_response(id, json!({ "content": content })),
            Err(error) => self.write_error(id, -32000, error.to_string()),
        };

        if let Err(error) = response {
            tracing::warn!("Failed to answer fs/read_text_file: {error:#}");
        }
    }

    fn handle_write_file(&mut self, id: Value, params: &Value) {
        let path = params["path"].as_str().unwrap_or_default();
        let content = params["content"].as_str().unwrap_or_default();

        let result = write_text_file(Path::new(path), content);
        let response = match result {
            Ok(()) => self.write_response(id, json!({})),
            Err(error) => self.write_error(id, -32000, error.to_string()),
        };

        if let Err(error) = response {
            tracing::warn!("Failed to answer fs/write_text_file: {error:#}");
        }
    }

    fn handle_permission_request(
        &mut self,
        id: Value,
        params: &Value,
        commands: Option<&Receiver<AgentCommand>>,
    ) -> Result<Flow> {
        let options = params["options"]
            .as_array()
            .map(|options| {
                options
                    .iter()
                    .filter_map(|option| {
                        Some(PermissionOption {
                            option_id: option["optionId"].as_str()?.to_string(),
                            name: option["name"]
                                .as_str()
                                .unwrap_or("Continue")
                                .to_string(),
                            kind: option["kind"].as_str().unwrap_or("").to_string(),
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        if self.auto_approve {
            let option = options
                .iter()
                .find(|option| option.kind == "allow_once")
                .or_else(|| {
                    options.iter().find(|option| option.kind == "allow_always")
                })
                .map(|option| option.option_id.clone());
            self.respond_permission(id, option)?;
            return Ok(Flow::Continue);
        }

        let title = params["toolCall"]["title"]
            .as_str()
            .or_else(|| params["toolCall"]["rawInput"]["command"].as_str())
            .unwrap_or("The agent needs permission to continue")
            .to_string();

        let (sender, receiver) = bounded(1);
        {
            let mut store = self.store.lock();
            store.permission = Some(PendingPermission { title, options });
            store.permission_tx = Some(sender);
            store.status = AgentStatus::WaitingForPermission;
        }
        self.notifier.notify();

        let option = loop {
            let choice = match commands.cloned() {
                Some(commands) => select! {
                    recv(receiver) -> choice => Some(choice.map_err(|_| ())),
                    recv(commands) -> command => {
                        match command {
                            Ok(AgentCommand::Shutdown) | Err(_) => {
                                Some(Err(()))
                            }
                            Ok(AgentCommand::Cancel) => Some(Ok(None)),
                            Ok(AgentCommand::NewSession)
                            | Ok(AgentCommand::UseSession(_))
                            | Ok(AgentCommand::Prompt(_))
                            | Ok(AgentCommand::SetModel(_))
                            | Ok(AgentCommand::SetMode(_))
                            | Ok(AgentCommand::SetConfigOption { .. }) => None,
                        }
                    }
                },
                None => Some(receiver.recv().map_err(|_| ())),
            };

            let Some(choice) = choice else {
                continue;
            };

            match choice {
                Ok(option) => break option,
                Err(()) => {
                    // Shutdown while waiting for permission.
                    self.store.lock().permission = None;
                    self.notifier.notify();
                    self.respond_permission(id, None)?;
                    return Ok(Flow::Shutdown);
                }
            }
        };

        {
            let mut store = self.store.lock();
            store.permission = None;
            store.permission_tx = None;
            store.status = AgentStatus::Thinking;
        }
        self.notifier.notify();

        self.respond_permission(id, option)?;
        Ok(Flow::Continue)
    }

    fn handle_elicitation_request(
        &mut self,
        id: Value,
        params: &Value,
        commands: Option<&Receiver<AgentCommand>>,
    ) -> Result<Flow> {
        let mode = params["mode"].as_str().unwrap_or("form");
        if mode != "form" {
            self.write_response(id, json!({ "action": "decline" }))?;
            return Ok(Flow::Continue);
        }

        let message = params["message"]
            .as_str()
            .unwrap_or("The agent needs your input")
            .to_string();
        let fields = parse_question_fields(&params["requestedSchema"]);
        if fields.is_empty() {
            self.write_response(id, json!({ "action": "decline" }))?;
            return Ok(Flow::Continue);
        }

        let (sender, receiver) = bounded(1);
        {
            let mut store = self.store.lock();
            store.next_question_id += 1;
            let question_id = store.next_question_id;
            store.question = Some(AgentQuestion {
                id: question_id,
                message,
                fields,
            });
            store.question_tx = Some(sender);
            store.status = AgentStatus::WaitingForAnswer;
        }
        self.notifier.notify();

        let answer = loop {
            let choice = match commands.cloned() {
                Some(commands) => select! {
                    recv(receiver) -> answer => Some(answer.map_err(|_| ())),
                    recv(commands) -> command => {
                        match command {
                            Ok(AgentCommand::Shutdown) | Err(_) => Some(Err(())),
                            Ok(AgentCommand::Cancel) => {
                                self.cancel_turn()?;
                                Some(Ok(AgentQuestionAnswer::Cancel))
                            }
                            Ok(AgentCommand::SetModel(_))
                            | Ok(AgentCommand::SetMode(_))
                            | Ok(AgentCommand::SetConfigOption { .. })
                            | Ok(AgentCommand::NewSession)
                            | Ok(AgentCommand::UseSession(_))
                            | Ok(AgentCommand::Prompt(_)) => None,
                        }
                    }
                },
                None => Some(receiver.recv().map_err(|_| ())),
            };

            let Some(choice) = choice else {
                continue;
            };
            match choice {
                Ok(answer) => break answer,
                Err(()) => {
                    self.store.lock().question = None;
                    self.notifier.notify();
                    self.write_response(id, json!({ "action": "cancel" }))?;
                    return Ok(Flow::Shutdown);
                }
            }
        };

        {
            let mut store = self.store.lock();
            store.question = None;
            store.question_tx = None;
            store.status = AgentStatus::Thinking;
        }
        self.notifier.notify();

        let response = match answer {
            AgentQuestionAnswer::Accept(content) => json!({
                "action": "accept",
                "content": content,
            }),
            AgentQuestionAnswer::Decline => json!({ "action": "decline" }),
            AgentQuestionAnswer::Cancel => json!({ "action": "cancel" }),
        };
        self.write_response(id, response)?;
        Ok(Flow::Continue)
    }

    fn respond_permission(
        &mut self,
        id: Value,
        option_id: Option<String>,
    ) -> Result<()> {
        let outcome = match option_id {
            Some(option_id) => json!({
                "outcome": {
                    "outcome": "selected",
                    "optionId": option_id,
                },
            }),
            None => json!({
                "outcome": {
                    "outcome": "cancelled",
                },
            }),
        };
        self.write_response(id, outcome)
    }

    fn handle_session_update(&mut self, params: &Value) {
        let update = &params["update"];
        let Some(kind) = update["sessionUpdate"].as_str() else {
            return;
        };

        {
            let mut store = self.store.lock();
            match kind {
                "agent_message_chunk" => {
                    store.append_stream(
                        AgentRole::Assistant,
                        &content_text(&update["content"]),
                    );
                }
                "agent_thought_chunk" => {
                    store.append_stream(
                        AgentRole::Thought,
                        &content_text(&update["content"]),
                    );
                }
                "tool_call" => {
                    store.tool_call(
                        update["toolCallId"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        update["title"].as_str().unwrap_or("Tool call").to_string(),
                        update["kind"].as_str().unwrap_or("other").to_string(),
                        update["status"].as_str().unwrap_or("pending").to_string(),
                        render_tool_content(&update["content"]),
                    );
                }
                "tool_call_update" => {
                    let content = update
                        .get("content")
                        .map(render_tool_content)
                        .filter(|content| !content.is_empty())
                        .or_else(|| {
                            update["rawOutput"].as_str().map(str::to_string)
                        });
                    store.update_tool(
                        update["toolCallId"].as_str().unwrap_or_default(),
                        update["title"].as_str().map(str::to_string),
                        update["kind"].as_str().map(str::to_string),
                        update["status"].as_str().map(str::to_string),
                        content,
                    );
                }
                "plan" => {
                    if let Some(entries) = update["entries"].as_array() {
                        store.set_plan(entries);
                    }
                }
                "config_option_update" => {
                    store.config_options =
                        parse_config_options(&update["configOptions"]);
                }
                "current_mode_update" => {
                    if let Some(mode_id) = update["currentModeId"].as_str() {
                        if let Some(modes) = store.modes.as_mut() {
                            modes.current = mode_id.to_string();
                        }
                    }
                }
                "usage_update" => {
                    store.context_usage = Some(AgentContextUsage {
                        used: update["used"].as_u64().unwrap_or(0),
                        size: update["size"].as_u64().unwrap_or(0),
                        cost: parse_cost(&update["cost"]),
                    });
                }
                _ => {}
            }
        }

        self.notifier.notify();
    }

    fn set_status(&self, status: AgentStatus) {
        self.store.lock().status = status;
        self.notifier.notify();
    }

    fn write(&mut self, message: Value) -> Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("The agent stdin is closed"))?;
        serde_json::to_writer(&mut *stdin, &message)
            .context("Failed to serialize an ACP message")?;
        stdin
            .write_all(b"\n")
            .context("Failed to write to the agent")?;
        stdin.flush().context("Failed to flush the agent stdin")?;
        Ok(())
    }

    fn write_response(&mut self, id: Value, result: Value) -> Result<()> {
        self.write(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }))
    }

    fn write_error(
        &mut self,
        id: Value,
        code: i64,
        message: impl Into<String>,
    ) -> Result<()> {
        self.write(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": code,
                "message": message.into(),
            },
        }))
    }

    fn closed_error(&self) -> anyhow::Error {
        let tail = self
            .stderr_tail
            .lock()
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        if tail.is_empty() {
            anyhow!("The agent closed the connection")
        } else {
            anyhow!("The agent closed the connection\n{tail}")
        }
    }
}

impl Drop for ProtocolConnection {
    fn drop(&mut self) {
        // Closing stdin is the graceful way to ask an ACP agent to exit.
        self.stdin.take();
        for _ in 0..10 {
            if !matches!(self.child.try_wait(), Ok(None)) {
                return;
            }
            thread::sleep(std::time::Duration::from_millis(20));
        }
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn read_agent_output(
    stdout: BufReader<std::process::ChildStdout>,
    sender: Sender<Incoming>,
) {
    for line in stdout.lines() {
        match line {
            Ok(line) => {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(&line) {
                    Ok(value) => {
                        if sender.send(Incoming::Message(value)).is_err() {
                            return;
                        }
                    }
                    Err(error) => {
                        tracing::debug!(
                            "Ignoring non JSON output from ACP agent: {error}: {line}"
                        );
                    }
                }
            }
            Err(error) => {
                let _ = sender.send(Incoming::Error(error.to_string()));
                break;
            }
        }
    }
    let _ = sender.send(Incoming::Eof);
}

fn read_agent_stderr(
    stderr: BufReader<std::process::ChildStderr>,
    tail: Arc<Mutex<VecDeque<String>>>,
) {
    for line in stderr.lines() {
        match line {
            Ok(line) => {
                tracing::debug!(target: "lapce_agent", "{line}");
                let mut tail = tail.lock();
                if tail.len() >= 20 {
                    tail.pop_front();
                }
                tail.push_back(line);
            }
            Err(_) => break,
        }
    }
}

fn is_response_for(value: &Value, id: u64) -> bool {
    value.get("method").is_none()
        && value.get("id").and_then(Value::as_u64) == Some(id)
        && (value.get("result").is_some() || value.get("error").is_some())
}

fn response_result(response: Value) -> Result<Value> {
    if let Some(error) = response.get("error") {
        let message = error["message"]
            .as_str()
            .unwrap_or("The agent returned an error");
        let details = error
            .pointer("/data/details")
            .and_then(Value::as_str)
            .map(|details| format!(": {details}"))
            .unwrap_or_default();
        bail!("{message}{details}");
    }
    Ok(response.get("result").cloned().unwrap_or(Value::Null))
}

fn parse_usage(value: &Value) -> Option<AgentUsage> {
    if !value.is_object() {
        return None;
    }

    Some(AgentUsage {
        total_tokens: value["totalTokens"].as_u64().unwrap_or(0),
        input_tokens: value["inputTokens"].as_u64().unwrap_or(0),
        output_tokens: value["outputTokens"].as_u64().unwrap_or(0),
        thought_tokens: value["thoughtTokens"].as_u64(),
        cached_read_tokens: value["cachedReadTokens"].as_u64(),
        cached_write_tokens: value["cachedWriteTokens"].as_u64(),
    })
}

fn parse_cost(value: &Value) -> Option<AgentCost> {
    let object = value.as_object()?;
    Some(AgentCost {
        amount: object.get("amount")?.as_f64()?,
        currency: object
            .get("currency")
            .and_then(Value::as_str)
            .unwrap_or("USD")
            .to_string(),
    })
}

fn parse_choice_state(
    value: &Value,
    available_key: &str,
    current_key: &str,
    id_key: &str,
) -> Option<AgentChoiceState> {
    let object = value.as_object()?;
    let current = object.get(current_key)?.as_str()?.to_string();
    let available = object
        .get(available_key)?
        .as_array()?
        .iter()
        .filter_map(|item| {
            let id = item.get(id_key)?.as_str()?.to_string();
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(&id)
                .to_string();
            let description = item
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string);
            Some(AgentChoice {
                id,
                name,
                description,
            })
        })
        .collect();

    Some(AgentChoiceState { available, current })
}

fn parse_models(value: &Value) -> Option<AgentChoiceState> {
    parse_choice_state(value, "availableModels", "currentModelId", "modelId")
}

fn parse_modes(value: &Value) -> Option<AgentChoiceState> {
    parse_choice_state(value, "availableModes", "currentModeId", "id")
}

fn parse_config_options(value: &Value) -> Vec<AgentConfigOption> {
    value
        .as_array()
        .map(|options| options.iter().filter_map(parse_config_option).collect())
        .unwrap_or_default()
}

fn parse_config_option(value: &Value) -> Option<AgentConfigOption> {
    let id = value.get("id")?.as_str()?.to_string();
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(&id)
        .to_string();
    let description = value
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_string);
    let category = value
        .get("category")
        .and_then(Value::as_str)
        .map(str::to_string);

    let kind = match value.get("type").and_then(Value::as_str)? {
        "select" => AgentConfigOptionKind::Select {
            current: value
                .get("currentValue")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            options: parse_config_value_options(&value["options"]),
        },
        "boolean" => AgentConfigOptionKind::Boolean {
            current: value
                .get("currentValue")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        _ => return None,
    };

    Some(AgentConfigOption {
        id,
        name,
        description,
        category,
        kind,
    })
}

fn parse_config_value_options(value: &Value) -> Vec<AgentConfigValueOption> {
    let mut options = Vec::new();
    if let Some(items) = value.as_array() {
        for item in items {
            if let Some(group) = item.get("options").and_then(Value::as_array) {
                for option in group {
                    if let Some(option) = parse_config_value_option(option) {
                        options.push(option);
                    }
                }
            } else if let Some(option) = parse_config_value_option(item) {
                options.push(option);
            }
        }
    }
    options
}

fn parse_config_value_option(value: &Value) -> Option<AgentConfigValueOption> {
    let value_id = value.get("value")?.as_str()?.to_string();
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(&value_id)
        .to_string();
    let description = value
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(AgentConfigValueOption {
        value: value_id,
        name,
        description,
    })
}

fn parse_question_fields(schema: &Value) -> Vec<AgentQuestionField> {
    let Some(properties) = schema["properties"].as_object() else {
        return Vec::new();
    };
    let required: Vec<String> = schema["required"]
        .as_array()
        .map(|required| {
            required
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    properties
        .iter()
        .filter_map(|(name, property)| {
            parse_question_field(
                name,
                property,
                required.iter().any(|required| required == name),
            )
        })
        .collect()
}

fn parse_question_field(
    name: &str,
    property: &Value,
    required: bool,
) -> Option<AgentQuestionField> {
    let title = property["title"].as_str().unwrap_or(name).to_string();
    let description = property["description"].as_str().map(str::to_string);

    let kind = match property["type"].as_str()? {
        "string" => {
            let options = parse_question_options(&property["oneOf"])
                .or_else(|| parse_question_options(&property["enum"]))
                .unwrap_or_default();
            if options.is_empty() {
                AgentQuestionKind::Text {
                    default: property["default"].as_str().map(str::to_string),
                }
            } else {
                AgentQuestionKind::Select {
                    default: property["default"].as_str().map(str::to_string),
                    options,
                }
            }
        }
        "integer" => AgentQuestionKind::Integer {
            default: property["default"].as_i64(),
            minimum: property["minimum"].as_f64(),
            maximum: property["maximum"].as_f64(),
        },
        "number" => AgentQuestionKind::Number {
            default: property["default"].as_f64(),
            minimum: property["minimum"].as_f64(),
            maximum: property["maximum"].as_f64(),
        },
        "boolean" => AgentQuestionKind::Boolean {
            default: property["default"].as_bool().unwrap_or(false),
        },
        "array" => {
            let options = parse_question_options(&property["items"]["anyOf"])
                .or_else(|| parse_question_options(&property["items"]["enum"]))
                .unwrap_or_default();
            let default = property["default"]
                .as_array()
                .map(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            AgentQuestionKind::MultiSelect { default, options }
        }
        _ => return None,
    };

    Some(AgentQuestionField {
        name: name.to_string(),
        title,
        description,
        required,
        kind,
    })
}

fn parse_question_options(value: &Value) -> Option<Vec<AgentQuestionOption>> {
    let values = value.as_array()?;
    let options: Vec<AgentQuestionOption> = values
        .iter()
        .filter_map(|item| {
            if let Some(value) = item.as_str() {
                return Some(AgentQuestionOption {
                    value: value.to_string(),
                    title: value.to_string(),
                    description: None,
                });
            }

            let value = item["const"].as_str().or_else(|| item["value"].as_str())?;
            let title = item["title"]
                .as_str()
                .or_else(|| item["name"].as_str())
                .unwrap_or(value);
            Some(AgentQuestionOption {
                value: value.to_string(),
                title: title.to_string(),
                description: item["description"].as_str().map(str::to_string),
            })
        })
        .collect();

    if options.is_empty() {
        None
    } else {
        Some(options)
    }
}

fn truncate_tool_output(mut content: String) -> String {
    const MAX_TOOL_OUTPUT: usize = 32_000;
    if content.len() > MAX_TOOL_OUTPUT {
        let mut end = MAX_TOOL_OUTPUT;
        while !content.is_char_boundary(end) {
            end -= 1;
        }
        content.truncate(end);
        content.push_str("\n\n… output truncated");
    }
    content
}

fn content_text(content: &Value) -> String {
    match content["type"].as_str() {
        Some("text") => content["text"].as_str().unwrap_or_default().to_string(),
        Some("resource_link") => {
            let name = content["name"]
                .as_str()
                .or_else(|| content["uri"].as_str())
                .unwrap_or("resource");
            let uri = content["uri"].as_str().unwrap_or_default();
            format!("[{name}]({uri})")
        }
        _ => String::new(),
    }
}

fn render_tool_content(content: &Value) -> String {
    let Some(items) = content.as_array() else {
        return String::new();
    };

    let mut rendered = String::new();
    for item in items {
        match item["type"].as_str() {
            Some("content") => {
                let text = content_text(&item["content"]);
                if !text.is_empty() {
                    rendered.push_str(&text);
                    rendered.push('\n');
                }
            }
            Some("diff") => {
                let path = item["path"].as_str().unwrap_or_default();
                rendered.push_str(&format!("`{path}`\n"));
            }
            Some("terminal") => {
                if let Some(terminal_id) = item["terminalId"].as_str() {
                    rendered.push_str(&format!("terminal `{terminal_id}`\n"));
                }
            }
            _ => {}
        }
    }
    rendered
}

fn read_text_file(
    path: &Path,
    line: Option<u64>,
    limit: Option<u64>,
) -> Result<String> {
    const MAX_FILE_SIZE: u64 = 8 * 1024 * 1024;

    let metadata = std::fs::metadata(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    if metadata.len() > MAX_FILE_SIZE {
        bail!("{} is too large to read", path.display());
    }

    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;

    if line.is_none() && limit.is_none() {
        return Ok(content);
    }

    let start = line.unwrap_or(1).saturating_sub(1) as usize;
    let end = limit
        .map(|limit| start.saturating_add(limit as usize))
        .unwrap_or(usize::MAX);

    Ok(content
        .lines()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect::<Vec<_>>()
        .join("\n"))
}

fn write_text_file(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    std::fs::write(path, content)
        .with_context(|| format!("Failed to write {}", path.display()))
}
