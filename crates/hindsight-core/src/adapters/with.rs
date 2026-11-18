use super::parse_utils::*;
use super::sqlite_ro::{db_cache_stamp, has_table, open_sqlite_ro, table_columns, virtual_path};
use super::AgentAdapter;
use crate::models::*;
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// With 桌面版(腾讯):对话在云端,本地只有两份缓存库(`<应用数据>/com.tencent.with`)。
/// - `data.db`:`qa_session_message_index(session_id, message_id, request_at, status)` 是每轮
///   提问的索引(request_at 为本地时间);`prompt_history(scope_key='session:<id>', text,
///   created_at_ms)` 是输入框历史,每会话最多留 20 条。
/// - `qa_detail.db`:`qa_message_detail(message_id, session_id, payload_json, payload_zstd)`
///   是打开过的轮次的完整内容。新版只写 `payload_zstd`(zstd 压缩的同一份 JSON),老版写
///   `payload_json`,两种都认。payload 的 `response` 是 AG-UI 事件串。
///
/// 有详情的轮次给完整问答与工具调用;没详情的轮次用 prompt_history 补回提问、并留一条
/// 说明;两样都没有的会话不列。无每会话文件,用 `<data.db>#<id>` 作虚拟路径。
pub struct WithAdapter {
    index_db: PathBuf,
    detail_db: PathBuf,
    rows_cache: Mutex<Option<(i64, Vec<WithRow>)>>,
    memories: super::MemoryCache,
    home: Option<PathBuf>,
}

#[derive(Clone)]
struct WithRow {
    id: String,
    last_ms: i64,
    size: i64,
}

/// 一轮提问的索引行
struct IndexTurn {
    message_id: String,
    request_ms: i64,
    status: String,
}

impl WithAdapter {
    pub fn new() -> Self {
        Self::at(super::electron_user_data("com.tencent.with"))
    }

    fn at(base: PathBuf) -> Self {
        Self {
            index_db: base.join("data.db"),
            detail_db: base.join("qa_detail.db"),
            rows_cache: Mutex::new(None),
            memories: super::MemoryCache::new(),
            home: Some(base),
        }
    }

    fn stamp(&self) -> i64 {
        db_cache_stamp(&self.index_db) ^ db_cache_stamp(&self.detail_db).rotate_left(1)
    }

    /// 全部会话的聚合;读不出 = None
    fn rows(&self) -> Option<Vec<WithRow>> {
        let stamp = self.stamp();
        {
            let cache = self.rows_cache.lock().unwrap();
            if let Some((t, rows)) = cache.as_ref() {
                if *t == stamp {
                    return Some(rows.clone());
                }
            }
        }
        let ro = open_sqlite_ro(&self.index_db, "with")?;
        if !has_table(&ro.conn, "qa_session_message_index").ok()? {
            *self.rows_cache.lock().unwrap() = Some((stamp, Vec::new()));
            return Some(Vec::new());
        }

        // 每会话:轮数、最后一轮时间
        let mut sessions: HashMap<String, (i64, i64)> = HashMap::new();
        {
            let mut stmt = ro
                .conn
                .prepare(
                    "SELECT session_id, COUNT(*), MAX(request_at)
                     FROM qa_session_message_index GROUP BY session_id",
                )
                .ok()?;
            let it = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    ))
                })
                .ok()?;
            for row in it {
                let (id, n, last) = row.ok()?;
                sessions.insert(id, (n, local_dt_ms(&last)));
            }
        }

        // 输入框历史:会话 → (条数, 文本总长, 最新时间)
        let mut prompts: HashMap<String, (i64, i64, i64)> = HashMap::new();
        if has_table(&ro.conn, "prompt_history").unwrap_or(false) {
            if let Ok(mut stmt) = ro.conn.prepare(
                "SELECT substr(scope_key, 9), COUNT(*), COALESCE(SUM(LENGTH(text)),0), MAX(created_at_ms)
                 FROM prompt_history WHERE scope_key LIKE 'session:%' GROUP BY scope_key",
            ) {
                if let Ok(it) = stmt.query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                    ))
                }) {
                    for (id, n, len, ms) in it.flatten() {
                        prompts.insert(id, (n, len, ms));
                    }
                }
            }
        }

        // 详情库:会话 → (解压后总字节, 最新 updated_at)
        let details = self.detail_summary();

        let mut rows = Vec::new();
        for (id, (turns, last_ms)) in sessions {
            let p = prompts.get(&id).copied().unwrap_or_default();
            let d = details.get(&id).copied().unwrap_or_default();
            if p.0 == 0 && d.0 == 0 {
                continue;
            }
            rows.push(WithRow {
                last_ms: last_ms.max(d.1).max(p.2),
                size: d.0 + p.1 + turns,
                id,
            });
        }
        *self.rows_cache.lock().unwrap() = Some((stamp, rows.clone()));
        Some(rows)
    }

    fn detail_summary(&self) -> HashMap<String, (i64, i64)> {
        let mut out = HashMap::new();
        let Some(ro) = open_sqlite_ro(&self.detail_db, "with-detail") else {
            return out;
        };
        if !has_table(&ro.conn, "qa_message_detail").unwrap_or(false) {
            return out;
        }
        let cols = table_columns(&ro.conn, "qa_message_detail");
        let bytes = if cols.contains("payload_bytes") {
            "COALESCE(payload_bytes, 0) + LENGTH(COALESCE(payload_json,''))"
        } else {
            "LENGTH(COALESCE(payload_json,''))"
        };
        let updated = if cols.contains("updated_at") {
            "COALESCE(updated_at, 0)"
        } else {
            "0"
        };
        let sql = format!(
            "SELECT session_id, SUM({bytes}), MAX({updated}) FROM qa_message_detail GROUP BY session_id"
        );
        if let Ok(mut stmt) = ro.conn.prepare(&sql) {
            if let Ok(it) = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                ))
            }) {
                for (id, b, u) in it.flatten() {
                    out.insert(id, (b.max(1), epoch_ms(u as f64)));
                }
            }
        }
        out
    }

    /// 本会话全部轮次的 payload:message_id → JSON
    fn load_payloads(&self, session_id: &str) -> HashMap<String, Value> {
        let mut out = HashMap::new();
        let Some(ro) = open_sqlite_ro(&self.detail_db, "with-detail") else {
            return out;
        };
        if !has_table(&ro.conn, "qa_message_detail").unwrap_or(false) {
            return out;
        }
        let cols = table_columns(&ro.conn, "qa_message_detail");
        let zcol = if cols.contains("payload_zstd") {
            "payload_zstd"
        } else {
            "NULL"
        };
        let sql = format!(
            "SELECT message_id, payload_json, {zcol} FROM qa_message_detail WHERE session_id = ?1"
        );
        let Ok(mut stmt) = ro.conn.prepare(&sql) else {
            return out;
        };
        let Ok(it) = stmt.query_map([session_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<Vec<u8>>>(2)?,
            ))
        }) else {
            return out;
        };
        for (mid, json, z) in it.flatten() {
            let parsed = match json.filter(|s| !s.trim().is_empty()) {
                Some(s) => serde_json::from_str::<Value>(&s).ok(),
                None => z
                    .and_then(|b| zstd::stream::decode_all(&b[..]).ok())
                    .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok()),
            };
            if let Some(v) = parsed {
                out.insert(mid, v);
            }
        }
        out
    }

    fn parse(&self, r: &SessionFileRef) -> Result<(SessionMeta, Vec<TranscriptMessage>)> {
        let ro = open_sqlite_ro(&self.index_db, "with")
            .ok_or_else(|| anyhow!("cannot open With data.db"))?;
        let sid = r.native_id.as_str();

        let mut turns: Vec<IndexTurn> = Vec::new();
        {
            let mut stmt = ro.conn.prepare(
                "SELECT message_id, request_at, COALESCE(status,'')
                 FROM qa_session_message_index WHERE session_id = ?1
                 ORDER BY request_at, rowid",
            )?;
            let it = stmt.query_map([sid], |x| {
                Ok(IndexTurn {
                    message_id: x.get(0)?,
                    request_ms: local_dt_ms(&x.get::<_, Option<String>>(1)?.unwrap_or_default()),
                    status: x.get(2)?,
                })
            })?;
            turns.extend(it.flatten());
        }
        if turns.is_empty() {
            return Err(anyhow!("With session {sid} not in index"));
        }

        let mut prompts: Vec<(i64, String)> = Vec::new();
        if has_table(&ro.conn, "prompt_history").unwrap_or(false) {
            let mut stmt = ro.conn.prepare(
                "SELECT created_at_ms, text FROM prompt_history WHERE scope_key = ?1 ORDER BY created_at_ms",
            )?;
            let it = stmt.query_map([format!("session:{sid}")], |x| {
                Ok((
                    x.get::<_, Option<i64>>(0)?.unwrap_or(0),
                    x.get::<_, Option<String>>(1)?.unwrap_or_default(),
                ))
            })?;
            prompts.extend(it.flatten().filter(|(_, t)| !t.trim().is_empty()));
        }
        let mut used_prompts: HashSet<usize> = HashSet::new();

        let payloads = self.load_payloads(sid);

        let mut messages: Vec<TranscriptMessage> = Vec::new();
        let mut cwd_counts: HashMap<String, usize> = HashMap::new();
        let mut model: Option<String> = None;
        let mut tokens: i64 = 0;
        let mut missing = 0usize;

        for turn in &turns {
            if let Some(p) = payloads.get(&turn.message_id) {
                if let Some(m) = p.get("model_name").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                    model = Some(m.to_string());
                }
                tokens += p.get("total_tokens").and_then(Value::as_i64).unwrap_or(0);
                let req = p.get("request").and_then(Value::as_str).unwrap_or("").trim();
                if let Some(i) = prompts
                    .iter()
                    .enumerate()
                    .position(|(i, (_, t))| !used_prompts.contains(&i) && t.trim() == req)
                {
                    used_prompts.insert(i);
                }
                render_turn(p, turn, model.as_deref(), &mut messages, &mut cwd_counts);
                continue;
            }
            // 没缓存详情:按时间找最近的一条输入框历史(±10 分钟内)补回提问
            let best = prompts
                .iter()
                .enumerate()
                .filter(|(i, (ms, _))| {
                    !used_prompts.contains(i) && turn.request_ms > 0 && (ms - turn.request_ms).abs() <= 600_000
                })
                .min_by_key(|(_, (ms, _))| (ms - turn.request_ms).abs())
                .map(|(i, _)| i);
            match best {
                Some(i) => {
                    used_prompts.insert(i);
                    messages.push(text_msg(Role::User, &prompts[i].1, turn.request_ms));
                }
                None => {
                    missing += 1;
                    continue;
                }
            }
            let note = if turn.status == "cancel" {
                "(本轮已取消;回复未在本地缓存)"
            } else {
                "(回复内容未在本地缓存,在 With 中打开该会话后会缓存到本地)"
            };
            messages.push(meta_msg(note, turn.request_ms));
        }
        // 剩下没对上轮次的输入框历史(索引已滚掉的早期轮次)按时间插回
        let leftovers: Vec<TranscriptMessage> = prompts
            .iter()
            .enumerate()
            .filter(|(i, _)| !used_prompts.contains(i))
            .map(|(_, (ms, t))| text_msg(Role::User, t, *ms))
            .collect();
        if !leftovers.is_empty() {
            messages.extend(leftovers);
            messages.sort_by_key(|m| m.timestamp.unwrap_or(0));
        }
        if missing > 0 && messages.is_empty() {
            return Err(anyhow!("With session {sid} has no local content"));
        }
        assign_seq(&mut messages);

        let count = messages
            .iter()
            .filter(|m| m.kind == MessageKind::Text)
            .count() as i64;
        let project_path = resolve_project(&ro.conn, cwd_counts);
        let created = turns
            .iter()
            .map(|t| t.request_ms)
            .filter(|&ms| ms > 0)
            .min()
            .or_else(|| messages.iter().filter_map(|m| m.timestamp).min())
            .unwrap_or(r.mtime_ms);
        let updated = messages
            .iter()
            .filter_map(|m| m.timestamp)
            .max()
            .unwrap_or(0)
            .max(turns.iter().map(|t| t.request_ms).max().unwrap_or(0))
            .max(created);
        let meta = SessionMeta {
            key: format!("with:{sid}"),
            host: String::new(),
            id: sid.to_string(),
            agent: AgentId::With,
            title: session_title("", false, &messages),
            project_name: if project_path.is_empty() {
                project_name_of("")
            } else {
                project_name_of(&project_path)
            },
            project_path,
            file_path: r.file_path.clone(),
            created_at: created,
            updated_at: updated,
            message_count: count,
            size_bytes: r.size,
            git_branch: None,
            model,
            tokens_used: if tokens > 0 { Some(tokens) } else { None },
            archived: false,
            source: None,
            spawned: false,
            favorite: false,
            pinned: false,
        };
        Ok((meta, messages))
    }
}

/// 项目路径:终端 cwd 出现最多的那个;没有 cwd 时,把文件 / 目录参数归到 With 登记的
/// 工作区根(`workspaces.path`)里,命中最多的那个
fn resolve_project(conn: &rusqlite::Connection, counts: HashMap<String, usize>) -> String {
    let (cwds, hints): (Vec<_>, Vec<_>) = counts.into_iter().partition(|(k, _)| !k.starts_with('\u{1}'));
    if let Some((p, _)) = cwds
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
    {
        return p;
    }
    if hints.is_empty() || !has_table(conn, "workspaces").unwrap_or(false) {
        return String::new();
    }
    let roots: Vec<String> = conn
        .prepare("SELECT path FROM workspaces WHERE path IS NOT NULL AND path <> ''")
        .and_then(|mut s| {
            s.query_map([], |r| r.get::<_, String>(0))
                .map(|it| it.flatten().collect())
        })
        .unwrap_or_default();
    let norm = |s: &str| s.replace('\\', "/").trim_end_matches('/').to_lowercase();
    let mut hits: HashMap<&str, usize> = HashMap::new();
    for (hint, n) in &hints {
        let h = norm(&hint[1..]);
        // 嵌套工作区取最长(最具体)的根
        if let Some(root) = roots
            .iter()
            .filter(|r| {
                let r = norm(r);
                !r.is_empty() && (h == r || h.starts_with(&format!("{r}/")))
            })
            .max_by_key(|r| r.len())
        {
            *hits.entry(root.as_str()).or_default() += n;
        }
    }
    hits.into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(r, _)| canonical_project_path(r.trim_end_matches(['/', '\\'])))
        .unwrap_or_default()
}

/// `"2026-11-17 15:40:02"`(本地时间)→ epoch ms;带时区/T 的也认
fn local_dt_ms(s: &str) -> i64 {
    let s = s.trim();
    if s.is_empty() {
        return 0;
    }
    let ms = iso_ms(s);
    if ms > 0 {
        return ms;
    }
    let t = s.replace('T', " ");
    for fmt in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"] {
        if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(&t, fmt) {
            return local_ms(ndt).unwrap_or(0);
        }
    }
    0
}

/// AG-UI 事件串:`{..},{..}`、`[{..},{..}]`、换行分隔都认;坏在中途就停在那里
fn parse_events(s: &str) -> Vec<Value> {
    let t = s.trim();
    if t.starts_with('[') {
        if let Ok(Value::Array(a)) = serde_json::from_str::<Value>(t) {
            return a;
        }
    }
    let mut body = t.strip_prefix('[').unwrap_or(t);
    body = body.strip_suffix(']').unwrap_or(body);
    let mut out = Vec::new();
    let mut rest = body;
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == ',');
        if rest.is_empty() {
            break;
        }
        let mut it = serde_json::Deserializer::from_str(rest).into_iter::<Value>();
        match it.next() {
            Some(Ok(v)) => {
                let off = it.byte_offset();
                out.push(v);
                rest = &rest[off..];
            }
            _ => break,
        }
    }
    out
}

fn raw<'a>(e: &'a Value, key: &str) -> Option<&'a Value> {
    e.get("rawEvent").and_then(|r| r.get(key)).or_else(|| e.get(key))
}

fn raw_str<'a>(e: &'a Value, key: &str) -> &'a str {
    raw(e, key).and_then(Value::as_str).unwrap_or("")
}

fn value_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    }
}

/// 仅界面展示用、不是调用参数的键
const DISPLAY_KEYS: &[&str] = &[
    "sub_content",
    "sub_content_tip",
    "sub_content_event",
    "sub_content_event_value",
];

struct PendingTool {
    id: String,
    name: String,
    args: Value,
    output: Option<String>,
    is_error: bool,
}

#[derive(Default)]
struct Segment {
    text: String,
    thinking: String,
    tools: Vec<PendingTool>,
    ts: i64,
}

impl Segment {
    fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.thinking.trim().is_empty() && self.tools.is_empty()
    }
}

/// 一轮(一条 payload)→ 用户消息 + 若干助手消息(按 STEP 切段)
fn render_turn(
    p: &Value,
    turn: &IndexTurn,
    model: Option<&str>,
    out: &mut Vec<TranscriptMessage>,
    cwd_counts: &mut HashMap<String, usize>,
) {
    let response = p.get("response").and_then(Value::as_str).unwrap_or("");
    let events = parse_events(response);

    let run_ts = events
        .iter()
        .find(|e| e.get("type").and_then(Value::as_str) == Some("RUN_STARTED"))
        .and_then(|e| e.get("timestamp"))
        .map(to_epoch_ms)
        .unwrap_or(0);
    let user_ts = if turn.request_ms > 0 { turn.request_ms } else { run_ts };

    let mut request = p
        .get("request")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if let Some(imgs) = p.get("attached_images").and_then(Value::as_array) {
        // 只留占位:cos 链接是带签名的临时地址,写进正文还会污染标题
        for _ in imgs {
            if !request.is_empty() {
                request.push('\n');
            }
            request.push_str(IMAGE_PLACEHOLDER);
        }
    }
    if !request.is_empty() {
        out.push(text_msg(Role::User, &request, user_ts));
    }

    // 被 CUSTOM remove-tool 隐去的调用(task_planning 之类的内部工具)
    let removed: HashSet<String> = events
        .iter()
        .filter(|e| e.get("type").and_then(Value::as_str) == Some("CUSTOM"))
        .filter(|e| raw_str(e, "type") == "remove-tool")
        .map(|e| raw_str(e, "tool_call_id").to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let mut segs: Vec<Segment> = Vec::new();
    let mut cur = Segment::default();
    let mut tool_index: HashMap<String, usize> = HashMap::new();
    let mut errors: Vec<String> = Vec::new();

    for e in &events {
        let ty = e.get("type").and_then(Value::as_str).unwrap_or("");
        let ts = e.get("timestamp").map(to_epoch_ms).unwrap_or(0);
        if cur.ts == 0 && ts > 0 && ty != "RUN_STARTED" {
            cur.ts = ts;
        }
        match ty {
            "TEXT_MESSAGE_CONTENT" => cur.text.push_str(raw_str(e, "content")),
            "THINKING_TEXT_MESSAGE_CONTENT" => cur.thinking.push_str(raw_str(e, "content")),
            "TOOL_CALL_START" => {
                let id = raw_str(e, "tool_call_id").to_string();
                if removed.contains(&id) {
                    continue;
                }
                let name = raw_str(e, "name");
                let name = if name.is_empty() { raw_str(e, "display_name") } else { name };
                tool_index.insert(id.clone(), cur.tools.len());
                cur.tools.push(PendingTool {
                    id,
                    name: name.to_string(),
                    args: Value::Null,
                    output: None,
                    is_error: false,
                });
            }
            "TOOL_CALL_ARGS" => {
                let id = raw_str(e, "tool_call_id");
                let Some(&i) = tool_index.get(id) else { continue };
                let Some(tool) = cur.tools.get_mut(i).filter(|t| t.id == id) else { continue };
                let mut doc = raw(e, "document").cloned().unwrap_or(Value::Null);
                if let Value::Object(map) = &mut doc {
                    for k in DISPLAY_KEYS {
                        map.remove(*k);
                    }
                    if let Some(cwd) = map.get("cwd").and_then(Value::as_str) {
                        let c = canonical_project_path(cwd.trim().trim_end_matches(['/', '\\']));
                        if !c.is_empty() {
                            *cwd_counts.entry(c).or_default() += 1;
                        }
                    }
                    // 其余路径参数只作旁证(键带前缀,parse 里按工作区根归并)
                    for k in ["path", "search_directory", "file_path", "searchDirectory"] {
                        if let Some(p) = map.get(k).and_then(Value::as_str) {
                            if !p.trim().is_empty() {
                                *cwd_counts.entry(format!("\u{1}{}", p.trim())).or_default() += 1;
                            }
                        }
                    }
                }
                tool.args = doc;
            }
            "TOOL_CALL_RESULT" => {
                let id = raw_str(e, "tool_call_id");
                let Some(&i) = tool_index.get(id) else { continue };
                let Some(tool) = cur.tools.get_mut(i).filter(|t| t.id == id) else { continue };
                let err = raw(e, "error").filter(|v| !v.is_null());
                let result = raw(e, "result").map(value_text).unwrap_or_default();
                match err {
                    Some(err) => {
                        let msg = err
                            .get("content")
                            .map(value_text)
                            .unwrap_or_else(|| value_text(err));
                        let level = err.get("level").and_then(Value::as_str).unwrap_or("error");
                        tool.is_error = level != "info";
                        tool.output = Some(if result.is_empty() {
                            msg
                        } else {
                            format!("{result}\n{msg}")
                        });
                    }
                    None => tool.output = Some(result),
                }
            }
            "STEP_FINISHED" => {
                if !cur.is_empty() {
                    segs.push(std::mem::take(&mut cur));
                    tool_index.clear();
                } else {
                    cur.ts = 0;
                }
            }
            "RUN_ERROR" => {
                let msg = raw(e, "message")
                    .or_else(|| raw(e, "error"))
                    .map(value_text)
                    .unwrap_or_default();
                if !msg.trim().is_empty() {
                    errors.push(msg);
                }
            }
            _ => {}
        }
    }
    if !cur.is_empty() {
        segs.push(cur);
    }

    for seg in segs {
        let ts = if seg.ts > 0 { seg.ts } else { user_ts };
        let mut m = text_msg(Role::Assistant, &seg.text, ts);
        if !seg.thinking.trim().is_empty() {
            m.thinking = Some(clip(seg.thinking.trim(), 200_000).0);
        }
        m.tool_calls = seg
            .tools
            .into_iter()
            .map(|t| tool_call_view(t.id, &t.name, &t.args, t.output, t.is_error))
            .collect();
        m.model = model.map(str::to_string);
        out.push(m);
    }
    for err in errors {
        out.push(meta_msg(&format!("Error: {err}"), user_ts));
    }
}

impl AgentAdapter for WithAdapter {
    fn agent(&self) -> AgentId {
        AgentId::With
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        if !self.index_db.is_file() {
            return Ok(Vec::new());
        }
        let rows = self
            .rows()
            .ok_or_else(|| anyhow!("{} exists but could not be read", self.index_db.display()))?;
        Ok(rows
            .into_iter()
            .map(|row| SessionFileRef {
                agent: AgentId::With,
                file_path: virtual_path(&self.index_db, &row.id),
                native_id: row.id,
                mtime_ms: row.last_ms,
                size: row.size,
            })
            .collect())
    }

    fn parse_session(&self, r: &SessionFileRef) -> Result<ParsedSession> {
        let (meta, messages) = self.parse(r)?;
        Ok(ParsedSession::derive(meta, &messages, 0))
    }

    fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript> {
        let (meta, messages) = self.parse(r)?;
        Ok(ParsedTranscript {
            meta,
            mainline: messages,
            sidechains: Vec::new(),
            unknown_line_count: 0,
        })
    }

    fn list_memories(
        &self,
        sources: &[MemorySource],
        projects: &[PathBuf],
    ) -> Result<Vec<MemoryDoc>> {
        super::generic_memory_docs(&self.memories, AgentId::With, sources, projects)
    }

    fn with_custom_root(&self, dir: PathBuf) -> Box<dyn AgentAdapter> {
        // 给的是 data.db 本身时取其所在目录
        let base = if dir.is_file() {
            dir.parent().map(Path::to_path_buf).unwrap_or(dir)
        } else {
            dir
        };
        Box::new(Self::at(base))
    }

    fn data_roots(&self) -> Vec<PathBuf> {
        vec![self.index_db.clone(), self.detail_db.clone()]
    }

    fn homes(&self) -> Vec<PathBuf> {
        self.home.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_comma_and_array() {
        let a = parse_events(r#"{"type":"A"},{"type":"B"}"#);
        assert_eq!(a.len(), 2);
        let b = parse_events(r#"[{"type":"A"},{"type":"B"}]"#);
        assert_eq!(b.len(), 2);
        let c = parse_events("{\"type\":\"A\"}\n{\"type\":\"B\"},{bad");
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn turn_rendering() {
        let resp = serde_json::json!([
            {"type":"RUN_STARTED","timestamp":1794901203,"rawEvent":{}},
            {"type":"THINKING_TEXT_MESSAGE_CONTENT","timestamp":1794901204,"rawEvent":{"content":"think"}},
            {"type":"TOOL_CALL_START","timestamp":1794901205,"rawEvent":{"tool_call_id":"t1","name":"terminal"}},
            {"type":"TOOL_CALL_ARGS","rawEvent":{"tool_call_id":"t1","document":{"command":"ls","cwd":"d:/proj/x/","sub_content":"x"}}},
            {"type":"TOOL_CALL_RESULT","rawEvent":{"tool_call_id":"t1","result":"ok"}},
            {"type":"TOOL_CALL_START","rawEvent":{"tool_call_id":"t2","name":"task_planning"}},
            {"type":"CUSTOM","rawEvent":{"type":"remove-tool","tool_call_id":"t2"}},
            {"type":"STEP_FINISHED","rawEvent":{}},
            {"type":"TEXT_MESSAGE_CONTENT","timestamp":1794901210,"rawEvent":{"content":"he"}},
            {"type":"TEXT_MESSAGE_CONTENT","rawEvent":{"content":"llo"}},
            {"type":"STEP_FINISHED","rawEvent":{}},
            {"type":"RUN_FINISHED","rawEvent":{}}
        ]);
        let s = resp.as_array().unwrap().iter().map(|v| v.to_string()).collect::<Vec<_>>().join(",");
        let p = serde_json::json!({"request":"hi","response":s,"attached_images":[{"cos_url":"https://x/y.png"}]});
        let turn = IndexTurn { message_id: "m".into(), request_ms: 5, status: "done".into() };
        let mut out = Vec::new();
        let mut cwd = HashMap::new();
        render_turn(&p, &turn, Some("m1"), &mut out, &mut cwd);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].role, Role::User);
        assert!(out[0].text.ends_with("[image]"));
        assert_eq!(out[1].tool_calls.len(), 1);
        assert_eq!(out[1].tool_calls[0].output.as_deref(), Some("ok"));
        assert!(!out[1].tool_calls[0].input.as_deref().unwrap().contains("sub_content"));
        assert_eq!(out[1].thinking.as_deref(), Some("think"));
        assert_eq!(out[2].text, "hello");
        assert_eq!(out[2].timestamp, Some(1794901210000));
        assert_eq!(cwd.len(), 1);
    }

    #[test]
    fn local_time_parse() {
        assert!(local_dt_ms("2026-11-17 15:40:02") > 0);
        assert_eq!(local_dt_ms(""), 0);
    }
}
