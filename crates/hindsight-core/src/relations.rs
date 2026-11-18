//! 会话关联文件:第二期「可能相关」推断 + 第三期「点击标识符跳定义」。
//!
//! 第二期:从会话改动内容里提取标识符(函数名、类名、`require`/`import`/
//! `use` 的模块名),在项目目录里搜索命中的文件,列出为「可能相关」并标注
//! 推断依据;这一层是猜测,与会话里真实读过的文件严格区分。
//!
//! 第三期:对项目源码做一次轻量的定义行扫描,得到 `name → (文件, 行号)`,
//! 点击标识符时据此跳转。不是完整 LSP/语法树,只覆盖常见定义写法。
//!
//! 这里只放纯函数,磁盘扫描与结果组装都在后台任务里跑,不占 UI 线程。

use std::collections::{HashMap, HashSet};
use std::path::Path;

use walkdir::WalkDir;

use crate::changes::path_key;
use crate::models::TranscriptMessage;

/// 扫描项目时跳过的目录(命中任一级即整棵跳过)
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "target",
    "dist",
    "build",
    "out",
    ".cache",
    "__pycache__",
    ".venv",
    "venv",
    "vendor",
    "Saved",
    "Intermediate",
    "DerivedDataCache",
    ".idea",
    ".vscode",
    ".vs",
    ".codebuddy",
    ".prism-cache",
    "Pods",
    "Binaries",
];

/// 只收这些扩展名(代码 + 配置 + 文档),避免把二进制和资源文件全读进内存
const SOURCE_EXTS: &[&str] = &[
    "lua", "py", "js", "jsx", "ts", "tsx", "mjs", "cjs", "rs", "go", "java", "kt",
    "swift", "c", "h", "cc", "cpp", "hpp", "cs", "rb", "php", "scala", "sh", "bat",
    "ps1", "vue", "svelte", "json", "toml", "yaml", "yml", "md", "sql", "proto",
];

/// 单个文件最大读取字节数
const MAX_FILE_BYTES: u64 = 512 * 1024;
/// 最多扫描多少个文件
const MAX_PROJECT_FILES: usize = 4000;
/// 最多遍历多少个目录条目(超大项目里 walkdir 不能无限走)
const MAX_WALK_ENTRIES: usize = 30000;
/// 「可能相关」最多列多少个
pub const MAX_RELATED: usize = 24;
/// 最多提取多少个标识符
const MAX_IDENTIFIERS: usize = 80;

/// 一条推断出来的「可能相关」文件
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelatedFile {
    /// 绝对路径(用于打开与显示)
    pub path: String,
    /// 归一化 key(用于去重与选中态)
    pub key: String,
    /// 推断依据:因为改动里出现了哪个标识符
    pub because: String,
    /// 文件名命中(比内容命中更可靠)
    pub name_hit: bool,
}

/// 一个符号定义的位置(第三期)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolDef {
    pub name: String,
    pub path: String,
    pub key: String,
    /// 1-based 行号
    pub line: usize,
    /// 定义行原文(去首尾空白)
    pub text: String,
}

/// 扫描项目目录得到的源码文件
#[derive(Debug, Clone)]
pub struct ProjectFile {
    pub path: String,
    /// 文件名(含扩展)
    pub name: String,
    /// 文件名去扩展
    pub stem: String,
    pub text: String,
}

/// 把会话里所有改动的「改后」文本拼起来,作为标识符提取的输入
pub fn changed_text(messages: &[TranscriptMessage]) -> String {
    let mut out = String::new();
    for m in messages {
        for call in &m.tool_calls {
            for edit in &call.edits {
                for h in &edit.hunks {
                    out.push_str(&h.new);
                    out.push('\n');
                }
            }
        }
    }
    out
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// 从一行里抽出 `require("...")` / `import ... from "..."` / `use ...;` 里的模块名
fn module_names(line: &str, out: &mut Vec<String>) {
    // 收集所有引号包裹的字符串,再挑出像模块路径的
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' || bytes[i] == b'\'' || bytes[i] == b'`' {
            let q = bytes[i];
            let start = i + 1;
            i += 1;
            while i < bytes.len() && bytes[i] != q {
                i += 1;
            }
            if i <= bytes.len() {
                let s = &line[start..i.min(line.len())];
                if !s.is_empty() && s.len() < 200 {
                    // 模块路径:/ 或 \ 分隔、或以 . 分隔的多段
                    if s.contains('/') || s.contains('\\') || s.contains('.') {
                        let last = s
                            .split(|c| c == '/' || c == '\\' || c == '.')
                            .filter(|p| !p.is_empty())
                            .last()
                            .unwrap_or("")
                            .to_string();
                        if is_identifier(&last) && !is_keyword(&last) {
                            out.push(last);
                        }
                    }
                }
            }
        }
        i += 1;
    }
}

fn is_identifier(s: &str) -> bool {
    let b = s.as_bytes();
    if b.is_empty() || b.len() > 64 || !(b[0].is_ascii_alphabetic() || b[0] == b'_') {
        return false;
    }
    b.iter().all(|&c| is_ident_byte(c))
}

fn is_keyword(s: &str) -> bool {
    matches!(
        s,
        "local"
            | "function"
            | "end"
            | "if"
            | "then"
            | "else"
            | "elseif"
            | "for"
            | "while"
            | "do"
            | "return"
            | "break"
            | "nil"
            | "true"
            | "false"
            | "and"
            | "or"
            | "not"
            | "in"
            | "self"
            | "this"
            | "class"
            | "def"
            | "fn"
            | "pub"
            | "let"
            | "const"
            | "var"
            | "import"
            | "export"
            | "from"
            | "require"
            | "use"
            | "async"
            | "await"
            | "new"
            | "struct"
            | "enum"
            | "impl"
            | "trait"
            | "interface"
            | "type"
            | "public"
            | "private"
            | "static"
            | "void"
            | "null"
            | "None"
            | "True"
            | "False"
            | "print"
            | "string"
            | "table"
            | "math"
            | "pairs"
            | "ipairs"
            | "tostring"
            | "tonumber"
    )
}

fn looks_like_symbol(s: &str) -> bool {
    if s.len() < 3 || is_keyword(s) {
        return false;
    }
    let has_upper = s.bytes().any(|b| b.is_ascii_uppercase());
    let has_lower = s.bytes().any(|b| b.is_ascii_lowercase());
    // PascalCase / camelCase / 含下划线的多段;排除全大写常量与纯小写普通词
    has_upper && has_lower || s.contains('_') || (has_upper && s.len() >= 4)
}

/// 从改动文本里提取标识符:模块名(优先)+ 代码里出现的具名符号
pub fn extract_identifiers(text: &str) -> Vec<String> {
    let mut raw: Vec<String> = Vec::new();
    let mut freq: HashMap<String, usize> = HashMap::new();
    for line in text.lines() {
        let mut mods = Vec::new();
        module_names(line, &mut mods);
        for m in mods {
            *freq.entry(m.clone()).or_insert(0) += 3; // 模块名权重更高
            raw.push(m);
        }
        // 通用标识符 token
        let b = line.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if b[i].is_ascii_alphabetic() || b[i] == b'_' {
                let start = i;
                i += 1;
                while i < b.len() && is_ident_byte(b[i]) {
                    i += 1;
                }
                let tok = &line[start..i];
                if looks_like_symbol(tok) {
                    *freq.entry(tok.to_string()).or_insert(0) += 1;
                }
            } else {
                i += 1;
            }
        }
    }
    // 按权重(模块名 > 出现次数)排序,去重
    let mut seen = HashSet::new();
    let mut ids: Vec<(String, usize)> = freq
        .into_iter()
        .filter(|(s, _)| seen.insert(s.clone()))
        .collect();
    ids.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ids.into_iter()
        .take(MAX_IDENTIFIERS)
        .map(|(s, _)| s)
        .collect()
}

fn ext_of(name: &str) -> Option<&str> {
    name.rsplit_once('.').map(|(_, e)| e)
}

fn should_skip(name: &str) -> bool {
    SKIP_DIRS.iter().any(|d| d.eq_ignore_ascii_case(name))
}

/// 扫描项目目录,收集源码文件(受数量/体积上限约束)。
/// `priority_dirs` 里的目录(通常是改动文件所在目录)先扫,确保相关源码在条目
/// 上限内优先被覆盖——大型项目按字母序遍历会先撞进无关目录而耗尽预算。
pub fn scan_project_with_priority(root: &str, priority_dirs: &[String]) -> Vec<ProjectFile> {
    if root.is_empty() || !Path::new(root).is_dir() {
        return Vec::new();
    }
    let mut out: Vec<ProjectFile> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    let collect = |base: &str, out: &mut Vec<ProjectFile>, seen: &mut HashSet<String>| {
        if out.len() >= MAX_PROJECT_FILES {
            return;
        }
        let mut entries = 0usize;
        for entry in WalkDir::new(base)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                !e.file_type().is_dir() || !should_skip(&e.file_name().to_string_lossy())
            })
            .take(MAX_WALK_ENTRIES)
            .filter_map(Result::ok)
        {
            entries += 1;
            if entries > MAX_WALK_ENTRIES || out.len() >= MAX_PROJECT_FILES {
                break;
            }
            if !entry.file_type().is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(ext) = ext_of(&name) else { continue };
            if !SOURCE_EXTS.contains(&ext.to_ascii_lowercase().as_str()) {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.len() > MAX_FILE_BYTES || meta.len() == 0 {
                continue;
            }
            let path = entry.path().to_string_lossy().into_owned();
            // Windows 上 WalkDir 给反斜杠、join 出来的优先目录可能是正斜杠,
            // 统一成小写+正斜杠再去重,避免同一文件被收两次
            let dedup_key = path.replace('\\', "/").to_lowercase();
            if !seen.insert(dedup_key) {
                continue;
            }
            let Ok(bytes) = std::fs::read(entry.path()) else {
                continue;
            };
            if bytes.contains(&0) {
                continue;
            }
            let Ok(text) = String::from_utf8(bytes) else {
                continue;
            };
            let stem = name
                .rsplit_once('.')
                .map(|(s, _)| s)
                .unwrap_or(&name)
                .to_string();
            out.push(ProjectFile {
                path,
                name,
                stem,
                text,
            });
        }
    };

    // 1) 优先目录:改动文件所在目录(及上一层,覆盖同模块的定义文件)
    let mut prio: Vec<String> = Vec::new();
    for d in priority_dirs {
        if let Some(p) = Path::new(d).parent() {
            prio.push(p.to_string_lossy().into_owned());
        }
        prio.push(d.clone());
    }
    prio.sort();
    prio.dedup();
    for d in &prio {
        if d.starts_with(root) && out.len() < MAX_PROJECT_FILES {
            collect(d, &mut out, &mut seen);
        }
    }
    // 2) 再从项目根补齐,但跳过已优先扫过的子树的重复(靠 seen 去重)
    if out.len() < MAX_PROJECT_FILES {
        collect(root, &mut out, &mut seen);
    }
    out
}

/// 不带优先目录的扫描(测试与简单调用用)
pub fn scan_project(root: &str) -> Vec<ProjectFile> {
    scan_project_with_priority(root, &[])
}

/// 第二期:推断「可能相关」文件。
/// `known_keys` 是会话里已改动 / 已读过的文件 key,命中的一律排除。
pub fn infer_related(
    identifiers: &[String],
    project: &[ProjectFile],
    known_keys: &HashSet<String>,
    root: &str,
    max: usize,
) -> Vec<RelatedFile> {
    // 归一化路径 → (原始路径, because, name_hit, score),避免斜杠差异导致重复
    let mut best: HashMap<String, (String, String, bool, usize)> = HashMap::new();
    for f in project {
        let key = path_key(&f.path, root);
        if known_keys.contains(&key) {
            continue;
        }
        let dedup = f.path.replace('\\', "/").to_lowercase();
        for (rank, id) in identifiers.iter().enumerate() {
            if id.len() < 3 {
                continue;
            }
            let name_hit = f.stem.eq_ignore_ascii_case(id);
            let content_hit = !name_hit && word_mentioned(&f.text, id);
            if !name_hit && !content_hit {
                continue;
            }
            // 文件名命中且标识符越靠前(越具体)得分越高
            let score = (if name_hit { 1000 } else { 0 }) + (identifiers.len() - rank);
            let dominates = best
                .get(&dedup)
                .map_or(true, |(_, _, _, s)| score > *s);
            if dominates {
                best.insert(
                    dedup.clone(),
                    (f.path.clone(), id.clone(), name_hit, score),
                );
            }
            if name_hit {
                break;
            }
        }
    }
    let mut list: Vec<RelatedFile> = best
        .into_iter()
        .map(|(_, (path, because, name_hit, _))| RelatedFile {
            key: path_key(&path, root),
            path,
            because,
            name_hit,
        })
        .collect();
    // 文件名命中的排前面,其余按依据与路径稳定排序
    list.sort_by(|a, b| {
        b.name_hit
            .cmp(&a.name_hit)
            .then_with(|| a.because.cmp(&b.because))
            .then_with(|| a.path.cmp(&b.path))
    });
    list.truncate(max.min(MAX_RELATED));
    list
}

/// `id` 是否作为独立词出现在 text 里(前后不是标识符字符)
fn word_mentioned(text: &str, id: &str) -> bool {
    let mut start = 0;
    while let Some(pos) = text[start..].find(id) {
        let p = start + pos;
        let before_ok = p == 0 || !is_ident_byte(text.as_bytes()[p - 1]);
        let after = p + id.len();
        let after_ok = after >= text.len() || !is_ident_byte(text.as_bytes()[after]);
        if before_ok && after_ok {
            return true;
        }
        start = p + id.len();
        if start >= text.len() {
            break;
        }
    }
    false
}

/// 第三期:从项目文件里扫描出符号定义位置
pub fn build_symbol_index(project: &[ProjectFile], root: &str) -> Vec<SymbolDef> {
    let mut out = Vec::new();
    for f in project {
        for (i, line) in f.text.lines().enumerate() {
            for name in definition_names(line) {
                out.push(SymbolDef {
                    name,
                    path: f.path.clone(),
                    key: path_key(&f.path, root),
                    line: i + 1,
                    text: line.trim().to_string(),
                });
            }
        }
    }
    out
}

/// 大小写不敏感地剥掉前缀,返回原文切片(保留大小写)
fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len() && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes()) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

/// 判断一行是否是某个符号的定义,返回主名(最后一段)。仅测试用。
#[cfg(test)]
fn definition_name(line: &str) -> Option<String> {
    definition_names(line).into_iter().next()
}

/// 返回该行定义涉及的所有标识符段(如 `function GetCoinPopupPanel:Open()` 给出
/// `GetCoinPopupPanel` 与 `Open`)。点击任意一段都应能跳到这一行。
/// 顺序:宿主/类名在前,方法名在后;主名(最后一段)优先返回。
fn definition_names(line: &str) -> Vec<String> {
    let s = line.trim_start();

    // 名字后跟 `(` 的定义:function / def / fn / func
    const CALL_PREFIXES: &[&str] = &[
        "local function ",
        "export async function ",
        "export function ",
        "async function ",
        "function ",
        "async def ",
        "def ",
        "pub async fn ",
        "pub fn ",
        "async fn ",
        "fn ",
        "func ",
    ];
    for p in CALL_PREFIXES {
        if let Some(rest) = strip_prefix_ci(s, p) {
            if let Some(name) = take_call_name(rest) {
                return split_name_segments(&name);
            }
        }
    }
    // `class Name` / `struct Name` / `enum Name` / `interface Name` / `trait Name`
    const WORD_PREFIXES: &[&str] = &[
        "export default class ",
        "export class ",
        "class ",
        "pub struct ",
        "struct ",
        "pub enum ",
        "enum ",
        "pub trait ",
        "trait ",
        "interface ",
    ];
    for p in WORD_PREFIXES {
        if let Some(rest) = strip_prefix_ci(s, p) {
            if let Some(name) = take_word(rest) {
                return split_name_segments(&name);
            }
        }
    }
    // `Name = function` / `local Name = function` / `Name = class` / `export const Name = arrow`
    const ASSIGN_PREFIXES: &[&str] = &[
        "export const ",
        "export let ",
        "export var ",
        "local ",
        "const ",
        "let ",
        "var ",
        "",
    ];
    for p in ASSIGN_PREFIXES {
        if let Some(rest) = strip_prefix_ci(s, p) {
            if let Some(eq) = rest.find('=') {
                let lhs = rest[..eq].trim();
                if !lhs.is_empty() && lhs.bytes().all(|b| is_ident_byte(b)) {
                    let rhs = rest[eq + 1..].trim_start();
                    if rhs.starts_with("function")
                        || rhs.starts_with("class")
                        || rhs.starts_with("async")
                        || rhs.starts_with("(")
                        || rhs.starts_with("=>")
                    {
                        return split_name_segments(lhs);
                    }
                }
            }
        }
    }
    Vec::new()
}

/// 把 `A.B` / `A:B` / `A:B.C` 拆成有效段,主名(最后一段)放最前,其余宿主段随后;
/// 过滤关键字与过短名,保留原大小写。
fn split_name_segments(name: &str) -> Vec<String> {
    let parts: Vec<&str> = name
        .split(|c| c == '.' || c == ':')
        .filter(|p| !p.is_empty())
        .collect();
    let mut out = Vec::new();
    // 主名优先
    if let Some(last) = parts.last() {
        if let Some(n) = clean_name(last) {
            out.push(n);
        }
    }
    // 其余宿主段(如类名)也收,便于按类名跳到方法定义
    for p in parts.iter().take(parts.len().saturating_sub(1)) {
        if let Some(n) = clean_name(p) {
            if !out.contains(&n) {
                out.push(n);
            }
        }
    }
    out
}

/// 从 `Foo(...)` / `M.Foo(...)` / `M:Foo(...)` 开头取出名字段
fn take_call_name(s: &str) -> Option<String> {
    let end = s.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == ':'))?;
    let name = &s[..end];
    if name.is_empty() {
        return None;
    }
    Some(name.to_string())
}

fn take_word(s: &str) -> Option<String> {
    let end = s
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(s.len());
    let w = &s[..end];
    if w.is_empty() {
        None
    } else {
        Some(w.to_string())
    }
}

/// 去掉 `M.` / `M:` 前缀,只留最后一段;过滤关键字与过短名
fn clean_name(name: &str) -> Option<String> {
    let last = name
        .rsplit(|c| c == '.' || c == ':')
        .next()
        .unwrap_or(name)
        .to_string();
    if is_identifier(&last) && !is_keyword(&last) && last.len() >= 2 {
        Some(last)
    } else {
        None
    }
}

/// 第三期:在索引里查某个标识符的定义(精确匹配,取行号最靠前的)
pub fn find_definition<'a>(index: &'a [SymbolDef], name: &str) -> Option<&'a SymbolDef> {
    index
        .iter()
        .filter(|d| d.name == name)
        .min_by_key(|d| d.line)
}

/// 把行文本拆成 (片段, 是否为标识符) 的序列,供 GUI 把标识符渲染成可点击
pub fn tokenize_line(line: &str) -> Vec<(String, bool)> {
    let b = line.as_bytes();
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut i = 0;
    let mut plain_start = 0;
    let push_plain = |out: &mut Vec<(String, bool)>, s: &str, a: usize, z: usize| {
        if a < z {
            out.push((s[a..z].to_string(), false));
        }
    };
    while i < b.len() {
        if b[i].is_ascii_alphabetic() || b[i] == b'_' {
            push_plain(&mut out, line, plain_start, i);
            let start = i;
            i += 1;
            while i < b.len() && is_ident_byte(b[i]) {
                i += 1;
            }
            out.push((line[start..i].to_string(), true));
            plain_start = i;
        } else {
            i += 1;
        }
    }
    push_plain(&mut out, line, plain_start, line.len());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn extracts_modules_and_pascal_case() {
        let text = r#"
local popup = GetCoinPopupPanel.new()
require("ActivityCenterModule/Panel/ThemeActivityMain/View/ActivityConst")
local ThemeActivity_Store = require("UAGame.Modules.ThemeActivity")
import FooBarWidget from "./widgets/FooBarWidget"
local counter = 1
"#;
        let ids = extract_identifiers(text);
        assert!(ids.contains(&"ActivityConst".to_string()), "{ids:?}");
        assert!(ids.contains(&"GetCoinPopupPanel".to_string()), "{ids:?}");
        assert!(ids.contains(&"ThemeActivity_Store".to_string()), "{ids:?}");
        assert!(ids.contains(&"FooBarWidget".to_string()), "{ids:?}");
        // 纯小写普通词不该进
        assert!(!ids.contains(&"counter".to_string()), "{ids:?}");
    }

    #[test]
    fn scan_and_infer_excludes_known_and_marks_because() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("node_modules/junk")).unwrap();
        fs::write(
            root.join("src/GetCoinPopupPanel.lua"),
            "local GetCoinPopupPanel = {}\nfunction GetCoinPopupPanel:Open()\nend\n",
        )
        .unwrap();
        fs::write(
            root.join("src/ActivityConst.lua"),
            "local ActivityConst = {}\nreturn ActivityConst\n",
        )
        .unwrap();
        fs::write(root.join("src/Unrelated.lua"), "print('nothing')\n").unwrap();
        fs::write(
            root.join("node_modules/junk/GetCoinPopupPanel.lua"),
            "decoy",
        )
        .unwrap();

        let root_s = root.to_string_lossy().into_owned();
        let project = scan_project(&root_s);
        // node_modules 被跳过
        assert!(project.iter().all(|f| !f.path.contains("node_modules")));
        assert_eq!(project.len(), 3);

        let ids = vec!["GetCoinPopupPanel".to_string(), "ActivityConst".to_string()];
        let mut known = HashSet::new();
        // 假设 GetCoinPopupPanel.lua 是本次改动过的文件,应被排除
        known.insert(path_key(
            &root.join("src/GetCoinPopupPanel.lua").to_string_lossy(),
            &root_s,
        ));
        let related = infer_related(&ids, &project, &known, &root_s, 10);
        let because: Vec<(&str, &str)> = related
            .iter()
            .map(|r| (r.path.rsplit('\\').next().unwrap_or(&r.path), r.because.as_str()))
            .collect();
        assert!(
            related.iter().all(|r| !r.path.contains("GetCoinPopupPanel.lua")),
            "changed file must be excluded: {because:?}"
        );
        assert!(
            related.iter().any(|r| r.path.ends_with("ActivityConst.lua") && r.because == "ActivityConst"),
            "{because:?}"
        );
        assert!(
            related.iter().all(|r| !r.path.ends_with("Unrelated.lua")),
            "{because:?}"
        );
    }

    #[test]
    fn builds_symbol_index_and_finds_definition() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/widgets.lua"),
            "local M = {}\nfunction GetCoinPopupPanel:Open()\n  return true\nend\nfunction M.Helper()\nend\nlocal Other = function() end\nclass FooBar\nend\nfn rusty() {}\ndef pyname(x):\n    pass\n",
        )
        .unwrap();
        let root_s = root.to_string_lossy().into_owned();
        let project = scan_project(&root_s);
        let index = build_symbol_index(&project, &root_s);
        let names: Vec<&str> = index.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"GetCoinPopupPanel"), "{names:?}");
        assert!(names.contains(&"Helper"), "{names:?}");
        assert!(names.contains(&"Other"), "{names:?}");
        assert!(names.contains(&"FooBar"), "{names:?}");
        assert!(names.contains(&"rusty"), "{names:?}");
        assert!(names.contains(&"pyname"), "{names:?}");

        let d = find_definition(&index, "GetCoinPopupPanel").unwrap();
        assert_eq!(d.line, 2);
        assert!(d.text.starts_with("function GetCoinPopupPanel"));
        assert!(find_definition(&index, "NoSuchThing").is_none());
    }

    #[test]
    fn tokenize_splits_identifiers() {
        let tokens = tokenize_line("local x = GetPanel.new()");
        assert_eq!(
            tokens,
            vec![
                ("local".to_string(), true),
                (" ".to_string(), false),
                ("x".to_string(), true),
                (" = ".to_string(), false),
                ("GetPanel".to_string(), true),
                (".".to_string(), false),
                ("new".to_string(), true),
                ("()".to_string(), false),
            ]
        );
        // 拼回去必须和原文一致
        let joined: String = tokens.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(joined, "local x = GetPanel.new()");
    }

    #[test]
    fn definition_name_variants() {
        let cases = [
            ("function Foo()", Some("Foo")),
            ("function M.Bar()", Some("Bar")),
            ("function M:Baz()", Some("Baz")),
            ("local function localfn(a)", Some("localfn")),
            ("class Widget", Some("Widget")),
            ("pub struct Thing", Some("Thing")),
            ("async def worker():", Some("worker")),
            ("pub fn compute()", Some("compute")),
            ("local Handler = function()", Some("Handler")),
            ("const make = () =>", Some("make")),
            ("    print('hi')", None),
            ("end", None),
        ];
        for (line, want) in cases {
            assert_eq!(definition_name(line), want.map(str::to_string), "line: {line}");
        }
    }
}
