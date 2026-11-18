//! 会话级改动汇总:把一条会话里所有改文件的工具调用按文件归组(`collect`),
//! 再把同一文件的多次改动合成一份净改动(`merge`)。
//!
//! 合并的四条路,按能拿到的信息从多到少:
//! 0. 工具结果里附带了改动后的整文件(With 的 `full_content`):从第一个快照撤回到会话开始、
//!    从最后一个快照重放到会话结束。不依赖磁盘,文件之后被还原或再改也不影响。
//!    CodeBuddy / Claude Code 的 file-history 备份(改动前的整文件)同样当锚点用。
//! 1. 第一次改动就是新建(Write / Add File):改动前是空文件,顺着重放所有改动得到结束时的整文件。
//! 2. 磁盘上还有这个文件:从现在的内容倒着撤销每次改动,得到会话开始前的整文件;再正着重放一遍,
//!    结果必须和磁盘一字不差才算数(防止撤销时定位错了地方)。这两条得到的是整文件对比,行号真实。
//! 3. 都不行(文件不在、之后又被改过、片段被截断):按片段合并——后一次改动的 old 落在前一次的
//!    new 里就并进去,改了又改回的片段丢掉。没有整文件,也就没有行号。

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde_json::Value;

use crate::diff::MAX_EDIT_TEXT;
use crate::models::{EditHunk, EditKind, FileEdit, MessageKind, Role, TranscriptMessage};

/// 一次改动在转录里的位置
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditRef {
    pub msg_ix: usize,
    pub call_ix: usize,
    pub edit_ix: usize,
    pub seq: i64,
    pub timestamp: Option<i64>,
    pub kind: EditKind,
    pub added: usize,
    pub removed: usize,
}

/// 一个文件在这条会话里的全部改动
#[derive(Debug, Clone)]
pub struct FileChanges {
    /// 第一次出现时的原样路径
    pub path: String,
    /// 时间顺序
    pub edits: Vec<EditRef>,
    /// 各次改动增删行数之和(逐次累计,不是净改动)
    pub added: usize,
    pub removed: usize,
    pub created: bool,
    pub deleted: bool,
    /// 会话里新建的临时脚本 / 中间产物,列表里默认收起
    pub scratch: bool,
}

/// 路径归一化成分组 key:分隔符统一成 `/`、相对路径挂到项目目录下、Windows 盘符路径不分大小写
pub fn path_key(path: &str, project_root: &str) -> String {
    let mut p = path.trim().replace('\\', "/");
    while let Some(rest) = p.strip_prefix("./") {
        p = rest.to_string();
    }
    if !is_absolute(&p) && !project_root.is_empty() {
        let root = project_root.replace('\\', "/");
        p = format!("{}/{}", root.trim_end_matches('/'), p);
    }
    if is_drive_path(&p) || cfg!(windows) {
        p = p.to_lowercase();
    }
    p
}

fn is_drive_path(p: &str) -> bool {
    let b = p.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

fn is_absolute(p: &str) -> bool {
    p.starts_with('/') || p.starts_with("~/") || is_drive_path(p)
}

/// 中间产物常落的目录名(任一级目录命中即算)
const SCRATCH_DIRS: &[&str] = &[
    "tmp",
    "temp",
    ".tmp",
    "scratch",
    "saved",
    "target",
    "node_modules",
    ".cache",
    "__pycache__",
];

fn is_scratch(key: &str, root_key: &str, created: bool, deleted: bool) -> bool {
    if !created {
        return false;
    }
    if deleted {
        return true;
    }
    let outside =
        !root_key.is_empty() && is_absolute(key) && !key.starts_with(&format!("{root_key}/"));
    let dirs: Vec<&str> = key.split('/').collect();
    let in_tmp = dirs[..dirs.len().saturating_sub(1)]
        .iter()
        .any(|d| SCRATCH_DIRS.iter().any(|s| d.eq_ignore_ascii_case(s)));
    outside || in_tmp
}

/// 按文件归组。排序:正经改动在前、临时脚本在后;同组内改动量大的在前
pub fn collect(messages: &[TranscriptMessage], project_root: &str) -> Vec<FileChanges> {
    let root_key = if project_root.is_empty() {
        String::new()
    } else {
        path_key(project_root, "").trim_end_matches('/').to_string()
    };
    let mut order: Vec<String> = Vec::new();
    let mut files: HashMap<String, FileChanges> = HashMap::new();
    for (msg_ix, m) in messages.iter().enumerate() {
        for (call_ix, call) in m.tool_calls.iter().enumerate() {
            for (edit_ix, edit) in call.edits.iter().enumerate() {
                let key = path_key(&edit.path, project_root);
                let entry = files.entry(key.clone()).or_insert_with(|| {
                    order.push(key.clone());
                    FileChanges {
                        path: edit.path.clone(),
                        edits: Vec::new(),
                        added: 0,
                        removed: 0,
                        created: edit.kind == EditKind::Create,
                        deleted: false,
                        scratch: false,
                    }
                });
                entry.edits.push(EditRef {
                    msg_ix,
                    call_ix,
                    edit_ix,
                    seq: m.seq,
                    timestamp: m.timestamp,
                    kind: edit.kind,
                    added: edit.added,
                    removed: edit.removed,
                });
                entry.added += edit.added;
                entry.removed += edit.removed;
                entry.deleted = edit.kind == EditKind::Delete;
            }
        }
    }
    let mut out: Vec<FileChanges> = order
        .into_iter()
        .filter_map(|key| {
            let mut f = files.remove(&key)?;
            f.scratch = is_scratch(&key, &root_key, f.created, f.deleted);
            Some(f)
        })
        .collect();
    // 稳定排序:同量的按首次出现先后
    out.sort_by_key(|f| (f.scratch, std::cmp::Reverse(f.added + f.removed)));
    out
}

/// 一轮里读过的一个文件
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadItem {
    /// 转录里第一次出现时的原样路径
    pub path: String,
    /// 这一轮里读了几次
    pub count: usize,
    /// 会话里这个文件被改过(用来在列表里标注「改动 / 参考」)
    pub changed: bool,
}

/// 一轮对话里读过的文件。index 0 是首条用户消息之前的读取,之后第 N 轮对应第 N 条用户消息
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadTurn {
    pub index: usize,
    /// 这一轮用户消息的摘要(首个非空行,截断);index 0 为空
    pub prompt: String,
    pub files: Vec<ReadItem>,
}

const PROMPT_CHARS: usize = 40;

fn summarize_prompt(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    if line.chars().count() > PROMPT_CHARS {
        let head: String = line.chars().take(PROMPT_CHARS).collect();
        format!("{head}…")
    } else {
        line.to_string()
    }
}

/// 按轮次整理会话里读过的文件。轮次以用户的文本消息切分,同一轮内同一文件去重并累计次数,
/// 保持首次出现的顺序;没有读取的轮次不返回
pub fn collect_read_turns(messages: &[TranscriptMessage], project_root: &str) -> Vec<ReadTurn> {
    let changed: HashSet<String> = collect(messages, project_root)
        .iter()
        .map(|f| path_key(&f.path, project_root))
        .collect();
    let mut turns = vec![ReadTurn {
        index: 0,
        prompt: String::new(),
        files: Vec::new(),
    }];
    let mut user_turns = 0usize;
    // 当前轮次内 key → files 下标
    let mut pos: HashMap<String, usize> = HashMap::new();
    for m in messages {
        if m.role == Role::User && m.kind == MessageKind::Text {
            user_turns += 1;
            turns.push(ReadTurn {
                index: user_turns,
                prompt: summarize_prompt(&m.text),
                files: Vec::new(),
            });
            pos.clear();
        }
        for call in &m.tool_calls {
            let Some(raw) = call.input.as_deref() else {
                continue;
            };
            let Ok(input) = serde_json::from_str::<Value>(raw) else {
                continue;
            };
            let Some(path) = crate::diff::read_path(&call.name, &input) else {
                continue;
            };
            if path.trim().is_empty() {
                continue;
            }
            let key = path_key(&path, project_root);
            let Some(turn) = turns.last_mut() else {
                continue;
            };
            match pos.get(&key) {
                Some(&ix) => turn.files[ix].count += 1,
                None => {
                    pos.insert(key.clone(), turn.files.len());
                    turn.files.push(ReadItem {
                        path,
                        count: 1,
                        changed: changed.contains(&key),
                    });
                }
            }
        }
    }
    turns.retain(|t| !t.files.is_empty());
    turns
}

/// 按位置取出那次改动
pub fn edit_at<'a>(messages: &'a [TranscriptMessage], r: &EditRef) -> Option<&'a FileEdit> {
    messages
        .get(r.msg_ix)?
        .tool_calls
        .get(r.call_ix)?
        .edits
        .get(r.edit_ix)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeBase {
    /// 会话里新建的文件,改动前为空
    NewFile,
    /// 由磁盘上的当前文件倒推出改动前
    Disk,
    /// 由工具结果里附带的整文件快照推出
    Snapshot,
    /// 由 agent 自己的 file-history 备份(CodeBuddy / Claude Code)推出
    FileHistory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnippetReason {
    /// 拿不到磁盘文件(不存在、远程会话)
    NoFile,
    /// 磁盘上的文件和会话记录对不上(之后又被改过)
    FileChanged,
    /// 有改动片段太长被截断
    Truncated,
    /// 文件在会话里被删除
    Deleted,
}

/// 片段合并的一段净改动
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetHunk {
    pub old: String,
    pub new: String,
    /// 参与合成这段的改动(`FileChanges::edits` 的下标,升序)
    pub sources: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Merged {
    /// 整文件:old = 会话开始前,new = 会话结束时
    Full {
        old: String,
        new: String,
        base: MergeBase,
        /// 从 old 依次重放会话里的改动得不到 new:中途有工具之外的改动(脚本、手改、格式化)也算进来了
        external: bool,
    },
    /// 只能按片段合并
    Snippets {
        hunks: Vec<NetHunk>,
        reason: SnippetReason,
    },
}

fn norm(s: &str) -> String {
    s.replace("\r\n", "\n")
}

fn normalized(edit: &FileEdit) -> FileEdit {
    FileEdit {
        hunks: edit
            .hunks
            .iter()
            .map(|h| EditHunk {
                old: norm(&h.old),
                new: norm(&h.new),
            })
            .collect(),
        ..edit.clone()
    }
}

/// 在 doc 里把 `from` 的第一处换成 `to`。patch 片段每行都带换行,文件末行可能没有,
/// 找不到时去掉末尾换行再试一次
fn replace_once(doc: &mut String, from: &str, to: &str) -> bool {
    if from.is_empty() {
        return false;
    }
    if let Some(p) = doc.find(from) {
        doc.replace_range(p..p + from.len(), to);
        return true;
    }
    if let (Some(f), Some(t)) = (from.strip_suffix('\n'), to.strip_suffix('\n')) {
        if !f.is_empty() && doc.ends_with(f) {
            let p = doc.len() - f.len();
            doc.replace_range(p.., t);
            return true;
        }
    }
    false
}

fn apply_forward(doc: &mut String, edit: &FileEdit) -> bool {
    match edit.kind {
        EditKind::Create => {
            *doc = edit.hunks.iter().map(|h| h.new.as_str()).collect();
            true
        }
        EditKind::Delete => {
            doc.clear();
            true
        }
        EditKind::Update => edit.hunks.iter().all(|h| replace_once(doc, &h.old, &h.new)),
    }
}

fn apply_reverse(doc: &mut String, edit: &FileEdit) -> bool {
    match edit.kind {
        EditKind::Update => edit.hunks.iter().rev().all(|h| {
            // 倒推没有工具帮忙校验唯一性:新内容在文件里出现多次就不知道撤哪一处
            let unique = h.new.is_empty() || doc.matches(h.new.as_str()).nth(1).is_none();
            unique && replace_once(doc, &h.new, &h.old)
        }),
        EditKind::Create | EditKind::Delete => false,
    }
}

/// 用整文件锚点(工具结果里的改后快照 `after`、file-history 的改前备份 `before`):
/// 最早的锚点往前撤到会话开始,最晚的锚点往后重放到会话结束。锚点之间若对不上就换下一个
fn merge_with_snapshots(edits: &[FileEdit]) -> Option<(String, String)> {
    // (边界, 内容):边界 i 表示 edits[..i] 已应用、edits[i..] 还没应用
    let mut anchors: Vec<(usize, &str)> = Vec::new();
    for (i, e) in edits.iter().enumerate() {
        if let Some(b) = e.before.as_deref() {
            anchors.push((i, b));
        }
        if let Some(a) = e.after.as_deref() {
            anchors.push((i + 1, a));
        }
    }
    if anchors.is_empty() {
        return None;
    }
    let start = anchors.iter().find_map(|&(b, text)| {
        let mut doc = text.to_string();
        edits[..b]
            .iter()
            .rev()
            .all(|e| apply_reverse(&mut doc, e))
            .then_some(doc)
    })?;
    let end = anchors.iter().rev().find_map(|&(b, text)| {
        let mut doc = text.to_string();
        edits[b..]
            .iter()
            .all(|e| apply_forward(&mut doc, e))
            .then_some(doc)
    })?;
    Some((start, end))
}

/// 锚点来源:有 file-history 备份的算 FileHistory,否则是工具结果快照
fn anchor_base(edits: &[FileEdit]) -> MergeBase {
    if edits.iter().any(|e| e.before.is_some()) {
        MergeBase::FileHistory
    } else {
        MergeBase::Snapshot
    }
}

/// 把同一文件按时间顺序的若干次改动合成净改动。`disk` 是磁盘上该文件现在的内容
pub fn merge(edits: &[&FileEdit], disk: Option<&str>) -> Merged {
    let edits: Vec<FileEdit> = edits.iter().map(|e| normalized(e)).collect();
    let snippets = |reason| Merged::Snippets {
        hunks: merge_snippets(&edits),
        reason,
    };
    let Some(first) = edits.first() else {
        return snippets(SnippetReason::NoFile);
    };
    if edits.iter().any(|e| e.truncated) {
        return snippets(SnippetReason::Truncated);
    }
    if first.kind == EditKind::Create {
        let mut doc = String::new();
        if edits.iter().all(|e| apply_forward(&mut doc, e)) {
            return Merged::Full {
                old: String::new(),
                new: doc,
                base: MergeBase::NewFile,
                external: false,
            };
        }
        return snippets(SnippetReason::FileChanged);
    }
    if edits.last().is_some_and(|e| e.kind == EditKind::Delete) {
        return snippets(SnippetReason::Deleted);
    }
    if let Some((old, new)) = merge_with_snapshots(&edits) {
        let mut replay = old.clone();
        let external = !edits.iter().all(|e| apply_forward(&mut replay, e)) || replay != new;
        return Merged::Full {
            old,
            new,
            base: anchor_base(&edits),
            external,
        };
    }
    let Some(disk) = disk else {
        return snippets(SnippetReason::NoFile);
    };
    let end = norm(disk);
    let mut start = end.clone();
    if !edits.iter().rev().all(|e| apply_reverse(&mut start, e)) {
        return snippets(SnippetReason::FileChanged);
    }
    let mut replay = start.clone();
    if !edits.iter().all(|e| apply_forward(&mut replay, e)) || replay != end {
        return snippets(SnippetReason::FileChanged);
    }
    Merged::Full {
        old: start,
        new: end,
        base: MergeBase::Disk,
        external: false,
    }
}

/// 一份 file-history 备份:某文件在 `time` 时刻的整文件内容
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileBackup {
    pub path: String,
    pub time: i64,
    pub content: String,
}

/// 读 CodeBuddy / Claude Code 的 file-history 备份。转录里 `file-history-snapshot` 行的
/// `snapshot.trackedFileBackups` 记着 `{相对 cwd 的路径: {backupFileName, backupTime}}`,
/// 备份文件在 `<home>/file-history/<sessionId>/<backupFileName>`,内容是 backupTime 那一刻
/// 的文件(也就是之后那些改动之前)。backupFileName 为 null 表示那时文件还不存在
pub fn read_file_history(jsonl: &Path, history_dir: &Path) -> Vec<FileBackup> {
    use std::io::BufRead;
    if !history_dir.is_dir() {
        return Vec::new();
    }
    let Ok(file) = std::fs::File::open(jsonl) else {
        return Vec::new();
    };
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut out = Vec::new();
    for line in std::io::BufReader::new(file).lines() {
        let Ok(line) = line else { break };
        if !line.contains("trackedFileBackups") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(tracked) = v
            .pointer("/snapshot/trackedFileBackups")
            .and_then(Value::as_object)
        else {
            continue;
        };
        for (path, info) in tracked {
            let name = info.get("backupFileName").and_then(Value::as_str);
            let time = info.get("backupTime").map(epoch_ms).unwrap_or(0);
            if time <= 0 {
                continue;
            }
            let id = name.map_or_else(|| format!("null@{time}"), str::to_string);
            if !seen.insert((path.clone(), id)) {
                continue;
            }
            let content = match name {
                None => String::new(),
                Some(name) => {
                    // 文件名只认单段,防目录穿越
                    if name.contains(['/', '\\']) || name.contains("..") {
                        continue;
                    }
                    let p = history_dir.join(name);
                    match std::fs::metadata(&p) {
                        Ok(m) if m.len() as usize <= MAX_EDIT_TEXT => {}
                        _ => continue,
                    }
                    match std::fs::read_to_string(&p) {
                        Ok(s) => norm(&s),
                        Err(_) => continue,
                    }
                }
            };
            out.push(FileBackup {
                path: path.clone(),
                time,
                content,
            });
        }
    }
    out
}

fn epoch_ms(v: &Value) -> i64 {
    match v {
        Value::Number(n) => n.as_i64().unwrap_or(0),
        Value::String(s) => chrono::DateTime::parse_from_rfc3339(s)
            .map(|d| d.timestamp_millis())
            .unwrap_or(0),
        _ => 0,
    }
}

/// 把 file-history 备份挂到对应改动上:备份时刻之前已完成的改动数为边界,挂成边界处那次改动的
/// `before`(全都在之前就挂成最后一次的 `after`)。备份可能恰在某次改动执行时才拍(首次改某文件),
/// 这时那次改动所在消息的时间早于备份——同一时刻的几次改动里,取能从备份顺着重放过去的最早一处
pub fn attach_file_history(messages: &mut [TranscriptMessage], backups: &[FileBackup], cwd: &str) {
    if backups.is_empty() {
        return;
    }
    // 文件 key → 按时间顺序的 (消息, 调用, 改动, 时间)
    let mut by_file: HashMap<String, Vec<(usize, usize, usize, i64)>> = HashMap::new();
    for (mi, m) in messages.iter().enumerate() {
        for (ci, call) in m.tool_calls.iter().enumerate() {
            for (ei, e) in call.edits.iter().enumerate() {
                by_file
                    .entry(path_key(&e.path, cwd))
                    .or_default()
                    .push((mi, ci, ei, m.timestamp.unwrap_or(0)));
            }
        }
    }
    let mut sorted: Vec<&FileBackup> = backups.iter().collect();
    sorted.sort_by_key(|b| b.time);
    for b in sorted {
        let Some(list) = by_file.get(&path_key(&b.path, cwd)) else {
            continue;
        };
        let edit = |i: usize| -> FileEdit {
            let (mi, ci, ei, _) = list[i];
            normalized(&messages[mi].tool_calls[ci].edits[ei])
        };
        let k0 = list.iter().filter(|x| x.3 < b.time).count();
        let mut lo = k0;
        while lo > 0 && k0 > 0 && list[lo - 1].3 == list[k0 - 1].3 {
            lo -= 1;
        }
        let k = (lo..=k0)
            .find(|&k| {
                let mut doc = b.content.clone();
                (k..k0).all(|i| apply_forward(&mut doc, &edit(i)))
            })
            .unwrap_or(k0);
        let slot = if k < list.len() {
            let (mi, ci, ei, _) = list[k];
            &mut messages[mi].tool_calls[ci].edits[ei].before
        } else {
            let (mi, ci, ei, _) = list[list.len() - 1];
            &mut messages[mi].tool_calls[ci].edits[ei].after
        };
        if slot.is_none() {
            *slot = Some(b.content.clone());
        }
    }
}

/// 没有整文件时的片段合并
fn merge_snippets(edits: &[FileEdit]) -> Vec<NetHunk> {
    let mut net: Vec<NetHunk> = Vec::new();
    for (i, edit) in edits.iter().enumerate() {
        match edit.kind {
            EditKind::Create => {
                // 整文件写入:之前的片段都被覆盖了
                net.clear();
                net.push(NetHunk {
                    old: String::new(),
                    new: edit.hunks.iter().map(|h| h.new.as_str()).collect(),
                    sources: vec![i],
                });
            }
            EditKind::Delete => {
                if net.is_empty() {
                    net.push(NetHunk {
                        old: String::new(),
                        new: String::new(),
                        sources: vec![i],
                    });
                }
                for h in &mut net {
                    h.new.clear();
                    h.sources.push(i);
                }
            }
            EditKind::Update => {
                for hunk in &edit.hunks {
                    merge_hunk(&mut net, hunk, i);
                }
            }
        }
    }
    net.retain(|h| h.old != h.new || h.sources.iter().any(|&s| edits[s].kind == EditKind::Delete));
    net
}

fn merge_hunk(net: &mut Vec<NetHunk>, hunk: &EditHunk, source: usize) {
    // 改的是之前某次改动写进去的内容:直接并进那一段
    if !hunk.old.is_empty() {
        if let Some(h) = net.iter_mut().find(|h| h.new.contains(hunk.old.as_str())) {
            h.new = h.new.replacen(hunk.old.as_str(), &hunk.new, 1);
            if !h.sources.contains(&source) {
                h.sources.push(source);
            }
            return;
        }
    }
    // 这次改动的范围盖住了之前的若干段:把那些段在 old 里换回改动前的样子
    let covered: Vec<usize> = net
        .iter()
        .enumerate()
        .filter(|(_, h)| !h.new.is_empty() && hunk.old.contains(h.new.as_str()))
        .map(|(ix, _)| ix)
        .collect();
    let mut old = hunk.old.clone();
    let mut sources = Vec::new();
    for &ix in covered.iter().rev() {
        let h = net.remove(ix);
        old = old.replacen(h.new.as_str(), &h.old, 1);
        sources.extend(h.sources);
    }
    sources.push(source);
    sources.sort_unstable();
    sources.dedup();
    net.push(NetHunk {
        old,
        new: hunk.new.clone(),
        sources,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{MessageKind, Role, ToolCallView};

    fn upd(path: &str, pairs: &[(&str, &str)]) -> FileEdit {
        FileEdit {
            path: path.into(),
            kind: EditKind::Update,
            hunks: pairs
                .iter()
                .map(|(o, n)| EditHunk {
                    old: (*o).into(),
                    new: (*n).into(),
                })
                .collect(),
            truncated: false,
            added: 1,
            removed: 1,
            after: None,
            before: None,
        }
    }

    fn create(path: &str, content: &str) -> FileEdit {
        FileEdit {
            path: path.into(),
            kind: EditKind::Create,
            hunks: vec![EditHunk {
                old: String::new(),
                new: content.into(),
            }],
            truncated: false,
            added: content.lines().count(),
            removed: 0,
            after: None,
            before: None,
        }
    }

    fn msg(seq: i64, edits: Vec<FileEdit>) -> TranscriptMessage {
        let mut m: TranscriptMessage = serde_json::from_value(serde_json::json!({
            "seq": seq, "role": "assistant", "kind": "text", "text": "", "truncated": false,
            "tool_calls": [], "thinking": null, "timestamp": null, "model": null
        }))
        .expect("message");
        m.role = Role::Assistant;
        m.kind = MessageKind::Text;
        m.tool_calls.push(ToolCallView {
            id: String::new(),
            name: "Edit".into(),
            input_preview: String::new(),
            input: None,
            output: None,
            is_error: false,
            child_id: None,
            edits,
        });
        m
    }

    #[test]
    fn collect_groups_by_normalized_path_and_flags_scratch() {
        let messages = vec![
            msg(1, vec![upd(r"D:\Proj\src\a.lua", &[("x", "y")])]),
            msg(2, vec![create(r"D:\Proj\Saved\tool\mk.py", "print(1)\n")]),
            msg(3, vec![upd("D:/proj/src/a.lua", &[("y", "z")])]),
            msg(4, vec![upd("src/b.lua", &[("1", "2")])]),
        ];
        let files = collect(&messages, r"D:\Proj");
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].path, r"D:\Proj\src\a.lua");
        assert_eq!(files[0].edits.len(), 2);
        assert_eq!(files[0].edits[1].msg_ix, 2);
        assert!(!files[1].scratch);
        assert_eq!(files[2].path, r"D:\Proj\Saved\tool\mk.py");
        assert!(files[2].scratch && files[2].created);
    }

    #[test]
    fn new_file_replays_forward() {
        let c = create("a", "a\nb\nc\n");
        let u = upd("a", &[("b\n", "B\n")]);
        let merged = merge(&[&c, &u], None);
        assert_eq!(
            merged,
            Merged::Full {
                old: String::new(),
                new: "a\nB\nc\n".into(),
                base: MergeBase::NewFile,
                external: false
            }
        );
    }

    #[test]
    fn disk_file_is_rewound_to_session_start() {
        // 第 2 次把第 1 次加的调试行删掉、第 3 次给改名
        let e1 = upd("f", &[("    DoBuy()\n", "    print(1)\n    Check()\n    DoBuy()\n")]);
        let e2 = upd("f", &[("    print(1)\n    Check()", "    Check()")]);
        let e3 = upd("f", &[("Check()", "CheckCoin()")]);
        let disk = "function Buy()\r\n    CheckCoin()\r\n    DoBuy()\r\nend\r\n";
        let Merged::Full { old, new, base, .. } = merge(&[&e1, &e2, &e3], Some(disk)) else {
            panic!("expected full merge");
        };
        assert_eq!(base, MergeBase::Disk);
        assert_eq!(old, "function Buy()\n    DoBuy()\nend\n");
        assert_eq!(new, "function Buy()\n    CheckCoin()\n    DoBuy()\nend\n");
    }

    #[test]
    fn pure_deletion_cannot_be_rewound_but_snippets_still_chain() {
        let e1 = upd("f", &[("    DoBuy()\n", "    print(1)\n    Check()\n    DoBuy()\n")]);
        let e2 = upd("f", &[("    print(1)\n", "")]);
        let disk = "function Buy()\n    Check()\n    DoBuy()\nend\n";
        let Merged::Snippets { hunks, .. } = merge(&[&e1, &e2], Some(disk)) else {
            panic!("expected snippets");
        };
        assert_eq!(hunks[0].old, "    DoBuy()\n");
        assert_eq!(hunks[0].new, "    Check()\n    DoBuy()\n");
    }

    #[test]
    fn changed_disk_falls_back_to_snippets_that_chain() {
        let e1 = upd("f", &[("a = 1", "a = 2\nprint(a)")]);
        let e2 = upd("f", &[("print(a)", "log(a)")]);
        let e3 = upd("f", &[("k = 1", "k = 9")]);
        let e4 = upd("f", &[("k = 9", "k = 1")]);
        let Merged::Snippets { hunks, reason } = merge(&[&e1, &e2, &e3, &e4], Some("other")) else {
            panic!("expected snippets");
        };
        assert_eq!(reason, SnippetReason::FileChanged);
        assert_eq!(
            hunks,
            vec![NetHunk {
                old: "a = 1".into(),
                new: "a = 2\nlog(a)".into(),
                sources: vec![0, 1]
            }]
        );
    }

    #[test]
    fn wider_later_edit_absorbs_earlier_snippet() {
        let e1 = upd("f", &[("b", "B")]);
        let e2 = upd("f", &[("a\nB\nc", "a\nB2\nc")]);
        let Merged::Snippets { hunks, .. } = merge(&[&e1, &e2], None) else {
            panic!("expected snippets");
        };
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old, "a\nb\nc");
        assert_eq!(hunks[0].new, "a\nB2\nc");
        assert_eq!(hunks[0].sources, vec![0, 1]);
    }

    #[test]
    fn snapshots_beat_a_reverted_disk_file() {
        let mut e1 = upd("f", &[("a = 1", "a = 2")]);
        e1.after = crate::diff::after_snapshot(&e1, r#"{"full_content": "top\na = 2\nk = 1\n"}"#);
        assert!(e1.after.is_some());
        let e2 = upd("f", &[("k = 1", "k = 9")]);
        // 磁盘已被 git 还原
        let Merged::Full { old, new, base, .. } = merge(&[&e1, &e2], Some("top\na = 1\nk = 1\n")) else {
            panic!("expected full merge");
        };
        assert_eq!(base, MergeBase::Snapshot);
        assert_eq!(old, "top\na = 1\nk = 1\n");
        assert_eq!(new, "top\na = 2\nk = 9\n");
        // 快照里没有这次写进去的内容:不认
        assert!(crate::diff::after_snapshot(&e2, r#"{"full_content": "unrelated"}"#).is_none());
    }

    #[test]
    fn file_history_backups_anchor_whole_file() {
        let at = |ts: i64, e: FileEdit| {
            let mut m = msg(ts, vec![e]);
            m.timestamp = Some(ts);
            m
        };
        let mut messages = vec![
            at(100, upd("src/f.lua", &[("a = 1", "a = 2")])),
            at(200, upd("src/f.lua", &[("k = 1", "k = 9")])),
            at(400, upd("src/f.lua", &[("a = 2", "a = 3")])),
        ];
        let backups = vec![
            // 首次改动执行时拍的(晚于那条消息的时间)
            FileBackup {
                path: r"src\f.lua".into(),
                time: 150,
                content: "a = 1\nk = 1\nz = 0\n".into(),
            },
            // 下一回合开始时拍的,其间用户手改了 z
            FileBackup {
                path: r"src\f.lua".into(),
                time: 300,
                content: "a = 2\nk = 9\nz = 5\n".into(),
            },
        ];
        attach_file_history(&mut messages, &backups, r"D:\Proj");
        let e: Vec<&FileEdit> = messages.iter().map(|m| &m.tool_calls[0].edits[0]).collect();
        assert!(e[0].before.is_some() && e[1].before.is_none() && e[2].before.is_some());
        let Merged::Full {
            old,
            new,
            base,
            external,
        } = merge(&e, None)
        else {
            panic!("expected full merge");
        };
        assert_eq!(base, MergeBase::FileHistory);
        assert_eq!(old, "a = 1\nk = 1\nz = 0\n");
        assert_eq!(new, "a = 3\nk = 9\nz = 5\n");
        assert!(external);
    }

    #[test]
    fn patch_hunk_matches_last_line_without_newline() {
        let e = upd("f", &[("x\nlast\n", "x\nLAST\n")]);
        let Merged::Full { old, new, .. } = merge(&[&e], Some("top\nx\nLAST")) else {
            panic!("expected full merge");
        };
        assert_eq!(old, "top\nx\nlast");
        assert_eq!(new, "top\nx\nLAST");
    }
}
