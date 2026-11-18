//! 聊天窗口整页(`Page::Chat`):和左侧选中的 agent 关联,可选不同 LLM 对话。
//!
//! 布局:顶部工具条(Agent/模型下拉 + 设置 + 清空)/ 多会话标签栏 / 消息流 /
//! 底部输入行。连接设置里可编辑自定义 endpoint/model/key(持久化到 prefs),
//! 预设 + 覆盖的解析、会话集合状态机都是 `hindsight_core::chat` 纯函数,可单测。
//! 发请求时把 `reqwest::blocking` 放进独立系统线程(不能进 GPUI 异步 executor),
//! 用 oneshot 把结果送回 UI executor,按 session_id + generation 归档,丢弃过期回复。

use super::*;
use crate::ui::open_closable_dialog;
use futures::channel::oneshot;
use futures::StreamExt;
use gpui::prelude::FluentBuilder as _;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::PopupMenuItem;
use gpui_component::text::TextView;
use hindsight_core::chat::{
    self, ChatMessage, ChatMessageKind, ChatRole, CliEvent, CloseResult, ModelPreset, SendAction,
    SessionSet,
};
use hindsight_core::models::{AgentId, TranscriptMessage};

/// Workbench 构造时一并建好的输入框(消息 + 连接覆盖三格 + 添加模型表单四格)。
pub(super) struct ChatInputs {
    pub(super) message: Entity<InputState>,
    pub(super) url: Entity<InputState>,
    pub(super) model: Entity<InputState>,
    pub(super) key: Entity<InputState>,
    pub(super) new_label: Entity<InputState>,
    pub(super) new_id: Entity<InputState>,
    pub(super) new_url: Entity<InputState>,
    pub(super) new_key: Entity<InputState>,
}

const CONN_PREF: &str = "chat_connection.json";
const SESSIONS_PREF: &str = "chat_sessions.json";
const MODELS_PREF: &str = "chat_models.json";

/// 持久化的模型库:用户自定义档位 + 网关 `/v1/models` 刷新得到的档位 +
/// 是否启用「自动发现本机 agent CLI 模型」。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(super) struct ModelLibrary {
    pub(super) custom: Vec<ModelPreset>,
    pub(super) gateway: Vec<ModelPreset>,
    #[serde(default = "default_auto_discover")]
    pub(super) auto_discover: bool,
}

fn default_auto_discover() -> bool {
    true
}

fn load_library() -> ModelLibrary {
    crate::prefs::read(MODELS_PREF)
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| ModelLibrary {
            auto_discover: true,
            ..Default::default()
        })
}

fn save_library(lib: &ModelLibrary) {
    if let Ok(bytes) = serde_json::to_vec(lib) {
        let _ = crate::prefs::write(MODELS_PREF, &bytes);
    }
}

/// 启动时从 prefs 读回自定义连接配置(不存在/损坏 = 全空,回落内置预设)。
/// 落盘文件若被手改成非法地址,该字段作废,避免把 key 发给畸形目标。
pub(super) fn load_connection() -> chat::ConnectionOverride {
    let mut ov: chat::ConnectionOverride = crate::prefs::read(CONN_PREF)
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    if !ov.base_url.is_empty() && chat::validate_base_url(&ov.base_url).is_err() {
        ov.base_url.clear();
    }
    ov
}

fn save_connection(ov: &chat::ConnectionOverride) {
    if let Ok(bytes) = serde_json::to_vec(ov) {
        let _ = crate::prefs::write(CONN_PREF, &bytes);
    }
}

/// 读回持久化的多会话集合(损坏/不存在 = None,调用方回落到全新集合)。
fn load_set() -> Option<SessionSet> {
    crate::prefs::read(SESSIONS_PREF).and_then(|s| serde_json::from_str(&s).ok())
}

/// 原子落盘会话集合(标签、消息、草稿、agent、模型、active)。
fn save_set(set: &SessionSet) {
    if let Ok(bytes) = serde_json::to_vec(set) {
        let _ = crate::prefs::write(SESSIONS_PREF, &bytes);
    }
}

/// URL 校验错误 -> 本地化文案(稳定 key,避免把内部错误枚举暴露到 UI)。
fn base_url_error_message(err: chat::BaseUrlError) -> String {
    let key = match err {
        chat::BaseUrlError::Parse => "Invalid endpoint URL.",
        chat::BaseUrlError::UnsupportedScheme => "Use http or https for the endpoint.",
        chat::BaseUrlError::MissingHost => "Endpoint is missing a host.",
        // Empty 由 decide_send 的 Unconfigured 分支处理,这里不会走到,给兜底即可
        chat::BaseUrlError::Empty => "Invalid endpoint URL.",
    };
    t(key).to_string()
}

/// 聊天页状态:多会话真相在 `set`(可持久化),这里只补 GPUI 运行时(滚动条、输入框、生效配置)。
pub(super) struct ChatState {
    pub(super) inputs: ChatInputs,
    /// 所有会话(id/title/messages/agent/model/draft/sending/generation/error)
    pub(super) set: SessionSet,
    /// 与 set.sessions 同序的滚动条(GPUI 运行时,不可序列化)
    scrolls: Vec<ScrollHandle>,
    /// 实际下拉列表 = 内置 + 自动发现(agent CLI) + 网关刷新 + 用户自定义(去重)
    models: Vec<ModelPreset>,
    /// 启动时自动发现的模型档位(CLI 列表或 HTTP 降级结果),缓存起来,
    /// 增删模型/重建下拉时复用,避免每次都联网/跑 CLI。
    discovered: Vec<ModelPreset>,
    /// 发现结果是否来自 CodeBuddy CLI 的 /model 列表。true 时下拉为
    /// 「接入内网 + 本地(自定义)」两组,不再塞 7 个公网硬编码预设。
    cli_ok: bool,
    /// 是否允许 CLI 会话执行工具(Bash/Read 等)并把命令/输出实时显示到聊天窗口。
    /// 默认 false(纯对话,无副作用);开启后等于 codebuddy -y,会真实执行工具。
    pub(super) allow_cli_tools: bool,
    /// 持久化的模型库(自定义 / 网关 / 自动发现开关)
    pub(super) library: ModelLibrary,
    /// 网关刷新进行中(禁用按钮)
    pub(super) refreshing_models: bool,
    settings_open: bool,
    /// 已生效的自定义连接覆盖(点「保存」才更新,并持久化)
    connection: chat::ConnectionOverride,
    /// 连接设置面板内的校验错误/提示(与消息流里的运行错误分开)
    connection_error: Option<String>,
}

/// 发现下拉模型:优先用 CodeBuddy CLI 的 /model 列表(分「接入内网官方 /
/// 本地自定义」两组,自带内网登录态);CLI 不可用/未装时降级到旧的
/// `models.json` + 本地网关 HTTP 发现,保证离线也能用。
fn discover_models() -> (Vec<ModelPreset>, bool) {
    match chat::fetch_cli_models() {
        Ok(models) if !models.is_empty() => (models, true),
        _ => (chat::discover_agent_cli_models(), false),
    }
}

/// 组装下拉列表。cli_ok=true 时下拉只有两组:接入内网官方 / 本地(自定义),
/// 不再有单独的「自定义连接」组和 Custom 占位档;用户自己新增的自定义模型
/// (library.custom / gateway)也归入「本地(自定义)」组,统一管理。
/// cli_ok=false(CLI 不可用降级)时保留完整 builtin 预设。
fn assemble_models(
    discovered: &[ModelPreset],
    cli_ok: bool,
    library: &ModelLibrary,
) -> Vec<ModelPreset> {
    let builtins: Vec<ModelPreset> = chat::builtin_models()
        .into_iter()
        .filter(|_| !cli_ok) // cli_ok 时不塞任何公网硬编码预设(含 Custom 占位)
        .map(|mut m| {
            m.group = "builtin".into();
            m
        })
        .collect();
    let mut rest: Vec<ModelPreset> = [library.gateway.clone(), library.custom.clone()].concat();
    for m in &mut rest {
        // 用户自定义 / 网关模型一律归到「本地(自定义)」组
        m.group = "custom".into();
    }
    chat::merge_models(&builtins, discovered, &rest)
}

impl ChatState {
    pub(super) fn new(
        inputs: ChatInputs,
        initial: chat::ConnectionOverride,
        sidebar_agent: Option<AgentId>,
    ) -> Self {
        let agent = chat::linked_agent(sidebar_agent, AgentId::ClaudeCode);
        let library = load_library();
        let (discovered, cli_ok) = if library.auto_discover {
            discover_models()
        } else {
            (Vec::new(), false)
        };
        let models = assemble_models(&discovered, cli_ok, &library);
        let default_ix = chat::default_model_ix(&models);
        // 恢复会话集合并修正不变量(防 sending 残留卡死、active/越界脏数据)
        let mut set = load_set().unwrap_or_else(|| SessionSet::new(agent, default_ix));
        set.sanitize(models.len());
        // 平行滚动条数组必须与恢复后的会话数严格等长
        let scrolls = (0..set.sessions.len())
            .map(|_| ScrollHandle::new())
            .collect();
        Self {
            inputs,
            set,
            scrolls,
            models,
            discovered,
            cli_ok,
            library,
            allow_cli_tools: false,
            refreshing_models: false,
            settings_open: false,
            connection: initial,
            connection_error: None,
        }
    }

    /// 重建下拉列表。删档位后必须同步重索引所有会话的 model_ix,否则会指向
    /// 错误的模型。`old_len` 用于判断是否需要把越界的 ix 收敛。
    fn rebuild_models(&mut self, cx: &mut Context<Workbench>) {
        // 复用启动时缓存的自动发现结果,避免每次增删/开关都联网拉目录。
        let discovered: Vec<ModelPreset> = if self.library.auto_discover {
            self.discovered.clone()
        } else {
            Vec::new()
        };
        self.models = assemble_models(&discovered, self.cli_ok, &self.library);
        // 只收敛越界的 model_ix,不碰 sending/error:用户可能在发送途中增删模型,
        // 此时不能像启动时那样用 sanitize 清掉在途状态。
        for s in &mut self.set.sessions {
            if s.model_ix >= self.models.len() {
                s.model_ix = 0;
            }
        }
        save_library(&self.library);
        self.persist();
        cx.notify();
    }

    /// 把会话集合落盘(GUI 各处变更后调用)。
    pub(super) fn persist(&self) {
        save_set(&self.set);
    }

    /// 进入页时,左侧选中的 agent 仅作为「全新空会话」的默认 agent;
    /// 不覆盖已恢复/进行中的会话选择,避免每次进页都丢掉用户的 per-session 设定。
    pub(super) fn sync_sidebar(&mut self, sidebar_agent: Option<AgentId>) {
        if let Some(a) = sidebar_agent {
            if self.set.active().messages.is_empty() && self.set.active().draft.is_empty() {
                self.set.active_mut().agent = a;
                self.persist();
            }
        }
    }

    fn preset(&self) -> &ModelPreset {
        let ix = self.set.active().model_ix;
        &self.models[ix.min(self.models.len() - 1)]
    }

    fn active_scroll(&self) -> &ScrollHandle {
        let i = self.set.active_index();
        &self.scrolls[i.min(self.scrolls.len() - 1)]
    }

    /// 生效的 (model, base_url, key):预设 + 自定义覆盖解析。
    fn effective(&self) -> (String, String, String) {
        chat::resolve_connection(self.preset(), &self.connection)
    }

    /// 清空当前会话内容(保留标签)。把输入框一并清空,避免视觉残留。
    pub(super) fn clear_active(&mut self, window: &mut Window, cx: &mut Context<Workbench>) {
        let s = self.set.active_mut();
        s.messages.clear();
        s.title.clear();
        s.error = None;
        s.sending = false;
        s.draft.clear();
        s.generation = s.generation.wrapping_add(1);
        self.inputs.message.update(cx, |st, cx| {
            st.set_value("", window, cx);
            st.focus(window, cx);
        });
        self.persist();
        cx.notify();
    }

    fn save_draft(&mut self, cx: &mut Context<Workbench>) {
        let draft = self.inputs.message.read(cx).value().to_string();
        self.set.active_mut().draft = draft;
    }

    fn load_draft(&mut self, window: &mut Window, cx: &mut Context<Workbench>) {
        let draft = self.set.active().draft.clone();
        self.inputs.message.update(cx, |st, cx| {
            st.set_value(draft, window, cx);
            st.focus(window, cx);
        });
    }

    pub(super) fn switch_session(
        &mut self,
        id: u64,
        window: &mut Window,
        cx: &mut Context<Workbench>,
    ) {
        self.save_draft(cx);
        if self.set.switch(id) {
            self.load_draft(window, cx);
            self.persist();
            cx.notify();
        } else {
            // 即使没切换,草稿也变了(保存了当前输入),落盘
            self.persist();
        }
    }

    pub(super) fn add_session(&mut self, window: &mut Window, cx: &mut Context<Workbench>) {
        self.save_draft(cx);
        // 已有空会话(还没发首条消息)就切过去,不再新建一个——否则连点 +
        // 会堆出一排同名「新对话」空标签,彼此无法区分(2026-11-18 用户反馈)。
        if let Some(empty) = self.set.find_empty_session() {
            self.connection_error = None;
            if self.set.switch(empty) {
                self.load_draft(window, cx);
            }
            self.persist();
            cx.notify();
            return;
        }
        if self.set.sessions.len() >= chat::MAX_SESSIONS {
            self.connection_error = Some(t("Conversation limit reached.").to_string());
            cx.notify();
            return;
        }
        self.connection_error = None;
        self.set.add();
        self.scrolls.push(ScrollHandle::new());
        self.load_draft(window, cx);
        self.persist();
        cx.notify();
    }

    /// 把一条只读历史会话「在聊天窗口打开」:派生(或复用)一个自建续聊窗口。
    /// 原历史/磁盘/SQLite 全程只读,转录只被复制进 Hindsight 自己的会话里,
    /// 之后发消息带上全部历史上下文(prepare_send 把 s.messages 整体当 history),
    /// 新消息只落 chat_sessions.json。切换这个标签即「切换聊天窗口」。
    pub(super) fn open_history_in_chat(
        &mut self,
        source_key: &str,
        title: String,
        transcript: &[TranscriptMessage],
        agent: AgentId,
        window: &mut Window,
        cx: &mut Context<Workbench>,
    ) {
        self.save_draft(cx);
        let messages = chat::transcript_to_chat_messages(transcript);
        let (_id, is_new) = self
            .set
            .open_from_source(source_key, title, messages, agent);
        if is_new {
            self.scrolls.push(ScrollHandle::new());
        }
        // open_from_source 已把 active_id 指向目标窗口(新建或复用)
        self.load_draft(window, cx);
        self.persist();
        cx.notify();
    }

    pub(super) fn close_session(
        &mut self,
        id: u64,
        window: &mut Window,
        cx: &mut Context<Workbench>,
    ) {
        self.save_draft(cx);
        let old_active = self.set.active_id;
        match self.set.close(id) {
            CloseResult::Removed(ix) => {
                if ix < self.scrolls.len() {
                    self.scrolls.remove(ix);
                }
            }
            CloseResult::Cleared => {
                // 最后一个会话被「清空重置」:草稿也清空,输入框同步
                self.load_draft(window, cx);
            }
        }
        if self.set.active_id != old_active {
            self.load_draft(window, cx);
        }
        self.persist();
        cx.notify();
    }

    /// 点「保存」:校验 URL 后把三个输入框写成生效配置并持久化。
    /// 非法地址不会生效,避免把 Bearer key 发给畸形/非 http(s) 目标。
    pub(super) fn apply_connection(&mut self, cx: &mut Context<Workbench>) {
        let base_url = self.inputs.url.read(cx).value().trim().to_string();
        let model = self.inputs.model.read(cx).value().trim().to_string();
        let api_key = self.inputs.key.read(cx).value().to_string();
        // 仅当用户填了自定义地址时才校验;留空表示回落内置预设。
        if !base_url.is_empty() {
            if let Err(e) = chat::validate_base_url(&base_url) {
                self.connection_error = Some(base_url_error_message(e));
                cx.notify();
                return;
            }
        }
        self.connection_error = None;
        self.connection = chat::ConnectionOverride {
            base_url,
            model,
            api_key,
        };
        save_connection(&self.connection);
        cx.notify();
    }

    /// 点「重置」:清空输入框与生效配置,回落到纯内置预设。
    pub(super) fn reset_connection(&mut self, window: &mut Window, cx: &mut Context<Workbench>) {
        for input in [&self.inputs.url, &self.inputs.model, &self.inputs.key] {
            input.update(cx, |st, cx| st.set_value("", window, cx));
        }
        self.connection = chat::ConnectionOverride::default();
        self.connection_error = None;
        save_connection(&self.connection);
        cx.notify();
    }

    /// 「添加模型」:把"添加模型"表单(显示名/Model ID/Base URL/API Key)
    /// 打成一个自定义档位,加入下拉并持久化。返回 true=成功(弹窗可关闭),
    /// false=校验失败(弹窗留在表单上改)。与连接覆盖三格完全解耦。
    pub(super) fn add_custom_model(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Workbench>,
    ) -> bool {
        let label = self.inputs.new_label.read(cx).value().trim().to_string();
        let id = self.inputs.new_id.read(cx).value().trim().to_string();
        let base_url = self.inputs.new_url.read(cx).value().trim().to_string();
        let api_key = self.inputs.new_key.read(cx).value().to_string();
        if id.is_empty() || base_url.is_empty() {
            self.connection_error = Some(t("Model ID and Base URL are required.").to_string());
            cx.notify();
            return false;
        }
        if let Err(e) = chat::validate_base_url(&base_url) {
            self.connection_error = Some(base_url_error_message(e));
            cx.notify();
            return false;
        }
        let label = if label.is_empty() { id.clone() } else { label };
        self.library.custom.push(ModelPreset {
            label,
            id,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            custom: true,
            group: "custom".into(),
            ..Default::default()
        });
        self.connection_error = None;
        for input in [
            &self.inputs.new_label,
            &self.inputs.new_id,
            &self.inputs.new_url,
            &self.inputs.new_key,
        ] {
            input.update(cx, |st, cx| st.set_value("", window, cx));
        }
        self.rebuild_models(cx);
        true
    }

    /// 「删除」某个自定义/网关档位(内置档不可删)。重索引所有会话的 model_ix。
    pub(super) fn remove_model(&mut self, ix: usize, cx: &mut Context<Workbench>) {
        if ix >= self.models.len() || !self.models[ix].custom {
            return; // 内置档或越界:忽略
        }
        let target = self.models[ix].clone();
        // 从两个来源数组里按 (id, base_url) 移除
        self.library
            .custom
            .retain(|m| m.id != target.id || m.base_url != target.base_url);
        self.library
            .gateway
            .retain(|m| m.id != target.id || m.base_url != target.base_url);
        // 删档位会让其后的下标左移/失效,逐个会话修正
        for s in &mut self.set.sessions {
            s.model_ix = chat::reindex_model_ix(s.model_ix, ix);
        }
        self.rebuild_models(cx);
    }

    /// 切换「自动发现本机 agent CLI 模型」开关。关→开时重新跑一次 CLI /model 发现。
    pub(super) fn toggle_auto_discover(&mut self, cx: &mut Context<Workbench>) {
        self.library.auto_discover = !self.library.auto_discover;
        let (discovered, cli_ok) = if self.library.auto_discover {
            discover_models()
        } else {
            (Vec::new(), false)
        };
        self.discovered = discovered;
        self.cli_ok = cli_ok;
        self.rebuild_models(cx);
    }

    /// 切换「允许 CLI 执行工具并显示命令输出」开关。默认关;开启等价于
    /// codebuddy -y,工具会真实执行命令/读写文件,需用户明确开启。
    pub(super) fn toggle_allow_cli_tools(&mut self, cx: &mut Context<Workbench>) {
        self.allow_cli_tools = !self.allow_cli_tools;
        cx.notify();
    }

    /// 网关 `/v1/models` 刷新结果入库(替换网关段,去重合并)。
    pub(super) fn apply_gateway_models(
        &mut self,
        models: Vec<ModelPreset>,
        cx: &mut Context<Workbench>,
    ) {
        self.refreshing_models = false;
        self.library.gateway = models;
        self.connection_error = None;
        self.rebuild_models(cx);
    }

    /// 用户点发送:改当前会话状态并返回后台请求参数。
    fn prepare_send(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Workbench>,
    ) -> Option<SendJob> {
        // 当前会话正在等回复:忽略重复发送(回车与按钮共用同一守卫)
        if self.set.active().sending {
            return None;
        }
        let text = self.inputs.message.read(cx).value().trim().to_string();
        if text.is_empty() {
            return None;
        }
        // 通道由所选中预设决定:CLI 发现的模型走 CodeBuddy CLI 会话
        // (自带内网登录态、--resume 续多轮);用户自定义 endpoint/Ollama 走 HTTP。
        let via_cli = self.preset().via_cli;
        let model_id = self.preset().id.clone();
        let cli_resume = self.set.active().cli_session_id.clone();

        // HTTP 通道仍走 decide_send + base_url 校验;CLI 通道无需 url/key。
        let (model, base_url, key) = self.effective();
        if !via_cli {
            match chat::decide_send(&text, &model, &base_url) {
                SendAction::Empty => return None,
                SendAction::Unconfigured(_) => {
                    self.set.active_mut().error =
                        Some(t("Set a model and API base URL in chat settings first.").to_string());
                    cx.notify();
                    return None;
                }
                SendAction::Ready(_) => {}
            }
            // 发送前二次校验最终 base_url(不让 key 流向畸形目标)。
            if let Err(e) = chat::validate_base_url(&base_url) {
                self.set.active_mut().error = Some(base_url_error_message(e));
                cx.notify();
                return None;
            }
        }

        let (session_id, gen, agent, history) = {
            let s = self.set.active_mut();
            s.error = None;
            if s.title.is_empty() {
                s.title = chat::derive_title(&text);
            }
            let now = chrono::Utc::now().timestamp_millis();
            s.messages.push(ChatMessage {
                role: ChatRole::User,
                content: text.clone(),
                timestamp: now,
                kind: ChatMessageKind::Normal,
            });
            s.sending = true;
            s.draft.clear();
            s.generation = s.generation.wrapping_add(1);
            (s.id, s.generation, s.agent, s.messages.clone())
        };
        self.inputs.message.update(cx, |st, cx| {
            st.set_value("", window, cx);
            st.focus(window, cx);
        });
        self.persist();
        cx.notify();

        let body = if via_cli {
            String::new()
        } else {
            chat::build_request_body(&model, agent, &history).to_string()
        };
        let url = if via_cli {
            String::new()
        } else {
            chat::completions_url(&base_url).unwrap_or_default()
        };
        Some(SendJob {
            url,
            key: if via_cli { String::new() } else { key },
            body,
            session_id,
            generation: gen,
            via_cli,
            model_id,
            prompt: text,
            cli_resume,
            allow_tools: self.allow_cli_tools,
        })
    }
}

/// 后台线程要执行的发送请求(带会话归属,便于回复归档)。
/// `via_cli=true` 走 CodeBuddy CLI 会话通道(用 model_id/prompt/cli_resume,
/// 忽略 url/key/body);否则走原 OpenAI-compatible HTTP 直连(url/key/body)。
struct SendJob {
    url: String,
    key: String,
    body: String,
    session_id: u64,
    generation: u64,
    via_cli: bool,
    model_id: String,
    prompt: String,
    cli_resume: Option<String>,
    /// 是否给 CLI 会话加 -y 放行工具(命令/输出会实时显示)
    allow_tools: bool,
}

/// 把一条 CLI 过程(思考/命令调用/命令输出)作为 Tool 消息追加到会话。
/// 这类消息在聊天窗口渲染成等宽卡片,且不会拼进 HTTP 请求体。
fn push_process(s: &mut hindsight_core::chat::ChatSessionData, content: String) {
    s.messages.push(ChatMessage {
        role: ChatRole::Assistant,
        content,
        timestamp: chrono::Utc::now().timestamp_millis(),
        kind: ChatMessageKind::Tool,
    });
}

/// 发送结果:回复文本 + 可选的 CLI 会话 id(用于下一轮 --resume 续接)。
struct SendOutcome {
    reply: String,
    cli_session_id: Option<String>,
}

/// 后台线程执行 HTTP 直连发送(阻塞),委托给 core 的可测函数。
/// CLI 会话通道走 `send_cli_stream`(流式),不经此函数。
fn do_send(job: SendJob) -> Result<SendOutcome, String> {
    let text = chat::send_once(&job.url, &job.key, &job.body)?;
    Ok(SendOutcome {
        reply: text,
        cli_session_id: None,
    })
}

/// 三段布局(header / 消息流 / 输入行)组装,抽成自由函数以便无头渲染测试复用。
/// `flex_1 + min_h_0` 让消息流吃掉剩余高度,`flex_shrink_0` 让输入行固定在底部。
pub(crate) fn chat_column(
    bg: Hsla,
    header: AnyElement,
    messages: AnyElement,
    input: AnyElement,
) -> AnyElement {
    v_flex()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .h_full()
        .bg(bg)
        .child(div().w_full().flex_shrink_0().child(header))
        .child(div().flex_1().min_h_0().w_full().child(messages))
        .child(div().w_full().flex_shrink_0().child(input))
        .into_any_element()
}

impl Workbench {
    /// 「+ 添加自定义模型」弹窗:紧凑表单,复用连接设置里那 4 个 InputState,
    /// 确定时校验并入库(add_custom_model 返回 true 关窗、false 留在表单改)。
    /// 菜单项点击会关闭下拉,故用独立弹窗承接,而非把表单塞进菜单。
    fn open_add_model_dialog(&mut self, window: &mut Window, cx: &mut Context<Workbench>) {
        let entity = cx.entity();
        let label = self.chat.inputs.new_label.clone();
        let id = self.chat.inputs.new_id.clone();
        let url = self.chat.inputs.new_url.clone();
        let key = self.chat.inputs.new_key.clone();
        self.chat.connection_error = None;
        open_closable_dialog(window, cx, 460., move |dialog, _, cx| {
            let muted = cx.theme().muted_foreground;
            let field = |text: SharedString, input: &Entity<InputState>| {
                h_flex()
                    .w_full()
                    .gap(SPACE_SM)
                    .items_center()
                    .child(
                        div()
                            .w(zpx(92.))
                            .flex_shrink_0()
                            .text_size(FONT_CAPTION)
                            .text_color(muted)
                            .child(text),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(input).appearance(true)),
                    )
            };
            let ok_e = entity.clone();
            dialog
                .title(t("Add custom model"))
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default().ok_text(t("Add")),
                )
                .child(
                    v_flex()
                        .w_full()
                        .gap(SPACE_SM)
                        .child(field(t("Display name").into(), &label))
                        .child(field(t("Model ID").into(), &id))
                        .child(field(t("Base URL").into(), &url))
                        .child(field(t("API Key").into(), &key))
                        .child(
                            div()
                                .text_size(FONT_CAPTION)
                                .text_color(muted)
                                .child(t("Model ID and Base URL are required.")),
                        ),
                )
                .footer(
                    gpui_component::dialog::DialogFooter::new()
                        .w_full()
                        .child(
                            gpui_component::dialog::DialogClose::new().child(
                                Button::new("add-model-cancel").label(t("Cancel")).outline(),
                            ),
                        )
                        .child(
                            gpui_component::dialog::DialogAction::new()
                                .child(Button::new("add-model-ok").label(t("Add")).primary()),
                        ),
                )
                .on_ok(move |_, window, cx| {
                    ok_e.update(cx, |this: &mut Workbench, cx| {
                        this.chat.add_custom_model(window, cx)
                    })
                })
        });
    }

    /// 顶部工具条 + 标签栏 + 消息流 + 底部输入行,整页三明治。
    pub(super) fn render_chat(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        chat_column(
            theme.background,
            self.render_chat_header(cx),
            self.render_chat_messages(cx),
            self.render_chat_input(cx),
        )
    }

    /// header = 工具条(Agent/模型/设置/清空) + 下面一行多会话标签。
    fn render_chat_header(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();

        // Agent 下拉:切换当前会话 agent,并反向联动左侧筛选高亮
        let current_agent = self.chat.set.active().agent;
        let entity = cx.entity().downgrade();
        let agent_dd = Button::new("chat-agent-dd")
            .small()
            .outline()
            .rounded(RADIUS_BUTTON)
            .label(current_agent.display_name())
            .icon(icon("icons/chevron-down.svg").with_size(zpx(13.)))
            .dropdown_menu(move |menu, _, _| {
                let mut m = menu.min_w(zpx(200.));
                for a in AgentId::ALL {
                    let e = entity.clone();
                    m = m.item(
                        PopupMenuItem::new(a.display_name())
                            .checked(current_agent == a)
                            .on_click(move |_, _, cx| {
                                e.update(cx, |this: &mut Workbench, cx| {
                                    this.chat.set.active_mut().agent = a;
                                    this.selected_agent = Some(a);
                                    this.chat.persist();
                                    cx.notify();
                                })
                                .ok();
                            }),
                    );
                }
                m
            });

        // 模型下拉:按内置/自定义分组,每项带消耗速率(credit_rate),切换当前会话模型。
        // 分组信息来自 core 的 ModelPreset.group;倍率来自网关 /v1/models 的真实
        // credit_rate,未知显示占位符,绝不编造。
        let current_model = self.chat.set.active().model_ix;
        #[derive(Clone)]
        struct Row {
            label: String,
            rate: String,
            group: String,
            ix: usize,
        }
        let rows: Vec<Row> = self
            .chat
            .models
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let group = if p.group.is_empty() {
                    // 旧持久化数据无 group:按 custom 推断
                    if p.custom { "custom" } else { "builtin" }.to_string()
                } else {
                    p.group.clone()
                };
                Row {
                    label: p.label.clone(),
                    rate: p.credit_rate.clone(),
                    group,
                    ix: i,
                }
            })
            .collect();
        let entity2 = cx.entity().downgrade();
        let entity3 = cx.entity().downgrade();
        let model_dd = Button::new("chat-model-dd")
            .small()
            .outline()
            .rounded(RADIUS_BUTTON)
            .label(self.chat.preset().label.clone())
            .icon(icon("icons/chevron-down.svg").with_size(zpx(13.)))
            .dropdown_menu(move |menu, _, _| {
                // 71+ 项:限高并开启滚动,避免下拉顶出窗口、底部被截断。
                let mut m = menu.min_w(zpx(280.)).scrollable(true).max_h(zpx(420.));
                let mut last_key = String::new();
                for row in &rows {
                    // 两组:接入内网官方 / 本地(自定义)。用户新增的自定义模型
                    // 与 custom-local:* 都归到 custom,不再有单独的「自定义连接」。
                    let key = match row.group.as_str() {
                        "official" => "official",
                        _ => "custom",
                    };
                    if key != last_key {
                        if !last_key.is_empty() {
                            m = m.separator();
                        }
                        let title: SharedString = if key == "official" {
                            t("Intranet models")
                        } else {
                            t("Local models")
                        }
                        .into();
                        m = m.item(PopupMenuItem::element(move |_, cx| {
                            div()
                                .px(SPACE_SM)
                                .text_size(FONT_CAPTION)
                                .font_semibold()
                                .text_color(cx.theme().muted_foreground)
                                .child(title.clone())
                        }));
                        last_key = key.to_string();
                    }
                    let e = entity2.clone();
                    let ix = row.ix;
                    let rate = if row.rate.is_empty() {
                        "—".to_string()
                    } else {
                        row.rate.clone()
                    };
                    // 倍率拼进 label 右侧,避免依赖菜单项右侧布局 API
                    let label = format!("{}   {}", row.label, rate);
                    m = m.item(
                        PopupMenuItem::new(label)
                            .checked(current_model == ix)
                            .on_click(move |_, _, cx| {
                                e.update(cx, |this: &mut Workbench, cx| {
                                    let s = this.chat.set.active_mut();
                                    s.model_ix = ix;
                                    s.error = None;
                                    this.chat.persist();
                                    cx.notify();
                                })
                                .ok();
                            }),
                    );
                }
                // 「+ 添加自定义模型…」:弹出一个紧凑表单,填 4 项后点 Add 即新增
                // 一个本地(自定义)模型。菜单项点击会关闭菜单,故用独立弹窗承接。
                m = m.separator();
                let e3 = entity3.clone();
                m = m.item(PopupMenuItem::new(t("+ Add custom model…")).on_click(
                    move |_, window, cx| {
                        e3.update(cx, |this: &mut Workbench, cx| {
                            this.open_add_model_dialog(window, cx);
                        })
                        .ok();
                    },
                ));
                m
            });

        let toolbar = h_flex()
            .w_full()
            .flex_shrink_0()
            .items_center()
            .gap(SPACE_SM)
            .px(SPACE_LG)
            .py(SPACE_SM)
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .text_size(FONT_LABEL)
                    .font_semibold()
                    .text_color(theme.muted_foreground)
                    .child(t("Chat")),
            )
            .child(agent_dd)
            .child(model_dd)
            .child(div().flex_1())
            .child(
                Button::new("chat-settings")
                    .ghost()
                    .small()
                    .rounded(RADIUS_BUTTON)
                    .icon(icon("icons/sliders-horizontal.svg").with_size(zpx(14.)))
                    .tooltip(t("Connection settings"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.chat.settings_open = !this.chat.settings_open;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("chat-clear")
                    .ghost()
                    .small()
                    .rounded(RADIUS_BUTTON)
                    .icon(icon("icons/trash-2.svg").with_size(zpx(14.)))
                    .tooltip(t("Clear conversation"))
                    .on_click(
                        cx.listener(|this, _, window, cx| this.chat.clear_active(window, cx)),
                    ),
            );

        v_flex()
            .w_full()
            .flex_shrink_0()
            .child(toolbar)
            .child(self.render_chat_tabs(cx))
            .into_any_element()
    }

    /// 多会话标签栏:[+] + 每个会话一个「切换 + 关闭」。
    fn render_chat_tabs(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let active_id = self.chat.set.active_id;
        let mut row = h_flex()
            .w_full()
            .flex_shrink_0()
            .items_center()
            .gap(SPACE_SM)
            .px(SPACE_LG)
            .py(SPACE_SM)
            .border_b_1()
            .border_color(theme.border)
            .child(
                Button::new("chat-new-tab")
                    .ghost()
                    .small()
                    .rounded(RADIUS_BUTTON)
                    .label("+")
                    .tooltip(t("New conversation"))
                    .on_click(cx.listener(|this, _, window, cx| this.chat.add_session(window, cx))),
            );

        let show_close = self.chat.set.sessions.len() > 1;
        for (tab_no, s) in self.chat.set.sessions.iter().enumerate() {
            let id = s.id;
            // 空会话(还没发首条消息)用唯一序号占位,避免一堆同名「新对话」;
            // 也不与 + 的「新对话」动作撞名。发过消息后 title 已是首行摘要。
            let title = if s.title.is_empty() {
                format!("{} {}", t("Conversation"), tab_no + 1)
            } else {
                s.title.clone()
            };
            let is_active = id == active_id;
            let switch = Button::new(SharedString::from(format!("chat-tab-{id}")))
                .small()
                .rounded(RADIUS_BUTTON)
                .when(is_active, |b| b.primary())
                .when(!is_active, |b| b.ghost())
                .label(title)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.chat.switch_session(id, window, cx);
                }));
            let mut tab = h_flex().items_center().child(switch);
            if show_close {
                let close = Button::new(SharedString::from(format!("chat-tab-close-{id}")))
                    .ghost()
                    .small()
                    .rounded(RADIUS_BUTTON)
                    .label("×")
                    .tooltip(t("Close conversation"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.chat.close_session(id, window, cx);
                    }));
                tab = tab.child(close);
            }
            row = row.child(tab);
        }
        row.into_any_element()
    }

    fn render_chat_messages(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let session = self.chat.set.active();
        let scroll = self.chat.active_scroll();
        // 外层定高(flex_1 + min_h_0 + relative),内层滚动容器才能拿到确定高度,
        // 否则 overflow_y_scroll 会把底部输入行挤出视口
        let mut col = v_flex()
            .id("chat-messages")
            .flex_1()
            .min_h_0()
            .w_full()
            .p(SPACE_LG)
            .gap(SPACE_MD)
            .overflow_y_scroll()
            .track_scroll(scroll);

        if session.messages.is_empty() {
            col = col.child(
                v_flex()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .gap(SPACE_SM)
                    .child(
                        icon("icons/message-square.svg")
                            .with_size(zpx(32.))
                            .text_color(theme.muted_foreground.opacity(0.5)),
                    )
                    .child(
                        div()
                            .text_size(FONT_CAPTION)
                            .text_color(theme.muted_foreground)
                            .child(t("Start a conversation. Pick an agent and a model above.")),
                    ),
            );
        } else {
            for (i, m) in session.messages.iter().enumerate() {
                col = col.child(self.chat_bubble(session.id, i, m, cx));
            }
            if session.sending {
                col = col.child(
                    h_flex()
                        .gap(SPACE_SM)
                        .items_center()
                        .text_size(FONT_LABEL)
                        .text_color(theme.muted_foreground)
                        .child(Spinner::new().small())
                        .child(t("Thinking…")),
                );
            }
        }
        if let Some(err) = &session.error {
            col = col.child(
                div()
                    .w_full()
                    .px(SPACE_MD)
                    .py(SPACE_SM)
                    .rounded(RADIUS_BUTTON)
                    .bg(theme.danger.opacity(0.1))
                    .text_size(FONT_LABEL)
                    .text_color(theme.danger)
                    .child(err.clone()),
            );
        }
        col.vertical_scrollbar(scroll).into_any_element()
    }

    fn chat_bubble(
        &self,
        session_id: u64,
        i: usize,
        m: &ChatMessage,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        // 工具过程(思考/命令调用/命令输出):全宽等宽卡片,和普通对话气泡区分开,
        // 让"命令执行输出"在聊天窗口里一眼可见、可复制。
        if m.kind == ChatMessageKind::Tool {
            let tool_card = v_flex()
                .w_full()
                .px(SPACE_MD)
                .py(SPACE_SM)
                .rounded(RADIUS_KBD)
                .bg(theme.muted.opacity(0.25))
                .border_1()
                .border_color(theme.border)
                .gap(zpx(4.))
                .child(
                    div()
                        .text_size(FONT_CAPTION)
                        .font_semibold()
                        .text_color(theme.muted_foreground)
                        .child(t("Tool output")),
                )
                .child(
                    div()
                        .font_family(theme.mono_font_family.clone())
                        .text_size(FONT_CAPTION)
                        .text_color(theme.foreground)
                        .child(m.content.clone()),
                );
            return h_flex().w_full().child(tool_card).into_any_element();
        }
        let is_user = m.role == ChatRole::User;
        let bubble_bg = if is_user {
            theme.primary.opacity(0.12)
        } else {
            theme.muted.opacity(0.5)
        };
        let content: AnyElement = if is_user {
            div()
                .text_size(FONT_CAPTION)
                .text_color(theme.foreground)
                .child(m.content.clone())
                .into_any_element()
        } else {
            TextView::markdown(
                SharedString::from(format!("chat-msg-{session_id}-{i}")),
                m.content.clone(),
            )
            .selectable(true)
            .into_any_element()
        };
        let bubble = v_flex()
            .max_w(relative(0.85))
            .px(SPACE_MD)
            .py(SPACE_SM)
            .rounded(RADIUS_KBD)
            .bg(bubble_bg)
            .gap(zpx(4.))
            .child(
                div()
                    .text_size(FONT_LABEL)
                    .font_semibold()
                    .text_color(theme.muted_foreground)
                    // assistant 回复统一标"助手":OpenAI-compatible 后端并非左侧那个
                    // coding agent,且当前 agent 会随下拉切换,不应反向改写历史署名。
                    .child(if is_user { t("You") } else { t("Assistant") }),
            )
            .child(content);
        h_flex()
            .w_full()
            .when(is_user, |row| row.justify_end())
            .when(!is_user, |row| row.justify_start())
            .child(bubble)
            .into_any_element()
    }

    fn render_chat_input(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let sending = self.chat.set.active().sending;
        let input = self.chat.inputs.message.clone();
        v_flex()
            .w_full()
            .flex_shrink_0()
            .px(SPACE_LG)
            .py(SPACE_MD)
            .gap(SPACE_SM)
            .border_t_1()
            .border_color(theme.border)
            .when(self.chat.settings_open, |col| {
                col.child(self.render_chat_settings(cx))
            })
            .child(
                h_flex()
                    .w_full()
                    .gap(SPACE_SM)
                    .items_end()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&input).appearance(false)),
                    )
                    .child(
                        Button::new("chat-send")
                            .primary()
                            .small()
                            .rounded(RADIUS_BUTTON)
                            .label(t("Send"))
                            .disabled(sending)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.send_chat(window, cx)),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// 连接设置 + 模型管理:三格连接覆盖 + 自动发现开关 + 添加自定义模型 +
    /// 网关 /v1/models 刷新 + 可删除的模型库列表。
    fn render_chat_settings(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (model, url, _) = self.chat.effective();
        let preset = self.chat.preset();
        let key_hint: SharedString = if !self.chat.connection.api_key.is_empty() {
            t("Using a custom API key.").into()
        } else if !preset.api_key.is_empty() {
            t("Using the model's built-in API key.").into()
        } else if !preset.key_env.is_empty() {
            crate::tf!(
                "API key is read from {} in the environment.",
                preset.key_env.clone()
            )
            .into()
        } else {
            t("No API key needed for this endpoint.").into()
        };
        v_flex()
            .w_full()
            .gap(SPACE_SM)
            .px(SPACE_MD)
            .py(SPACE_SM)
            .rounded(RADIUS_BUTTON)
            .bg(theme.muted.opacity(0.4))
            .text_size(FONT_LABEL)
            .child(
                div()
                    .font_semibold()
                    .text_color(theme.foreground)
                    .child(t("Connection settings")),
            )
            .child(self.setting_row(cx, t("Base URL"), &self.chat.inputs.url))
            .child(self.setting_row(cx, t("Model ID"), &self.chat.inputs.model))
            .child(self.setting_row(cx, t("API Key"), &self.chat.inputs.key))
            .child(
                h_flex()
                    .w_full()
                    .gap(SPACE_SM)
                    .items_center()
                    .child(
                        Button::new("chat-save-conn")
                            .primary()
                            .small()
                            .rounded(RADIUS_BUTTON)
                            .label(t("Save connection"))
                            .on_click(cx.listener(|this, _, _, cx| this.chat.apply_connection(cx))),
                    )
                    .child(
                        Button::new("chat-reset-conn")
                            .ghost()
                            .small()
                            .rounded(RADIUS_BUTTON)
                            .label(t("Reset"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.chat.reset_connection(window, cx)
                            })),
                    )
                    .child(div().flex_1()),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap(SPACE_SM)
                    .items_center()
                    .child(
                        Button::new("chat-cli-tools")
                            .small()
                            .rounded(RADIUS_BUTTON)
                            .when(self.chat.allow_cli_tools, |b| b.primary())
                            .label(if self.chat.allow_cli_tools {
                                t("CLI tools: ON (commands run)")
                            } else {
                                t("CLI tools: OFF")
                            })
                            .on_click(
                                cx.listener(|this, _, _, cx| this.chat.toggle_allow_cli_tools(cx)),
                            ),
                    )
                    .child(
                        div()
                            .text_size(FONT_CAPTION)
                            .text_color(theme.muted_foreground)
                            .child(t("Show CLI command output in chat")),
                    ),
            )
            .child(
                div()
                    .text_color(theme.muted_foreground)
                    .child(crate::tf!("Endpoint: {}", url)),
            )
            .child(
                div()
                    .text_color(theme.muted_foreground)
                    .child(crate::tf!("Model: {}", model)),
            )
            .child(div().text_color(theme.muted_foreground).child(key_hint))
            // ---- 模型管理 ----
            .child(
                div()
                    .mt(SPACE_SM)
                    .font_semibold()
                    .text_color(theme.foreground)
                    .child(crate::tf!("Models ({})", self.chat.models.len())),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap(SPACE_SM)
                    .items_center()
                    .child(
                        Button::new("chat-toggle-discover")
                            .ghost()
                            .small()
                            .rounded(RADIUS_BUTTON)
                            .label(if self.chat.library.auto_discover {
                                t("Agent CLI models: ON")
                            } else {
                                t("Agent CLI models: OFF")
                            })
                            .on_click(
                                cx.listener(|this, _, _, cx| this.chat.toggle_auto_discover(cx)),
                            ),
                    )
                    .child(
                        Button::new("chat-refresh-gateway")
                            .ghost()
                            .small()
                            .rounded(RADIUS_BUTTON)
                            .label(t("Refresh from gateway"))
                            .disabled(self.chat.refreshing_models)
                            .on_click(
                                cx.listener(|this, _, _, cx| this.refresh_gateway_models(cx)),
                            ),
                    )
                    .child(div().flex_1()),
            )
            // 添加自定义模型:独立的四格表单,与上面的连接覆盖互不影响
            .child(
                div()
                    .mt(SPACE_SM)
                    .font_semibold()
                    .text_color(theme.foreground)
                    .child(t("Add model")),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap(SPACE_SM)
                    .items_center()
                    .child(
                        div()
                            .w(zpx(72.))
                            .text_color(theme.muted_foreground)
                            .child(t("Display name")),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&self.chat.inputs.new_label).appearance(true)),
                    )
                    .child(
                        div()
                            .w(zpx(72.))
                            .text_color(theme.muted_foreground)
                            .child(t("Model ID")),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&self.chat.inputs.new_id).appearance(true)),
                    ),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap(SPACE_SM)
                    .items_center()
                    .child(
                        div()
                            .w(zpx(72.))
                            .text_color(theme.muted_foreground)
                            .child(t("Base URL")),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&self.chat.inputs.new_url).appearance(true)),
                    )
                    .child(
                        div()
                            .w(zpx(72.))
                            .text_color(theme.muted_foreground)
                            .child(t("API Key")),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&self.chat.inputs.new_key).appearance(true)),
                    )
                    .child(
                        Button::new("chat-add-model")
                            .small()
                            .rounded(RADIUS_BUTTON)
                            .label(t("Add"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                let _ = this.chat.add_custom_model(window, cx);
                            })),
                    ),
            )
            .child(self.render_model_library(cx))
            .when(self.chat.connection_error.is_some(), |col| {
                let err = self.chat.connection_error.clone().unwrap_or_default();
                col.child(
                    div()
                        .w_full()
                        .px(SPACE_MD)
                        .py(SPACE_SM)
                        .rounded(RADIUS_BUTTON)
                        .bg(theme.danger.opacity(0.1))
                        .text_color(theme.danger)
                        .child(err),
                )
            })
            .into_any_element()
    }

    /// 可删除的模型库列表(只列 custom 档位,内置/自动发现不可删)。
    fn render_model_library(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let mut col = v_flex()
            .w_full()
            .gap(zpx(2.))
            .max_h(zpx(120.))
            .overflow_y_scrollbar();
        for (ix, m) in self.chat.models.iter().enumerate() {
            if !m.custom {
                continue;
            }
            col = col.child(
                h_flex()
                    .w_full()
                    .gap(SPACE_SM)
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_color(theme.muted_foreground)
                            .child(m.label.clone()),
                    )
                    .child(
                        Button::new(SharedString::from(format!("chat-del-model-{ix}")))
                            .ghost()
                            .small()
                            .rounded(RADIUS_BUTTON)
                            .label("×")
                            .tooltip(t("Remove model"))
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.chat.remove_model(ix, cx)),
                            ),
                    ),
            );
        }
        col.into_any_element()
    }

    /// 设置面板的一行:左标签 + 右输入框。
    fn setting_row(
        &self,
        cx: &Context<Self>,
        label: &'static str,
        input: &Entity<InputState>,
    ) -> AnyElement {
        h_flex()
            .w_full()
            .gap(SPACE_SM)
            .items_center()
            .child(
                div()
                    .w(zpx(72.))
                    .text_color(cx.theme().muted_foreground)
                    .child(label),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(input).appearance(true)),
            )
            .into_any_element()
    }

    /// 「从网关刷新」:用连接设置里的 base_url/key GET `{base_url}/models`,
    /// 在独立系统线程里跑(不能进 GPUI 异步 executor),结果回传后并入下拉。
    fn refresh_gateway_models(&mut self, cx: &mut Context<Workbench>) {
        let base_url = self.chat.inputs.url.read(cx).value().trim().to_string();
        let api_key = self.chat.inputs.key.read(cx).value().to_string();
        if base_url.is_empty() {
            self.chat.connection_error = Some(t("Set a Base URL first.").to_string());
            cx.notify();
            return;
        }
        if let Err(e) = chat::validate_base_url(&base_url) {
            self.chat.connection_error = Some(base_url_error_message(e));
            cx.notify();
            return;
        }
        self.chat.refreshing_models = true;
        self.chat.connection_error = None;
        let (tx, rx) = oneshot::channel();
        std::thread::spawn(move || {
            let _ = tx.send(chat::fetch_openai_models(&base_url, &api_key));
        });
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or(Err("discover task dropped".to_string()));
            this.update(cx, |this, cx| match result {
                Ok(models) => {
                    let n = models.len();
                    this.chat.apply_gateway_models(models, cx);
                    if n == 0 {
                        this.chat.connection_error =
                            Some(t("No models returned by the gateway.").to_string());
                        cx.notify();
                    }
                }
                Err(e) => {
                    this.chat.refreshing_models = false;
                    this.chat.connection_error = Some(e);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn send_chat(&mut self, window: &mut Window, cx: &mut Context<Workbench>) {
        let Some(job) = self.chat.prepare_send(window, cx) else {
            return;
        };
        if job.via_cli {
            self.send_cli_stream(job, cx);
        } else {
            self.send_http(job, cx);
        }
    }

    /// HTTP 直连通道(自定义 endpoint 兜底):一次性请求,oneshot 回传结果。
    fn send_http(&mut self, job: SendJob, cx: &mut Context<Workbench>) {
        let (gen, session_id) = (job.generation, job.session_id);
        let (tx, rx) = oneshot::channel();
        std::thread::spawn(move || {
            let _ = tx.send(do_send(job));
        });
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or(Err("send task dropped".to_string()));
            this.update(cx, |this, cx| {
                let Some(s) = this.chat.set.get_mut(session_id) else {
                    return;
                };
                if s.generation != gen {
                    return;
                }
                s.sending = false;
                match result {
                    Ok(outcome) => {
                        if let Some(sid) = outcome.cli_session_id {
                            if !sid.is_empty() {
                                s.cli_session_id = Some(sid);
                            }
                        }
                        s.messages.push(ChatMessage {
                            role: ChatRole::Assistant,
                            content: outcome.reply,
                            timestamp: chrono::Utc::now().timestamp_millis(),
                            kind: ChatMessageKind::Normal,
                        });
                    }
                    Err(e) => s.error = Some(e),
                }
                this.chat.persist();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// CLI 会话通道:后台线程逐行跑 `codebuddy -p --output-format stream-json`,
    /// 经 std mpsc + futures unbounded 桥接,把思考/命令调用/命令输出/正文
    /// 实时追加到聊天窗口(`CliEvent`),最终一条 Done 收尾并存 session_id。
    fn send_cli_stream(&mut self, job: SendJob, cx: &mut Context<Workbench>) {
        let (gen, session_id) = (job.generation, job.session_id);
        let model_id = job.model_id.clone();
        let resume = job.cli_resume.clone();
        let prompt = job.prompt.clone();
        let allow_tools = job.allow_tools;

        let (std_tx, std_rx) = std::sync::mpsc::channel::<CliEvent>();
        let (utx, mut urx) = futures::channel::mpsc::unbounded::<CliEvent>();
        // 线程 1:跑 CLI 会话,逐行把 stream-json 解析成事件写进 std mpsc
        std::thread::spawn(move || {
            chat::stream_cli_session(&model_id, resume.as_deref(), &prompt, allow_tools, std_tx);
        });
        // 线程 2:std mpsc -> futures unbounded(GPUI async 侧接收)
        std::thread::spawn(move || {
            for ev in std_rx {
                if utx.unbounded_send(ev).is_err() {
                    break; // 接收方已关闭
                }
            }
        });

        cx.spawn(async move |this, cx| {
            let mut saw_text = false;
            let mut final_reply = String::new();
            let mut stale = false;
            while let Some(ev) = urx.next().await {
                let done = matches!(ev, CliEvent::Done { .. });
                this.update(cx, |this, cx| {
                    let Some(s) = this.chat.set.get_mut(session_id) else {
                        stale = true;
                        return;
                    };
                    if s.generation != gen {
                        stale = true; // 过期回复:丢弃,不动新一轮的 sending
                        return;
                    }
                    match ev {
                        CliEvent::Session(sid) => {
                            s.cli_session_id = Some(sid);
                        }
                        CliEvent::Thinking(t) => {
                            push_process(s, format!("[思考] {t}"));
                        }
                        CliEvent::ToolUse { name, input } => {
                            push_process(s, format!("$ [{name}] {input}"));
                        }
                        CliEvent::ToolResult { output, is_error } => {
                            let prefix = if is_error { "[错误] " } else { "[输出] " };
                            push_process(s, format!("{prefix}{output}"));
                        }
                        CliEvent::Text(t) => {
                            saw_text = true;
                            s.messages.push(ChatMessage {
                                role: ChatRole::Assistant,
                                content: t,
                                timestamp: chrono::Utc::now().timestamp_millis(),
                                kind: ChatMessageKind::Normal,
                            });
                        }
                        CliEvent::Done { reply } => {
                            final_reply = reply;
                        }
                    }
                    cx.notify();
                })
                .ok();
                if stale {
                    return;
                }
                if done {
                    break;
                }
            }
            // 收尾:结束 sending;若全程没有 Text 片段(如纯工具/异常),用 Done.reply 补一条。
            this.update(cx, |this, cx| {
                let Some(s) = this.chat.set.get_mut(session_id) else {
                    return;
                };
                if s.generation != gen {
                    return;
                }
                if !saw_text && !final_reply.is_empty() {
                    s.messages.push(ChatMessage {
                        role: ChatRole::Assistant,
                        content: final_reply,
                        timestamp: chrono::Utc::now().timestamp_millis(),
                        kind: ChatMessageKind::Normal,
                    });
                }
                s.sending = false;
                this.chat.persist();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 输入框回车即发送(在 Workbench 构造里订阅 InputEvent)。
    pub(super) fn on_chat_input_event(
        &mut self,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Workbench>,
    ) {
        if matches!(event, InputEvent::PressEnter { .. }) && !self.chat.set.active().sending {
            self.send_chat(window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{assemble_models, discover_models, push_process, ModelLibrary};
    use hindsight_core::chat::{self, ChatMessageKind, ChatRole, ModelPreset, SessionSet};
    use hindsight_core::models::AgentId;

    fn cli_models() -> Vec<ModelPreset> {
        vec![
            ModelPreset {
                label: "claude-opus-5.5".into(),
                id: "claude-opus-5.5".into(),
                group: "official".into(),
                via_cli: true,
                ..Default::default()
            },
            ModelPreset {
                label: "space-bunny".into(),
                id: "custom-local:space-bunny".into(),
                group: "custom".into(),
                via_cli: true,
                ..Default::default()
            },
        ]
    }

    #[test]
    fn assemble_models_cli_ok_only_two_groups_and_defaults_to_cli() {
        let mut lib = ModelLibrary::default();
        // 用户自己新增的一个自定义模型(HTTP 通道)应集成进「本地(自定义)」组
        lib.custom.push(ModelPreset {
            label: "my-ollama".into(),
            id: "qwen".into(),
            group: "".into(),
            custom: true,
            ..Default::default()
        });
        let models = assemble_models(&cli_models(), true, &lib);
        // cli_ok 时不应再有 7 个公网硬编码预设(gpt-4o 等)
        assert!(!models.iter().any(|m| m.id == "gpt-4o"));
        // 不再有单独的「自定义连接」组/Custom 占位(id 空)
        assert!(!models.iter().any(|m| m.id.is_empty()));
        assert!(!models
            .iter()
            .any(|m| m.group == "builtin" || m.group == "connection"));
        // 只有两组:接入内网官方 + 本地(自定义,含 custom-local 与用户新增)
        assert_eq!(models.iter().filter(|m| m.group == "official").count(), 1);
        assert_eq!(models.iter().filter(|m| m.group == "custom").count(), 2);
        assert!(models.iter().filter(|m| m.via_cli).count() >= 2);
        assert!(models.iter().any(|m| m.id == "qwen" && m.group == "custom"));
        // 默认选中首个 CLI 模型
        let ix = chat::default_model_ix(&models);
        assert!(models[ix].via_cli);
    }

    #[test]
    fn assemble_models_fallback_keeps_public_builtins() {
        let lib = ModelLibrary::default();
        let models = assemble_models(&[], false, &lib);
        // 降级(CLI 不可用)时保留完整 builtin 预设(其中含 Custom 占位)
        assert_eq!(models.iter().filter(|m| m.group == "builtin").count(), 7);
        assert_eq!(chat::default_model_ix(&models), 0); // 无 CLI 模型退回 0
    }

    /// 真机集成:跑一次真实的 CLI 发现 + 组装,打印两组分组数量。
    /// 依赖本机 codebuddy CLI;用 `cargo test -p hindsight --bin Hindsight real_discover -- --ignored --nocapture` 手动验证。
    #[test]
    #[ignore = "requires local codebuddy CLI"]
    fn real_discover_assembles_two_groups() {
        let (discovered, cli_ok) = discover_models();
        assert!(cli_ok, "CLI /model 列表应可用");
        let official = discovered.iter().filter(|m| m.group == "official").count();
        let local = discovered.iter().filter(|m| m.group == "custom").count();
        assert!(official > 0 && local > 0);
        let models = assemble_models(&discovered, cli_ok, &ModelLibrary::default());
        println!(
            "real dropdown: official={} local={} total={} default_ix={}",
            official,
            local,
            models.len(),
            chat::default_model_ix(&models)
        );
        assert!(!models
            .iter()
            .any(|m| m.group == "builtin" || m.group == "connection"));
        assert!(!models.iter().any(|m| m.id.is_empty())); // 无 Custom 占位/自定义连接组
        assert!(models[chat::default_model_ix(&models)].via_cli);
        assert!(!models.iter().any(|m| m.id == "gpt-4o")); // 公网硬编码预设已不混入
    }

    #[test]
    fn push_process_builds_tool_kind_message() {
        let mut set = SessionSet::new(AgentId::Codex, 0);
        push_process(set.active_mut(), "$ [Bash] echo OK".into());
        push_process(set.active_mut(), "[输出] Stdout: OK".into());
        let msgs = &set.active().messages;
        assert_eq!(msgs.len(), 2);
        assert!(msgs.iter().all(|m| m.kind == ChatMessageKind::Tool));
        assert!(msgs.iter().all(|m| m.role == ChatRole::Assistant));
        assert!(msgs[0].content.contains("echo OK"));
        assert!(msgs[1].content.contains("Stdout: OK"));
    }
}
