//! 会话级改动面板:详情页头部的「N 个文件」按钮打开,整块替换聊天正文。
//! 左栏是这条会话改过的文件(临时脚本收成一组),右栏是选中文件的 diff,两种看法:
//! - 合并:整条会话对这个文件的净改动。能还原出整文件时是带真实行号的整文件对比,
//!   否则退回片段合并(`hindsight_core::changes::merge`)。
//! - 逐次:每次改动一张卡,可以跳回对应的消息。
//!
//! 合并结果在选中文件 / 切到合并模式时算好缓存,render 里只画。

use super::diff_view::{
    diff_row_numbered, expand_tabs, file_edit_view, note_row, skip_row, split_path, stat_badge,
};
use super::*;
use gpui::ScrollAnchor;
use gpui_component::{notification::Notification, WindowExt as _};
use hindsight_core::changes::{
    self, FileChanges, MergeBase, Merged, ReadItem, ReadTurn, SnippetReason,
};
use hindsight_core::diff::{
    align_side_by_side, collapse, diff_lines, DiffLine, DiffRow, LineTag, SideCell, SideRow,
};
use hindsight_core::relations::{self, RelatedFile, SymbolDef};

/// 合并视图里改动行前后保留的上下文行数
const MERGED_CONTEXT: usize = 3;
/// 右栏最多画多少行,再多的提示数量
const MAX_PANEL_ROWS: usize = 3000;
/// 磁盘文件超过这个大小就不读(改动片段本身也有 256KB 上限)
const MAX_DISK_BYTES: u64 = 4 * 1024 * 1024;
/// 左栏宽度
const LIST_WIDTH: f32 = 220.;

/// 合并视图的排版
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub(super) enum DiffLayout {
    /// 上下排列的内联 diff(默认)
    #[default]
    Inline,
    /// 左右对照:改动前在左、改动后在右
    SideBySide,
}

/// 一段未改动行的展开/收起状态按「文件 + 视图 + 是否只看改动 + 行下标」分别记,
/// 不同折叠策略下同一行下标指的不是同一段,不能混
type ExpandKey = (usize, DiffLayout, bool, usize);

/// 左栏筛选:全部(按轮次时间线) / 只看改动 / 只看只读参考
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ListFilter {
    All,
    Changed,
    Reference,
}

/// 读过的文件的查看结果
enum ReadView {
    Text(Vec<String>),
    Unavailable(&'static str),
}

pub(super) struct ChangesPanel {
    /// 按文件归组的改动,加载完成时在后台算好
    pub(super) files: Rc<Vec<FileChanges>>,
    pub(super) open: bool,
    pub(super) selected: usize,
    /// false = 合并(默认),true = 逐次
    pub(super) per_edit: bool,
    /// 合并视图的排版(内联 / 并排),记住上次选择
    pub(super) layout: DiffLayout,
    /// 只看改动:不画未改动行
    pub(super) only_changes: bool,
    /// 专注:隐藏侧栏与会话列,让改动面板占满窗口(并排对比需要宽度)
    pub(super) focus: bool,
    pub(super) show_scratch: bool,
    pub(super) list_scroll: ScrollHandle,
    pub(super) diff_scroll: ScrollHandle,
    /// 当前渲染出的改动块锚点(每次渲染重建),上一处 / 下一处据此滚动
    pub(super) change_anchors: std::cell::RefCell<Vec<ScrollAnchor>>,
    /// 上一次跳转到的改动块下标,None 表示还未跳转
    pub(super) change_cursor: Option<usize>,
    /// 文件下标 → 合并结果
    merged: HashMap<usize, Rc<MergedView>>,
    /// 合并视图里展开了的未改动段
    expanded: HashSet<ExpandKey>,
    /// 左栏筛选:全部 / 改动 / 只读参考
    pub(super) filter: ListFilter,
    /// 会话里读过的文件,按轮次切分
    pub(super) reads: Rc<Vec<ReadTurn>>,
    /// 当前选中的文件(归一化路径 key),改动与读取共用同一个选择键
    pub(super) sel_key: Option<String>,
    /// 源码视图的虚拟列表滚动
    pub(super) read_scroll: UniformListScrollHandle,
    /// 读过的文件路径 → 内容,选中时才读盘
    read_cache: HashMap<String, Rc<ReadView>>,
    /// 第二期:标识符推断出的「可能相关」文件(后台算好)
    related: Rc<Vec<RelatedFile>>,
    /// 是否显示「可能相关」分组(推断有误报,允许关闭)
    show_related: bool,
    /// 第三期:项目里的符号定义索引(后台算好)
    symbols: Rc<Vec<SymbolDef>>,
    /// 点击标识符后要跳到的目标(文件 key + 1-based 行号),渲染消费后清空
    symbol_jump: std::cell::RefCell<Option<(String, usize)>>,
}

impl ChangesPanel {
    pub(super) fn new(files: Rc<Vec<FileChanges>>, reads: Rc<Vec<ReadTurn>>) -> Self {
        Self {
            files,
            filter: ListFilter::All,
            reads,
            sel_key: None,
            read_scroll: UniformListScrollHandle::new(),
            read_cache: HashMap::new(),
            related: Rc::new(Vec::new()),
            show_related: true,
            symbols: Rc::new(Vec::new()),
            symbol_jump: std::cell::RefCell::new(None),
            open: false,
            selected: 0,
            per_edit: false,
            layout: DiffLayout::SideBySide,
            only_changes: false,
            focus: true,
            show_scratch: false,
            list_scroll: ScrollHandle::new(),
            diff_scroll: ScrollHandle::new(),
            change_anchors: std::cell::RefCell::new(Vec::new()),
            change_cursor: None,
            merged: HashMap::new(),
            expanded: HashSet::new(),
        }
    }

    /// 后台算好第二/三期数据后补齐(其余状态保持 new 的默认值)
    pub(super) fn with_relations(
        mut self,
        related: Vec<RelatedFile>,
        symbols: Vec<SymbolDef>,
    ) -> Self {
        self.related = Rc::new(related);
        self.symbols = Rc::new(symbols);
        self
    }
}

type Numbered = (DiffLine, Option<usize>, Option<usize>);

enum NumberedRow {
    Line(DiffLine, Option<usize>, Option<usize>),
    /// 折叠掉的未改动行,原样留着,点开就能看
    Skip(Vec<Numbered>),
}

/// 并排视图折叠后的一行:一条对齐行,或一段折叠起来的未改动对齐行
enum SideFoldRow {
    Line(SideRow),
    Skip(Vec<SideRow>),
}

/// 改动行前后各留 context 行,其余标成可折叠(与 core::collapse 同款)
fn fold_mask(changed: &[bool], context: usize) -> Vec<bool> {
    let n = changed.len();
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
    keep
}

fn collapse_side(rows: Vec<SideRow>, context: usize) -> Vec<SideFoldRow> {
    let changed: Vec<bool> = rows
        .iter()
        .map(|r| matches!(r, SideRow::Changed { .. }))
        .collect();
    let keep = fold_mask(&changed, context);
    let mut out = Vec::new();
    let mut skipped: Vec<SideRow> = Vec::new();
    for (row, keep) in rows.into_iter().zip(keep) {
        if keep {
            if !skipped.is_empty() {
                out.push(SideFoldRow::Skip(std::mem::take(&mut skipped)));
            }
            out.push(SideFoldRow::Line(row));
        } else {
            skipped.push(row);
        }
    }
    if !skipped.is_empty() {
        out.push(SideFoldRow::Skip(skipped));
    }
    out
}

enum MergedView {
    Full {
        /// 内联,改动行前后留 MERGED_CONTEXT 行上下文
        rows: Vec<NumberedRow>,
        /// 内联,只看改动(不留上下文)
        rows_flat: Vec<NumberedRow>,
        /// 并排,留上下文
        side: Vec<SideFoldRow>,
        /// 并排,只看改动
        side_flat: Vec<SideFoldRow>,
        /// 最大行号,决定行号栏宽度
        max_line: usize,
        /// 整文件对齐后的总行数
        total_side_lines: usize,
        added: usize,
        removed: usize,
        base: MergeBase,
        external: bool,
    },
    Snippets {
        /// (来源改动下标, 折叠后的行)
        hunks: Vec<(Vec<usize>, Vec<DiffRow>)>,
        added: usize,
        removed: usize,
        reason: SnippetReason,
    },
}

impl MergedView {
    fn totals(&self) -> (usize, usize) {
        match self {
            MergedView::Full { added, removed, .. }
            | MergedView::Snippets { added, removed, .. } => (*added, *removed),
        }
    }
}

fn count(lines: &[DiffLine]) -> (usize, usize) {
    lines.iter().fold((0, 0), |(a, r), l| match l.tag {
        LineTag::Added => (a + 1, r),
        LineTag::Removed => (a, r + 1),
        LineTag::Context => (a, r),
    })
}

fn numbered_fold(
    lines: &[DiffLine],
    nums: &[(Option<usize>, Option<usize>)],
    context: usize,
) -> Vec<NumberedRow> {
    let all: Vec<Numbered> = lines
        .iter()
        .cloned()
        .zip(nums.iter().copied())
        .map(|(l, (a, b))| (l, a, b))
        .collect();
    let mut i = 0usize;
    collapse(lines.to_vec(), context)
        .into_iter()
        .map(|row| match row {
            DiffRow::Line(l) => {
                let (a, b) = nums.get(i).copied().unwrap_or((None, None));
                i += 1;
                NumberedRow::Line(l, a, b)
            }
            DiffRow::Skip(k) => {
                let end = (i + k).min(all.len());
                let hidden = all.get(i..end).map(<[_]>::to_vec).unwrap_or_default();
                i += k;
                NumberedRow::Skip(hidden)
            }
        })
        .collect()
}

fn full_view(old: &str, new: &str, base: MergeBase, external: bool) -> MergedView {
    let lines = diff_lines(&expand_tabs(old), &expand_tabs(new));
    let (added, removed) = count(&lines);
    // 行号在折叠前按顺序算好,折叠后按 Skip 的行数跳过
    let mut nums = Vec::with_capacity(lines.len());
    let (mut o, mut n) = (0usize, 0usize);
    for l in &lines {
        nums.push(match l.tag {
            LineTag::Context => {
                o += 1;
                n += 1;
                (Some(o), Some(n))
            }
            LineTag::Removed => {
                o += 1;
                (Some(o), None)
            }
            LineTag::Added => {
                n += 1;
                (None, Some(n))
            }
        });
    }
    let rows = numbered_fold(&lines, &nums, MERGED_CONTEXT);
    let rows_flat = numbered_fold(&lines, &nums, 0);
    let side_all = align_side_by_side(&lines);
    let total_side_lines = side_all.len();
    let side = collapse_side(side_all.clone(), MERGED_CONTEXT);
    let side_flat = collapse_side(side_all, 0);
    MergedView::Full {
        rows,
        rows_flat,
        side,
        side_flat,
        max_line: o.max(n),
        total_side_lines,
        added,
        removed,
        base,
        external,
    }
}

fn read_disk(path: &str, project_root: &str) -> Option<String> {
    let p = PathBuf::from(expand_tilde(path));
    let p = if p.is_absolute() || project_root.is_empty() {
        p
    } else {
        PathBuf::from(expand_tilde(project_root)).join(p)
    };
    let meta = std::fs::metadata(&p).ok()?;
    if !meta.is_file() || meta.len() > MAX_DISK_BYTES {
        return None;
    }
    let text = std::fs::read_to_string(&p).ok()?;
    Some(if text.contains("\r\n") {
        text.replace("\r\n", "\n")
    } else {
        text
    })
}

fn compute(file: &FileChanges, messages: &[TranscriptMessage], meta: &SessionMeta) -> MergedView {
    let edits: Vec<&FileEdit> = file
        .edits
        .iter()
        .filter_map(|r| changes::edit_at(messages, r))
        .collect();
    // 远程会话的文件不在本机,读到的同名文件不可信
    let disk = if meta.host.is_empty() {
        read_disk(&file.path, &meta.project_path)
    } else {
        None
    };
    match changes::merge(&edits, disk.as_deref()) {
        Merged::Full {
            old,
            new,
            base,
            external,
        } => full_view(&old, &new, base, external),
        Merged::Snippets { hunks, reason } => {
            let (mut added, mut removed) = (0, 0);
            let hunks = hunks
                .into_iter()
                .map(|h| {
                    let lines = diff_lines(&expand_tabs(&h.old), &expand_tabs(&h.new));
                    let (a, r) = count(&lines);
                    added += a;
                    removed += r;
                    (h.sources, collapse(lines, MERGED_CONTEXT))
                })
                .collect();
            MergedView::Snippets {
                hunks,
                added,
                removed,
                reason,
            }
        }
    }
}

fn ensure_merged(detail: &mut DetailState) {
    let panel = &detail.changes;
    if panel.per_edit || panel.merged.contains_key(&panel.selected) {
        return;
    }
    let Some(file) = panel.files.get(panel.selected) else {
        return;
    };
    let view = compute(file, &detail.transcript, &detail.meta);
    let selected = panel.selected;
    detail.changes.merged.insert(selected, Rc::new(view));
}

/// 读过的文件内容。远程会话的文件不在本机,读到的同名文件不可信
fn read_view(path: &str, meta: &SessionMeta) -> ReadView {
    if !meta.host.is_empty() {
        return ReadView::Unavailable(t("Remote session: the file isn't on this machine"));
    }
    match read_disk(path, &meta.project_path) {
        Some(text) => ReadView::Text(text.lines().map(str::to_string).collect()),
        None => ReadView::Unavailable(t(
            "Can't read this file here: missing, too large, or not text",
        )),
    }
}

/// 去重后的文件数(同一文件在多轮读过只算一次)
fn distinct_reads(reads: &[ReadTurn]) -> usize {
    reads
        .iter()
        .flat_map(|t| t.files.iter().map(|f| f.path.as_str()))
        .collect::<HashSet<_>>()
        .len()
}

/// 轮次标题行:首条消息之前,或「第 N 轮 · 摘要」
fn read_turn_header(turn: &ReadTurn, theme: &gpui_component::Theme) -> AnyElement {
    let title: SharedString = if turn.index == 0 {
        t("Before the first message").into()
    } else {
        crate::tf!("Turn {}", turn.index).into()
    };
    h_flex()
        .w_full()
        .min_w_0()
        .gap(SPACE_SM)
        .px(SPACE_SM)
        .pt(zpx(8.))
        .child(
            div()
                .flex_shrink_0()
                .text_size(FONT_LABEL)
                .font_medium()
                .text_color(theme.muted_foreground)
                .child(title),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_size(FONT_LABEL)
                .text_color(theme.muted_foreground)
                .child(turn.prompt.clone()),
        )
        .into_any_element()
}

/// 选中项在改动列表里的下标(只认改动过的文件)
fn sel_change_ix(detail: &DetailState) -> Option<usize> {
    let key = detail.changes.sel_key.as_deref()?;
    let root = detail.meta.project_path.as_str();
    detail
        .changes
        .files
        .iter()
        .position(|f| changes::path_key(&f.path, root) == key)
}

/// 选中项的源码路径:改动过的取改动路径,否则取读取项路径
fn source_path(detail: &DetailState) -> Option<String> {
    let key = detail.changes.sel_key.as_deref()?;
    let root = detail.meta.project_path.as_str();
    if let Some(f) = detail
        .changes
        .files
        .iter()
        .find(|f| changes::path_key(&f.path, root) == key)
    {
        return Some(f.path.clone());
    }
    let from_reads = detail
        .changes
        .reads
        .iter()
        .flat_map(|t| t.files.iter())
        .find(|f| changes::path_key(&f.path, root) == key)
        .map(|f| f.path.clone());
    from_reads.or_else(|| {
        detail
            .changes
            .related
            .iter()
            .find(|r| r.key == key)
            .map(|r| r.path.clone())
    })
}

/// 是否显示源码:只读文件(没有改动)恒为源码,改过的文件直接看改动 diff
fn showing_source(detail: &DetailState) -> bool {
    sel_change_ix(detail).is_none()
}

/// 同步改动下标,供合并视图与 diff 使用
fn sync_selected(detail: &mut DetailState) {
    if let Some(ix) = sel_change_ix(detail) {
        detail.changes.selected = ix;
    }
}

/// 打开面板时默认选中的第一个文件(优先改动列表)
fn first_entry_key(detail: &DetailState) -> Option<String> {
    let root = detail.meta.project_path.as_str();
    detail
        .changes
        .files
        .iter()
        .find(|f| !f.scratch)
        .or(detail.changes.files.first())
        .map(|f| changes::path_key(&f.path, root))
        .or_else(|| {
            detail
                .changes
                .reads
                .iter()
                .flat_map(|t| t.files.iter())
                .next()
                .map(|f| changes::path_key(&f.path, root))
        })
}

/// 选中且需要看源码时才读盘
fn ensure_read(detail: &mut DetailState) {
    if !showing_source(detail) {
        return;
    }
    let Some(path) = source_path(detail) else {
        return;
    };
    if detail.changes.read_cache.contains_key(&path) {
        return;
    }
    let view = read_view(&path, &detail.meta);
    detail.changes.read_cache.insert(path, Rc::new(view));
}

/// 用 VSCode 打开并定位到文件的某一行(1-based)。优先 PATH 里的 `code`,
/// Windows 再退回常见的 `code.cmd`。失败时返回错误串交给调用方弹通知。
fn open_in_vscode(path: &str, line: usize) -> Result<(), String> {
    let mut candidates: Vec<String> = vec!["code".to_string()];
    if cfg!(windows) {
        if let Some(local) = dirs::data_local_dir() {
            candidates.push(
                local
                    .join("Programs/Microsoft VS Code/bin/code.cmd")
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        candidates.push("C:\\Program Files\\Microsoft VS Code\\bin\\code.cmd".to_string());
    }
    let target = format!("{}:{}", path, line.max(1));
    let mut last_err = String::new();
    for bin in candidates {
        let res = std::process::Command::new(&bin)
            .arg("--goto")
            .arg(&target)
            .spawn();
        match res {
            Ok(_) => return Ok(()),
            Err(e) => last_err = format!("{bin}: {e}"),
        }
    }
    Err(last_err)
}

fn kind_label(kind: EditKind) -> Option<&'static str> {
    match kind {
        EditKind::Create => Some(t("New file")),
        EditKind::Delete => Some(t("Deleted")),
        EditKind::Update => None,
    }
}

fn pill(label: impl Into<SharedString>, theme: &gpui_component::Theme) -> Div {
    div()
        .flex_shrink_0()
        .px(zpx(6.))
        .rounded(RADIUS_BADGE)
        .text_size(FONT_LABEL)
        .text_color(theme.muted_foreground)
        .bg(theme.muted)
        .child(label.into())
}

impl Workbench {
    /// 改动面板是否处于专注模式(打开 + 用户开启)。关闭面板时不该继续隐藏侧栏
    pub(super) fn changes_focus(&self) -> bool {
        self.detail
            .as_ref()
            .is_some_and(|d| d.changes.open && d.changes.focus)
    }

    pub(super) fn toggle_changes(&mut self, cx: &mut Context<Self>) {
        let Some(detail) = &mut self.detail else {
            return;
        };
        detail.changes.open = !detail.changes.open;
        if detail.changes.open {
            if detail.changes.sel_key.is_none() {
                detail.changes.sel_key = first_entry_key(detail);
            }
            sync_selected(detail);
            ensure_merged(detail);
            ensure_read(detail);
        }
        cx.notify();
    }

    /// Esc 关闭;返回是否处理了
    pub(super) fn close_changes(&mut self, cx: &mut Context<Self>) -> bool {
        match &mut self.detail {
            Some(detail) if detail.changes.open && detail.zoom.is_none() => {
                detail.changes.open = false;
                cx.notify();
                true
            }
            _ => false,
        }
    }

    /// 统一的选中入口:改动与读取共用,切换到新文件时回到改动 diff
    fn select_entry(&mut self, key: String, cx: &mut Context<Self>) {
        let Some(detail) = &mut self.detail else {
            return;
        };
        if detail.changes.sel_key.as_deref() != Some(key.as_str()) {
            detail.changes.sel_key = Some(key);
            detail.changes.diff_scroll.set_offset(point(px(0.), px(0.)));
            detail.changes.change_cursor = None;
            detail.changes.read_scroll = UniformListScrollHandle::new();
        }
        sync_selected(detail);
        ensure_merged(detail);
        ensure_read(detail);
        cx.notify();
    }

    fn set_list_filter(&mut self, filter: ListFilter, cx: &mut Context<Self>) {
        let Some(detail) = &mut self.detail else {
            return;
        };
        detail.changes.filter = filter;
        cx.notify();
    }

    /// 第二期:「可能相关」是推断结果,允许一键隐藏
    fn toggle_related(&mut self, cx: &mut Context<Self>) {
        if let Some(detail) = &mut self.detail {
            detail.changes.show_related = !detail.changes.show_related;
        }
        cx.notify();
    }

    fn toggle_change_scratch(&mut self, cx: &mut Context<Self>) {
        if let Some(detail) = &mut self.detail {
            detail.changes.show_scratch = !detail.changes.show_scratch;
        }
        cx.notify();
    }

    /// 点开 / 收起合并视图里的一段未改动行
    fn toggle_change_skip(&mut self, row: usize, cx: &mut Context<Self>) {
        let Some(detail) = &mut self.detail else {
            return;
        };
        let key = (
            detail.changes.selected,
            detail.changes.layout,
            detail.changes.only_changes,
            row,
        );
        if !detail.changes.expanded.remove(&key) {
            detail.changes.expanded.insert(key);
        }
        cx.notify();
    }

    /// 第三期:点击标识符跳到它的定义。跨文件时选中该文件并滚到定义行。
    fn goto_definition(&mut self, name: String, cx: &mut Context<Self>) {
        let Some(detail) = &mut self.detail else {
            return;
        };
        let Some(def) = relations::find_definition(&detail.changes.symbols, &name) else {
            return;
        };
        let key = def.key.clone();
        let line = def.line;
        let same_file = detail.changes.sel_key.as_deref() == Some(key.as_str());
        if !same_file {
            detail.changes.sel_key = Some(key.clone());
            detail.changes.read_scroll = UniformListScrollHandle::new();
            detail.changes.read_cache.remove(&def.path);
            sync_selected(detail);
            ensure_read(detail);
        }
        *detail.changes.symbol_jump.borrow_mut() = Some((key, line));
        cx.notify();
    }

    /// 头部「在 VSCode 打开」按钮:打开当前文件并定位到指定行
    fn vscode_button(&self, path: &str, line: usize, cx: &Context<Self>) -> AnyElement {
        let path = path.to_string();
        Button::new("chg-open-vscode")
            .ghost()
            .small()
            .rounded(RADIUS_BUTTON)
            .icon(icon("icons/external-link.svg").with_size(zpx(12.)))
            .label(t("Open in VS Code"))
            .on_click(cx.listener(move |_, _, window, cx| {
                if let Err(e) = open_in_vscode(&path, line) {
                    window.push_notification(
                        Notification::error(crate::tf!("Couldn't open VS Code: {}", e)),
                        cx,
                    );
                }
            }))
            .into_any_element()
    }

    /// 关掉面板,滚到这次改动所在的消息、展开它的工具簇并高亮
    fn jump_to_change(&mut self, msg_ix: usize, cx: &mut Context<Self>) {
        let Some(detail) = &mut self.detail else {
            return;
        };
        let Some(seq) = detail.transcript.get(msg_ix).map(|m| m.seq) else {
            return;
        };
        detail.changes.open = false;
        let previous = detail
            .jump_seq
            .and_then(|s| detail.transcript.iter().position(|m| m.seq == s));
        detail.jump_seq = Some(seq);
        detail.expanded_tools.insert(msg_ix);
        if let Some(prev) = previous.filter(|&p| p != msg_ix) {
            detail.msg_list.splice(prev..prev + 1, 1);
        }
        detail.msg_list.splice(msg_ix..msg_ix + 1, 1);
        pin_list_row(&detail.msg_list, msg_ix);
        cx.notify();
    }

    /// 头部「N 个文件」按钮;没有任何文件改动时不显示
    pub(super) fn render_changes_button(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let detail = self.detail.as_ref()?;
        let files = &detail.changes.files;
        let reads = &detail.changes.reads;
        if files.is_empty() && reads.is_empty() {
            return None;
        }
        let open = detail.changes.open;
        let label: SharedString = if files.is_empty() {
            crate::tf!("{} files read", distinct_reads(reads)).into()
        } else {
            crate::tp!("{} file", "{} files", files.len()).into()
        };
        let theme = cx.theme();
        Some(
            Button::new("detail-changes")
                .ghost()
                .when(open, |b| b.outline())
                .rounded(RADIUS_BUTTON)
                .icon(
                    icon("icons/file-diff.svg")
                        .with_size(zpx(16.))
                        .when(open, |i| i.text_color(theme.primary)),
                )
                .label(label)
                .tooltip(if open {
                    t("Back to conversation (Esc)")
                } else {
                    t("Review file changes in this session")
                })
                .on_click(cx.listener(|this, _, _, cx| this.toggle_changes(cx)))
                .into_any_element(),
        )
    }

    pub(super) fn render_changes_panel(&self, cx: &Context<Self>) -> AnyElement {
        let Some(detail) = &self.detail else {
            return div().into_any_element();
        };
        let body = if showing_source(detail) {
            self.render_read_pane(cx)
        } else {
            self.render_changes_pane(cx)
        };
        v_flex()
            .flex_1()
            .min_h_0()
            .w_full()
            .bg(cx.theme().popover)
            .child(self.render_filter_bar(cx))
            .child(body)
            .into_any_element()
    }

    /// 顶部筛选条:全部 / 改动 / 只读参考
    fn render_filter_bar(&self, cx: &Context<Self>) -> AnyElement {
        let Some(detail) = &self.detail else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        let panel = &detail.changes;
        let filter_btn = |id: &'static str, label: SharedString, target: ListFilter| {
            let active = panel.filter == target;
            Button::new(id)
                .small()
                .rounded(RADIUS_BUTTON)
                .label(label)
                .map(|b| if active { b.outline() } else { b.ghost() })
                .on_click(cx.listener(move |this, _, _, cx| this.set_list_filter(target, cx)))
        };
        let ref_count = panel
            .reads
            .iter()
            .flat_map(|t| {
                t.files
                    .iter()
                    .filter(|f| !f.changed)
                    .map(|f| f.path.as_str())
            })
            .collect::<HashSet<_>>()
            .len();
        h_flex()
            .w_full()
            .min_w_0()
            .flex_shrink_0()
            .gap(zpx(2.))
            .px(SPACE_LG)
            .py(SPACE_SM)
            .border_b_1()
            .border_color(theme.border)
            .child(filter_btn(
                "chg-filter-all",
                t("All").into(),
                ListFilter::All,
            ))
            .child(filter_btn(
                "chg-filter-changed",
                crate::tf!("Changes · {}", panel.files.len()).into(),
                ListFilter::Changed,
            ))
            .child(filter_btn(
                "chg-filter-ref",
                crate::tf!("Reference · {}", ref_count).into(),
                ListFilter::Reference,
            ))
            .child(div().flex_1())
            .when(!panel.related.is_empty(), |d| {
                d.child(
                    Button::new("chg-related-toggle")
                        .ghost()
                        .small()
                        .rounded(RADIUS_BUTTON)
                        .label(crate::tf!("Possibly related · {}", panel.related.len()))
                        .map(|b| {
                            if panel.show_related {
                                b.outline()
                            } else {
                                b.ghost()
                            }
                        })
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_related(cx))),
                )
            })
            .into_any_element()
    }

    /// 左栏统一列表:全部为轮次时间线(末尾附未读过的改动),改动为改动列表,只读参考为未改过的读取项
    fn render_entry_list(&self, cx: &Context<Self>) -> AnyElement {
        let Some(detail) = &self.detail else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        let panel = &detail.changes;
        let root = detail.meta.project_path.as_str();
        let filter = panel.filter;
        let mut read_count: HashMap<String, usize> = HashMap::new();
        for item in panel.reads.iter().flat_map(|t| t.files.iter()) {
            *read_count
                .entry(changes::path_key(&item.path, root))
                .or_insert(0) += item.count;
        }
        let change_map: HashMap<String, &FileChanges> = panel
            .files
            .iter()
            .map(|f| (changes::path_key(&f.path, root), f))
            .collect();
        let sel_scratch = panel
            .sel_key
            .as_deref()
            .and_then(|k| change_map.get(k))
            .is_some_and(|f| f.scratch);

        let mut items: Vec<AnyElement> = Vec::new();
        match filter {
            ListFilter::Changed => {
                for f in panel.files.iter().filter(|f| !f.scratch) {
                    let key = changes::path_key(&f.path, root);
                    let reads = read_count.get(&key).copied().unwrap_or(0);
                    items.push(self.change_entry(f, key, reads, cx));
                }
                let scratch_count = panel.files.iter().filter(|f| f.scratch).count();
                if scratch_count > 0 {
                    let show = panel.show_scratch || sel_scratch;
                    items.push(
                        h_flex()
                            .id("chg-scratch-group")
                            .w_full()
                            .mt(SPACE_SM)
                            .px(SPACE_SM)
                            .py(zpx(4.))
                            .gap(zpx(6.))
                            .items_center()
                            .cursor_pointer()
                            .text_size(FONT_LABEL)
                            .text_color(theme.muted_foreground)
                            .hover(|s| s.text_color(theme.foreground))
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_change_scratch(cx)))
                            .child(
                                icon("icons/chevron-right.svg")
                                    .with_size(zpx(10.))
                                    .when(show, |i| {
                                        i.rotate(gpui::Radians(std::f32::consts::FRAC_PI_2))
                                    }),
                            )
                            .child(crate::tf!("Scratch files · {}", scratch_count))
                            .into_any_element(),
                    );
                    if show {
                        for f in panel.files.iter().filter(|f| f.scratch) {
                            let key = changes::path_key(&f.path, root);
                            let reads = read_count.get(&key).copied().unwrap_or(0);
                            items.push(self.change_entry(f, key, reads, cx));
                        }
                    }
                }
            }
            ListFilter::All | ListFilter::Reference => {
                let only_ref = filter == ListFilter::Reference;
                for turn in panel.reads.iter() {
                    let shown: Vec<&ReadItem> = turn
                        .files
                        .iter()
                        .filter(|f| !only_ref || !f.changed)
                        .collect();
                    if shown.is_empty() {
                        continue;
                    }
                    items.push(read_turn_header(turn, theme));
                    for item in shown {
                        let key = changes::path_key(&item.path, root);
                        let stat = change_map.get(&key).copied();
                        items.push(self.read_entry(item, key, stat, cx));
                    }
                }
                if !only_ref {
                    let unread: Vec<&FileChanges> = panel
                        .files
                        .iter()
                        .filter(|f| {
                            !f.scratch
                                && !read_count.contains_key(&changes::path_key(&f.path, root))
                        })
                        .collect();
                    if !unread.is_empty() {
                        items.push(
                            div()
                                .w_full()
                                .mt(SPACE_SM)
                                .px(SPACE_SM)
                                .text_size(FONT_LABEL)
                                .text_color(theme.muted_foreground)
                                .child(t("Changed, not read"))
                                .into_any_element(),
                        );
                        for f in unread {
                            let key = changes::path_key(&f.path, root);
                            items.push(self.change_entry(f, key, 0, cx));
                        }
                    }
                }
            }
        }
        // 第二期:标识符推断出的「可能相关」分组,标注为推断并写清依据
        if panel.show_related && !panel.related.is_empty() {
            items.push(
                div()
                    .w_full()
                    .mt(SPACE_SM)
                    .px(SPACE_SM)
                    .pt(zpx(8.))
                    .text_size(FONT_LABEL)
                    .text_color(theme.muted_foreground)
                    .child(t("Possibly related (inferred)"))
                    .into_any_element(),
            );
            for r in panel.related.iter() {
                let key = r.key.clone();
                let reason = r.because.clone();
                items.push(self.entry_row(
                    key,
                    &r.path,
                    None,
                    vec![div()
                        .text_size(FONT_LABEL)
                        .text_color(theme.muted_foreground)
                        .child(crate::tf!("inferred from {}", reason))
                        .into_any_element()],
                    cx,
                ));
            }
        }
        if items.is_empty() {
            items.push(
                div()
                    .px(SPACE_SM)
                    .py(SPACE_SM)
                    .text_size(FONT_LABEL)
                    .text_color(theme.muted_foreground)
                    .child(t("Nothing here yet"))
                    .into_any_element(),
            );
        }

        v_flex()
            .relative()
            .w(zpx(LIST_WIDTH))
            .flex_shrink_0()
            .h_full()
            .border_r_1()
            .border_color(theme.border)
            .child(
                v_flex()
                    .id("chg-entry-list")
                    .size_full()
                    .p(SPACE_SM)
                    .gap(zpx(2.))
                    .overflow_y_scroll()
                    .track_scroll(&panel.list_scroll)
                    .children(items),
            )
            .vertical_scrollbar(&panel.list_scroll)
            .into_any_element()
    }

    /// 改动文件的列表行:改动量在右侧,编辑次数与读取次数在下方
    fn change_entry(
        &self,
        file: &FileChanges,
        key: String,
        reads: usize,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let mut meta: Vec<AnyElement> = vec![div()
            .text_size(FONT_LABEL)
            .text_color(theme.muted_foreground)
            .child(crate::tp!("{} edit", "{} edits", file.edits.len()))
            .into_any_element()];
        if reads > 0 {
            meta.push(
                div()
                    .text_size(FONT_LABEL)
                    .text_color(theme.muted_foreground)
                    .child(crate::tf!("Read {} times", reads))
                    .into_any_element(),
            );
        }
        let trailing = stat_badge(file.added, file.removed, theme).into_any_element();
        self.entry_row(key, &file.path, Some(trailing), meta, cx)
    }

    /// 读取项的列表行:改动标签与读取次数;改过的文件附带改动量
    fn read_entry(
        &self,
        item: &ReadItem,
        key: String,
        stat: Option<&FileChanges>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let mut meta: Vec<AnyElement> = vec![pill(
            if item.changed {
                t("Changed")
            } else {
                t("Reference")
            },
            theme,
        )
        .into_any_element()];
        if item.count > 1 {
            meta.push(
                div()
                    .text_size(FONT_LABEL)
                    .text_color(theme.muted_foreground)
                    .child(crate::tf!("Read {} times", item.count))
                    .into_any_element(),
            );
        }
        let trailing = stat.map(|f| stat_badge(f.added, f.removed, theme).into_any_element());
        self.entry_row(key, &item.path, trailing, meta, cx)
    }

    /// 统一的列表行,点击后按路径 key 选中
    fn entry_row(
        &self,
        key: String,
        path: &str,
        trailing: Option<AnyElement>,
        meta: Vec<AnyElement>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let is_sel = self
            .detail
            .as_ref()
            .is_some_and(|d| d.changes.sel_key.as_deref() == Some(key.as_str()));
        let (dir, name) = split_path(path);
        let id = SharedString::from(format!("chg-entry-{key}"));
        let mut col = v_flex()
            .id(id)
            .w_full()
            .min_w_0()
            .px(SPACE_SM)
            .py(zpx(5.))
            .rounded(RADIUS_BUTTON)
            .cursor_pointer()
            .when(is_sel, |d| d.bg(theme.muted))
            .when(!is_sel, |d| d.hover(|s| s.bg(theme.muted.opacity(0.5))))
            .on_click(cx.listener(move |this, _, _, cx| this.select_entry(key.clone(), cx)))
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap(SPACE_SM)
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(FONT_CAPTION)
                            .text_color(theme.foreground)
                            .child(name.to_string()),
                    )
                    .when_some(trailing, |d, t| d.child(t)),
            )
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .truncate()
                    .text_size(FONT_LABEL)
                    .text_color(theme.muted_foreground)
                    .child(dir.to_string()),
            );
        if !meta.is_empty() {
            col = col.child(h_flex().gap(SPACE_SM).pt(zpx(2.)).children(meta));
        }
        col.into_any_element()
    }

    /// 读过的文件栏:左侧列表,右侧只读代码(带行号)
    fn render_read_pane(&self, cx: &Context<Self>) -> AnyElement {
        let Some(detail) = &self.detail else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        let panel = &detail.changes;
        let left = self.render_entry_list(cx);
        let selected_path = source_path(detail).unwrap_or_default();
        let (dir, name) = split_path(&selected_path);
        let header = h_flex()
            .w_full()
            .min_w_0()
            .flex_shrink_0()
            .items_center()
            .gap(SPACE_SM)
            .px(SPACE_LG)
            .py(SPACE_SM)
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex_shrink_0()
                    .font_family(theme.mono_font_family.clone())
                    .text_size(FONT_CAPTION)
                    .font_medium()
                    .text_color(theme.foreground)
                    .child(name.to_string()),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(FONT_LABEL)
                    .text_color(theme.muted_foreground)
                    .child(dir.to_string()),
            )
            .child(div().flex_1())
            .when(sel_change_ix(detail).is_none(), |d| {
                d.child(pill(t("Read only"), theme))
            })
            .child(self.vscode_button(&selected_path, 1, cx));

        let cached = panel.read_cache.get(&selected_path).cloned();
        let body: AnyElement = match cached.as_deref() {
            Some(ReadView::Text(lines)) => {
                let total = lines.len();
                let digits = total.max(1).to_string().len() as f32;
                let fg = theme.foreground;
                let muted = theme.muted_foreground;
                let mono = theme.mono_font_family.clone();
                let view_for_rows = cached.clone();
                let symbol_names: Rc<HashSet<String>> =
                    Rc::new(panel.symbols.iter().map(|d| d.name.clone()).collect());
                let code_fg = fg;
                let code_muted = muted;
                let code_mono = mono.clone();
                let code_gutter = zpx(digits * 7.5 + 10.);
                let sym_color = theme.primary;
                let sym_hover = theme.primary.opacity(0.7);
                v_flex()
                    .relative()
                    .size_full()
                    .min_w_0()
                    .rounded(RADIUS_KBD)
                    .border_1()
                    .border_color(theme.border)
                    .overflow_hidden()
                    .bg(theme.background)
                    .py(zpx(4.))
                    .child(
                        uniform_list(
                            "chg-read-lines",
                            total,
                            cx.processor(
                                move |this: &mut Workbench, range: Range<usize>, _, cx| {
                                    // 第三期:渲染前消费「跳到定义」请求,滚到目标行
                                    if let Some(jump) = this
                                        .detail
                                        .as_ref()
                                        .and_then(|d| d.changes.symbol_jump.borrow_mut().take())
                                    {
                                        let (_, line) = jump;
                                        this.detail
                                            .as_ref()
                                            .unwrap()
                                            .changes
                                            .read_scroll
                                            .scroll_to_item(
                                                line.saturating_sub(1),
                                                ScrollStrategy::Center,
                                            );
                                    }
                                    let Some(view) = view_for_rows.as_deref() else {
                                        return Vec::new();
                                    };
                                    let ReadView::Text(lines) = view else {
                                        return Vec::new();
                                    };
                                    range
                                        .map(|i| {
                                            let raw =
                                                lines.get(i).map(String::as_str).unwrap_or("");
                                            let text = expand_tabs(raw).into_owned();
                                            let text = if text.is_empty() {
                                                " ".to_string()
                                            } else {
                                                text
                                            };
                                            let mut row = h_flex()
                                                .w_full()
                                                .min_w_0()
                                                .items_start()
                                                .text_size(FONT_LABEL)
                                                .line_height(relative(1.55))
                                                .font_family(code_mono.clone())
                                                .child(
                                                    div()
                                                        .w(code_gutter)
                                                        .flex_shrink_0()
                                                        .flex()
                                                        .justify_end()
                                                        .pr(zpx(8.))
                                                        .text_color(code_muted.opacity(0.7))
                                                        .child((i + 1).to_string()),
                                                );
                                            let mut code = h_flex()
                                                .flex_1()
                                                .min_w_0()
                                                .whitespace_nowrap()
                                                .pr(SPACE_SM)
                                                .text_color(code_fg);
                                            for (tok, is_ident) in relations::tokenize_line(&text) {
                                                let has_def =
                                                    is_ident && symbol_names.contains(&tok);
                                                if has_def {
                                                    let name = tok.clone();
                                                    code = code.child(
                                                        div()
                                                            .id(SharedString::from(format!(
                                                                "sym-{i}-{name}"
                                                            )))
                                                            .cursor_pointer()
                                                            .text_color(sym_color)
                                                            .hover(move |s| s.text_color(sym_hover))
                                                            .child(name.clone())
                                                            .on_click(cx.listener(
                                                                move |this, _, _, cx| {
                                                                    this.goto_definition(
                                                                        name.clone(),
                                                                        cx,
                                                                    )
                                                                },
                                                            )),
                                                    );
                                                } else {
                                                    code = code.child(tok);
                                                }
                                            }
                                            row = row.child(code);
                                            row.into_any_element()
                                        })
                                        .collect()
                                },
                            ),
                        )
                        .track_scroll(&panel.read_scroll)
                        .size_full(),
                    )
                    .vertical_scrollbar(&panel.read_scroll)
                    .into_any_element()
            }
            Some(ReadView::Unavailable(msg)) => h_flex()
                .w_full()
                .gap(zpx(6.))
                .items_center()
                .text_size(FONT_LABEL)
                .text_color(theme.warning)
                .child(
                    icon("icons/triangle-alert.svg")
                        .with_size(zpx(12.))
                        .flex_shrink_0(),
                )
                .child(div().min_w_0().child(*msg))
                .into_any_element(),
            None => div()
                .text_size(FONT_LABEL)
                .text_color(theme.muted_foreground)
                .child(t("Nothing here yet"))
                .into_any_element(),
        };

        let right = v_flex().flex_1().min_w_0().h_full().child(header).child(
            v_flex()
                .id("chg-read-code")
                .flex_1()
                .min_h_0()
                .relative()
                .w_full()
                .p(SPACE_LG)
                .child(body),
        );

        h_flex()
            .flex_1()
            .min_h_0()
            .w_full()
            .bg(theme.popover)
            .child(left)
            .child(right)
            .into_any_element()
    }

    fn render_changes_pane(&self, cx: &Context<Self>) -> AnyElement {
        let Some(detail) = &self.detail else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        let panel = &detail.changes;
        let files = panel.files.clone();
        let selected = panel.selected.min(files.len().saturating_sub(1));
        let left = self.render_entry_list(cx);

        // ---- 右栏:选中文件 ----
        let Some(file) = files.get(selected) else {
            return h_flex()
                .flex_1()
                .min_h_0()
                .bg(theme.popover)
                .child(left)
                .into_any_element();
        };
        let merged = (!panel.per_edit)
            .then(|| panel.merged.get(&selected).cloned())
            .flatten();
        let (added, removed) = match &merged {
            Some(view) => view.totals(),
            None => (file.added, file.removed),
        };
        let (dir, name) = split_path(&file.path);
        let can_full = matches!(merged.as_deref(), Some(MergedView::Full { .. }));
        // 只有能还原整文件(MergedView::Full)才能并排;片段合并没有整文件,保持内联。
        // 之前这里用聊天阅读宽(封顶 720)判宽度,恒小于门槛,导致并排永远不生效。
        let side_by_side = !panel.per_edit && can_full;
        let mut header = h_flex()
            .w_full()
            .min_w_0()
            .flex_shrink_0()
            .items_center()
            .gap(SPACE_SM)
            .px(SPACE_LG)
            .py(SPACE_SM)
            .border_b_1()
            .border_color(theme.border);
        header = header
            .child(
                div()
                    .flex_shrink_0()
                    .font_family(theme.mono_font_family.clone())
                    .text_size(FONT_CAPTION)
                    .font_medium()
                    .text_color(theme.foreground)
                    .child(name.to_string()),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(FONT_LABEL)
                    .text_color(theme.muted_foreground)
                    .child(dir.to_string()),
            );
        header = header
            .when(file.created, |d| d.child(pill(t("New file"), theme)))
            .when(file.deleted, |d| d.child(pill(t("Deleted"), theme)))
            .child(div().flex_1());

        panel.change_anchors.borrow_mut().clear();
        let mut body: Vec<AnyElement> = Vec::new();
        if panel.per_edit {
            for (k, r) in file.edits.iter().enumerate() {
                let Some(edit) = changes::edit_at(&detail.transcript, r) else {
                    continue;
                };
                let msg_ix = r.msg_ix;
                let when = r.timestamp.map(smart_time);
                body.push(
                    v_flex()
                        .w_full()
                        .min_w_0()
                        .gap(zpx(6.))
                        .child(
                            h_flex()
                                .w_full()
                                .min_w_0()
                                .gap(SPACE_SM)
                                .items_center()
                                .text_size(FONT_LABEL)
                                .text_color(theme.muted_foreground)
                                .child(
                                    div()
                                        .font_medium()
                                        .text_color(theme.foreground)
                                        .child(crate::tf!("Edit {}", k + 1)),
                                )
                                .when_some(when, |d, when| d.child("·").child(when))
                                .when_some(kind_label(r.kind), |d, l| d.child(pill(l, theme)))
                                .child(div().flex_1())
                                .child(
                                    Button::new(SharedString::from(format!("chg-jump-{k}")))
                                        .ghost()
                                        .small()
                                        .rounded(RADIUS_BUTTON)
                                        .icon(icon("icons/corner-left-up.svg").with_size(zpx(12.)))
                                        .label(t("Jump to message"))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.jump_to_change(msg_ix, cx)
                                        })),
                                ),
                        )
                        .child(file_edit_view(edit, cx))
                        .into_any_element(),
                );
            }
        } else if let Some(view) = merged {
            if side_by_side {
                body.push(self.side_by_side_body(&view, cx));
            } else {
                body.push(self.merged_body(&view, file, cx));
            }
        }

        let change_count = panel.change_anchors.borrow().len();
        if change_count > 0 {
            header = header.child(self.change_nav(change_count, panel.change_cursor, cx));
        }
        header = header
            .child(stat_badge(added, removed, theme))
            .child(self.vscode_button(&file.path, 1, cx));
        let right = v_flex().flex_1().min_w_0().h_full().child(header).child(
            v_flex()
                .relative()
                .flex_1()
                .min_h_0()
                .w_full()
                .child(
                    v_flex()
                        .id("chg-diff")
                        .size_full()
                        .p(SPACE_LG)
                        .gap(SPACE_LG)
                        .overflow_scroll()
                        .track_scroll(&panel.diff_scroll)
                        .children(body),
                )
                .vertical_scrollbar(&panel.diff_scroll)
                .horizontal_scrollbar(&panel.diff_scroll),
        );
        h_flex()
            .flex_1()
            .min_h_0()
            .w_full()
            .bg(theme.popover)
            .child(left)
            .child(right)
            .into_any_element()
    }

    /// 登记一个改动块的锚点;同一改动块只在起始行调用一次
    fn new_change_anchor(&self) -> Option<(usize, ScrollAnchor)> {
        let panel = &self.detail.as_ref()?.changes;
        let anchor = ScrollAnchor::for_handle(panel.diff_scroll.clone());
        let mut anchors = panel.change_anchors.borrow_mut();
        let ix = anchors.len();
        anchors.push(anchor.clone());
        Some((ix, anchor))
    }

    /// 改动块起始行包一层带锚点的容器(anchor_scroll 需要有 id),其余行原样返回
    fn anchored(el: AnyElement, anchor: Option<(usize, ScrollAnchor)>) -> AnyElement {
        match anchor {
            Some((ix, a)) => div()
                .id(SharedString::from(format!("chg-anchor-{ix}")))
                .w_full()
                .min_w_0()
                .anchor_scroll(Some(a))
                .child(el)
                .into_any_element(),
            None => el,
        }
    }

    /// 上一处 / 下一处改动:从游标出发滚动到相邻的改动块
    fn jump_change(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(detail) = &mut self.detail else {
            return;
        };
        let anchors = detail.changes.change_anchors.borrow();
        let count = anchors.len();
        if count == 0 {
            return;
        }
        let next = match detail.changes.change_cursor {
            None if forward => 0,
            None => count - 1,
            Some(c) if forward => (c + 1).min(count - 1),
            Some(c) => c.saturating_sub(1),
        };
        detail.changes.change_cursor = Some(next);
        anchors[next].scroll_to(window, cx);
        cx.notify();
    }

    /// 差异区头部的改动导航:当前序号 / 总数,加上一处、下一处按钮
    fn change_nav(&self, count: usize, cursor: Option<usize>, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let at = cursor.map(|c| c + 1).unwrap_or(0);
        h_flex()
            .flex_shrink_0()
            .items_center()
            .gap(zpx(2.))
            .child(
                div()
                    .px(zpx(4.))
                    .text_size(FONT_LABEL)
                    .text_color(theme.muted_foreground)
                    .child(format!("{at}/{count}")),
            )
            .child(
                Button::new("chg-prev")
                    .ghost()
                    .small()
                    .rounded(RADIUS_BUTTON)
                    .label(t("Previous change"))
                    .on_click(
                        cx.listener(|this, _, window, cx| this.jump_change(false, window, cx)),
                    ),
            )
            .child(
                Button::new("chg-next")
                    .ghost()
                    .small()
                    .rounded(RADIUS_BUTTON)
                    .label(t("Next change"))
                    .on_click(
                        cx.listener(|this, _, window, cx| this.jump_change(true, window, cx)),
                    ),
            )
            .into_any_element()
    }

    fn merged_body(&self, view: &MergedView, file: &FileChanges, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let empty = HashSet::new();
        let (detail_selected, only, layout, expanded) = match &self.detail {
            Some(d) => (
                d.changes.selected,
                d.changes.only_changes,
                d.changes.layout,
                &d.changes.expanded,
            ),
            None => (0, false, DiffLayout::Inline, &empty),
        };
        let mono = theme.mono_font_family.clone();
        let mut notes: Vec<(&'static str, bool)> = Vec::new();
        let mut rows: Vec<AnyElement> = Vec::new();
        let mut shown = 0usize;
        let mut hidden = 0usize;
        let (added, removed) = view.totals();
        match view {
            MergedView::Full {
                rows: inline,
                rows_flat: inline_flat,
                max_line,
                base,
                external,
                ..
            } => {
                let lines = if only { inline_flat } else { inline };
                notes.push((
                    match base {
                        MergeBase::NewFile => t("Whole file: created in this session"),
                        MergeBase::Snapshot => {
                            t("Whole file: rebuilt from file snapshots in tool results")
                        }
                        MergeBase::Disk => t("Whole file: rewound from the file on disk"),
                        MergeBase::FileHistory => {
                            t("Whole file: rebuilt from the agent's file-history backups")
                        }
                    },
                    false,
                ));
                if *external {
                    notes.push((
                        t("Includes changes made outside the edit tools (scripts, manual edits)"),
                        true,
                    ));
                }
                let digits = (*max_line).max(1).to_string().len() as f32;
                let width = zpx(digits * 7.5 + 10.);
                let file_ix = detail_selected;
                let mut in_change = false;
                for (row_ix, row) in lines.iter().enumerate() {
                    if shown >= MAX_PANEL_ROWS {
                        match row {
                            NumberedRow::Line(..) => hidden += 1,
                            NumberedRow::Skip(v)
                                if expanded.contains(&(file_ix, layout, only, row_ix)) =>
                            {
                                hidden += v.len()
                            }
                            NumberedRow::Skip(_) => {}
                        }
                        continue;
                    }
                    match row {
                        NumberedRow::Line(l, a, b) => {
                            shown += 1;
                            let changed = l.tag != LineTag::Context;
                            let anchor = if changed && !in_change {
                                self.new_change_anchor()
                            } else {
                                None
                            };
                            in_change = changed;
                            rows.push(Self::anchored(
                                diff_row_numbered(l.clone(), Some((*a, *b, width)), theme)
                                    .into_any_element(),
                                anchor,
                            ));
                        }
                        NumberedRow::Skip(v) => {
                            let open = expanded.contains(&(file_ix, layout, only, row_ix));
                            shown += 1;
                            in_change = false;
                            let label: SharedString = if open {
                                t("⋯ Collapse unchanged lines").into()
                            } else {
                                crate::tf!("⋯ {} unchanged lines · click to expand", v.len()).into()
                            };
                            rows.push(
                                div()
                                    .id(SharedString::from(format!("chg-skip-{row_ix}")))
                                    .w_full()
                                    .min_w_0()
                                    .my(zpx(2.))
                                    .pl(zpx(22.))
                                    .bg(theme.muted.opacity(0.35))
                                    .text_color(theme.muted_foreground)
                                    .cursor_pointer()
                                    .hover(|s| s.bg(theme.muted).text_color(theme.foreground))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_change_skip(row_ix, cx)
                                    }))
                                    .child(label)
                                    .into_any_element(),
                            );
                            if open {
                                for (l, a, b) in v {
                                    shown += 1;
                                    rows.push(
                                        diff_row_numbered(l.clone(), Some((*a, *b, width)), theme)
                                            .into_any_element(),
                                    );
                                }
                            }
                        }
                    }
                }
            }
            MergedView::Snippets { hunks, reason, .. } => {
                notes.push((
                    match reason {
                        SnippetReason::NoFile => t("Pieced together from edit snippets: the file isn't available locally"),
                        SnippetReason::FileChanged => t("Pieced together from edit snippets: the file has changed since this session"),
                        SnippetReason::Truncated => t("Pieced together from edit snippets: some edits were too long and got truncated"),
                        SnippetReason::Deleted => t("Pieced together from edit snippets: the file was deleted in this session"),
                    },
                    false,
                ));
                for (hunk_ix, (sources, lines)) in hunks.iter().enumerate() {
                    if shown >= MAX_PANEL_ROWS {
                        hidden += lines
                            .iter()
                            .filter(|r| matches!(r, DiffRow::Line(_)))
                            .count();
                        continue;
                    }
                    let from = sources
                        .iter()
                        .map(|s| (s + 1).to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    rows.push(
                        div()
                            .w_full()
                            .when(hunk_ix > 0, |d| {
                                d.mt(SPACE_SM).border_t_1().border_color(theme.border)
                            })
                            .pl(zpx(22.))
                            .py(zpx(2.))
                            .text_color(theme.muted_foreground)
                            .child(crate::tf!("From edit {}", from))
                            .into_any_element(),
                    );
                    let mut in_change = false;
                    for row in lines {
                        shown += 1;
                        let (el, changed) = match row {
                            DiffRow::Line(l) => (
                                diff_row_numbered(l.clone(), None, theme).into_any_element(),
                                l.tag != LineTag::Context,
                            ),
                            DiffRow::Skip(n) => {
                                (skip_row(Some(*n), theme).into_any_element(), false)
                            }
                        };
                        let anchor = if changed && !in_change {
                            self.new_change_anchor()
                        } else {
                            None
                        };
                        in_change = changed;
                        rows.push(Self::anchored(el, anchor));
                    }
                }
            }
        }
        if added == 0 && removed == 0 {
            notes.push((t("Changed and changed back: no net change"), false));
            rows.clear();
            hidden = 0;
        }
        if hidden > 0 {
            rows.push(note_row(
                crate::tf!("{} more lines not shown", hidden).into(),
                theme,
            ));
        }
        let has_rows = !rows.is_empty();
        let _ = file;

        v_flex()
            .w_full()
            .min_w_0()
            .gap(SPACE_SM)
            .children(notes.into_iter().map(|(text, warn)| {
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap(zpx(6.))
                    .items_center()
                    .text_size(FONT_LABEL)
                    .text_color(if warn {
                        theme.warning
                    } else {
                        theme.muted_foreground
                    })
                    .child(
                        icon(if warn {
                            "icons/triangle-alert.svg"
                        } else {
                            "icons/info.svg"
                        })
                        .with_size(zpx(12.))
                        .flex_shrink_0(),
                    )
                    .child(div().min_w_0().child(text))
            }))
            .when(has_rows, |d| {
                d.child(
                    v_flex()
                        .w_full()
                        .min_w_0()
                        .rounded(RADIUS_KBD)
                        .border_1()
                        .border_color(theme.border)
                        .overflow_hidden()
                        .bg(theme.background)
                        .py(zpx(4.))
                        .text_size(FONT_LABEL)
                        .line_height(relative(1.55))
                        .font_family(mono)
                        .children(rows),
                )
            })
            .into_any_element()
    }

    /// 左右对照:改动前在左、改动后在右,同一水平行对齐。两侧共用一个滚动容器,
    /// 天然同步滚动;未改动段两侧一起折叠 / 展开。
    fn side_by_side_body(&self, view: &MergedView, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let Some(detail) = &self.detail else {
            return div().into_any_element();
        };
        let only = detail.changes.only_changes;
        let expanded = &detail.changes.expanded;
        let (side, max_line, added, removed, base, external, total) = match view {
            MergedView::Full {
                side,
                side_flat,
                max_line,
                total_side_lines,
                added,
                removed,
                base,
                external,
                ..
            } => (
                if only { side_flat } else { side },
                *max_line,
                *added,
                *removed,
                *base,
                *external,
                *total_side_lines,
            ),
            MergedView::Snippets { .. } => return div().into_any_element(),
        };
        let _ = total;
        let file_ix = detail.changes.selected;
        let layout = DiffLayout::SideBySide;
        let digits = max_line.max(1).to_string().len() as f32;
        let gutter = zpx(digits * 7.5 + 12.);
        // 两列严格等分视口宽度:不再按全文件最长行给单元格设 min_w,
        // 否则右列会被整体推到视口之外。单元格内的超长行自行裁剪。
        let cell = |cell_data: Option<&SideCell>, side_tag: LineTag| -> AnyElement {
            let Some(c) = cell_data else {
                return div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .bg(theme.muted.opacity(0.18))
                    .into_any_element();
            };
            let (bg, strong) = match side_tag {
                LineTag::Removed => (theme.danger.opacity(0.10), theme.danger.opacity(0.30)),
                LineTag::Added => (theme.success.opacity(0.10), theme.success.opacity(0.30)),
                LineTag::Context => (theme.background, theme.muted),
            };
            let highlights: Vec<(Range<usize>, HighlightStyle)> = c
                .emphasis
                .iter()
                .cloned()
                .map(|r| {
                    (
                        r,
                        HighlightStyle {
                            background_color: Some(strong),
                            ..Default::default()
                        },
                    )
                })
                .collect();
            let text = if c.text.is_empty() {
                " ".to_string()
            } else {
                c.text.clone()
            };
            let text_color = if side_tag == LineTag::Context {
                theme.foreground.opacity(0.72)
            } else {
                theme.foreground
            };
            h_flex()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .items_start()
                .bg(bg)
                .child(
                    div()
                        .w(gutter)
                        .flex_shrink_0()
                        .flex()
                        .justify_end()
                        .pr(zpx(6.))
                        .text_color(theme.muted_foreground.opacity(0.7))
                        .child(c.line_no.to_string()),
                )
                .child(
                    div()
                        .w(zpx(18.))
                        .flex_shrink_0()
                        .flex()
                        .justify_center()
                        .text_color(if side_tag == LineTag::Removed {
                            theme.danger
                        } else if side_tag == LineTag::Added {
                            theme.success
                        } else {
                            theme.muted_foreground
                        })
                        .child(match side_tag {
                            LineTag::Removed => "−",
                            LineTag::Added => "+",
                            LineTag::Context => " ",
                        }),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .whitespace_nowrap()
                        .pr(zpx(8.))
                        .text_color(text_color)
                        .child(StyledText::new(text).with_highlights(highlights)),
                )
                .into_any_element()
        };

        let pair = |row: &SideRow| -> AnyElement {
            h_flex()
                .flex_shrink_0()
                .w_full()
                .min_w_0()
                .items_stretch()
                .child(match row {
                    SideRow::Context(a, b) => h_flex()
                        .flex_1()
                        .min_w_0()
                        .items_stretch()
                        .child(cell(Some(a), LineTag::Context))
                        .child(div().w(px(1.)).flex_shrink_0().bg(theme.border))
                        .child(cell(Some(b), LineTag::Context))
                        .into_any_element(),
                    SideRow::Changed { old, new } => h_flex()
                        .flex_1()
                        .min_w_0()
                        .items_stretch()
                        .child(cell(old.as_ref(), LineTag::Removed))
                        .child(div().w(px(1.)).flex_shrink_0().bg(theme.border))
                        .child(cell(new.as_ref(), LineTag::Added))
                        .into_any_element(),
                })
                .into_any_element()
        };

        let mut notes: Vec<(&'static str, bool)> = vec![(
            match base {
                MergeBase::NewFile => t("Whole file: created in this session"),
                MergeBase::Snapshot => t("Whole file: rebuilt from file snapshots in tool results"),
                MergeBase::Disk => t("Whole file: rewound from the file on disk"),
                MergeBase::FileHistory => {
                    t("Whole file: rebuilt from the agent's file-history backups")
                }
            },
            false,
        )];
        if external {
            notes.push((
                t("Includes changes made outside the edit tools (scripts, manual edits)"),
                true,
            ));
        }

        let mut rows: Vec<AnyElement> = Vec::new();
        let mut shown = 0usize;
        let mut hidden = 0usize;
        let mut in_change = false;
        for (row_ix, row) in side.iter().enumerate() {
            if shown >= MAX_PANEL_ROWS {
                match row {
                    SideFoldRow::Line(_) => hidden += 1,
                    SideFoldRow::Skip(v)
                        if !only || expanded.contains(&(file_ix, layout, only, row_ix)) =>
                    {
                        hidden += v.len()
                    }
                    SideFoldRow::Skip(_) => {}
                }
                continue;
            }
            match row {
                SideFoldRow::Line(r) => {
                    shown += 1;
                    let changed = matches!(r, SideRow::Changed { .. });
                    let anchor = if changed && !in_change {
                        self.new_change_anchor()
                    } else {
                        None
                    };
                    in_change = changed;
                    rows.push(Self::anchored(pair(r), anchor));
                }
                SideFoldRow::Skip(v) => {
                    if !only {
                        in_change = false;
                        for r in v {
                            shown += 1;
                            rows.push(pair(r));
                        }
                        continue;
                    }
                    let open = expanded.contains(&(file_ix, layout, only, row_ix));
                    shown += 1;
                    in_change = false;
                    let label: SharedString = if open {
                        t("⋯ Collapse unchanged lines").into()
                    } else {
                        crate::tf!("⋯ {} unchanged lines · click to expand", v.len()).into()
                    };
                    rows.push(
                        h_flex()
                            .id(SharedString::from(format!("chg-side-skip-{row_ix}")))
                            .w_full()
                            .min_w_0()
                            .gap(px(1.))
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.toggle_change_skip(row_ix, cx)
                            }))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .my(zpx(2.))
                                    .pl(gutter)
                                    .bg(theme.muted.opacity(0.35))
                                    .text_color(theme.muted_foreground)
                                    .hover(|s| s.text_color(theme.foreground))
                                    .child(label.clone()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .my(zpx(2.))
                                    .pl(gutter)
                                    .bg(theme.muted.opacity(0.35))
                                    .hover(|s| s.text_color(theme.foreground))
                                    .child(" "),
                            )
                            .into_any_element(),
                    );
                    if open {
                        for r in v {
                            shown += 1;
                            rows.push(pair(r));
                        }
                    }
                }
            }
        }
        if added == 0 && removed == 0 {
            notes.push((t("Changed and changed back: no net change"), false));
            rows.clear();
            hidden = 0;
        }
        if hidden > 0 {
            rows.push(note_row(
                crate::tf!("{} more lines not shown", hidden).into(),
                theme,
            ));
        }

        // 两栏的表头(改动前 / 改动后):与代码行同宽,随横向滚动一起移动
        let col_head = h_flex()
            .w_full()
            .flex_shrink_0()
            .items_center()
            .text_size(FONT_LABEL)
            .text_color(theme.muted_foreground)
            .bg(theme.muted.opacity(0.5))
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .pl(gutter + zpx(18.))
                    .py(zpx(3.))
                    .child(t("Before")),
            )
            .child(div().w(px(1.)).flex_shrink_0().bg(theme.border))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .pl(gutter + zpx(18.))
                    .py(zpx(3.))
                    .child(t("After")),
            );

        v_flex()
            .w_full()
            .min_w_0()
            .gap(SPACE_SM)
            .children(notes.into_iter().map(|(text, warn)| {
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap(zpx(6.))
                    .items_center()
                    .text_size(FONT_LABEL)
                    .text_color(if warn {
                        theme.warning
                    } else {
                        theme.muted_foreground
                    })
                    .child(
                        icon(if warn {
                            "icons/triangle-alert.svg"
                        } else {
                            "icons/info.svg"
                        })
                        .with_size(zpx(12.))
                        .flex_shrink_0(),
                    )
                    .child(div().min_w_0().child(text))
            }))
            .child(
                v_flex()
                    .w_full()
                    .min_w_0()
                    .rounded(RADIUS_KBD)
                    .border_1()
                    .border_color(theme.border)
                    .overflow_hidden()
                    .bg(theme.background)
                    .text_size(FONT_LABEL)
                    .line_height(relative(1.55))
                    .font_family(theme.mono_font_family.clone())
                    .child(col_head)
                    .children(rows),
            )
            .into_any_element()
    }
}
