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
    #[serde(default)]
    pub screen: bool,
    pub region: Option<shellmate_core::terminal_interaction::Region>,
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
    Input(Input),
    Wait(Wait),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(crate = "rig_core::serde", deny_unknown_fields)]
pub struct Input {
    pub session_id: String,
    #[serde(default)]
    pub screen_id: String,
    #[serde(default)]
    pub actions: Vec<shellmate_core::terminal_interaction::Action>,
    #[serde(default)]
    pub release: bool,
    pub purpose: String,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(crate = "rig_core::serde", deny_unknown_fields)]
pub struct Wait {
    pub session_id: String,
    pub region: Option<shellmate_core::terminal_interaction::Region>,
    pub text: Option<String>,
    pub fingerprint: Option<String>,
    pub stable_ms: Option<u64>,
    #[serde(default = "wait_timeout")]
    pub timeout_ms: u64,
}
fn wait_timeout() -> u64 {
    3000
}
pub fn parse(name: &str, value: serde_json::Value) -> Result<Request, String> {
    if name == "terminal_input" {
        let input: Input = serde_json::from_value(value).map_err(|_| "终端交互参数无效")?;
        if input.session_id.is_empty()
            || input.purpose.trim().is_empty()
            || input.purpose.len() > 2000
        {
            return Err("需要指定终端和操作目的".into());
        }
        if input.release {
            if !input.actions.is_empty() {
                return Err("释放控制不能同时发送按键".into());
            }
        } else {
            if input.screen_id.is_empty() {
                return Err("先读取 screen=true 的当前画面，再提供 screen_id".into());
            }
            shellmate_core::terminal_interaction::validate_actions(&input.actions)?;
        }
        return Ok(Request::Input(input));
    }
    if name == "wait_terminal" {
        let wait: Wait = serde_json::from_value(value).map_err(|_| "等待终端参数无效")?;
        if wait.session_id.is_empty()
            || wait.timeout_ms == 0
            || wait.timeout_ms > 10000
            || wait.stable_ms.is_some_and(|n| !(100..=3000).contains(&n))
            || wait
                .text
                .as_ref()
                .is_some_and(|s| s.is_empty() || s.len() > 2000)
        {
            return Err("等待超出范围，timeout_ms 为 1–10000，stable_ms 为 100–3000".into());
        }
        if wait.text.is_none() && wait.fingerprint.is_none() && wait.stable_ms.is_none() {
            return Err("请指定 text、fingerprint 或 stable_ms 中的一种等待条件".into());
        }
        return Ok(Request::Wait(wait));
    }
    if name == "list_sessions" {
        let a: List = serde_json::from_value(value).map_err(|_| "终端列表参数无效")?;
        if a.query.len() > 2000 || a.offset > 10000 {
            return Err("终端列表查询超出范围".into());
        }
        Ok(Request::List(a))
    } else if name == "read_terminal" {
        let a: Read = serde_json::from_value(value).map_err(|_| "读取终端参数无效")?;
        if a.session_id.is_empty() || a.session_id.len() > 160 || !(1..=500).contains(&a.lines) {
            return Err("需要有效 session_id，读取行数为 1–500".into());
        }
        Ok(Request::Read(a))
    } else {
        Err("未知终端工具".into())
    }
}
pub const RESOURCE_RULES: &str = "\n应用内已打开的终端是主代理和子代理共同可用的资源，无需加入操作范围。先用 list_sessions 发现目标，结合用户要求、名称/目录和上下文选择 session_id；名称缩写或第 1、第 2 个由你理解，不要求用户输入完整标题或内部引用。序号对应对话中用户指向的那次列表，保持当时 session_id 映射，重排/重连后不可按新位置偷偷替换。用户说每个终端时，以该次任务列举的列表为准，不自动纳入后来新开的标签。只有真正歧义才询问。默认终端只是未指定目标时的起点，不代表当前焦点；工具调用目标始终明确，连接失效返回后不得回退其他终端。资源可用不等于任意操作均获授权，遵守用户的任务目的和只操作某些终端等明确约束；按目标执行现有命令审批，不额外请求加入终端。\n";
pub fn tools() -> Vec<ToolDefinition> {
    vec![
        input_tool(),
        wait_tool(),
        ToolDefinition::new(
            ToolName::new("list_sessions").unwrap(),
            "列出 Shellmate 所有窗口已打开的终端资源，主/子代理均可根据任务直接使用。返回本次列表 number、稳定 session_id、名称、连接状态、目录与执行通道能力。用户引用第几个时将对应列表的 number 映射为 session_id；调用必须传引用，不传序号或名称。default 是对话默认终端，不是当前标签。不自动连接主机。",
            serde_json::json!({"type":"object","properties":{"query":{"type":"string"},"offset":{"type":"integer","minimum":0}},"additionalProperties":false}),
        ),
        ToolDefinition::new(
            ToolName::new("read_terminal").unwrap(),
            "读取指定终端最近输出与目录快照。先用 list_sessions 查找准确目标，根据用户任务选择；无需加入操作范围。",
            serde_json::json!({"type":"object","properties":{"screen":{"type":"boolean","default":false,"description":"true 读取程序当前物理屏幕，包含光标、样式和输入所需 screen_id，不受用户历史滚动位置影响"},"region":region_schema(),"session_id":{"type":"string"},"lines":{"type":"integer","minimum":1,"maximum":500,"default":40}},"required":["session_id"],"additionalProperties":false}),
        ),
    ]
}

fn region_schema() -> serde_json::Value {
    serde_json::json!({"type":"object","description":"可选观察区域，物理单元格，行列从 0 开始；持续刷新的 TUI 可避开时钟/指标，只观察菜单或状态栏。","properties":{"row":{"type":"integer","minimum":0},"column":{"type":"integer","minimum":0},"rows":{"type":"integer","minimum":1},"columns":{"type":"integer","minimum":1}},"required":["row","column","rows","columns"],"additionalProperties":false})
}
fn input_tool() -> ToolDefinition {
    ToolDefinition::new(
        ToolName::new("terminal_input").unwrap(),
        "操作已有终端中的交互程序。先读当前 screen，以 screen_id 校验观察区域，按目的发送短动作，再等待并核对画面。输入到真实终端，不是执行一条 shell 命令。release=true 交还控制且不停止程序。",
        serde_json::json!({"type":"object","properties":{"session_id":{"type":"string"},"screen_id":{"type":"string"},"purpose":{"type":"string"},"release":{"type":"boolean","default":false},"actions":{"type":"array","minItems":1,"maxItems":8,"items":{"type":"object","properties":{"type":{"type":"string","enum":["text","paste","key"]},"text":{"type":"string","maxLength":4096,"description":"text 模拟输入文字（如 vim 的 i、:wq）；paste 使用程序支持的括号粘贴。text 不含换行，回车用单独 key 动作。"},"key":{"type":"string","description":"enter、escape、up/down/left/right、tab、shift-tab、pageup/pagedown、home/end、F1–F12（小写 f1）或 ctrl/alt/shift 组合。不支持系统 cmd 快捷键。"},"repeat":{"type":"integer","minimum":1,"maximum":20}},"required":["type"],"additionalProperties":false}}},"required":["session_id","purpose"],"additionalProperties":false}),
    )
}
fn wait_tool() -> ToolDefinition {
    ToolDefinition::new(
        ToolName::new("wait_terminal").unwrap(),
        "等待终端画面包含文字、相对 fingerprint 变化或观察区域稳定。只等待，不发送输入；超时仍返回当前 screen 以及 matched=false，不表示操作成功。多个条件同时提供时全部满足才返回。",
        serde_json::json!({"type":"object","properties":{"session_id":{"type":"string"},"region":region_schema(),"text":{"type":"string"},"fingerprint":{"type":"string","description":"此前 screen 的 fingerprint，等待观察区域发生变化"},"stable_ms":{"type":"integer","minimum":100,"maximum":3000},"timeout_ms":{"type":"integer","minimum":1,"maximum":10000,"default":3000}},"required":["session_id"],"additionalProperties":false}),
    )
}
pub const INTERACTION_RULES: &str = "\n终端交互程序使用 read_terminal(screen=true) → terminal_input → wait_terminal → 再观察。只能向用户任务指定的终端输入，主/子代理能力一致；不需要切换页面焦点。每次引用最新 screen_id，画面变化或尺寸改变时重新读屏；持续刷新的程序用 region 观察有意义的菜单/状态，不能盲发长串按键。text 表示键入字符，paste 表示粘贴（编辑器模式语义不同），文字和 enter 分开发送；程序启动也可通过终端输入完成，run_command 仍只用于空闲 shell 的命令捕获。TUI 不等于备用屏幕，按当前画面判断。等待的 matched=false 不是成功。修改文件、结束进程、提交操作等不能因表现为按键就当成只读，输入需遵循目标审批；用户接管后停止自动输入，只有用户允许继续才能恢复。release=true 只交还控制，不退出程序；任务结束应用也交还控制。需要用户选择时使用 ask_user。\n";

#[cfg(test)]
mod tests {
    #[test]
    fn unknown_tools_are_not_parsed_as_terminal_reads() {
        let read = serde_json::json!({"session_id":"local-ref","lines":40});
        assert!(super::parse("read_terminal", read.clone()).is_ok());
        assert!(super::parse("compare_directories", read.clone()).is_err());
        assert!(super::parse("unknown", read).is_err());
    }
}
