//! A small client for the [Agent Client Protocol](https://agentclientprotocol.com/).
//!
//! Lapce starts an ACP compatible agent as a child process and exchanges
//! newline-delimited JSON-RPC 2.0 messages over its stdin/stdout. The agent
//! handles `session/update` notifications, permission requests and file
//! requests while the UI only has to talk to [`AgentData`].

use std::{
    collections::{HashMap, VecDeque},
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
    reactive::{ReadSignal, RwSignal, Scope, SignalGet, SignalUpdate, SignalWith},
};
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::{
    config::agent::AgentConfig, editor::EditorData, main_split::Editors,
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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum AgentStatus {
    #[default]
    Idle,
    Starting,
    Ready,
    Thinking,
    WaitingForPermission,
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
    Shutdown,
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
    plan_message: Option<u64>,
    tool_messages: HashMap<String, u64>,
    restart_requested: bool,
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
        self.plan_message = None;
        self.tool_messages.clear();
        self.permission = None;
        self.permission_tx = None;
        self.status = AgentStatus::Thinking;
    }

    fn clear(&mut self) {
        self.messages.clear();
        self.tool_messages.clear();
        self.assistant_message = None;
        self.thought_message = None;
        self.plan_message = None;
        self.permission = None;
        self.permission_tx = None;
        self.status = AgentStatus::Idle;
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
        let mut content = String::from("**Plan**\n");
        for entry in entries {
            let status = entry["status"].as_str().unwrap_or("pending");
            let marker = match status {
                "completed" => "✓",
                "in_progress" => "→",
                _ => "○",
            };
            content.push_str(&format!(
                "{marker} {}\n",
                entry["content"].as_str().unwrap_or_default()
            ));
        }

        if let Some(message_id) = self.plan_message {
            if let Some(message) =
                self.messages.iter_mut().find(|m| m.id == message_id)
            {
                message.content = content;
                return;
            }
        }

        let message_id = self.push_message(AgentRole::Plan, content);
        self.plan_message = Some(message_id);
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
    store: Arc<Mutex<Store>>,
    notifier: Arc<Notifier>,
    sender: Arc<Mutex<Option<Sender<AgentCommand>>>>,
    running: Arc<AtomicBool>,
    epoch: Arc<AtomicU64>,
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

        let data = Self {
            common,
            input,
            messages: cx.create_rw_signal(im::Vector::new()),
            status: cx.create_rw_signal(AgentStatus::Idle),
            agent_name: cx.create_rw_signal(None),
            permission: cx.create_rw_signal(None),
            revision,
            auto_scroll: cx.create_rw_signal(true),
            store,
            notifier,
            sender: Arc::new(Mutex::new(None)),
            running: Arc::new(AtomicBool::new(false)),
            epoch: Arc::new(AtomicU64::new(0)),
        };

        let data_for_effect = data.clone();
        cx.create_effect(move |_| {
            if data_for_effect.revision.get().is_some() {
                data_for_effect.sync();
            }
        });

        data
    }

    fn sync(&self) {
        let (messages, status, agent_name, permission, restart) = {
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
            if store.status != AgentStatus::WaitingForPermission {
                store.status = AgentStatus::Stopping;
            }
        }
        self.notifier.notify();

        if let Some(sender) = self.sender.lock().as_ref() {
            let _ = sender.send(AgentCommand::Cancel);
        }
    }

    /// Start a fresh ACP session, keeping the agent process alive.
    pub fn new_session(&self) {
        if self.status.get_untracked().is_busy() {
            return;
        }

        {
            let mut store = self.store.lock();
            store.clear();
            store.status = AgentStatus::Starting;
        }
        self.auto_scroll.set(true);
        self.notifier.notify();

        if self.running.load(Ordering::SeqCst) {
            if let Some(sender) = self.sender.lock().as_ref() {
                let _ = sender.send(AgentCommand::NewSession);
            }
        } else {
            self.ensure_started();
        }
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

        let config = self.common.config.get_untracked().agent.clone();
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
            .spawn(move || run_agent(context, receiver, config, cwd, epoch));

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
    epoch: u64,
) {
    let result = run_agent_inner(&context, &commands, config, cwd);
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
) -> Result<()> {
    let mut connection = ProtocolConnection::spawn(context, config, cwd)?;
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

        self.store.lock().agent_name = agent_name;
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

        self.session_id = Some(session_id);
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
