use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

use arc_swap::{ArcSwap, Guard};
use maki_storage::StateDir;
use ratatui::style::{Color, Modifier, Style};
use serde::Deserialize;
use syntect::highlighting::{
    Color as SynColor, FontStyle, ScopeSelectors, StyleModifier, ThemeItem, ThemeSettings,
};

const DEFAULT_THEME: &str = "dracula";
const RESERVED_KEYS: &[&str] = &["palette", "ui", "inherits"];

const HELIX_TO_TEXTMATE: &[(&str, &str)] = &[
    ("comment", "comment, comment punctuation.definition.comment"),
    (
        "comment.line",
        "comment.line, comment.line punctuation.definition.comment",
    ),
    (
        "comment.block",
        "comment.block, comment.block punctuation.definition.comment",
    ),
    (
        "comment.line.documentation",
        "comment.line.documentation, comment.line.documentation punctuation.definition.comment",
    ),
    (
        "comment.block.documentation",
        "comment.block.documentation, comment.block.documentation punctuation.definition.comment",
    ),
    ("string", "string, string punctuation.definition.string"),
    (
        "string.regexp",
        "string.regexp, string.regexp punctuation.definition.string",
    ),
    (
        "string.special",
        "string.special, string.quoted.single punctuation.definition.string, string.quoted.double.raw punctuation.definition.string",
    ),
    ("function", "entity.name.function, variable.function"),
    ("function.builtin", "support.function"),
    (
        "function.call",
        "entity.name.function, variable.function, support.function",
    ),
    (
        "function.macro",
        "entity.name.function.macro, support.macro",
    ),
    (
        "function.method",
        "entity.name.function, meta.function-call",
    ),
    ("constructor", "entity.name.function.constructor"),
    (
        "type",
        "entity.name.type, entity.name.class, entity.name.struct, entity.name.enum, entity.name.trait, entity.name.union, entity.name.impl, support.type, support.class, meta.generic",
    ),
    ("type.builtin", "support.type, storage.type.primitive"),
    ("type.enum.variant", "entity.name.type.enum"),
    ("tag", "entity.name.tag"),
    ("tag.attribute", "entity.other.attribute-name"),
    ("tag.delimiter", "punctuation.definition.tag"),
    ("variable", "variable.other"),
    ("variable.builtin", "variable.language"),
    ("variable.parameter", "variable.parameter"),
    (
        "variable.other.member",
        "variable.other.member, variable.other.property",
    ),
    (
        "constant",
        "constant, variable.other.constant, entity.name.constant",
    ),
    ("constant.builtin", "constant.language"),
    (
        "constant.builtin.boolean",
        "constant.language.boolean, constant.language",
    ),
    (
        "constant.character.escape",
        "constant.character.escape, constant.character.escaped",
    ),
    (
        "keyword.storage.type",
        "storage.type, keyword.declaration, keyword.declaration.function, keyword.declaration.class, keyword.declaration.struct, keyword.declaration.enum, keyword.declaration.trait, keyword.declaration.impl",
    ),
    ("keyword.storage.modifier", "storage.modifier"),
    (
        "keyword.function",
        "keyword.declaration.function, storage.type.function",
    ),
    (
        "keyword.control.import",
        "keyword.control.import, keyword.other",
    ),
    ("keyword.return", "keyword.control.return, keyword.control"),
    ("keyword.directive", "meta.preprocessor"),
    ("keyword.control.exception", "keyword.control.exception"),
    ("punctuation", "punctuation, punctuation.accessor.dot"),
    (
        "punctuation.special",
        "punctuation.section.embedded, punctuation.section.interpolation, punctuation.separator.namespace, punctuation.accessor",
    ),
    ("label", "entity.name.label, storage.modifier.lifetime"),
    (
        "attribute",
        "entity.other.attribute-name, meta.annotation, variable.annotation, meta.annotation punctuation.definition.annotation, meta.annotation punctuation.section.group",
    ),
    (
        "namespace",
        "entity.name.namespace, entity.name.module, meta.path",
    ),
    (
        "markup.raw",
        "markup.raw, markup.raw.inline, markup.raw.block",
    ),
    ("markup.link.url", "markup.underline.link"),
    ("operator", "keyword.operator"),
];

pub struct ThemeEntry {
    pub name: &'static str,
    pub toml: &'static str,
}

pub static BUNDLED_THEMES: &[ThemeEntry] = &[
    ThemeEntry {
        name: "ayu_dark",
        toml: include_str!("themes/ayu_dark.toml"),
    },
    ThemeEntry {
        name: "carbonfox",
        toml: include_str!("themes/carbonfox.toml"),
    },
    ThemeEntry {
        name: "catppuccin_frappe",
        toml: include_str!("themes/catppuccin_frappe.toml"),
    },
    ThemeEntry {
        name: "catppuccin_latte",
        toml: include_str!("themes/catppuccin_latte.toml"),
    },
    ThemeEntry {
        name: "catppuccin_macchiato",
        toml: include_str!("themes/catppuccin_macchiato.toml"),
    },
    ThemeEntry {
        name: "catppuccin_mocha",
        toml: include_str!("themes/catppuccin_mocha.toml"),
    },
    ThemeEntry {
        name: "dracula",
        toml: include_str!("themes/dracula.toml"),
    },
    ThemeEntry {
        name: "everforest_dark",
        toml: include_str!("themes/everforest_dark.toml"),
    },
    ThemeEntry {
        name: "fleet_dark",
        toml: include_str!("themes/fleet_dark.toml"),
    },
    ThemeEntry {
        name: "github_dark",
        toml: include_str!("themes/github_dark.toml"),
    },
    ThemeEntry {
        name: "gruvbox",
        toml: include_str!("themes/gruvbox.toml"),
    },
    ThemeEntry {
        name: "gruvbox_light",
        toml: include_str!("themes/gruvbox_light.toml"),
    },
    ThemeEntry {
        name: "kanagawa",
        toml: include_str!("themes/kanagawa.toml"),
    },
    ThemeEntry {
        name: "material_darker",
        toml: include_str!("themes/material_darker.toml"),
    },
    ThemeEntry {
        name: "monokai_pro",
        toml: include_str!("themes/monokai_pro.toml"),
    },
    ThemeEntry {
        name: "night_owl",
        toml: include_str!("themes/night_owl.toml"),
    },
    ThemeEntry {
        name: "nightfox",
        toml: include_str!("themes/nightfox.toml"),
    },
    ThemeEntry {
        name: "nord",
        toml: include_str!("themes/nord.toml"),
    },
    ThemeEntry {
        name: "onedark",
        toml: include_str!("themes/onedark.toml"),
    },
    ThemeEntry {
        name: "rose_pine",
        toml: include_str!("themes/rose_pine.toml"),
    },
    ThemeEntry {
        name: "solarized_dark",
        toml: include_str!("themes/solarized_dark.toml"),
    },
    ThemeEntry {
        name: "solarized_light",
        toml: include_str!("themes/solarized_light.toml"),
    },
    ThemeEntry {
        name: "tokyonight",
        toml: include_str!("themes/tokyonight.toml"),
    },
    ThemeEntry {
        name: "vscode_dark_plus",
        toml: include_str!("themes/vscode_dark_plus.toml"),
    },
    ThemeEntry {
        name: "zenburn",
        toml: include_str!("themes/zenburn.toml"),
    },
];

static THEME: LazyLock<ArcSwap<Theme>> =
    LazyLock::new(|| ArcSwap::from_pointee(Theme::load_or_bundled()));

static GENERATION: AtomicU64 = AtomicU64::new(0);

pub fn current() -> Guard<Arc<Theme>> {
    THEME.load()
}

pub fn set(theme: Theme) {
    // Order matters: install colors before bumping the counter, otherwise a
    // reader could see the new generation but bake with the old palette.
    THEME.store(Arc::new(theme));
    crate::highlight::refresh_syntax_theme();
    GENERATION.fetch_add(1, Ordering::Release);
}

pub fn generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

pub fn load_by_name(name: &str) -> Result<Theme, String> {
    BUNDLED_THEMES
        .iter()
        .find(|e| e.name == name)
        .map(|e| Theme::from_toml(e.toml))
        .unwrap_or_else(|| Err(format!("unknown theme: {name}")))
}

pub fn persist_theme(name: &str) {
    if let Ok(dir) = StateDir::resolve() {
        maki_storage::theme::persist_theme_name(&dir, name);
    }
}

fn read_theme_name() -> Option<String> {
    let dir = StateDir::resolve().ok()?;
    maki_storage::theme::read_theme_name(&dir)
}

pub fn current_theme_name() -> String {
    read_theme_name().unwrap_or_else(|| DEFAULT_THEME.to_owned())
}

pub fn style_by_name(name: &str) -> Style {
    let t = current();
    match name {
        "dim" | "tool_dim" => t.tool_dim,
        "path" | "tool_path" => t.tool_path,
        "tool" => t.tool,
        "error" => t.error,
        "bold" => t.bold,
        "italic" => t.italic,
        "bold_italic" => t.bold_italic,
        "inline_code" => t.inline_code,
        "strikethrough" => t.strikethrough,
        "heading" => t.heading,
        "list_marker" => t.list_marker,
        "horizontal_rule" => t.horizontal_rule,
        "code_gutter" => t.code_gutter,
        "table_border" => t.table_border,
        "keyword" | "index_keyword" => t.index_keyword,
        "section" | "index_section" => t.index_section,
        "line_nr" | "index_line_nr" => t.index_line_nr,
        "diff_old" => t.diff_old,
        "diff_new" => t.diff_new,
        "item" => t.item,
        "item_desc" => t.item_desc,
        "item_selected" | "selected" => t.item_selected,
        "item_match" | "match" => t.item_match,
        "item_match_selected" | "match_selected" => t.item_match_selected,
        "cursor" => t.cursor,
        "accent" => t.accent,
        "active" => t.active,
        "success" => t.todo_completed,
        "warning" => t.todo_in_progress,
        _ => Style::default(),
    }
}

#[derive(Debug)]
pub struct Theme {
    pub background: Color,
    pub foreground: Color,

    pub user: Style,
    pub assistant: Style,
    pub assistant_prefix: Style,
    pub thinking: Style,
    pub tool_bg: Style,
    pub tool: Style,
    pub tool_path: Style,
    pub tool_annotation: Style,
    pub tool_prefix: Style,
    pub tool_success: Style,
    pub tool_error: Style,
    pub tool_dim: Style,
    pub error: Style,
    pub status_dim: Style,
    pub bold: Style,
    pub italic: Style,
    pub bold_italic: Style,
    pub inline_code: Style,
    pub code_block: Style,
    pub code_gutter: Style,
    pub strikethrough: Style,
    pub heading: Style,
    pub list_marker: Style,
    pub horizontal_rule: Style,
    pub plan_rule: Style,
    pub table_border: Style,
    pub diff_old: Style,
    pub diff_new: Style,
    pub diff_old_emphasis: Style,
    pub diff_new_emphasis: Style,
    pub diff_line_nr: Style,
    pub todo_completed: Style,
    pub todo_in_progress: Style,
    #[allow(dead_code)]
    pub todo_pending: Style,
    #[allow(dead_code)]
    pub todo_cancelled: Style,
    pub item_selected: Style,
    pub item: Style,
    pub item_desc: Style,
    pub item_match: Style,
    pub item_match_selected: Style,
    pub panel_border: Style,
    pub panel_title: Style,
    pub cursor: Style,
    pub input_border: Style,
    pub accent: Style,
    pub active: Style,
    pub keybind_key: Style,
    pub keybind_desc: Style,
    pub keybind_section: Style,
    pub mode_build: Color,
    pub mode_plan: Color,
    pub mode_bash: Color,
    pub queue: Style,
    pub plan_path: Style,
    pub status_notice: Style,
    pub status_retry_error: Style,
    pub status_retry_info: Style,
    pub input_placeholder: Style,
    pub queue_delete: Style,
    pub timestamp: Style,
    pub spinner: Style,
    pub index_section: Style,
    pub index_line_nr: Style,
    pub index_keyword: Style,
    pub shell_prefix: Style,

    pub syntax: syntect::highlighting::Theme,
}

#[derive(Deserialize)]
struct StyleDef {
    fg: Option<String>,
    bg: Option<String>,
    #[serde(default)]
    modifiers: Vec<String>,
}

fn helix_to_textmate_scope(key: &str) -> &str {
    for &(helix, tm) in HELIX_TO_TEXTMATE {
        if key == helix {
            return tm;
        }
    }
    key
}

fn parse_hex_rgb(s: &str) -> Option<(u8, u8, u8)> {
    let hex = s.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some((r, g, b))
}

fn parse_hex(s: &str) -> Option<Color> {
    let (r, g, b) = parse_hex_rgb(s)?;
    Some(Color::Rgb(r, g, b))
}

fn parse_syn_color(s: &str, palette: &HashMap<String, String>) -> Option<SynColor> {
    let resolved = if s.starts_with('#') {
        s
    } else {
        palette.get(s)?.as_str()
    };
    let (r, g, b) = parse_hex_rgb(resolved)?;
    Some(SynColor { r, g, b, a: 0xFF })
}

fn resolve_color(name: &str, palette: &HashMap<String, Color>) -> Option<Color> {
    if name.starts_with('#') {
        parse_hex(name)
    } else {
        palette.get(name).copied()
    }
}

fn resolve_modifier(name: &str) -> Modifier {
    match name {
        "bold" => Modifier::BOLD,
        "italic" => Modifier::ITALIC,
        "underlined" => Modifier::UNDERLINED,
        "crossed_out" => Modifier::CROSSED_OUT,
        "dim" => Modifier::DIM,
        "reversed" => Modifier::REVERSED,
        _ => Modifier::empty(),
    }
}

fn resolve_style(def: &StyleDef, palette: &HashMap<String, Color>) -> Style {
    let mut style = Style::new();
    if let Some(fg) = def.fg.as_ref().and_then(|n| resolve_color(n, palette)) {
        style = style.fg(fg);
    }
    if let Some(bg) = def.bg.as_ref().and_then(|n| resolve_color(n, palette)) {
        style = style.bg(bg);
    }
    for m in &def.modifiers {
        style = style.add_modifier(resolve_modifier(m));
    }
    style
}

fn scope_fg(
    full_table: &toml::Table,
    palette: &HashMap<String, Color>,
    raw_palette: &HashMap<String, String>,
    scope: &str,
) -> Option<Color> {
    let table = full_table.get(scope)?.as_table()?;
    let fg_val = table.get("fg")?.as_str()?;
    resolve_color(fg_val, palette).or_else(|| {
        let resolved = raw_palette.get(fg_val)?;
        parse_hex(resolved)
    })
}

fn resolve_font_style(modifiers: &[String]) -> FontStyle {
    let mut fs = FontStyle::empty();
    for m in modifiers {
        match m.as_str() {
            "bold" => fs |= FontStyle::BOLD,
            "italic" => fs |= FontStyle::ITALIC,
            "underlined" => fs |= FontStyle::UNDERLINE,
            _ => {}
        }
    }
    fs
}

fn style_def_to_syn(def: &StyleDef, raw_palette: &HashMap<String, String>) -> StyleModifier {
    let has_color = def.fg.is_some() || def.bg.is_some();
    StyleModifier {
        foreground: def
            .fg
            .as_ref()
            .and_then(|n| parse_syn_color(n, raw_palette)),
        background: def
            .bg
            .as_ref()
            .and_then(|n| parse_syn_color(n, raw_palette)),
        font_style: if def.modifiers.is_empty() {
            if has_color {
                Some(FontStyle::empty())
            } else {
                None
            }
        } else {
            Some(resolve_font_style(&def.modifiers))
        },
    }
}

fn build_syntax_theme(
    toml_table: &toml::Table,
    raw_palette: &HashMap<String, String>,
) -> syntect::highlighting::Theme {
    let fg = parse_syn_color("foreground", raw_palette);
    let bg = parse_syn_color("background", raw_palette);

    let settings = ThemeSettings {
        foreground: fg,
        background: bg,
        caret: fg,
        line_highlight: parse_syn_color("current_line", raw_palette)
            .or_else(|| parse_syn_color("selection", raw_palette)),
        selection: parse_syn_color("selection", raw_palette)
            .or_else(|| parse_syn_color("current_line", raw_palette)),
        ..Default::default()
    };

    let mut scopes = Vec::new();

    for (key, value) in toml_table {
        if RESERVED_KEYS.contains(&key.as_str()) || key.starts_with("ui.") {
            continue;
        }

        let Some(table) = value.as_table() else {
            continue;
        };

        let def: StyleDef = match toml::Value::Table(table.clone()).try_into() {
            Ok(d) => d,
            Err(_) => continue,
        };

        let tm_scope = helix_to_textmate_scope(key);

        let Ok(scope) = tm_scope.parse::<ScopeSelectors>() else {
            continue;
        };

        scopes.push(ThemeItem {
            scope,
            style: style_def_to_syn(&def, raw_palette),
        });
    }

    syntect::highlighting::Theme {
        name: None,
        author: None,
        settings,
        scopes,
    }
}

impl Theme {
    fn from_toml(toml_str: &str) -> Result<Self, String> {
        let full_table: toml::Table = toml::from_str(toml_str).map_err(|e| e.to_string())?;

        let raw_palette: HashMap<String, String> = full_table
            .get("palette")
            .and_then(|v| v.as_table())
            .map(|t| {
                t.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                    .collect()
            })
            .unwrap_or_default();

        let palette: HashMap<String, Color> = raw_palette
            .iter()
            .filter_map(|(k, v)| parse_hex(v).map(|c| (k.clone(), c)))
            .collect();

        let ui: HashMap<String, StyleDef> = full_table
            .get("ui")
            .and_then(|v| v.as_table())
            .map(|t| {
                t.iter()
                    .filter_map(|(k, v)| {
                        let def: StyleDef = v.clone().try_into().ok()?;
                        Some((k.clone(), def))
                    })
                    .collect()
            })
            .unwrap_or_default();

        let style = |key: &str| -> Style {
            ui.get(key)
                .map(|d| resolve_style(d, &palette))
                .unwrap_or_default()
        };

        let derived_color = |ui_key: &str, scopes: &[&str]| -> Color {
            if let Some(c) = palette.get(ui_key) {
                return *c;
            }
            for scope in scopes {
                if let Some(c) = scope_fg(&full_table, &palette, &raw_palette, scope) {
                    return c;
                }
            }
            Color::Reset
        };

        let derived_style = |ui_key: &str, scopes: &[&str], mods: Modifier| -> Style {
            if let Some(d) = ui.get(ui_key) {
                return resolve_style(d, &palette);
            }
            for scope in scopes {
                if let Some(c) = scope_fg(&full_table, &palette, &raw_palette, scope) {
                    return Style::new().fg(c).add_modifier(mods);
                }
            }
            Style::default()
        };

        let syntax = build_syntax_theme(&full_table, &raw_palette);

        let color = |key: &str| -> Color { palette.get(key).copied().unwrap_or(Color::Reset) };

        let bold_style = derived_style(
            "bold",
            &["markup.bold", "variable.parameter"],
            Modifier::BOLD,
        );

        Ok(Self {
            background: color("background"),
            foreground: color("foreground"),

            user: style("user"),
            assistant: style("assistant"),
            assistant_prefix: style("assistant_prefix"),
            thinking: brighten_toward(
                style("thinking"),
                color("comment"),
                color("foreground"),
                0.3,
            ),
            tool_bg: style("tool_bg"),
            tool: style("tool"),
            tool_path: style("tool_path"),
            tool_annotation: style("tool_annotation"),
            tool_prefix: style("tool_prefix"),
            tool_success: style("tool_success"),
            tool_error: style("tool_error"),
            tool_dim: style("tool_dim"),
            error: style("error"),
            status_dim: style("status_dim"),
            bold: bold_style,
            italic: ui
                .get("italic")
                .map(|d| resolve_style(d, &palette))
                .unwrap_or_else(|| Style::default().add_modifier(Modifier::ITALIC)),
            bold_italic: ui
                .get("bold_italic")
                .map(|d| resolve_style(d, &palette))
                .unwrap_or_else(|| bold_style.add_modifier(Modifier::ITALIC)),
            inline_code: derived_style(
                "inline_code",
                &["function.call", "function"],
                Modifier::empty(),
            ),
            code_block: style("code_block"),
            code_gutter: derived_style(
                "code_gutter",
                &["variable.parameter", "string"],
                Modifier::empty(),
            ),
            strikethrough: style("strikethrough"),
            heading: derived_style(
                "heading",
                &["keyword.storage.type", "keyword"],
                Modifier::BOLD,
            ),
            list_marker: derived_style(
                "list_marker",
                &["keyword.storage.type", "keyword"],
                Modifier::empty(),
            ),
            horizontal_rule: style("horizontal_rule"),
            plan_rule: style("plan_rule"),
            table_border: style("table_border"),
            diff_old: style("diff_old"),
            diff_new: style("diff_new"),
            diff_old_emphasis: style("diff_old_emphasis"),
            diff_new_emphasis: style("diff_new_emphasis"),
            diff_line_nr: style("diff_line_nr"),
            todo_completed: style("todo_completed"),
            todo_in_progress: style("todo_in_progress"),
            todo_pending: style("todo_pending"),
            todo_cancelled: style("todo_cancelled"),
            item_selected: style("item_selected"),
            item: style("item"),
            item_desc: style("item_desc"),
            item_match: {
                let s = style("item_match");
                if s == Style::default() {
                    style("item")
                        .fg(style("accent").fg.unwrap_or_default())
                        .add_modifier(Modifier::BOLD)
                } else {
                    s
                }
            },
            item_match_selected: {
                let s = style("item_match_selected");
                if s == Style::default() {
                    style("item_selected")
                        .fg(style("accent").fg.unwrap_or_default())
                        .add_modifier(Modifier::BOLD)
                } else {
                    s
                }
            },
            panel_border: style("panel_border"),
            panel_title: style("panel_title"),
            cursor: style("cursor"),
            input_border: style("input_border"),
            accent: style("accent"),
            active: {
                let s = style("active");
                if s == Style::default() {
                    style("accent")
                } else {
                    s
                }
            },
            keybind_key: style("keybind_key"),
            keybind_desc: style("keybind_desc"),
            keybind_section: style("keybind_section"),
            mode_build: derived_color("mode_build", &["keyword.storage.type", "keyword"]),
            mode_plan: derived_color("mode_plan", &["keyword", "keyword.storage.type"]),
            mode_bash: derived_color("mode_bash", &["function.builtin", "function"]),
            queue: style("queue"),
            plan_path: style("plan_path"),
            status_notice: style("status_notice"),
            status_retry_error: style("status_retry_error"),
            status_retry_info: style("status_retry_info"),
            input_placeholder: style("input_placeholder"),
            queue_delete: style("queue_delete"),
            timestamp: style("timestamp"),
            spinner: style("spinner"),
            index_section: derived_style(
                "index_section",
                &["keyword.storage.type", "keyword"],
                Modifier::BOLD,
            ),
            index_line_nr: derived_style("index_line_nr", &["comment"], Modifier::empty()),
            index_keyword: derived_style("index_keyword", &["keyword"], Modifier::empty()),
            shell_prefix: derived_style("shell_prefix", &["string"], Modifier::BOLD),
            syntax,
        })
    }

    fn load_or_bundled() -> Self {
        if let Some(name) = read_theme_name()
            && let Ok(theme) = load_by_name(&name)
        {
            return theme;
        }
        Self::from_toml(BUNDLED_THEMES[0].toml).expect("bundled theme must parse")
    }
}

pub(crate) fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t.clamp(0.0, 1.0)) as u8
}

pub(crate) fn dim_style(style: Style, factor: f32) -> Style {
    match (style.fg, current().background) {
        (Some(Color::Rgb(fr, fg, fb)), Color::Rgb(br, bg, bb)) => style.fg(Color::Rgb(
            lerp_u8(fr, br, factor),
            lerp_u8(fg, bg, factor),
            lerp_u8(fb, bb, factor),
        )),
        _ => style,
    }
}

fn brighten_toward(style: Style, from: Color, to: Color, t: f32) -> Style {
    match (from, to) {
        (Color::Rgb(fr, fg, fb), Color::Rgb(tr, tg, tb)) => style.fg(Color::Rgb(
            lerp_u8(fr, tr, t),
            lerp_u8(fg, tg, t),
            lerp_u8(fb, tb, t),
        )),
        _ => style,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    fn dracula_toml() -> &'static str {
        BUNDLED_THEMES
            .iter()
            .find(|e| e.name == "dracula")
            .expect("dracula theme must exist")
            .toml
    }

    fn dracula() -> Theme {
        Theme::from_toml(dracula_toml()).unwrap()
    }

    #[test]
    fn dracula_theme_fields() {
        let t = dracula();
        assert_eq!(t.background, Color::Rgb(0x28, 0x2a, 0x36));
        assert_eq!(t.foreground, Color::Rgb(0xf8, 0xf8, 0xf2));
        assert_eq!(t.user.fg, Some(Color::Rgb(0x8b, 0xe9, 0xfd)));
        assert_eq!(t.error.fg, Some(Color::Rgb(0xff, 0x55, 0x55)));
        assert!(t.bold.add_modifier.contains(Modifier::BOLD));
        assert!(t.thinking.add_modifier.contains(Modifier::ITALIC));
        assert!(t.strikethrough.add_modifier.contains(Modifier::CROSSED_OUT));
        assert_eq!(t.diff_old.bg, Some(Color::Rgb(0x4D, 0x1F, 0x1F)));
        assert_eq!(t.diff_new.bg, Some(Color::Rgb(0x1F, 0x3D, 0x1F)));
        assert_eq!(t.input_border.fg, Some(Color::Rgb(0x62, 0x72, 0xa4)));
    }

    #[test]
    fn dracula_derivations() {
        let t = dracula();
        assert_eq!(t.mode_build, Color::Rgb(0x8b, 0xe9, 0xfd));
        assert_eq!(t.mode_plan, Color::Rgb(0xff, 0x79, 0xc6));
        assert_eq!(t.heading.fg, Some(Color::Rgb(0x8b, 0xe9, 0xfd)));
        assert!(t.heading.add_modifier.contains(Modifier::BOLD));
        assert_eq!(t.inline_code.fg, Some(Color::Rgb(0x50, 0xfa, 0x7b)));
        assert_eq!(t.code_gutter.fg, Some(Color::Rgb(0xff, 0xb8, 0x6c)));
        assert_eq!(t.list_marker.fg, Some(Color::Rgb(0x8b, 0xe9, 0xfd)));
        assert_eq!(t.bold.fg, Some(Color::Rgb(0xff, 0xb8, 0x6c)));
    }

    #[test]
    fn dracula_syntax_scopes() {
        let t = dracula();
        assert!(!t.syntax.scopes.is_empty());
        assert!(t.syntax.settings.foreground.is_some());
        assert!(t.syntax.settings.background.is_some());
    }

    const COMMENT_COLOR: SynColor = SynColor {
        r: 0x62,
        g: 0x72,
        b: 0xa4,
        a: 0xFF,
    };
    const STRING_COLOR: SynColor = SynColor {
        r: 0xf1,
        g: 0xfa,
        b: 0x8c,
        a: 0xFF,
    };
    const PINK_COLOR: SynColor = SynColor {
        r: 0xff,
        g: 0x79,
        b: 0xc6,
        a: 0xFF,
    };
    const CYAN_COLOR: SynColor = SynColor {
        r: 0x8b,
        g: 0xe9,
        b: 0xfd,
        a: 0xFF,
    };

    fn resolve_color_for_scope(
        theme: &syntect::highlighting::Theme,
        scope_str: &str,
    ) -> Option<SynColor> {
        use syntect::parsing::ScopeStack;

        let stack: ScopeStack = scope_str.parse().unwrap();
        let mut best_item: Option<&ThemeItem> = None;
        let mut best_score: f64 = 0.0;
        for item in &theme.scopes {
            if let Some(score) = item.scope.does_match(stack.as_slice())
                && score.0 > best_score
            {
                best_score = score.0;
                best_item = Some(item);
            }
        }
        best_item.and_then(|item| item.style.foreground)
    }

    #[test]
    fn scope_resolution_maps_helix_to_textmate() {
        let t = dracula();
        let cases: &[(&str, SynColor)] = &[
            (
                "source.rust comment.line.double-slash.rust punctuation.definition.comment.rust",
                COMMENT_COLOR,
            ),
            ("source.rust comment.line.double-slash.rust", COMMENT_COLOR),
            (
                "source.rust string.quoted.double.rust punctuation.definition.string.begin.rust",
                STRING_COLOR,
            ),
            ("source.rust meta.generic.rust", CYAN_COLOR),
            (
                "source.rust meta.path.rust punctuation.accessor.rust",
                PINK_COLOR,
            ),
        ];
        for (scope, expected) in cases {
            assert_eq!(
                resolve_color_for_scope(&t.syntax, scope),
                Some(*expected),
                "scope {scope} should resolve correctly"
            );
        }
    }

    #[test]
    fn missing_ui_key_defaults_to_empty_style() {
        let toml = r#"
[palette]
[ui]
"#;
        let theme = Theme::from_toml(toml).unwrap();
        assert_eq!(theme.user, Style::default());
    }

    #[test]
    fn invalid_toml_returns_error() {
        assert!(Theme::from_toml("not valid {{{{").is_err());
    }

    #[test]
    fn all_bundled_themes_parse() {
        for entry in BUNDLED_THEMES {
            let result = Theme::from_toml(entry.toml);
            assert!(
                result.is_ok(),
                "theme '{}' failed to parse: {}",
                entry.name,
                result.unwrap_err()
            );
        }
    }

    #[test]
    fn load_by_name_unknown() {
        assert!(load_by_name("nonexistent").is_err());
    }

    #[test]
    fn helix_theme_loads_without_ui_section() {
        let toml = r##"
"keyword" = { fg = "pink" }
"string" = { fg = "yellow" }
"comment" = { fg = "comment" }

[palette]
foreground = "#f8f8f2"
background = "#282a36"
pink = "#ff79c6"
yellow = "#f1fa8c"
comment = "#6272a4"
"##;
        let theme = Theme::from_toml(toml).unwrap();
        assert!(!theme.syntax.scopes.is_empty());
        assert_eq!(theme.background, Color::Rgb(0x28, 0x2a, 0x36));
    }

    #[test]
    fn ui_override_takes_precedence_over_derivation() {
        let toml = r##"
"keyword.storage.type" = { fg = "cyan" }
"keyword" = { fg = "pink" }
"function.call" = { fg = "green" }

[palette]
foreground = "#f8f8f2"
background = "#282a36"
cyan = "#8be9fd"
pink = "#ff79c6"
green = "#50fa7b"
custom = "#aabbcc"

[ui]
heading = { fg = "custom", modifiers = ["bold"] }
"##;
        let theme = Theme::from_toml(toml).unwrap();
        assert_eq!(theme.heading.fg, Some(Color::Rgb(0xaa, 0xbb, 0xcc)));
        assert_eq!(theme.mode_build, Color::Rgb(0x8b, 0xe9, 0xfd));
    }

    #[test]
    fn derivation_without_ui_section() {
        let toml = r##"
"keyword.storage.type" = { fg = "#8be9fd" }
"keyword" = { fg = "#ff79c6" }
"constant" = { fg = "#bd93f9" }
"function.call" = { fg = "#50fa7b" }
"variable.parameter" = { fg = "#ffb86c" }
"markup.bold" = { fg = "#ffb86c" }

[palette]
foreground = "#f8f8f2"
background = "#282a36"
"##;
        let theme = Theme::from_toml(toml).unwrap();
        assert_eq!(theme.mode_build, Color::Rgb(0x8b, 0xe9, 0xfd));
        assert_eq!(theme.mode_plan, Color::Rgb(0xff, 0x79, 0xc6));
        assert_eq!(theme.heading.fg, Some(Color::Rgb(0x8b, 0xe9, 0xfd)));
        assert!(theme.heading.add_modifier.contains(Modifier::BOLD));
        assert_eq!(theme.inline_code.fg, Some(Color::Rgb(0x50, 0xfa, 0x7b)));
        assert_eq!(theme.code_gutter.fg, Some(Color::Rgb(0xff, 0xb8, 0x6c)));
    }

    #[test]
    fn palette_override_takes_precedence_for_color() {
        let toml = r##"
"keyword.storage.type" = { fg = "#8be9fd" }

[palette]
foreground = "#f8f8f2"
background = "#282a36"
mode_build = "#112233"
"##;
        let theme = Theme::from_toml(toml).unwrap();
        assert_eq!(theme.mode_build, Color::Rgb(0x11, 0x22, 0x33));
    }

    #[test]
    fn style_by_name_resolves() {
        set(dracula());
        let t = current();
        assert_eq!(style_by_name("dim"), t.tool_dim);
        assert_eq!(style_by_name("tool_dim"), t.tool_dim);
        assert_eq!(style_by_name("path"), t.tool_path);
        assert_eq!(style_by_name("tool_path"), t.tool_path);
        assert_eq!(style_by_name("keyword"), t.index_keyword);
        assert_eq!(style_by_name("index_keyword"), t.index_keyword);
        assert_eq!(style_by_name("section"), t.index_section);
        assert_eq!(style_by_name("index_section"), t.index_section);
        assert_eq!(style_by_name("line_nr"), t.index_line_nr);
        assert_eq!(style_by_name("index_line_nr"), t.index_line_nr);
        assert_eq!(style_by_name("tool"), t.tool);
        assert_eq!(style_by_name("error"), t.error);
        assert_eq!(style_by_name("bold"), t.bold);
        assert_eq!(style_by_name("italic"), t.italic);
        assert_eq!(style_by_name("bold_italic"), t.bold_italic);
        assert_eq!(style_by_name("diff_old"), t.diff_old);
        assert_eq!(style_by_name("diff_new"), t.diff_new);
        assert_eq!(style_by_name("item_selected"), t.item_selected);
        assert_eq!(style_by_name("item"), t.item);
        assert_eq!(style_by_name("item_desc"), t.item_desc);
        assert_eq!(style_by_name("cursor"), t.cursor);
        assert_eq!(style_by_name("accent"), t.accent);
        assert_eq!(style_by_name("active"), t.active);
        assert_eq!(style_by_name("selected"), t.item_selected);
        assert_eq!(style_by_name("success"), t.todo_completed);
        assert_eq!(style_by_name("warning"), t.todo_in_progress);
        assert_eq!(style_by_name("match"), t.item_match);
        assert_eq!(style_by_name("match_selected"), t.item_match_selected);
    }

    #[test_case("nonexistent_style")]
    #[test_case("")]
    #[test_case("typo_keyword")]
    fn style_by_name_unknown_returns_default(name: &str) {
        assert_eq!(style_by_name(name), Style::default());
    }

    const DRACULA_BG: Color = Color::Rgb(0x28, 0x2a, 0x36);
    const TOKYONIGHT_BG: Color = Color::Rgb(0x1a, 0x1b, 0x26);

    fn tokyonight() -> Theme {
        load_by_name("tokyonight").expect("tokyonight theme must exist")
    }

    #[test]
    fn set_advances_generation() {
        let before = generation();
        set(dracula());
        assert!(generation() > before);
    }

    #[test]
    fn set_installs_theme_before_generation_observed() {
        let theme = tokyonight();
        let expected_syntax_bg = theme.syntax.settings.background;
        let before = generation();

        set(theme);

        let observed = generation();
        assert!(observed > before);
        assert_eq!(current().background, TOKYONIGHT_BG);
        assert_eq!(
            maki_highlight::theme().settings.background,
            expected_syntax_bg,
            "syntax palette must reflect the new theme once generation advances",
        );
    }

    #[test]
    fn set_generation_is_monotonic_across_switches() {
        let g0 = generation();
        set(dracula());
        let g1 = generation();
        assert!(g1 > g0);
        assert_eq!(current().background, DRACULA_BG);

        set(tokyonight());
        let g2 = generation();
        assert!(g2 > g1);
        assert_eq!(current().background, TOKYONIGHT_BG);
        assert_eq!(
            maki_highlight::theme().settings.background,
            tokyonight().syntax.settings.background,
        );
    }
}
