use maki_lua::docs_render;

const FRONTMATTER: &str = r#"+++
title = "Plugins"
weight = 23
[extra]
group = "Guides"
+++

"#;

pub fn generate() -> String {
    format!("{FRONTMATTER}{}", docs_render::guide_page())
}
