use std::sync::Arc;

use maki_agent::tools::ToolRegistry;
use maki_config::LuaPluginsConfig;
use maki_lua::PluginHost;

fn config() -> LuaPluginsConfig {
    LuaPluginsConfig {
        enabled: true,
        builtins: vec![],
        user_dirs: vec![],
    }
}

fn setup() -> (Arc<ToolRegistry>, PluginHost) {
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(&config(), Arc::clone(&reg)).unwrap();
    (reg, host)
}

fn run_lua(host: &PluginHost, name: &str, src: &str) {
    host.load_source(name, src)
        .unwrap_or_else(|e| panic!("lua error in {name}: {e}"));
}

const RUST_SOURCE: &str = r#"
use std::sync::Arc;

const MAX_SIZE: usize = 1024;

pub struct Config {
    pub name: String,
    pub value: i32,
}

impl Config {
    pub fn new(name: String) -> Self {
        Self { name, value: 0 }
    }
}

fn helper(x: &str) -> bool {
    x.len() > MAX_SIZE
}
"#;

#[test]
fn parse_and_walk_nodes() {
    let (_reg, host) = setup();
    let src = format!(
        r#"
local source = [==[{RUST_SOURCE}]==]
local parser = maki.treesitter.get_parser(source, "rust")
local trees = parser:parse()
local tree = trees[1]
local root = tree:root()

assert(root:type() == "source_file", "expected source_file, got " .. root:type())
assert(root:child_count() > 0)
assert(root:named_child_count() > 0)

local sp_row, sp_col, sp_byte = root:start()
assert(sp_row == 0, "start row should be 0")

local ep_row, ep_col, ep_byte = root:end_()
assert(ep_row > 0, "end row should be > 0")

assert(root:named() == true)
assert(root:has_error() == false)
assert(root:sexpr():sub(1, 12) == "(source_file")

local children = root:named_children()
assert(#children > 0, "should have named children")

local count = 0
local iter = root:iter_children()
while true do
    local child, field = iter()
    if child == nil then break end
    count = count + 1
end
assert(count == root:child_count(), "iter_children count mismatch")

local tree2 = root:tree()
local root2 = tree2:root()
assert(root:equal(root2), "root from tree() should equal original root")

local tree_copy = tree:copy()
local root_copy = tree_copy:root()
assert(root_copy:type() == "source_file")
"#
    );
    run_lua(&host, "parse_walk", &src);
}

#[test]
fn query_iter_captures() {
    let (_reg, host) = setup();
    let src = format!(
        r#"
local source = [==[{RUST_SOURCE}]==]
local parser = maki.treesitter.get_parser(source, "rust")
local root = parser:parse()[1]:root()

local query = maki.treesitter.query.parse("rust", [[
  (function_item name: (identifier) @fn_name)
  (struct_item name: (type_identifier) @struct_name)
]])

assert(#query.captures == 2)
assert(query.captures[1] == "fn_name")
assert(query.captures[2] == "struct_name")

local names = {{}}
for id, node, metadata in query:iter_captures(root, source) do
    local name = query.captures[id]
    local text = maki.treesitter.get_node_text(node, source)
    names[name .. ":" .. text] = true
end

assert(names["fn_name:helper"], "did not find fn_name:helper")
assert(names["struct_name:Config"], "did not find struct_name:Config")
"#
    );
    run_lua(&host, "query_captures", &src);
}

#[test]
fn query_iter_matches() {
    let (_reg, host) = setup();
    let src = format!(
        r#"
local source = [==[{RUST_SOURCE}]==]
local parser = maki.treesitter.get_parser(source, "rust")
local root = parser:parse()[1]:root()

local query = maki.treesitter.query.parse("rust", [[
  (function_item
    name: (identifier) @fn_name
    parameters: (parameters) @fn_params)
]])

local match_count = 0
for pattern_idx, captures, metadata in query:iter_matches(root, source) do
    match_count = match_count + 1
    assert(type(pattern_idx) == "number")
    assert(type(captures) == "table")
end
assert(match_count > 0, "should find at least one match")
"#
    );
    run_lua(&host, "iter_matches", &src);
}

#[test]
fn node_navigation() {
    let (_reg, host) = setup();
    let src = format!(
        r#"
local source = [==[{RUST_SOURCE}]==]
local parser = maki.treesitter.get_parser(source, "rust")
local root = parser:parse()[1]:root()

local first = root:child(0)
assert(first ~= nil)
assert(first:parent():equal(root), "parent should be root")

local second = first:next_sibling()
if second then
    assert(second:prev_sibling() ~= nil)
end

local first_named = root:named_child(0)
assert(first_named ~= nil)
local next_named = first_named:next_named_sibling()
if next_named then
    assert(next_named:named() == true)
end

local desc = root:descendant_for_range(0, 0, 0, 3)
assert(desc ~= nil)
local named_desc = root:named_descendant_for_range(0, 0, 0, 3)
assert(named_desc ~= nil)
assert(named_desc:named() == true)

local query = maki.treesitter.query.parse("rust", "(function_item) @fn")
for _, node in query:iter_captures(root, source) do
    local name_nodes = node:field("name")
    assert(#name_nodes > 0, "function should have a name field")
    break
end

local id_query = maki.treesitter.query.parse("rust", "(identifier) @id")
for _, node in id_query:iter_captures(root, source) do
    if node:parent() and node:parent():parent() then
        local child = root:child_with_descendant(node)
        assert(child ~= nil)
        assert(child:parent():equal(root), "result should be direct child of root")
        break
    end
end
"#
    );
    run_lua(&host, "node_navigation", &src);
}

#[test]
fn module_functions() {
    let (_reg, host) = setup();
    let src = format!(
        r#"
local source = [==[{RUST_SOURCE}]==]
local parser = maki.treesitter.get_parser(source, "rust")
local root = parser:parse()[1]:root()

local sr, sc, er, ec = root:range()
assert(sr == 0 and sc == 0)
assert(er > 0)

local sr2, sc2, sb, er2, ec2, eb = root:range(true)
assert(sb == 0 and eb > 0)
assert(root:byte_length() == eb - sb)

local gnr_sr, gnr_sc, gnr_er, gnr_ec = maki.treesitter.get_node_range(root)
assert(gnr_sr == sr and gnr_sc == sc)

local range6 = maki.treesitter.get_range(root)
assert(range6[1] == sr and range6[3] == sb)

assert(maki.treesitter.is_in_node_range(root, 1, 0) == true)

assert(maki.treesitter.node_contains(root, {{1, 0, 2, 0}}) == true)

local child = root:named_child(0)
assert(child ~= nil)
assert(maki.treesitter.is_ancestor(root, child) == true)
assert(maki.treesitter.is_ancestor(child, root) == false)

local text = maki.treesitter.get_node_text(child, source)
assert(type(text) == "string" and #text > 0)
"#
    );
    run_lua(&host, "module_functions", &src);
}

#[test]
fn language_tree_methods() {
    let (_reg, host) = setup();
    let src = format!(
        r#"
local source = [==[{RUST_SOURCE}]==]
local parser = maki.treesitter.get_parser(source, "rust")

assert(parser:lang() == "rust")
assert(parser:source() == source)
assert(parser:is_valid() == true)

local children = parser:children()
assert(next(children) == nil, "children should be empty (no injections)")

assert(parser:contains({{0, 0, 1, 0}}) == true)

local regions = parser:included_regions()
assert(regions[1] ~= nil)

local trees = parser:parse()
assert(trees[1] ~= nil)

local trees_after = parser:trees()
assert(trees_after[1] ~= nil)

local tree_count = 0
parser:for_each_tree(function(tree, ltree)
    tree_count = tree_count + 1
    assert(tree:root():type() == "source_file")
end)
assert(tree_count == 1)

parser:destroy()
"#
    );
    run_lua(&host, "language_tree", &src);
}

#[test]
fn language_module() {
    let (_reg, host) = setup();
    run_lua(
        &host,
        "language_mod",
        r#"
maki.treesitter.language.add("rust")

maki.treesitter.language.register("rust", "rs")
assert(maki.treesitter.language.get_lang("rs") == "rust")

maki.treesitter.language.register("python", {"py", "pyi"})
assert(maki.treesitter.language.get_lang("py") == "python")
assert(maki.treesitter.language.get_lang("pyi") == "python")

local fts = maki.treesitter.language.get_filetypes("python")
assert(#fts == 2, "expected 2 filetypes for python")

assert(maki.treesitter.language.get_lang("unknown_ext_xyz") == nil)

local info = maki.treesitter.language.inspect("rust")
assert(type(info.abi_version) == "number")
assert(#info.node_types > 0)
assert(#info.fields > 0)
"#,
    );
}

#[test]
fn python_parse_and_query() {
    let (_reg, host) = setup();
    run_lua(
        &host,
        "python_parse",
        r#"
local source = [[
def greet(name):
    return f"Hello, {name}!"

class Greeter:
    def __init__(self, prefix):
        self.prefix = prefix
]]

local parser = maki.treesitter.get_parser(source, "python")
local root = parser:parse()[1]:root()
assert(root:type() == "module")

local query = maki.treesitter.query.parse("python", [[
  (function_definition name: (identifier) @fn_name)
  (class_definition name: (identifier) @class_name)
]])

local names = {}
for id, node in query:iter_captures(root, source) do
    local name = query.captures[id]
    local text = maki.treesitter.get_node_text(node, source)
    names[name .. ":" .. text] = true
end

assert(names["fn_name:greet"], "should find greet function")
assert(names["class_name:Greeter"], "should find Greeter class")
"#,
    );
}

#[test]
fn typescript_parse_and_query() {
    let (_reg, host) = setup();
    run_lua(
        &host,
        "typescript_parse",
        r#"
local source = [[
interface Config {
    name: string;
    value: number;
}

function processConfig(config: Config): string {
    return config.name;
}

export const DEFAULT: Config = { name: "default", value: 0 };
]]

local parser = maki.treesitter.get_parser(source, "typescript")
local root = parser:parse()[1]:root()
assert(root:type() == "program")

local query = maki.treesitter.query.parse("typescript", [[
  (interface_declaration name: (type_identifier) @iface_name)
  (function_declaration name: (identifier) @fn_name)
]])

local found = {}
for id, node in query:iter_captures(root, source) do
    local text = maki.treesitter.get_node_text(node, source)
    found[query.captures[id] .. ":" .. text] = true
end

assert(found["iface_name:Config"], "should find Config interface")
assert(found["fn_name:processConfig"], "should find processConfig function")
"#,
    );
}
