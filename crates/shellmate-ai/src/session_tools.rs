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
    Create(Create),
    Manage(Manage),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(crate = "rig_core::serde", deny_unknown_fields)]
pub struct Input {
    #[serde(default)]
    pub guards:Vec<shellmate_core::terminal_interaction::Guard>,
    #[serde(default)]
    pub observe:shellmate_core::terminal_interaction::Observe,
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
    pub screen_id:Option<String>,
    pub absent_text:Option<String>,
    #[serde(default)]
    pub exited:bool,
    pub session_id: String,
    pub region: Option<shellmate_core::terminal_interaction::Region>,
    pub text: Option<String>,
    pub fingerprint: Option<String>,
    pub stable_ms: Option<u64>,
    #[serde(default = "wait_timeout")]
    pub timeout_ms: u64,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(crate = "rig_core::serde", deny_unknown_fields)]
pub struct Create {
    pub session_id: String,
    pub command: String,
    pub cwd: Option<String>,
    pub purpose: String,
    #[serde(default = "default_columns")]
    pub columns: u16,
    #[serde(default = "default_rows")]
    pub rows: u16,
}
fn default_columns()->u16{100}
fn default_rows()->u16{30}
#[derive(Clone, Debug, Deserialize)]
#[serde(crate = "rig_core::serde", deny_unknown_fields)]
pub struct Manage {pub session_id:String,pub action:ManageAction,pub purpose:String,pub columns:Option<u16>,pub rows:Option<u16>}
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(crate = "rig_core::serde", rename_all="snake_case")]
pub enum ManageAction {Stop,Close,Resize}
pub fn valid_size(columns:u16,rows:u16)->bool{(20..=240).contains(&columns)&&(5..=100).contains(&rows)}
fn wait_timeout() -> u64 {
    3000
}
pub fn parse(name: &str, value: serde_json::Value) -> Result<Request, String> {
    if name == "create_terminal" {
        let a:Create=serde_json::from_value(value).map_err(|_|"创建终端参数无效")?;
        if a.session_id.is_empty()||a.command.trim().is_empty()||a.command.len()>16384||a.command.contains('\0')||a.purpose.trim().is_empty()||a.purpose.len()>2000||!valid_size(a.columns,a.rows)||a.cwd.as_ref().is_some_and(|p|!p.starts_with('/')||p.contains('\0')){return Err("需要有效目标、命令和目的；目录使用绝对路径，尺寸为 20–240 列、5–100 行".into());}
        return Ok(Request::Create(a));
    }
    if name == "manage_terminal" {
        let a:Manage=serde_json::from_value(value).map_err(|_|"管理终端参数无效")?;
        if a.session_id.is_empty()||a.purpose.trim().is_empty()||a.purpose.len()>2000||matches!(a.action,ManageAction::Resize)&&!a.columns.zip(a.rows).is_some_and(|(c,r)|valid_size(c,r)){return Err("需要目标、目的和有效尺寸".into());}
        return Ok(Request::Manage(a));
    }
    if name == "terminal_input" {
        let input: Input = serde_json::from_value(value).map_err(|_| "终端交互参数无效")?;
        if input.session_id.is_empty()
            || input.purpose.trim().is_empty()
            || input.purpose.len() > 2000
        {
            return Err("需要指定终端和操作目的".into());
        }
        input.observe.validate()?;
        if input.guards.len()>4{return Err("最多指定 4 个前置条件".into());}
        for guard in &input.guards{guard.validate()?;}
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
        if wait.screen_id.as_ref().is_some_and(|s|s.is_empty()||s.len()>160)||wait.fingerprint.as_ref().is_some_and(|s|s.is_empty()||s.len()>80)||wait.screen_id.is_some()&&wait.fingerprint.is_some()||wait.absent_text.as_ref().is_some_and(|s|s.is_empty()||s.len()>2000){return Err("等待基准/文字无效；screen_id 和旧 fingerprint 只能提供一个".into());}
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
        if wait.text.is_none() && wait.absent_text.is_none() && wait.screen_id.is_none() && wait.fingerprint.is_none() && wait.stable_ms.is_none() && !wait.exited {
            return Err("请指定 text、absent_text、screen_id、stable_ms 或 exited 等待条件".into());
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
        ToolDefinition::new(ToolName::new("create_terminal").unwrap(),"在指定终端所在本机或 SSH 连接上创建独立 PTY 并启动交互命令，不占用原终端、不抢焦点。返回新 session_id 后用 read_terminal、terminal_input、wait_terminal 操作。运行归主会话持有，子代理结束不杀进程。新环境不继承原 shell 的临时变量/虚拟环境。普通非交互命令仍用 run_command。",serde_json::json!({"type":"object","properties":{"session_id":{"type":"string"},"command":{"type":"string"},"cwd":{"type":"string","description":"绝对目录；省略使用来源终端的已知目录"},"purpose":{"type":"string"},"columns":{"type":"integer","minimum":20,"maximum":240,"default":100},"rows":{"type":"integer","minimum":5,"maximum":100,"default":30}},"required":["session_id","command","purpose"],"additionalProperties":false})),
        ToolDefinition::new(ToolName::new("manage_terminal").unwrap(),"管理 Agent 创建的独立终端。stop 停止程序、保留画面；close 停止并移除终端；resize 改变尺寸使旧 screen_id 失效。不适用于用户已有终端。",serde_json::json!({"type":"object","properties":{"session_id":{"type":"string"},"action":{"type":"string","enum":["stop","close","resize"]},"purpose":{"type":"string"},"columns":{"type":"integer","minimum":20,"maximum":240},"rows":{"type":"integer","minimum":5,"maximum":100}},"required":["session_id","action","purpose"],"additionalProperties":false})),
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
fn guard_schema()->serde_json::Value{
    serde_json::json!({"type":"object","description":"显式前置条件：无谓词时区域必须不变；提供 text/absent_text/cursor 时它们须在基准和当前画面中同时成立，允许其他动画变化。必须对应实际操作目标，不用标题或空白代替。","properties":{"region":region_schema(),"text":{"type":"string"},"absent_text":{"type":"string"},"cursor":{"type":"object","properties":{"row":{"type":"integer","minimum":0},"column":{"type":"integer","minimum":0}},"required":["row","column"],"additionalProperties":false}},"required":["region"],"additionalProperties":false})
}
fn observe_schema()->serde_json::Value{
    serde_json::json!({"type":"object","description":"输入后短暂观察；默认最多 600ms。无目标文字时等首次变化后短暂稳定。返回已发送/变化/条件满足分别标识；无变化不重发。较长等待使用 wait_terminal。","properties":{"region":region_schema(),"text":{"type":"string"},"absent_text":{"type":"string"},"stable_ms":{"type":"integer","minimum":40,"maximum":1000},"timeout_ms":{"type":"integer","minimum":0,"maximum":2000,"default":600}},"additionalProperties":false})
}
fn input_tool() -> ToolDefinition {
    ToolDefinition::new(
        ToolName::new("terminal_input").unwrap(),
        "操作已有终端中的交互程序。先读当前 screen，以 screen_id 校验观察区域，按目的发送短动作，再等待并核对画面。输入到真实终端，不是执行一条 shell 命令。release=true 交还控制且不停止程序。",
        serde_json::json!({"type":"object","properties":{"session_id":{"type":"string"},"screen_id":{"type":"string"},"purpose":{"type":"string"},"release":{"type":"boolean","default":false},"guards":{"type":"array","maxItems":4,"items":guard_schema()},"observe":observe_schema(),"actions":{"type":"array","minItems":1,"maxItems":8,"items":{"type":"object","properties":{"type":{"type":"string","enum":["text","paste","key","click","scroll","drag"]},"text":{"type":"string","maxLength":4096,"description":"text 模拟输入文字（如 vim 的 i、:wq）；paste 使用程序支持的括号粘贴。text 不含换行；需要确认输入框已更新再提交时，把文字与 enter 分成两次调用。"},"key":{"type":"string","description":"enter、escape、up/down/left/right、tab、shift-tab、pageup/pagedown、home/end、F1–F12（小写 f1）或 ctrl/alt/shift 组合。也支持单个可打印字符如 6、?、k（区分大小写）。不支持系统 cmd 快捷键。"},"row":{"type":"integer","minimum":0},"column":{"type":"integer","minimum":0},"to_row":{"type":"integer","minimum":0},"to_column":{"type":"integer","minimum":0},"button":{"type":"string","enum":["left","middle","right"]},"lines":{"type":"integer","minimum":-20,"maximum":20,"description":"滚轮行数，正数向下，负数向上，不能为 0"},"repeat":{"type":"integer","minimum":1,"maximum":20}},"required":["type"],"additionalProperties":false}}},"required":["session_id","purpose"],"additionalProperties":false}),
    )
}
fn wait_tool() -> ToolDefinition {
    ToolDefinition::new(
        ToolName::new("wait_terminal").unwrap(),
        "等待终端画面出现/消失文字、相对 screen_id 变化、稳定或程序退出。稳定只表示画面未变化，不代表业务成功。只等待，不发送输入；超时仍返回当前 screen 以及 matched=false，不表示操作成功。多个条件同时提供时全部满足才返回。",
        serde_json::json!({"type":"object","properties":{"session_id":{"type":"string"},"region":region_schema(),"text":{"type":"string"},"screen_id":{"type":"string","description":"引用先前读屏，等待相同区域相对该画面变化；后端从完整基准提取 region，不传裸指纹"},"absent_text":{"type":"string","description":"等待指定文字消失"},"exited":{"type":"boolean","default":false,"description":"等待程序结束"},"stable_ms":{"type":"integer","minimum":100,"maximum":3000},"timeout_ms":{"type":"integer","minimum":1,"maximum":10000,"default":3000}},"required":["session_id"],"additionalProperties":false}),
    )
}
pub const INTERACTION_RULES: &str = "\n终端交互程序使用 read_terminal(screen=true) → terminal_input → wait_terminal → 再观察。只能向用户任务指定的终端输入，主/子代理能力一致；不需要切换页面焦点。每次引用最新 screen_id。region 只裁剪读屏结果，不缩小输入前默认完整画面校验。动态程序可显式提供 guards：相关区域保持不变，或区域内目标文字/光标条件仍成立；条件必须对应实际菜单、选中项或输入框，不能只读标题/空白规避变化。模式、尺寸、人工接管仍严格校验。画面变化的错误附带当前画面和 changed_rows，可直接根据新画面调整操作，不能盲发长串按键。text 表示键入字符，paste 表示粘贴（编辑器模式语义不同），文字和 enter 是不同动作；TUI 文本输入后先观察已出现输入，再独立发送 enter 提交，避免同批文字被程序当成粘贴而吞掉回车；程序启动也可通过终端输入完成，run_command 仍只用于空闲 shell 的命令捕获。TUI 不等于备用屏幕，按当前画面判断。terminal_input 默认短暂观察，返回 actions_sent、changed、condition_matched、observation_status；已发送不等于目标实现，尚未观察到变化不能自动重发。wait_terminal 用 screen_id 绑定基准，等待相关文字/区域；稳定不等于操作成功，matched=false 不是成功。动作后先检查新画面，再决定是否查帮助。同一动作连续无进展时重新判断布局或查看程序内帮助，不重复相同按键；外部查询后重新读屏再输入。最终回复前确认最新实际状态。修改文件、结束进程、提交操作等不能因表现为按键就当成只读，输入需遵循目标审批；用户接管后停止自动输入，只有用户允许继续才能恢复。release=true 只交还控制，不退出程序；任务结束应用也交还控制。需要独立交互环境时使用 create_terminal，返回的新 session_id 与已有终端一样操作；默认后台运行，用户查看不重启程序，停止回复不停止程序。完成后不再需要的终端用 manage_terminal 关闭，仍需使用的交还控制。鼠标 click/scroll/drag 使用从 0 开始的物理行列坐标，整个动作必须在已观察区域内；程序不支持鼠标时改用键盘，不操控系统指针。需要用户选择时使用 ask_user。\n";

#[cfg(test)]
mod tests {
    #[test]
    fn observation_schema_and_validation_cover_current_and_legacy_calls(){
        use super::*;
        assert!(parse("terminal_input",serde_json::json!({"session_id":"s","screen_id":"x","purpose":"搜索","actions":[{"type":"key","key":"6"}],"guards":[{"region":{"row":1,"column":0,"rows":1,"columns":20},"text":"搜索"}],"observe":{"text":"按单曲"}})).is_ok());
        assert!(parse("terminal_input",serde_json::json!({"session_id":"s","screen_id":"x","purpose":"搜索","actions":[{"type":"key","key":"6"}],"guards":[{"region":{"row":1,"column":0,"rows":1,"columns":20},"text":" "}]})).is_err());
        assert!(parse("wait_terminal",serde_json::json!({"session_id":"s","screen_id":"x","region":{"row":2,"column":0,"rows":1,"columns":20}})).is_ok());
        assert!(parse("wait_terminal",serde_json::json!({"session_id":"s","exited":true})).is_ok());
        assert!(parse("wait_terminal",serde_json::json!({"session_id":"s","fingerprint":"old","stable_ms":200})).is_ok());
        assert!(parse("wait_terminal",serde_json::json!({"session_id":"s","fingerprint":"old","screen_id":"x"})).is_err());
        assert!(!wait_tool().parameters.to_string().contains("fingerprint"));
    }
    #[test]
    fn independent_and_pointer_requests_validate_semantics(){
        use super::*;
        assert!(parse("create_terminal",serde_json::json!({"session_id":"local","command":"vim file","purpose":"编辑"})).is_ok());
        for cwd in ["relative","/tmp\0bad"]{assert!(parse("create_terminal",serde_json::json!({"session_id":"local","command":"vim file","cwd":cwd,"purpose":"编辑"})).is_err());}
        assert!(parse("manage_terminal",serde_json::json!({"session_id":"pty","action":"resize","purpose":"调整","columns":0,"rows":30})).is_err());
        let input=|action|parse("terminal_input",serde_json::json!({"session_id":"local","screen_id":"screen","purpose":"菜单","actions":[action]}));
        assert!(input(serde_json::json!({"type":"click","row":2,"column":3})).is_ok());
        assert!(input(serde_json::json!({"type":"scroll","row":2,"column":3,"lines":0})).is_err());
        assert!(input(serde_json::json!({"type":"drag","row":2,"column":3,"to_row":100,"to_column":3})).is_err());
        assert!(input(serde_json::json!({"type":"click","row":-1,"column":3})).is_err());
    }
    #[test]
    fn unknown_tools_are_not_parsed_as_terminal_reads() {
        let read = serde_json::json!({"session_id":"local-ref","lines":40});
        assert!(super::parse("read_terminal", read.clone()).is_ok());
        assert!(super::parse("compare_directories", read.clone()).is_err());
        assert!(super::parse("unknown", read).is_err());
    }
}
