use serde::{Deserialize, Serialize};
use structdesc::FieldNames;

/// Configuration for the ACP (Agent Client Protocol) agent chat panel.
#[derive(FieldNames, Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct AgentConfig {
    #[field_names(
        desc = "The command used to start an ACP compatible agent over stdio"
    )]
    pub command: String,

    #[field_names(
        desc = "Automatically approve permission requests from the agent"
    )]
    pub auto_approve: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            command: "npx -y @zed-industries/claude-code-acp".to_string(),
            auto_approve: false,
        }
    }
}
