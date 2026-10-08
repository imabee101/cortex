//! The name CortexBuild offers for every tool a script can call, and the CortexBuild shape of its
//! arguments. A case writes arguments once, and [`Tool::pick`] fills required fields a case omits,
//! reshapes a call written in another tool's vocabulary, and resolves it against the names offered.
use crate::inference_request::{HistoryToolCall, OfferedTools};
use serde_json::{Map, Value, json};
use std::fmt;
const MCP_NAME_SEPARATOR: &str = "__";
/// The meta tool the shell offers for MCP dispatch when tool search is on; the model names
/// the target in its `tool_name`/`tool_input` arguments rather than calling `server__tool` directly.
const USE_TOOL_NAME: &str = "use_tool";
/// The subagent spawn tool's name in each toolset: CortexBuild spells it `spawn_subagent`, the
/// daemon worker spells it `Task`. Single-sourced so the two spellings cannot drift.
pub const CORTEX_BUILD_SPAWN_TOOL: &str = "spawn_subagent";
pub const DAEMON_SPAWN_TOOL: &str = "Task";
/// The task id a task call gets when the case names none.
pub(crate) const FIRST_TASK_ID: &str = "1";
/// The interval a cron call gets when the case names none.
pub(crate) const CRON_DEFAULT_INTERVAL: &str = "1h";
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tool {
    Shell,
    Read,
    Edit,
    Write,
    Grep,
    Glob,
    List,
    /// Removes one file.
    /// Only the daemon worker's toolset offers it.
    Delete,
    MemorySearch,
    MemoryGet,
    Task,
    Skill,
    SendMessage,
    EnterPlanMode,
    ExitPlanMode,
    WebFetch,
    WebSearch,
    ImageGen,
    /// Makes a video from reference images or voices.
    /// The daemon worker's toolset has no video tool.
    ReferenceToVideo,
    Question,
    Todo,
    SearchTool,
    KillTask,
    /// Blocks on a background command's output.
    TaskOutput,
    Monitor,
    SchedulerCreate,
    SchedulerList,
    SchedulerDelete,
    Workflow,
    Lsp,
    /// A created task in the cases' vocabulary; becomes one todo write.
    TaskCreate,
    /// A task update in the cases' vocabulary; also one todo write.
    TaskUpdate,
    /// A scheduled wakeup in the cases' vocabulary; becomes a scheduler create, defaulting the interval.
    CronCreate,
    McpListResources,
    McpReadResource,
    /// CortexBuild offers `server__tool` taking the arguments directly.
    Mcp {
        server: String,
        tool: String,
    },
}
impl fmt::Display for Tool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Tool::Shell
            | Tool::Read
            | Tool::Edit
            | Tool::Write
            | Tool::Grep
            | Tool::Glob
            | Tool::List
            | Tool::Delete
            | Tool::MemorySearch
            | Tool::MemoryGet
            | Tool::Task
            | Tool::Skill
            | Tool::SendMessage
            | Tool::EnterPlanMode
            | Tool::ExitPlanMode
            | Tool::WebFetch
            | Tool::WebSearch
            | Tool::ImageGen
            | Tool::ReferenceToVideo
            | Tool::Question
            | Tool::Todo
            | Tool::SearchTool
            | Tool::KillTask
            | Tool::TaskOutput
            | Tool::Monitor
            | Tool::SchedulerCreate
            | Tool::SchedulerList
            | Tool::SchedulerDelete
            | Tool::Workflow
            | Tool::Lsp
            | Tool::TaskCreate
            | Tool::TaskUpdate
            | Tool::CronCreate
            | Tool::McpListResources
            | Tool::McpReadResource => write!(f, "{self:?}"),
            Tool::Mcp { server, tool } => write!(f, "{tool} on MCP server {server}"),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PickedToolCall {
    pub(crate) name: String,
    pub(crate) arguments: Value,
}
/// A field the tool requires that a case may leave out, filled from another field of the call.
#[derive(Clone, Copy)]
struct FieldFill {
    field: &'static str,
    source: &'static str,
}
/// A reshaping of a call's arguments a table of renames cannot express.
type Shape = fn(Map<String, Value>) -> Map<String, Value>;
struct CortexBuildRow {
    /// `None` for the tools CortexBuild does not offer (Glob, delete, and the MCP resource kinds) and
    /// for an MCP call. `glob` is an OpenCode tool; CortexBuild lists directories with `list_dir`.
    /// `cortex_build_name` builds an MCP call's name from its server and tool.
    name: Option<&'static str>,
    fills: &'static [FieldFill],
    shape: Option<Shape>,
}
impl CortexBuildRow {
    const fn new(name: &'static str) -> Self {
        CortexBuildRow {
            name: Some(name),
            fills: &[],
            shape: None,
        }
    }
    const fn without_cortex_build_name() -> Self {
        CortexBuildRow {
            name: None,
            fills: &[],
            shape: None,
        }
    }
    const fn with_fills(mut self, fills: &'static [FieldFill]) -> Self {
        self.fills = fills;
        self
    }
    const fn with_shape(mut self, shape: Shape) -> Self {
        self.shape = Some(shape);
        self
    }
}
/// Every tool with a fixed name, in declaration order.
/// `Mcp` is the only variant left out.
const NAMED_TOOLS: &[Tool] = &[
    Tool::Shell,
    Tool::Read,
    Tool::Edit,
    Tool::Write,
    Tool::Grep,
    Tool::Glob,
    Tool::List,
    Tool::Delete,
    Tool::MemorySearch,
    Tool::MemoryGet,
    Tool::Task,
    Tool::Skill,
    Tool::SendMessage,
    Tool::EnterPlanMode,
    Tool::ExitPlanMode,
    Tool::WebFetch,
    Tool::WebSearch,
    Tool::ImageGen,
    Tool::ReferenceToVideo,
    Tool::Question,
    Tool::Todo,
    Tool::SearchTool,
    Tool::KillTask,
    Tool::TaskOutput,
    Tool::Monitor,
    Tool::SchedulerCreate,
    Tool::SchedulerList,
    Tool::SchedulerDelete,
    Tool::Workflow,
    Tool::Lsp,
    Tool::TaskCreate,
    Tool::TaskUpdate,
    Tool::CronCreate,
    Tool::McpListResources,
    Tool::McpReadResource,
];
impl Tool {
    pub fn mcp(server: impl Into<String>, tool: impl Into<String>) -> Self {
        Tool::Mcp {
            server: server.into(),
            tool: tool.into(),
        }
    }
    fn row(&self) -> CortexBuildRow {
        match self {
            Tool::Shell => CortexBuildRow::new("run_terminal_command").with_fills(&[FieldFill {
                field: "description",
                source: "command",
            }]),
            Tool::Read => CortexBuildRow::new("read_file"),
            Tool::Edit => CortexBuildRow::new("search_replace"),
            Tool::Write => CortexBuildRow::new("write"),
            Tool::Grep => CortexBuildRow::new("grep"),
            Tool::List => CortexBuildRow::new("list_dir"),
            Tool::MemorySearch => CortexBuildRow::new("memory_search"),
            Tool::MemoryGet => CortexBuildRow::new("memory_get"),
            Tool::Task => CortexBuildRow::new(CORTEX_BUILD_SPAWN_TOOL).with_fills(&[FieldFill {
                field: "description",
                source: "prompt",
            }]),
            Tool::Skill => CortexBuildRow::new("skill"),
            Tool::SendMessage => CortexBuildRow::new("send_subagent_message"),
            Tool::EnterPlanMode => CortexBuildRow::new("enter_plan_mode"),
            Tool::ExitPlanMode => CortexBuildRow::new("exit_plan_mode"),
            Tool::WebFetch => CortexBuildRow::new("web_fetch"),
            Tool::WebSearch => CortexBuildRow::new("web_search"),
            Tool::ImageGen => CortexBuildRow::new("image_gen"),
            Tool::ReferenceToVideo => CortexBuildRow::new("reference_to_video"),
            Tool::Question => CortexBuildRow::new("ask_user_question").with_shape(describe_options),
            Tool::Todo => CortexBuildRow::new("todo_write").with_shape(default_todos_to_pending),
            Tool::SearchTool => CortexBuildRow::new("search_tool"),
            Tool::KillTask => CortexBuildRow::new("kill_command_or_subagent"),
            Tool::TaskOutput => CortexBuildRow::new("get_command_or_subagent_output"),
            Tool::Monitor => CortexBuildRow::new("monitor"),
            Tool::SchedulerCreate => CortexBuildRow::new("scheduler_create"),
            Tool::SchedulerList => CortexBuildRow::new("scheduler_list"),
            Tool::SchedulerDelete => CortexBuildRow::new("scheduler_delete"),
            Tool::Workflow => CortexBuildRow::new("workflow"),
            Tool::Lsp => CortexBuildRow::new("lsp"),
            Tool::TaskCreate => {
                CortexBuildRow::new("todo_write").with_shape(todo_write_from_created_task)
            }
            Tool::TaskUpdate => CortexBuildRow::new("todo_write").with_shape(todo_write_from_task),
            Tool::CronCreate => {
                CortexBuildRow::new("scheduler_create").with_shape(fill_cron_interval)
            }
            Tool::Glob
            | Tool::Delete
            | Tool::McpListResources
            | Tool::McpReadResource
            | Tool::Mcp { .. } => CortexBuildRow::without_cortex_build_name(),
        }
    }
    /// An MCP tool is named `server__tool`.
    /// Glob, Delete, and the MCP resource kinds have no CortexBuild name.
    fn cortex_build_name(&self) -> Option<String> {
        if let Tool::Mcp { server, tool } = self {
            return Some(format!("{server}{MCP_NAME_SEPARATOR}{tool}"));
        }
        self.row().name.map(str::to_owned)
    }
    /// The built-in tool CortexBuild offers under `name`, or `None` if there is none.
    /// Tools that share a CortexBuild name, such as `Todo` and `TaskCreate`, also share a daemon
    /// name.
    #[must_use]
    pub fn from_cortex_build_name(name: &str) -> Option<Tool> {
        if let Some(tool) = NAMED_TOOLS
            .iter()
            .find(|tool| tool.cortex_build_name().as_deref() == Some(name))
            .cloned()
        {
            return Some(tool);
        }
        let (server, tool) = name.split_once(MCP_NAME_SEPARATOR)?;
        if server.is_empty() || tool.is_empty() {
            return None;
        }
        Some(Tool::Mcp {
            server: server.to_owned(),
            tool: tool.to_owned(),
        })
    }
    /// The name the daemon worker's toolset offers this tool under, or `None` when that toolset
    /// does not offer it.
    #[must_use]
    pub fn daemon_offered_name(&self) -> Option<&'static str> {
        None
    }
    /// Daemon route uses the worker toolset name when it differs; otherwise the CortexBuild name.
    #[must_use]
    pub fn wire_name(&self, daemon: bool) -> Option<String> {
        if daemon && let Some(name) = self.daemon_offered_name() {
            return Some(name.to_owned());
        }
        self.cortex_build_name()
    }
    /// The case's arguments in CortexBuild's shape: the fields a case may leave out are filled, and
    /// the row's shape applies. Arguments that are not a table pass through unchanged.
    fn cortex_build_arguments(&self, arguments: &Value) -> Value {
        let Some(fields) = arguments.as_object() else {
            return arguments.clone();
        };
        let row = self.row();
        let mut shaped = fields.clone();
        for fill in row.fills {
            if let Some(value) = shaped.get(fill.source).cloned() {
                shaped.entry(fill.field).or_insert(value);
            }
        }
        Value::Object(match row.shape {
            Some(shape) => shape(shaped),
            None => shaped,
        })
    }
    /// The call for CortexBuild's name when the request offers it.
    pub(crate) fn pick(&self, offered: &OfferedTools, arguments: &Value) -> Option<PickedToolCall> {
        let arguments = self.cortex_build_arguments(arguments);
        if let Some(name) = self.cortex_build_name().filter(|name| offered.has_tool(name)) {
            return Some(PickedToolCall { name, arguments });
        }
        if let Tool::Mcp { .. } = self
            && offered.has_tool(USE_TOOL_NAME)
            && let Some(name) = self.cortex_build_name()
        {
            return Some(PickedToolCall {
                name: USE_TOOL_NAME.to_owned(),
                arguments: json!({
                    "tool_name": name,
                    "tool_input": arguments,
                }),
            });
        }
        None
    }
    /// Whether a call the agent carried back in its history is this tool's under CortexBuild's name.
    pub(crate) fn is_called_by(&self, call: &HistoryToolCall) -> bool {
        if self.cortex_build_name().is_some_and(|name| call.name == name) {
            return true;
        }
        if let Tool::Mcp { .. } = self
            && call.name == USE_TOOL_NAME
            && call.arguments.get("tool_name").and_then(Value::as_str)
                == self.cortex_build_name().as_deref()
        {
            return true;
        }
        false
    }
}
/// Every question option gets its `label` as the description the tool requires.
fn describe_options(mut fields: Map<String, Value>) -> Map<String, Value> {
    for option in fields
        .get_mut("questions")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
        .filter_map(|question| question.get_mut("options"))
        .filter_map(Value::as_array_mut)
        .flatten()
        .filter_map(Value::as_object_mut)
    {
        if let Some(label) = option.get("label").cloned() {
            option.entry("description").or_insert(label);
        }
    }
    fields
}
/// Every todo item without a `status` is pending.
fn default_todos_to_pending(mut fields: Map<String, Value>) -> Map<String, Value> {
    for todo in fields
        .get_mut("todos")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object_mut)
    {
        todo.entry("status").or_insert_with(|| json!("pending"));
    }
    fields
}
/// A created task is pending unless the case says otherwise.
fn todo_write_from_created_task(mut task: Map<String, Value>) -> Map<String, Value> {
    task.entry("status").or_insert_with(|| json!("pending"));
    todo_write_from_task(task)
}
/// One todo write from a task call, with the `subject` carried over as the todo content.
fn todo_write_from_task(task: Map<String, Value>) -> Map<String, Value> {
    let mut todo = Map::from_iter([(
        "id".to_owned(),
        task.get("id")
            .or_else(|| task.get("taskId"))
            .cloned()
            .unwrap_or_else(|| json!(FIRST_TASK_ID)),
    )]);
    if let Some(subject) = task.get("subject") {
        todo.insert("content".to_owned(), subject.clone());
    }
    if let Some(status) = task.get("status") {
        todo.insert("status".to_owned(), status.clone());
    }
    Map::from_iter([("todos".to_owned(), json!([todo]))])
}
/// The scheduler takes an `interval` and no cron expression; the `schedule` stays as written.
fn fill_cron_interval(mut fields: Map<String, Value>) -> Map<String, Value> {
    fields
        .entry("interval")
        .or_insert_with(|| json!(CRON_DEFAULT_INTERVAL));
    fields
}
#[cfg(test)]
#[path = "tools_tests.rs"]
mod tests;
