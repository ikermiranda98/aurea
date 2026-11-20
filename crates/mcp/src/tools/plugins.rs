//! What remains of the plugin tools in Aurea: the shape `tools/list` sends.
//! Aurea has no Luau plugins, so there are no dynamic plugin tools and the
//! listing is the static catalog alone.

use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc;

use super::{ToolDef, Tools};

/// A tool definition with an owned name and description: the shape
/// `tools/list` sends.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListedTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

impl From<ToolDef> for ListedTool {
    fn from(def: ToolDef) -> Self {
        Self {
            name: def.name.to_owned(),
            description: def.description.to_owned(),
            input_schema: def.input_schema,
        }
    }
}

/// No plugin tools in Aurea.
pub(crate) fn is_plugin_tool_name(_name: &str) -> bool {
    false
}

impl Tools {
    /// The full `tools/list`: the static catalog.
    pub async fn list_all(&self) -> Vec<ListedTool> {
        self.list().into_iter().map(ListedTool::from).collect()
    }

    /// The plugin tool list never changes in Aurea: returns at once, as an
    /// engine without plugins did.
    pub async fn watch_plugin_tools(&self, _changed: mpsc::Sender<()>) {}
}
