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
        let claude = AgentProfile {
            name: "Claude Code".to_string(),
            command: "npx -y @zed-industries/claude-code-acp".to_string(),
        };
        let gemini = AgentProfile {
            name: "Gemini CLI".to_string(),
            command: "gemini --experimental-acp".to_string(),
        };
        Self {
            command: claude.command.clone(),
            auto_approve: false,
            active: claude.name.clone(),
            agents: vec![claude, gemini],
        }
    }
}
