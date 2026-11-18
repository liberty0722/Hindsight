//! 工具调用里的文件改动。
//!
//! - `extract_edits`:从各家改文件工具的原始参数里认出改了哪个文件、old → new 片段。
//!   Claude Code 的 Edit / MultiEdit / Write、With 的 replace_in_file / write_to_file、
//!   Codex 的 apply_patch(独立工具或 shell 里的 `apply_patch` 命令)都按参数形状识别,
//!   不按工具名逐家枚举,别家同形状的工具顺带支持。
//! - `diff_lines` / `collapse`:给详情页画 diff 用的行级 diff(LCS)+ 行内差异区间,
//!   以及长段未改动行的折叠。
//!
//! 只有片段,没有改动前后的整文件:会话只记了 old / new,磁盘上的文件之后可能又改过。

use std::ops::Range;

use serde_json::Value;

use crate::models::{EditHunk, EditKind, FileEdit};

/// 单个文件改动保留的片段总字节上限。远大于 MAX_TOOL_IO(16KB),整文件 Write 也基本够用
pub const MAX_EDIT_TEXT: usize = 256 * 1024;
/// LCS 表格的格子上限(old 行数 × new 行数,去掉首尾相同行之后)。超过就整段按删除 + 新增显示,
/// 防止一次大改动把渲染或扫描卡住
const MAX_LCS_CELLS: usize = 250_000;
/// Myers 的编辑距离上限。回溯表约 D² 个格子,1500 时十几 MB,超过就整段删 + 增
const MAX_MYERS_D: usize = 1500;

const PATH_KEYS: &[&str] = &[
    "file_path",
    "filePath",
    "path",
    "target_file",
    "targetFile",
    "file",
    "filename",
];
const OLD_KEYS: &[&str] = &["old_string", "oldString", "old_str", "old_text", "oldText"];
const NEW_KEYS: &[&str] = &["new_string", "newString", "new_str", "new_text", "newText"];
const CONTENT_KEYS: &[&str] = &[
    "content",
    "apply_content",
    "file_text",
    "fileText",
    "contents",
];
const MULTI_KEYS: &[&str] = &["edits", "replacements", "changes"];

const PATCH_BEGIN: &str = "*** Begin Patch";

/// 认出一次工具调用对文件的改动。不是改文件的调用返回空
pub fn extract_edits(name: &str, input: &Value) -> Vec<FileEdit> {
    let mut edits = raw_edits(name, input);
    for edit in &mut edits {
        finish(edit);
    }
    edits
}

fn raw_edits(name: &str, input: &Value) -> Vec<FileEdit> {
    if let Some(patch) = find_patch(input, 0) {
        let edits = parse_apply_patch(patch);
        if !edits.is_empty() {
            return edits;
        }
    }
    if !input.is_object() {
        return Vec::new();
    }
    let path = first_str(input, PATH_KEYS);

    // 一次调用多处替换(MultiEdit 的 edits、别家的 replacements),每项也可能自带路径
    for key in MULTI_KEYS {
        let Some(items) = input.get(*key).and_then(Value::as_array) else {
            continue;
        };
        let mut out: Vec<FileEdit> = Vec::new();
        for item in items {
            let Some(hunk) = hunk_of(item) else { continue };
            let Some(p) = first_str(item, PATH_KEYS).or(path) else {
                continue;
            };
            match out.last_mut() {
                Some(last) if last.path == p => last.hunks.push(hunk),
                _ => out.push(new_edit(p, EditKind::Update, vec![hunk])),
            }
        }
        if !out.is_empty() {
            return out;
        }
    }

    let Some(path) = path else {
        return Vec::new();
    };
    if let Some(hunk) = hunk_of(input) {
        return vec![new_edit(path, EditKind::Update, vec![hunk])];
    }
    if is_write_tool(name) {
        if let Some(content) = first_str(input, CONTENT_KEYS) {
            let hunk = EditHunk {
                old: String::new(),
                new: content.to_string(),
            };
            return vec![new_edit(path, EditKind::Create, vec![hunk])];
        }
    }
    Vec::new()
}

/// 读文件类工具(Read / read_file / view_file 等)的目标路径。不是读文件的调用返回空
pub fn read_path(name: &str, input: &Value) -> Option<String> {
    if !is_read_tool(name) || !input.is_object() {
        return None;
    }
    first_str(input, PATH_KEYS).map(str::to_string)
}

fn is_read_tool(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower.contains("write") || lower.contains("create") || lower.contains("edit") {
        return false;
    }
    lower == "read"
        || lower.contains("read_file")
        || lower.contains("readfile")
        || lower.contains("view_file")
        || lower.contains("viewfile")
        || lower.contains("open_file")
}

fn is_write_tool(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    (lower.contains("write") || lower.contains("create")) && !lower.contains("todo")
}

const SNAPSHOT_KEYS: &[&str] = &["full_content", "fullContent", "new_content", "newContent"];

/// 工具结果里附带的改动后整文件。必须包含这次改动写进去的每段新内容才算数,
/// 防止把别的字段(或改动前的内容)当成快照。超过 MAX_EDIT_TEXT 不留
pub fn after_snapshot(edit: &FileEdit, output: &str) -> Option<String> {
    if edit.kind == EditKind::Delete || !output.trim_start().starts_with('{') {
        return None;
    }
    let v: Value = serde_json::from_str(output).ok()?;
    let text = SNAPSHOT_KEYS
        .iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))?;
    if text.len() > MAX_EDIT_TEXT {
        return None;
    }
    let text = text.replace("\r\n", "\n");
    let contains_all = edit.hunks.iter().all(|h| {
        let new = h.new.replace("\r\n", "\n");
        new.is_empty() || text.contains(new.as_str()) || text.contains(new.trim_end_matches('\n'))
    });
    contains_all.then_some(text)
}

fn new_edit(path: &str, kind: EditKind, hunks: Vec<EditHunk>) -> FileEdit {
    FileEdit {
        path: path.to_string(),
        kind,
        hunks,
        truncated: false,
        added: 0,
        removed: 0,
        after: None,
        before: None,
    }
}

fn first_str<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))
        .filter(|s| !s.is_empty())
}

fn hunk_of(v: &Value) -> Option<EditHunk> {
    let old = OLD_KEYS
        .iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))?;
    let new = NEW_KEYS
        .iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))?;
    Some(EditHunk {
        old: old.to_string(),
        new: new.to_string(),
    })
}

/// 参数里任意位置的 apply_patch 正文:Codex 的 custom_tool_call 直接给字符串,
/// shell 调用则藏在 command 数组里
fn find_patch(v: &Value, depth: usize) -> Option<&str> {
    if depth > 4 {
        return None;
    }
    match v {
        Value::String(s) => s.contains(PATCH_BEGIN).then_some(s.as_str()),
        Value::Array(items) => items.iter().find_map(|i| find_patch(i, depth + 1)),
        Value::Object(map) => map.values().find_map(|i| find_patch(i, depth + 1)),
        _ => None,
    }
}

/// Codex apply_patch 格式:`*** Update/Add/Delete File: path`,`@@` 分段,
/// 行首 ` ` / `-` / `+` 分别是上下文、删除、新增
pub fn parse_apply_patch(text: &str) -> Vec<FileEdit> {
    let mut edits: Vec<FileEdit> = Vec::new();
    let mut old = String::new();
    let mut new = String::new();

    fn flush(edits: &mut [FileEdit], old: &mut String, new: &mut String) {
        if old.is_empty() && new.is_empty() {
            return;
        }
        if let Some(edit) = edits.last_mut() {
            edit.hunks.push(EditHunk {
                old: std::mem::take(old),
                new: std::mem::take(new),
            });
        } else {
            old.clear();
            new.clear();
        }
    }

    let Some(start) = text.find(PATCH_BEGIN) else {
        return edits;
    };
    for line in text[start..].lines() {
        let header = [
            ("*** Update File: ", EditKind::Update),
            ("*** Add File: ", EditKind::Create),
            ("*** Delete File: ", EditKind::Delete),
        ]
        .into_iter()
        .find_map(|(prefix, kind)| line.strip_prefix(prefix).map(|p| (p.trim(), kind)));
        if let Some((path, kind)) = header {
            flush(&mut edits, &mut old, &mut new);
            edits.push(new_edit(path, kind, Vec::new()));
            continue;
        }
        if let Some(to) = line.strip_prefix("*** Move to: ") {
            if let Some(edit) = edits.last_mut() {
                edit.path = format!("{} → {}", edit.path, to.trim());
            }
            continue;
        }
        if line.starts_with("*** ") {
            // Begin / End Patch、End of File
            if line.starts_with("*** End Patch") {
                flush(&mut edits, &mut old, &mut new);
                break;
            }
            continue;
        }
        if line.starts_with("@@") {
            flush(&mut edits, &mut old, &mut new);
            continue;
        }
        if edits.is_empty() {
            continue;
        }
        let (tag, rest) = match line.chars().next() {
            Some('+') => ('+', &line[1..]),
            Some('-') => ('-', &line[1..]),
            Some(' ') => (' ', &line[1..]),
            None => (' ', ""),
            // 不合规的行(没有前缀)按上下文收
            Some(_) => (' ', line),
        };
        if tag != '+' {
            old.push_str(rest);
            old.push('\n');
        }
        if tag != '-' {
            new.push_str(rest);
            new.push('\n');
        }
    }
    flush(&mut edits, &mut old, &mut new);
    edits
}

/// 截到 MAX_EDIT_TEXT 并算好增删行数
fn finish(edit: &mut FileEdit) {
    let mut budget = MAX_EDIT_TEXT;
    for hunk in &mut edit.hunks {
        for text in [&mut hunk.old, &mut hunk.new] {
            if text.len() > budget {
                let mut end = budget;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                // 截在整行上,避免最后一行半截参与 diff
                let end = text[..end].rfind('\n').map_or(end, |nl| nl + 1);
                text.truncate(end);
                edit.truncated = true;
            }
            budget -= text.len();
        }
    }
    let (mut added, mut removed) = (0, 0);
    for hunk in &edit.hunks {
        for line in diff_lines(&hunk.old, &hunk.new) {
            match line.tag {
                LineTag::Added => added += 1,
                LineTag::Removed => removed += 1,
                LineTag::Context => {}
            }
        }
    }
    edit.added = added;
    edit.removed = removed;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineTag {
    Context,
    Removed,
    Added,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub tag: LineTag,
    pub text: String,
    /// 行内真正改了的字节区间(与配对的删除/新增行比较得出),整行都不同时为空
    pub emphasis: Vec<Range<usize>>,
}

/// old → new 的逐行 diff。同一段改动里删除行在前、新增行在后,并按位置两两配对算行内差异
pub fn diff_lines(old: &str, new: &str) -> Vec<DiffLine> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();

    let prefix = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let mid_a = &a[prefix..a.len() - suffix];
    let mid_b = &b[prefix..b.len() - suffix];

    let mut tags: Vec<(LineTag, &str)> = Vec::with_capacity(a.len() + b.len());
    tags.extend(a[..prefix].iter().map(|l| (LineTag::Context, *l)));
    tags.extend(lcs_tags(mid_a, mid_b));
    tags.extend(a[a.len() - suffix..].iter().map(|l| (LineTag::Context, *l)));

    let mut out: Vec<DiffLine> = Vec::with_capacity(tags.len());
    let mut i = 0;
    while i < tags.len() {
        if tags[i].0 == LineTag::Context {
            out.push(plain(LineTag::Context, tags[i].1));
            i += 1;
            continue;
        }
        let run_end = tags[i..]
            .iter()
            .position(|(t, _)| *t == LineTag::Context)
            .map_or(tags.len(), |p| i + p);
        let removed: Vec<&str> = tags[i..run_end]
            .iter()
            .filter(|(t, _)| *t == LineTag::Removed)
            .map(|(_, l)| *l)
            .collect();
        let added: Vec<&str> = tags[i..run_end]
            .iter()
            .filter(|(t, _)| *t == LineTag::Added)
            .map(|(_, l)| *l)
            .collect();
        let mut rem_lines: Vec<DiffLine> =
            removed.iter().map(|l| plain(LineTag::Removed, l)).collect();
        let mut add_lines: Vec<DiffLine> = added.iter().map(|l| plain(LineTag::Added, l)).collect();
        for (r, a) in rem_lines.iter_mut().zip(add_lines.iter_mut()) {
            if let Some((er, ea)) = inline_emphasis(&r.text, &a.text) {
                r.emphasis = er;
                a.emphasis = ea;
            }
        }
        out.extend(rem_lines);
        out.extend(add_lines);
        i = run_end;
    }
    out
}

fn plain(tag: LineTag, text: &str) -> DiffLine {
    DiffLine {
        tag,
        text: text.to_string(),
        emphasis: Vec::new(),
    }
}

fn lcs_tags<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<(LineTag, &'a str)> {
    let (n, m) = (a.len(), b.len());
    if n == 0 || m == 0 {
        let mut out: Vec<(LineTag, &str)> = a.iter().map(|l| (LineTag::Removed, *l)).collect();
        out.extend(b.iter().map(|l| (LineTag::Added, *l)));
        return out;
    }
    if n.saturating_mul(m) > MAX_LCS_CELLS {
        // 整文件对比(会话级合并视图)常是几千行里改几十行:Myers 的代价跟改动量走,不跟文件大小走
        if let Some(out) = myers_tags(a, b) {
            return out;
        }
        let mut out: Vec<(LineTag, &str)> = a.iter().map(|l| (LineTag::Removed, *l)).collect();
        out.extend(b.iter().map(|l| (LineTag::Added, *l)));
        return out;
    }
    // dp[i][j] = a[i..] 与 b[j..] 的 LCS 长度
    let w = m + 1;
    let mut dp = vec![0u32; (n + 1) * w];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i * w + j] = if a[i] == b[j] {
                dp[(i + 1) * w + j + 1] + 1
            } else {
                dp[(i + 1) * w + j].max(dp[i * w + j + 1])
            };
        }
    }
    let mut out = Vec::with_capacity(n + m);
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push((LineTag::Context, a[i]));
            i += 1;
            j += 1;
        } else if dp[(i + 1) * w + j] >= dp[i * w + j + 1] {
            out.push((LineTag::Removed, a[i]));
            i += 1;
        } else {
            out.push((LineTag::Added, b[j]));
            j += 1;
        }
    }
    out.extend(a[i..].iter().map(|l| (LineTag::Removed, *l)));
    out.extend(b[j..].iter().map(|l| (LineTag::Added, *l)));
    out
}

type Emphasis = (Vec<Range<usize>>, Vec<Range<usize>>);

/// Myers O((N+M)·D) 最短编辑序列。D 超过上限(两边几乎完全不同)返回 None,由调用方整段删 + 增
fn myers_tags<'a>(a: &[&'a str], b: &[&'a str]) -> Option<Vec<(LineTag, &'a str)>> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    let max_d = (a.len() + b.len()).min(MAX_MYERS_D) as isize;
    let off = max_d + 1;
    let mut v = vec![0isize; (2 * max_d + 3) as usize];
    // trace[d] = 第 d 步开始前 k ∈ [-d, d] 的 v(回溯第 d 步要用它)
    let mut trace: Vec<Vec<isize>> = Vec::new();
    let mut found = None;
    'outer: for d in 0..=max_d {
        trace.push(v[(off - d) as usize..=(off + d) as usize].to_vec());
        let mut k = -d;
        while k <= d {
            let ix = (off + k) as usize;
            let mut x = if k == -d || (k != d && v[ix - 1] < v[ix + 1]) {
                v[ix + 1]
            } else {
                v[ix - 1] + 1
            };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[ix] = x;
            if x >= n && y >= m {
                found = Some(d);
                break 'outer;
            }
            k += 2;
        }
    }
    let found = found?;
    let mut rev: Vec<(LineTag, &str)> = Vec::with_capacity(a.len() + b.len());
    let (mut x, mut y) = (n, m);
    for d in (1..=found).rev() {
        let prev = &trace[d as usize];
        let get = |k: isize| prev[(k + d) as usize];
        let k = x - y;
        let prev_k = if k == -d || (k != d && get(k - 1) < get(k + 1)) {
            k + 1
        } else {
            k - 1
        };
        let prev_x = get(prev_k);
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            rev.push((LineTag::Context, a[(x - 1) as usize]));
            x -= 1;
            y -= 1;
        }
        if x == prev_x {
            rev.push((LineTag::Added, b[(y - 1) as usize]));
            y -= 1;
        } else {
            rev.push((LineTag::Removed, a[(x - 1) as usize]));
            x -= 1;
        }
    }
    while x > 0 && y > 0 {
        rev.push((LineTag::Context, a[(x - 1) as usize]));
        x -= 1;
        y -= 1;
    }
    rev.reverse();
    Some(rev)
}

/// 一对删除/新增行的行内差异:去掉共同前缀、后缀,中间就是改了的部分。
/// 两行共同的有效内容太少(基本是两行不同的代码)时不标,整行底色已经够了
fn inline_emphasis(old: &str, new: &str) -> Option<Emphasis> {
    let pre: usize = old
        .chars()
        .zip(new.chars())
        .take_while(|(x, y)| x == y)
        .map(|(c, _)| c.len_utf8())
        .sum();
    let (old_rest, new_rest) = (&old[pre..], &new[pre..]);
    let suf: usize = old_rest
        .chars()
        .rev()
        .zip(new_rest.chars().rev())
        .take_while(|(x, y)| x == y)
        .map(|(c, _)| c.len_utf8())
        .sum();
    let shared = old[..pre].trim_start().chars().count() + old[old.len() - suf..].chars().count();
    let shortest = old.trim().chars().count().min(new.trim().chars().count());
    if shared < 3 || shared * 3 < shortest {
        return None;
    }
    let range = |s: &str| {
        let r = pre..s.len() - suf;
        if r.is_empty() {
            Vec::new()
        } else {
            vec![r]
        }
    };
    Some((range(old), range(new)))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffRow {
    Line(DiffLine),
    /// 折叠掉的连续未改动行数
    Skip(usize),
}

/// 改动行前后各留 `context` 行上下文,更长的未改动段折叠成一行
pub fn collapse(lines: Vec<DiffLine>, context: usize) -> Vec<DiffRow> {
    let changed: Vec<bool> = lines.iter().map(|l| l.tag != LineTag::Context).collect();
    if !changed.iter().any(|c| *c) {
        return lines.into_iter().map(DiffRow::Line).collect();
    }
    let n = lines.len();
    // 每行到最近改动行的距离是否在 context 以内
    let mut keep = vec![false; n];
    let mut last: Option<usize> = None;
    for i in 0..n {
        if changed[i] {
            last = Some(i);
        }
        if last.is_some_and(|l| i - l <= context) {
            keep[i] = true;
        }
    }
    let mut next: Option<usize> = None;
    for i in (0..n).rev() {
        if changed[i] {
            next = Some(i);
        }
        if next.is_some_and(|nx| nx - i <= context) {
            keep[i] = true;
        }
    }
    let mut out = Vec::new();
    let mut skipped = 0;
    for (line, keep) in lines.into_iter().zip(keep) {
        if keep {
            if skipped > 0 {
                out.push(DiffRow::Skip(skipped));
                skipped = 0;
            }
            out.push(DiffRow::Line(line));
        } else {
            skipped += 1;
        }
    }
    if skipped > 0 {
        out.push(DiffRow::Skip(skipped));
    }
    out
}

/// 并排视图的一格:行号 + 文本 + 行内改动高亮
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideCell {
    pub line_no: usize,
    pub text: String,
    pub emphasis: Vec<Range<usize>>,
}

/// 并排视图的一行:左右各一格,同一水平位置上「改动前 → 改动后」
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SideRow {
    /// 未改动:左右内容相同,各自的行号(删/增多的地方两侧行号会错开)
    Context(SideCell, SideCell),
    /// 一处改动:左=改动前(删)、右=改动后(增);纯新增/纯删除时一侧为 None
    Changed {
        old: Option<SideCell>,
        new: Option<SideCell>,
    },
}

fn side_cell(line_no: usize, line: &DiffLine) -> SideCell {
    SideCell {
        line_no,
        text: line.text.clone(),
        emphasis: line.emphasis.clone(),
    }
}

/// 把逐行 diff 排成左右对齐的行:同一段改动里的删除行、新增行按序号两两配对,
/// 上下文行左右各放一份,两侧行号分别累计。配对后的行天然横向对齐、可同步滚动
pub fn align_side_by_side(lines: &[DiffLine]) -> Vec<SideRow> {
    let (mut old_no, mut new_no) = (0usize, 0usize);
    let tagged: Vec<(Option<usize>, Option<usize>, &DiffLine)> = lines
        .iter()
        .map(|l| match l.tag {
            LineTag::Context => {
                old_no += 1;
                new_no += 1;
                (Some(old_no), Some(new_no), l)
            }
            LineTag::Removed => {
                old_no += 1;
                (Some(old_no), None, l)
            }
            LineTag::Added => {
                new_no += 1;
                (None, Some(new_no), l)
            }
        })
        .collect();

    let mut out = Vec::new();
    let mut i = 0;
    while i < tagged.len() {
        let (o, n, l) = tagged[i];
        if l.tag == LineTag::Context {
            out.push(SideRow::Context(
                side_cell(o.unwrap(), l),
                side_cell(n.unwrap(), l),
            ));
            i += 1;
            continue;
        }
        // 一段连续改动:删除行、新增行按顺序配成对,短的一侧留空
        let end = tagged[i..]
            .iter()
            .position(|t| t.2.tag == LineTag::Context)
            .map_or(tagged.len(), |p| i + p);
        let mut olds = Vec::new();
        let mut news = Vec::new();
        for (o, n, l) in &tagged[i..end] {
            match l.tag {
                LineTag::Removed => olds.push(side_cell(o.unwrap(), l)),
                LineTag::Added => news.push(side_cell(n.unwrap(), l)),
                LineTag::Context => unreachable!("change run can't contain context"),
            }
        }
        let count = olds.len().max(news.len());
        let mut oi = olds.into_iter();
        let mut ni = news.into_iter();
        for _ in 0..count {
            out.push(SideRow::Changed {
                old: oi.next(),
                new: ni.next(),
            });
        }
        i = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tags(lines: &[DiffLine]) -> String {
        lines
            .iter()
            .map(|l| match l.tag {
                LineTag::Context => ' ',
                LineTag::Removed => '-',
                LineTag::Added => '+',
            })
            .collect()
    }

    #[test]
    fn claude_edit_and_with_replace_in_file() {
        for (name, input) in [
            (
                "Edit",
                json!({"file_path": "src/a.rs", "old_string": "a\nb\nc", "new_string": "a\nB\nc"}),
            ),
            (
                "replace_in_file",
                json!({"file_path": "src/a.rs", "old_string": "a\nb\nc", "new_string": "a\nB\nc", "apply_mode": "apply_only"}),
            ),
        ] {
            let edits = extract_edits(name, &input);
            assert_eq!(edits.len(), 1, "{name}");
            assert_eq!(edits[0].path, "src/a.rs");
            assert_eq!(edits[0].kind, EditKind::Update);
            assert_eq!((edits[0].added, edits[0].removed), (1, 1));
        }
    }

    #[test]
    fn multi_edit_groups_hunks_by_file() {
        let input = json!({"file_path": "x.lua", "edits": [
            {"old_string": "1", "new_string": "2"},
            {"old_string": "3", "new_string": "4\n5"},
        ]});
        let edits = extract_edits("MultiEdit", &input);
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].hunks.len(), 2);
        assert_eq!((edits[0].added, edits[0].removed), (3, 2));
    }

    #[test]
    fn write_tools_are_creates_and_others_are_ignored() {
        let w = extract_edits(
            "write_to_file",
            &json!({"file_path": "a.ps1", "apply_content": "x\ny\n"}),
        );
        assert_eq!(w[0].kind, EditKind::Create);
        assert_eq!(w[0].added, 2);
        assert!(extract_edits("read_file", &json!({"path": "a", "content": "x"})).is_empty());
        assert!(extract_edits("TodoWrite", &json!({"todos": [{"content": "x"}]})).is_empty());
        assert!(extract_edits("grep_search", &json!({"query": "x"})).is_empty());
    }

    #[test]
    fn codex_apply_patch_in_string_and_shell_command() {
        let patch = "*** Begin Patch\n*** Update File: src/lib.rs\n@@ fn main\n fn main() {\n-    old();\n+    new();\n }\n*** Add File: b.txt\n+hello\n*** Delete File: c.txt\n*** End Patch";
        for input in [
            Value::String(patch.into()),
            json!({"command": ["apply_patch", patch]}),
        ] {
            let edits = extract_edits("apply_patch", &input);
            assert_eq!(edits.len(), 3);
            assert_eq!(edits[0].path, "src/lib.rs");
            assert_eq!(edits[0].hunks[0].old, "fn main() {\n    old();\n}\n");
            assert_eq!(edits[0].hunks[0].new, "fn main() {\n    new();\n}\n");
            assert_eq!(edits[1].kind, EditKind::Create);
            assert_eq!(edits[1].hunks[0].new, "hello\n");
            assert_eq!(edits[2].kind, EditKind::Delete);
        }
    }

    #[test]
    fn diff_groups_removals_before_additions_with_inline_emphasis() {
        let lines = diff_lines(
            "keep\n    self:_SelectTab(TAB_STAGE1)\nend",
            "keep\n    self:_SelectTab(TAB_STAGE2)\n    extra()\nend",
        );
        assert_eq!(tags(&lines), " -++ ");
        let old = &lines[1];
        let new = &lines[2];
        assert_eq!(&old.text[old.emphasis[0].clone()], "1");
        assert_eq!(&new.text[new.emphasis[0].clone()], "2");
        assert!(lines[3].emphasis.is_empty());
    }

    #[test]
    fn unrelated_lines_get_no_inline_emphasis() {
        let lines = diff_lines("    local a = 1", "    return foo()");
        assert!(lines.iter().all(|l| l.emphasis.is_empty()));
    }

    #[test]
    fn collapse_keeps_context_around_changes() {
        let old: String = (0..20).map(|i| format!("l{i}\n")).collect();
        let new = old.replace("l10\n", "L10\n");
        let rows = collapse(diff_lines(&old, &new), 3);
        assert_eq!(rows.first(), Some(&DiffRow::Skip(7)));
        assert_eq!(rows.last(), Some(&DiffRow::Skip(6)));
        assert_eq!(rows.len(), 2 + 3 + 2 + 3);
    }

    #[test]
    fn side_by_side_pairs_removed_and_added_lines() {
        use super::{align_side_by_side, SideRow};
        let lines = diff_lines(
            "top\nkeep\n  self:Check()\nend",
            "top\nkeep\n  self:CheckCoin()\n  self:DoBuy()\nend",
        );
        let rows = align_side_by_side(&lines);
        assert!(matches!(&rows[0], SideRow::Context(a, b) if a.line_no == 1 && b.line_no == 1));
        let changed: Vec<&SideRow> = rows
            .iter()
            .filter(|r| matches!(r, SideRow::Changed { .. }))
            .collect();
        assert_eq!(changed.len(), 2);
        match changed[0] {
            SideRow::Changed {
                old: Some(a),
                new: Some(b),
            } => {
                assert_eq!(a.line_no, 3);
                assert_eq!(b.line_no, 3);
                // 改动只是插入了 "Coin":旧行没有删除区间,新行高亮插入的部分
                assert!(a.emphasis.is_empty());
                assert_eq!(&b.text[b.emphasis[0].clone()], "Coin");
            }
            _ => panic!("first pair should be a change"),
        }
        assert!(matches!(
            changed[1],
            SideRow::Changed {
                old: None,
                new: Some(_)
            }
        ));
        assert!(
            matches!(rows.last(), Some(SideRow::Context(a, b)) if a.line_no == 4 && b.line_no == 5)
        );
    }

    #[test]
    fn huge_edits_are_truncated_on_line_boundary() {
        let big = "0123456789\n".repeat(MAX_EDIT_TEXT / 11 + 10);
        let edits = extract_edits("Write", &json!({"file_path": "a", "content": big}));
        assert!(edits[0].truncated);
        assert!(edits[0].hunks[0].new.len() <= MAX_EDIT_TEXT);
        assert!(edits[0].hunks[0].new.ends_with('\n'));
    }

    #[test]
    fn large_files_use_myers_and_keep_unchanged_lines_as_context() {
        let old: String = (0..3000).map(|i| format!("line {i}\n")).collect();
        let new = old
            .replace("line 10\n", "line ten\n")
            .replace("line 2990\n", "")
            .replace("line 1500\n", "line 1500\nextra\n");
        let lines = diff_lines(&old, &new);
        let count = |tag| lines.iter().filter(|l| l.tag == tag).count();
        assert_eq!(count(LineTag::Removed), 2);
        assert_eq!(count(LineTag::Added), 2);
        assert_eq!(count(LineTag::Context), 2998);
    }
}
