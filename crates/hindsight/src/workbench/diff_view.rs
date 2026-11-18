//! 详情页工具卡里的文件改动视图:改文件的调用(Edit / replace_in_file / Write / apply_patch…)
//! 不再只给一坨 JSON 参数,而是画成上下排列的 diff——删除行红底、新增行绿底,行内真正改了
//! 的字再加深;长段未改动行折叠成一行。数据由 `hindsight_core::diff` 在解析时提取。
//!
//! 只有片段(old → new),没有整文件,所以不做左右对照,也不显示真实行号。

use super::*;
use hindsight_core::diff::{collapse, diff_lines, DiffLine, DiffRow, LineTag};

/// 改动行前后保留的上下文行数
const DIFF_CONTEXT: usize = 3;
/// 单个文件改动最多画多少行,再多的提示数量(完整内容在 Raw input 里能复制)
const MAX_DIFF_ROWS: usize = 400;

/// 一组工具调用合计的增删行数;没有任何文件改动时 None
pub(super) fn edit_totals(calls: &[ToolCallView]) -> Option<(usize, usize)> {
    let mut edits = calls.iter().flat_map(|c| &c.edits).peekable();
    edits.peek()?;
    Some(edits.fold((0, 0), |(a, r), e| (a + e.added, r + e.removed)))
}

/// `+12 −3`:绿 / 红两段,0 的那段不显示(都为 0 时显示 `±0`)
pub(super) fn stat_badge(added: usize, removed: usize, theme: &gpui_component::Theme) -> Div {
    let mono = theme.mono_font_family.clone();
    h_flex()
        .flex_shrink_0()
        .gap(zpx(5.))
        .text_size(FONT_LABEL)
        .font_family(mono)
        .when(added > 0, |d| {
            d.child(div().text_color(theme.success).child(format!("+{added}")))
        })
        .when(removed > 0, |d| {
            d.child(div().text_color(theme.danger).child(format!("−{removed}")))
        })
        .when(added == 0 && removed == 0, |d| {
            d.child(div().text_color(theme.muted_foreground).child("±0"))
        })
}

pub(super) fn split_path(path: &str) -> (&str, &str) {
    match path.rfind(['/', '\\']) {
        Some(i) => (&path[..=i], &path[i + 1..]),
        None => ("", path),
    }
}

pub(super) fn expand_tabs(s: &str) -> std::borrow::Cow<'_, str> {
    if s.contains('\t') {
        s.replace('\t', "    ").into()
    } else {
        s.into()
    }
}

/// 一个文件的改动卡:标题行(文件名、所在目录、新建/删除、增删数)+ diff 正文
pub(super) fn file_edit_view(edit: &FileEdit, cx: &App) -> Div {
    let theme = cx.theme();
    let mono = theme.mono_font_family.clone();
    let (dir, name) = split_path(&edit.path);
    let kind_label = match edit.kind {
        EditKind::Create => Some(t("New file")),
        EditKind::Delete => Some(t("Deleted")),
        EditKind::Update => None,
    };

    let mut rows: Vec<AnyElement> = Vec::new();
    let mut shown = 0usize;
    let mut hidden = 0usize;
    for (hunk_ix, hunk) in edit.hunks.iter().enumerate() {
        if hunk_ix > 0 && shown < MAX_DIFF_ROWS {
            rows.push(skip_row(None, theme).into_any_element());
        }
        let lines = diff_lines(&expand_tabs(&hunk.old), &expand_tabs(&hunk.new));
        for row in collapse(lines, DIFF_CONTEXT) {
            if shown >= MAX_DIFF_ROWS {
                if let DiffRow::Line(_) = row {
                    hidden += 1;
                }
                continue;
            }
            shown += 1;
            rows.push(match row {
                DiffRow::Line(line) => diff_row(line, theme).into_any_element(),
                DiffRow::Skip(n) => skip_row(Some(n), theme).into_any_element(),
            });
        }
    }
    if hidden > 0 {
        rows.push(note_row(
            crate::tf!("{} more lines not shown", hidden).into(),
            theme,
        ));
    }
    if edit.truncated {
        rows.push(note_row(t("Long edit truncated").into(), theme));
    }
    let has_body = !rows.is_empty();

    v_flex()
        .w_full()
        .min_w_0()
        .rounded(RADIUS_KBD)
        .border_1()
        .border_color(theme.border)
        .overflow_hidden()
        .bg(theme.background)
        .child(
            h_flex()
                .w_full()
                .min_w_0()
                .items_center()
                .gap(SPACE_SM)
                .px(zpx(10.))
                .py(zpx(5.))
                .bg(theme.muted.opacity(0.5))
                .when(has_body, |d| d.border_b_1().border_color(theme.border))
                .text_size(FONT_LABEL)
                .font_family(mono.clone())
                .child(
                    div()
                        .flex_shrink_0()
                        .font_medium()
                        .text_color(theme.foreground)
                        .child(name.to_string()),
                )
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_color(theme.muted_foreground)
                        .child(dir.to_string()),
                )
                .child(div().flex_1())
                .when_some(kind_label, |d, label| {
                    d.child(
                        div()
                            .flex_shrink_0()
                            .px(zpx(6.))
                            .rounded(RADIUS_BADGE)
                            .text_color(theme.muted_foreground)
                            .bg(theme.muted)
                            .child(label),
                    )
                })
                .child(stat_badge(edit.added, edit.removed, theme)),
        )
        .when(has_body, |d| {
            d.child(
                v_flex()
                    .w_full()
                    .min_w_0()
                    .py(zpx(4.))
                    .text_size(FONT_LABEL)
                    .line_height(relative(1.55))
                    .font_family(mono)
                    .children(rows),
            )
        })
}

fn diff_row(line: DiffLine, theme: &gpui_component::Theme) -> Div {
    diff_row_numbered(line, None, theme)
}

/// 行号栏:(改动前行号, 改动后行号, 每栏宽度)。只有整文件对比才有真实行号
pub(super) type Gutter = (Option<usize>, Option<usize>, Pixels);

pub(super) fn diff_row_numbered(
    line: DiffLine,
    gutter: Option<Gutter>,
    theme: &gpui_component::Theme,
) -> Div {
    let (sign, sign_color, bg, strong) = match line.tag {
        LineTag::Removed => (
            "−",
            theme.danger,
            Some(theme.danger.opacity(0.10)),
            theme.danger.opacity(0.30),
        ),
        LineTag::Added => (
            "+",
            theme.success,
            Some(theme.success.opacity(0.10)),
            theme.success.opacity(0.30),
        ),
        LineTag::Context => (" ", theme.muted_foreground, None, theme.muted),
    };
    let text_color = match line.tag {
        LineTag::Context => theme.foreground.opacity(0.72),
        _ => theme.foreground,
    };
    let highlights: Vec<(Range<usize>, HighlightStyle)> = line
        .emphasis
        .into_iter()
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
    // 空行也要占一行高
    let text = if line.text.is_empty() {
        " ".to_string()
    } else {
        line.text
    };
    h_flex()
        .w_full()
        .min_w_0()
        .items_start()
        .when_some(bg, |d, bg| d.bg(bg))
        .when_some(gutter, |d, (old_no, new_no, width)| {
            let num = |n: Option<usize>| {
                div()
                    .w(width)
                    .flex_shrink_0()
                    .flex()
                    .justify_end()
                    .pr(zpx(6.))
                    .text_color(theme.muted_foreground.opacity(0.7))
                    .child(n.map(|n| n.to_string()).unwrap_or_default())
            };
            d.child(num(old_no)).child(num(new_no))
        })
        .child(
            div()
                .w(zpx(22.))
                .flex_shrink_0()
                .flex()
                .justify_center()
                .text_color(sign_color)
                .child(sign),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .pr(zpx(10.))
                .text_color(text_color)
                .child(StyledText::new(text).with_highlights(highlights)),
        )
}

/// 折叠掉的未改动行;None 是两段片段之间的分隔(中间隔了多少行不知道)
pub(super) fn skip_row(count: Option<usize>, theme: &gpui_component::Theme) -> Div {
    let label: SharedString = match count {
        Some(n) => crate::tf!("⋯ {} unchanged lines", n).into(),
        None => "⋯".into(),
    };
    div()
        .w_full()
        .min_w_0()
        .my(zpx(2.))
        .pl(zpx(22.))
        .bg(theme.muted.opacity(0.35))
        .text_color(theme.muted_foreground)
        .child(label)
}

pub(super) fn note_row(text: SharedString, theme: &gpui_component::Theme) -> AnyElement {
    div()
        .w_full()
        .pl(zpx(22.))
        .pt(zpx(2.))
        .text_color(theme.muted_foreground)
        .child(text)
        .into_any_element()
}

/// 有 diff 的调用默认不展示原始参数 JSON,点这里展开 / 收起
pub(super) fn raw_input_toggle(
    ix: usize,
    call_ix: usize,
    open: bool,
    workbench: Entity<Workbench>,
    theme: &gpui_component::Theme,
) -> Stateful<Div> {
    h_flex()
        .id(SharedString::from(format!("tool-raw-{ix}-{call_ix}")))
        .flex_shrink_0()
        .items_center()
        .gap(zpx(4.))
        .cursor_pointer()
        .text_size(FONT_LABEL)
        .text_color(theme.muted_foreground)
        .hover(|style| style.text_color(theme.foreground))
        .child(
            icon("icons/chevron-right.svg")
                .with_size(zpx(10.))
                .when(open, |icon| {
                    icon.rotate(gpui::Radians(std::f32::consts::FRAC_PI_2))
                }),
        )
        .child(if open {
            t("Hide raw input")
        } else {
            t("Show raw input")
        })
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            workbench.update(cx, |this, cx| {
                if let Some(detail) = &mut this.detail {
                    if !detail.raw_tool_inputs.remove(&(ix, call_ix)) {
                        detail.raw_tool_inputs.insert((ix, call_ix));
                    }
                    detail.msg_list.splice(ix..ix + 1, 1);
                }
                cx.notify();
            });
        })
}
