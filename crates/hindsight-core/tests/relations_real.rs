//! 用真实项目根(若存在)确定性验证第二/三期数据管线能跑通并产出有效结果。
//! 项目根不存在时跳过(本地开发机不一定有该目录)。
use std::collections::HashSet;

use hindsight_core::changes::path_key;
use hindsight_core::relations as rel;

#[test]
fn real_project_pipeline_produces_related_and_symbols() {
    let root = r"D:\Project\0830Master\R6Game";
    if !std::path::Path::new(root).is_dir() {
        eprintln!("skip: real project root not present");
        return;
    }

    // 模拟一段改动文本:包含类名与 require 模块名(取自会话里的 Lua 改动风格)
    let changed = r#"
local popup = GetCoinPopupPanel.new()
require("UAGame.Modules.ActivityCenterModule.Panel.ThemeActivityMain.View.ThemeActivityConst")
local ThemeActivity_Store = require("UAGame.Services.ThemeActivity.ThemeActivity_Store")
function ThemeActivity_SGradeShop:OnInit()
    self:RefreshStageTabs()
end
"#;

    let identifiers = rel::extract_identifiers(changed);
    assert!(!identifiers.is_empty(), "should extract identifiers");
    assert!(
        identifiers.iter().any(|i| i.contains("ThemeActivity")
            || i.contains("GetCoin")
            || i.contains("Popup")),
        "identifiers: {identifiers:?}"
    );

    // 改动文件所在目录(真实存在,含 744 个 lua)优先扫描
    let view_dir = std::path::Path::new(root)
        .join("Content/Script/UAGame/Modules/ActivityCenterModule/Panel/ThemeActivityMain/View");
    assert!(view_dir.is_dir(), "view dir should exist: {view_dir:?}");
    let project = rel::scan_project_with_priority(
        root,
        &[view_dir.to_string_lossy().into_owned()],
    );
    assert!(!project.is_empty(), "should scan some source files");
    // 扫描上限生效,不会把 130 万文件全读进来
    assert!(project.len() <= 4000, "scanned too many: {}", project.len());
    // 优先目录里的文件必须被覆盖到
    assert!(
        project.iter().any(|f| f.path.contains("ThemeActivityMain")),
        "priority dir files should be present: {} files, sample {:?}",
        project.len(),
        project.iter().take(3).map(|f| &f.path).collect::<Vec<_>>()
    );

    let mut known = HashSet::new();
    // 假设本次改动过 ThemeActivity_SGradeShop.lua
    let changed_file = view_dir.join("ThemeActivity_SGradeShop.lua");
    known.insert(path_key(&changed_file.to_string_lossy(), root));

    let related = rel::infer_related(&identifiers, &project, &known, root, rel::MAX_RELATED);
    // 推断结果每一项都应带依据,且不与已知文件重复
    // 跨层契约:related.key 必须等于 path_key(related.path, root),否则 GUI 选中反查不到
    let mut norm_seen: HashSet<String> = HashSet::new();
    for r in &related {
        assert!(!r.because.is_empty());
        assert!(!known.contains(&r.key), "related must exclude changed file: {}", r.path);
        assert_eq!(r.key, path_key(&r.path, root), "key/path contract");
        let norm = r.path.replace('\\', "/").to_lowercase();
        assert!(norm_seen.insert(norm), "duplicate related path: {}", r.path);
        // GUI 点开时需要能从磁盘读到这个文件
        assert!(std::path::Path::new(&r.path).is_file(), "not a real file: {}", r.path);
    }
    eprintln!("identifiers = {identifiers:?}");
    eprintln!("scanned files = {}", project.len());
    eprintln!("related count = {}", related.len());
    for r in related.iter().take(8) {
        eprintln!("  {} <- {}", r.path, r.because);
    }

    let symbols = rel::build_symbol_index(&project, root);
    assert!(!symbols.is_empty(), "should build some symbol defs");
    // 每个定义都应带真实行号与文件 key
    for s in symbols.iter().take(200) {
        assert!(s.line >= 1);
        assert!(!s.key.is_empty());
        assert!(!s.name.is_empty());
    }
    eprintln!("symbol defs = {}", symbols.len());
    // 跨层契约:symbol.key 同样等于 path_key(path, root),GUI 跳转才能选中目标文件
    for s in symbols.iter().take(200) {
        assert_eq!(s.key, path_key(&s.path, root), "symbol key/path contract");
    }
    // 抽查:随便选一个索引里的名字应能反查到定义
    if let Some(first) = symbols.first() {
        assert!(rel::find_definition(&symbols, &first.name).is_some());
    }
    // 第三期:源码行 tokenize 后拼回必须还原原文(点击渲染不破坏代码)
    let sample = "function ThemeActivity_SGradeShop:OnInit() -- init";
    let toks = rel::tokenize_line(sample);
    let joined: String = toks.iter().map(|(s, _)| s.as_str()).collect();
    assert_eq!(joined, sample);
    assert!(toks.iter().any(|(s, ident)| *ident && s == "ThemeActivity_SGradeShop"));
}
