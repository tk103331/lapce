use serde::{Deserialize, Serialize};
use structdesc::FieldNames;

/// Configuration for the ACP (Agent Client Protocol) agent chat panel.
#[derive(FieldNames, Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct AgentConfig {
    #[field_names(
        desc = "The fallback command used when no agent profiles are configured"
    )]
    pub command: String,

    #[field_names(
        desc = "Automatically approve permission requests from the agent"
    )]
    pub auto_approve: bool,

    /// Name of the profile selected in the panel.
    #[field_names(skip)]
    #[serde(default)]
    pub active: String,

    /// Named ACP agent profiles.
    #[field_names(skip)]
    #[serde(default)]
    pub agents: Vec<AgentProfile>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct AgentProfile {
    /// Name shown in the agent picker.
    pub name: String,
    /// Command used to start the agent, including any arguments.
    pub command: String,
    /// Extra environment variables passed to the agent, as `KEY=VALUE`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    /// Whether the agent passed detection and may be used for new sessions.
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
}

fn enabled_by_default() -> bool {
    true
}

impl AgentConfig {
    pub fn profiles(&self) -> Vec<AgentProfile> {
        if self.agents.is_empty() {
            let name = if self.active.is_empty() {
                "Default".to_string()
            } else {
                self.active.clone()
            };
            vec![AgentProfile {
                name,
                command: self.command.clone(),
                env: Vec::new(),
                enabled: true,
            }]
        } else {
            self.agents.clone()
        }
    }

    pub fn active_profile(&self) -> AgentProfile {
        let profiles = self.profiles();
        profiles
            .iter()
            .find(|profile| profile.name == self.active)
            .cloned()
            .or_else(|| profiles.first().cloned())
            .unwrap_or_default()
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        let agents = vec![
            AgentProfile {
                name: "Claude Code".to_string(),
                command: "npx -y @zed-industries/claude-code-acp".to_string(),
                env: Vec::new(),
                enabled: true,
            },
            AgentProfile {
                name: "Codex CLI".to_string(),
                command: "npx -y @zed-industries/codex-acp".to_string(),
                env: Vec::new(),
                enabled: true,
            },
            AgentProfile {
                name: "OpenCode".to_string(),
                command: "opencode acp".to_string(),
                env: Vec::new(),
                enabled: true,
            },
            AgentProfile {
                name: "Pi Agent".to_string(),
                command: "npx -y pi-acp".to_string(),
                env: Vec::new(),
                enabled: true,
            },
            AgentProfile {
                name: "Gemini CLI".to_string(),
                command: "gemini --experimental-acp".to_string(),
                env: Vec::new(),
                enabled: true,
            },
        ];
        Self {
            command: agents[0].command.clone(),
            auto_approve: false,
            active: agents[0].name.clone(),
            agents,
        }
    }
}
