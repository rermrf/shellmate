//! Explicit terminal references; names are display metadata, never execution addresses.
use rig_core::{
    completion::ToolDefinition,
    message::ToolName,
    serde::{Deserialize, Serialize},
};
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "rig_core::serde")]
pub struct Destination {
    pub session_id: String,
    pub label: String,
    pub protocol: String,
    pub host_id: Option<String>,
    pub cwd: Option<String>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(crate = "rig_core::serde", deny_unknown_fields)]
pub struct List {
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub offset: usize,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(crate = "rig_core::serde", deny_unknown_fields)]
pub struct Read {
    pub session_id: String,
    #[serde(default = "lines")]
    pub lines: usize,
}
fn lines() -> usize {
    40
}
#[derive(Clone, Debug)]
pub enum Request {
    List(List),
    Read(Read),
    Compare(Compare),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(crate = "rig_core::serde", deny_unknown_fields)]
pub struct Side {
    pub session_id: String,
    pub path: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(crate = "rig_core::serde", deny_unknown_fields)]
pub struct Compare {
    pub left: Side,
    pub right: Side,
    #[serde(default)]
    pub metadata_only: bool,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub offset: usize,
}
pub fn parse(name: &str, value: serde_json::Value) -> Result<Request, String> {
    if name == "compare_directories" {
        let a: Compare = serde_json::from_value(value)
            .map_err(|_| "目录比较需要 left/right 的 session_id 和 path")?;
        if a.left.path.is_empty()
            || a.right.path.is_empty()
            || a.exclude.len() > 100
            || a.offset > 1_000_000
        {
            return Err("目录比较参数超出范围".into());
        }
        return Ok(Request::Compare(a));
    }
    if name == "list_sessions" {
        let a: List = serde_json::from_value(value).map_err(|_| "终端列表参数无效")?;
        if a.query.len() > 2000 || a.offset > 10000 {
            return Err("终端列表查询超出范围".into());
        }
        Ok(Request::List(a))
    } else {
        let a: Read = serde_json::from_value(value).map_err(|_| "读取终端参数无效")?;
        if a.session_id.is_empty() || a.session_id.len() > 160 || !(1..=500).contains(&a.lines) {
            return Err("需要有效 session_id，读取行数为 1–500".into());
        }
        Ok(Request::Read(a))
    }
}
pub fn tools() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::new(
            ToolName::new("compare_directories").unwrap(),
            "只读比较两个已选终端中的目录，默认核实内容。不会同步或修改文件，不跟随软链接。返回差异和完整性；用返回的 cursor/offset 分页或继续。metadata_only 仅比较元数据，不能据此声称内容相同。exclude 为根目录相对路径前缀。",
            serde_json::json!({"type":"object","properties":{"left":{"type":"object","properties":{"session_id":{"type":"string"},"path":{"type":"string"}},"required":["session_id","path"],"additionalProperties":false},"right":{"type":"object","properties":{"session_id":{"type":"string"},"path":{"type":"string"}},"required":["session_id","path"],"additionalProperties":false},"metadata_only":{"type":"boolean"},"exclude":{"type":"array","items":{"type":"string"}},"cursor":{"type":"string"},"offset":{"type":"integer","minimum":0}},"required":["left","right"],"additionalProperties":false}),
        ),
        ToolDefinition::new(
            ToolName::new("list_sessions").unwrap(),
            "列出 Shellmate 已打开的终端。返回 session_id、名称、连接状态、目录和是否属于任务范围。名称可能重复，调用使用 session_id，不使用列表序号。不自动连接主机。",
            serde_json::json!({"type":"object","properties":{"query":{"type":"string"},"offset":{"type":"integer","minimum":0}},"additionalProperties":false}),
        ),
        ToolDefinition::new(
            ToolName::new("read_terminal").unwrap(),
            "读取指定终端最近输出与目录快照。先用 list_sessions 查找准确目标；只读取用户指定或已加入本轮范围的终端。",
            serde_json::json!({"type":"object","properties":{"session_id":{"type":"string"},"lines":{"type":"integer","minimum":1,"maximum":500,"default":40}},"required":["session_id"],"additionalProperties":false}),
        ),
    ]
}
