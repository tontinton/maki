local helpers = require("tests.helpers")
local case = helpers.case
local idx = helpers.idx
local has = helpers.has
local lacks = helpers.lacks

case("bazel_build_fixture", function()
  local src = [==["""
# BUILD docs

Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor
incididunt ut labore et dolore magna aliqua. Ut enim ad minim veniam, quis
nostrud exercitation ullamco laboris nisi ut aliquip ex ea commodo consequat.

Duis aute irure dolor in reprehenderit in voluptate velit esse cillum dolore eu
fugiat nulla pariatur.
"""

load("@bazel_skylib//:bzl_library.bzl", "bzl_library")

package(default_visibility = ["//visibility:public"])

package_group(
    name = "tropical",
    packages = [
        "//fruits/mango",
        "//fruits/orange",
        "//fruits/papaya/...",
    ],
)

exports_files([
    "arch.bzl",
    "declare_outputs.bzl",
])

FOOBAR = 42

bzl_library(
    name = "arch",
    srcs = ["arch.bzl"],
)

bzl_library(
    name = "foo_%s" % FOOBAR,
    srcs = ["declare_outputs.bzl"],
    deps = [
        "@bazel_skylib//lib:paths",
    ],
)

cc_library(
    name = "foo_lib",
    srcs = ["foo_lib.cc"],
    hdrs = ["foo_lib.h"],
    deprecation = "Use //:new_foo_lib instead",
)

# Some test

cc_test(
    name = "foo_lib_test",
    srcs = ["foo_lib_test.cc"],
    deps = [":foo_lib"],
)]==]
  local expected = [==[build doc: [1-10]

loads:
  "@bazel_skylib//:bzl_library.bzl": bzl_library [12]

package_groups:
    tropical: [16-23]

exports_files:
    ["arch.bzl", "declare_outputs.bzl"] [25-28]

variable bindings:
    FOOBAR: 42 [30]

targets:
    arch: bzl_library [32-35]
    "foo_%s" % FOOBAR: bzl_library [37-43]
    foo_lib: cc_library, deprecated=True [45-50]
    foo_lib_test: cc_test [54-58]
]==]
  local out = idx(src, "bazel_build")
  assert(out == expected, "BUILD fixture mismatch:\n--- expected ---\n" .. expected .. "\n--- got ---\n" .. out)
end)

case("bazel_module_fixture", function()
  local src = [==[module(
    name = "monogres",
    # https://bazel.build/external/faq#module-versioning-best-practices
    version = "",
    bazel_compatibility = [">=8.4.2"],
    # https://bazel.build/external/faq#incrementing-compatibility-level
    compatibility_level = 0,
)

bazel_dep(name = "bazel_lib", version = "3.0.0")
bazel_dep(name = "bazel_skylib", version = "1.8.2")
bazel_dep(name = "download_archives", version = "0.1.0")
bazel_dep(name = "gawk", version = "5.3.2.bcr.3")
bazel_dep(name = "platforms", version = "1.0.0")

# see https://bazelbuild.slack.com/archives/CA31HN1T3/p1753021278448849
bazel_dep(name = "protobuf", version = "31.1")
bazel_dep(name = "rules_bison", version = "0.4")
bazel_dep(name = "rules_distroless", version = "0.6.1")
bazel_dep(name = "rules_flex", version = "0.4")
bazel_dep(name = "rules_foreign_cc", version = "0.15.1")
bazel_dep(name = "rules_m4", version = "0.3")
bazel_dep(name = "rules_pkg", version = "1.2.0")
bazel_dep(name = "rules_python", version = "1.6.3")
bazel_dep(name = "starlark_utils", version = "0.0.1")
bazel_dep(name = "tar.bzl", version = "0.6.0")
bazel_dep(name = "version_utils", version = "0.1.0")

bazel_dep(name = "toolchains_llvm", version = "1.5.0", dev_dependency = True)

local_path_override(
    module_name = "starlark_utils",
    path = "starlark_utils",
)

local_path_override(
    module_name = "download_archives",
    path = "/src/bazel_download_archives",
)

RFCC_COMMIT = "2eca712c15301c1f4e71ceec21e6271f373501e2"

archive_override(
    module_name = "rules_foreign_cc",
    patch_strip = 1,
    patches = [
        "//patches/rules_foreign_cc:0001-workaround-rfcc-0.15.1-ninja-version-breaks-with-gli.patch",
    ],
    sha256 = "62cf2de93583a40413cbf7ebca380a0c0d2421db7b3614eb1d8389bcba21723e",
    strip_prefix = "rules_foreign_cc-{}".format(RFCC_COMMIT),
    url = "https://github.com/bazel-contrib/rules_foreign_cc/archive/{}.tar.gz".format(RFCC_COMMIT),
)

# toolchains/
llvm = use_extension("@toolchains_llvm//toolchain/extensions:llvm.bzl", "llvm", dev_dependency = True)

# NOTE:
# We use a system LLVM toolchain, so it will use the LLVM installed in the
# Docker image. This requires llvm-14, clang-14, lld-14, and libc6-dev to be
# installed. Also, absolute_paths=True is needed because the system toolchain
# uses absolute paths for builtin headers:
# /usr/lib/llvm-14/lib/clang/14.0.6/include/
llvm.toolchain(
    name = "llvm_toolchain_system_linux_amd64",
    absolute_paths = True,
    exec_arch = "amd64",
    exec_os = "linux",
    llvm_version = "14.0.6",
)
llvm.toolchain_root(
    name = "llvm_toolchain_system_linux_amd64",
    path = "/usr/lib/llvm-14",
)
llvm.toolchain(
    name = "llvm_toolchain_system_linux_arm64",
    absolute_paths = True,
    exec_arch = "arm64",
    exec_os = "linux",
    llvm_version = "14.0.6",
)
llvm.toolchain_root(
    name = "llvm_toolchain_system_linux_arm64",
    path = "/usr/lib/llvm-14",
)
use_repo(llvm, "llvm_toolchain_system_linux_amd64", "llvm_toolchain_system_linux_arm64")

register_toolchains(
    "@llvm_toolchain_system_linux_arm64//:all",
    dev_dependency = True,
)

register_toolchains(
    "@llvm_toolchain_system_linux_amd64//:all",
    dev_dependency = True,
)

bison = use_extension(
    "@rules_bison//bison/extensions:bison_repository_ext.bzl",
    "bison_repository_ext",
)
bison.repository(
    name = "bison",
    version = "3.3.2",
)
use_repo(bison, "bison")

flex = use_extension("@rules_flex//flex/extensions:flex_repository_ext.bzl", "flex_repository_ext")
flex.repository(
    name = "flex",
    version = "2.6.4",
)
use_repo(flex, "flex")

m4 = use_extension("@rules_m4//m4/extensions:m4_repository_ext.bzl", "m4_repository_ext")
m4.repository(
    name = "m4",
    version = "1.4.18",
)
use_repo(m4, "m4")

tar_toolchains = use_extension("@tar.bzl//tar:extensions.bzl", "toolchains")
use_repo(tar_toolchains, "bsd_tar_toolchains")

register_toolchains("@bsd_tar_toolchains//:all")

zstd_toolchains = use_extension("@bazel_lib//lib:extensions.bzl", "toolchains")
use_repo(zstd_toolchains, "zstd_toolchains")

python = use_extension("@rules_python//python/extensions:python.bzl", "python")
python.toolchain(
    python_version = "3.11",
)
use_repo(python, "python_3_11")

override_repo(
  python,
  python_3_11 = "otherrepo",
)

monoext = use_extension("//monoext:monoext.bzl", "monoext")
monoext.monogres(
    name = "pg",
    pg = "//postgres:repo.json",
    extensions = "//extensions/catalog:index.json",
)
use_repo(monoext, "pg", "pg_ext", "pg_pkgs")

http_file = use_repo_rule("@bazel_tools//tools/build_defs/repo:http.bzl", "http_file")
http_file(
    name = "somefile",
    sha256 = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
    url = "https://example.com/somefile",
)

SOMEREPO = "somerepo"

local_repository = use_repo_rule("@bazel_tools//tools/build_defs/repo:local.bzl", "local_repository")
local_repository(name = SOMEREPO, path = "../%s" % SOMEREPO)

foo = use_extension("//foo:extensions.bzl", "foobar")
inject_repo(foo, SOMEREPO)

register_execution_platforms("@platforms//amd64")

include("//foo:foo.MODULE.bazel")]==]
  local expected = [==[module:
  "@monogres" [1-8]

bazel_deps:
  "@bazel_lib": "3.0.0" [10]
  "@bazel_skylib": "1.8.2" [11]
  "@download_archives": "0.1.0" [12]
  "@gawk": "5.3.2.bcr.3" [13]
  "@platforms": "1.0.0" [14]
  "@protobuf": "31.1" [17]
  "@rules_bison": "0.4" [18]
  "@rules_distroless": "0.6.1" [19]
  "@rules_flex": "0.4" [20]
  "@rules_foreign_cc": "0.15.1" [21]
  "@rules_m4": "0.3" [22]
  "@rules_pkg": "1.2.0" [23]
  "@rules_python": "1.6.3" [24]
  "@starlark_utils": "0.0.1" [25]
  "@tar.bzl": "0.6.0" [26]
  "@version_utils": "0.1.0" [27]
  "@toolchains_llvm": "1.5.0", dev=True [29]

module_extensions:
  llvm: "@toolchains_llvm//toolchain/extensions:llvm.bzl", "llvm", dev=True [55]
  bison: "@rules_bison//bison/extensions:bison_repository_ext.bzl", "bison_repository_ext" [97-100]
  flex: "@rules_flex//flex/extensions:flex_repository_ext.bzl", "flex_repository_ext" [107]
  m4: "@rules_m4//m4/extensions:m4_repository_ext.bzl", "m4_repository_ext" [114]
  tar_toolchains: "@tar.bzl//tar:extensions.bzl", "toolchains" [121]
  zstd_toolchains: "@bazel_lib//lib:extensions.bzl", "toolchains" [126]
  python: "@rules_python//python/extensions:python.bzl", "python" [129]
  monoext: "//monoext:monoext.bzl", "monoext" [140]
  foo: "//foo:extensions.bzl", "foobar" [160]

module_extensions.tags:
  llvm.toolchain: "llvm_toolchain_system_linux_amd64" [63-69]
  llvm.toolchain_root: "llvm_toolchain_system_linux_amd64" [70-73]
  llvm.toolchain: "llvm_toolchain_system_linux_arm64" [74-80]
  llvm.toolchain_root: "llvm_toolchain_system_linux_arm64" [81-84]
  bison.repository: "bison" [101-104]
  flex.repository: "flex" [108-111]
  m4.repository: "m4" [115-118]
  python.toolchain: [130-132]
  monoext.monogres: "pg" [141-145]

repos:
  llvm: "@llvm_toolchain_system_linux_amd64", "@llvm_toolchain_system_linux_arm64" [85]
  bison: "@bison" [105]
  flex: "@flex" [112]
  m4: "@m4" [119]
  tar_toolchains: "@bsd_tar_toolchains" [122]
  zstd_toolchains: "@zstd_toolchains" [127]
  python: "@python_3_11" [133]
  monoext: "@pg", "@pg_ext", "@pg_pkgs" [146]
  http_file: "@somefile" [149-153]
  local_repository: SOMEREPO [158]

register_toolchains:
  "@llvm_toolchain_system_linux_arm64//:all": dev=True [87-90]
  "@llvm_toolchain_system_linux_amd64//:all": dev=True [92-95]
  "@bsd_tar_toolchains//:all": [124]

register_execution_platforms:
  "@platforms//amd64": [163]

vars:
  RFCC_COMMIT: [41]
  SOMEREPO: [155]

includes:
  "//foo:foo.MODULE.bazel": [165]
]==]
  local out = idx(src, "bazel_module")
  assert(out == expected, "MODULE fixture mismatch:\n--- expected ---\n" .. expected .. "\n--- got ---\n" .. out)
end)

case("bazel_bzl_fixture", function()
  local src = [==["""
# Starlark module

Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor
incididunt ut labore et dolore magna aliqua. Ut enim ad minim veniam, quis
nostrud exercitation ullamco laboris nisi ut aliquip ex ea commodo consequat.

Duis aute irure dolor in reprehenderit in voluptate velit esse cillum dolore eu
fugiat nulla pariatur.
"""

load("@version_utils//spec:spec.bzl", "spec")
load("@version_utils//version:version.bzl", Version = "version")
load(
  "@foo//bar:foobar.bzl",
  "import1",
  "import2",
  Import3 = "import3",
  Import4 = "import4",
)

__MAX_ITERATIONS__ = 2 << 31 - 2

_BRACKETS = {
    "close": {
        "dict": "}",
        "list": "]",
        "tuple": ")",
    },
    "open": {
        "dict": "{",
        "list": "[",
        "tuple": "(",
    },
}

_FN_SENTINEL = "__starlark_fn__"
_FN_ARGS = "__starlark_fn_args__"

def _foo(s):
    pass

def _bar(*args, **kwargs):
    """Lorem ipsum dolor sit amet."""
    pass

def _foobar(
        arg1,
        arg2,
        arg3,
        arg4,
        arg5,
        kv1 = 42,
        kv2 = True,
        kv3 = ("a", "b", "c", "d"),
        kv4 = [1, 2, 3, 4]):
    '''Excepteur sint "occaecat cupidatat" non proident.

    Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod
    tempor incididunt ut labore et dolore magna aliqua. Ut enim ad minim
    veniam, quis nostrud exercitation ullamco laboris nisi ut aliquip ex ea
    commodo consequat.
    '''
    pass

foobar = struct(
    bar = _bar,
    lbar = lambda arg, **kwargs: _bar(arg, kv1 = True, **kwargs),
    foobar = _foobar,
    __test__ = struct(
        _foo = _foo,
    ),
)]==]
  local expected = [==[module doc: [1-10]

loads:
  "@version_utils//spec:spec.bzl": spec [12]
  "@version_utils//version:version.bzl": Version [13]
  "@foo//bar:foobar.bzl": import1, import2, Import3, Import4 [14-20]

variable bindings:
  __MAX_ITERATIONS__ = 2 << 31 - 2 [22]
  _BRACKETS = {
    "close": {
        "dict": "}",
        "li[truncated] [24-35]
  _FN_SENTINEL = "__starlark_fn__" [37]
  _FN_ARGS = "__starlark_fn_args__" [38]
  foobar = struct(
    bar = _bar,
    lbar = lambda arg, **[truncated] [66-73]

functions:
  _foo(s) [40-41]
  _bar(*args, **kwargs) [43-45]
  _foobar(arg1, arg2, arg3, arg4, arg5, kv1=42, kv2=True, kv3=("a", "b", "c", "d"), kv4=[1, 2, 3, 4]) [47-64]
]==]
  local out = idx(src, "bazel_bzl")
  assert(out == expected, "BZL fixture mismatch:\n--- expected ---\n" .. expected .. "\n--- got ---\n" .. out)
end)

case("bazel_build_sections", function()
  local src = [==[load("@rules_cc//cc:defs.bzl", "cc_library")

TIMEOUT = 300

cc_library(
    name = "mylib",
    srcs = ["mylib.cc"],
)]==]
  local out = idx(src, "bazel_build")
  has(out, {
    "loads:",
    '"@rules_cc//cc:defs.bzl": cc_library',
    "variable bindings:",
    "TIMEOUT: 300",
    "targets:",
    "mylib: cc_library",
  })
end)

case("bazel_build_deprecated_target", function()
  local src = [==[cc_library(
    name = "old_lib",
    deprecation = "Use new_lib instead",
)]==]
  local out = idx(src, "bazel_build")
  has(out, { "old_lib: cc_library, deprecated=True" })
end)

case("bazel_build_multiple_exports_files", function()
  local src = [==[exports_files(["one.txt"])
exports_files(["two.txt", "three.txt"])]==]
  local out = idx(src, "bazel_build")
  has(out, {
    "exports_files:",
    '["one.txt"] [1]',
    '["two.txt", "three.txt"] [2]',
  })
end)

case("bazel_build_exports_files_keyword_srcs", function()
  local src = [==[exports_files(srcs = ["one.txt"])]==]
  local out = idx(src, "bazel_build")
  has(out, {
    "exports_files:",
    '["one.txt"] [1]',
  })
  lacks(out, { "srcs =" })
end)

case("bazel_build_exports_files_multiline_without_trailing_comma", function()
  local src = [==[exports_files([
    "one.txt",
    "two.txt"
])]==]
  local out = idx(src, "bazel_build")
  has(out, { '["one.txt", "two.txt"] [1-4]' })
  lacks(out, { '"two.txt" ]' })
end)

case("bazel_build_binding_truncation", function()
  local src = [==[LONG_VALUE = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]==]
  local out = idx(src, "bazel_build")
  has(out, { "[truncated]" })
end)

case("bazel_build_package_group_constant_name", function()
  local src = [==[GROUP_NAME = "tropical"
package_group(name = GROUP_NAME)]==]
  local out = idx(src, "bazel_build")
  has(out, {
    "package_groups:",
    "GROUP_NAME: [2]",
  })
end)

case("bazel_module_alias_tracking", function()
  local src = [==[pip = use_extension("@rules_python//python/extensions:pip.bzl", "pip")
pip.parse(
    name = "pip_deps",
    python_version = "3.11",
    requirements_lock = "//requirements:lock.txt",
)
use_repo(pip, "pip_deps")]==]
  local out = idx(src, "bazel_module")
  has(out, {
    "module_extensions:",
    'pip: "@rules_python//python/extensions:pip.bzl", "pip"',
    "module_extensions.tags:",
    'pip.parse: "pip_deps"',
    "repos:",
    'pip: "@pip_deps"',
  })
end)

case("bazel_module_keyword_alias_assignments", function()
  local src = [==[ext = use_extension(extension_bzl_file = "//:ext.bzl", extension_name = "ext")
ext.tag(name = "repo")

repo_rule = use_repo_rule(repo_rule_bzl_file = "//:repo.bzl", repo_rule_name = "repo")
repo_rule(name = "generated")]==]
  local out = idx(src, "bazel_module")
  has(out, {
    'ext: "//:ext.bzl", "ext" [1]',
    'ext.tag: "repo" [2]',
    'repo_rule: "@generated" [5]',
  })
end)

case("bazel_module_tag_name_variable", function()
  local src = [==[PIP_NAME = "pip_deps"
pip = use_extension("@rules_python//python/extensions:pip.bzl", "pip")
pip.parse(name = PIP_NAME)]==]
  local out = idx(src, "bazel_module")
  has(out, {
    "vars:",
    "PIP_NAME: [1]",
    'pip: "@rules_python//python/extensions:pip.bzl", "pip" [2]',
    "module_extensions.tags:",
    "pip.parse: PIP_NAME [3]",
  })
  lacks(out, { 'pip.parse: "PIP_NAME"' })
end)

case("bazel_module_ignored_calls", function()
  local src = [==[bazel_dep(name = "foo", version = "1.0")

archive_override(
    module_name = "foo",
    sha256 = "abc123",
    url = "https://example.com/foo.tar.gz",
)

local_path_override(
    module_name = "bar",
    path = "/local/bar",
)]==]
  local out = idx(src, "bazel_module")
  has(out, { "bazel_deps:" })
  lacks(out, { "archive_override", "local_path_override" })
end)

case("bazel_module_dev_dependency", function()
  local src = [==[bazel_dep(name = "rules_testing", version = "0.6.0", dev_dependency = True)]==]
  local out = idx(src, "bazel_module")
  has(out, { '@rules_testing": "0.6.0", dev=True' })
end)

case("bazel_module_positional_module_and_dep_args", function()
  local src = [==[module("app")
bazel_dep("rules_cc", "0.1.1")
bazel_dep("rules_go", version = "0.53.0")
bazel_dep(name = "rules_java", version = "8.12.0")
bazel_dep("versionless")]==]
  local out = idx(src, "bazel_module")
  has(out, {
    '"@app" [1]',
    '"@rules_cc": "0.1.1" [2]',
    '"@rules_go": "0.53.0" [3]',
    '"@rules_java": "8.12.0" [4]',
    '"@versionless": "" [5]',
  })
end)

case("bazel_module_bazel_dep_repo_name", function()
  local src = [==[bazel_dep(name = "rules_foo", version = "1.0", repo_name = "foo")
bazel_dep(name = "default_name", version = "1.1", repo_name = "")
bazel_dep(name = "nodep", version = "2.0", repo_name = None)]==]
  local out = idx(src, "bazel_module")
  has(out, {
    '"@foo": "1.0" [1]',
    '"@default_name": "1.1" [2]',
    '"nodep": "2.0", repo_name=None [3]',
  })
  lacks(out, {
    '"@"',
    '"@nodep"',
    '"@rules_foo"',
  })
end)

case("bazel_module_use_repo_keyword_alias", function()
  local src = [==[ext = use_extension("//:ext.bzl", "ext")
use_repo(ext, visible_repo = "generated_repo")]==]
  local out = idx(src, "bazel_module")
  has(out, {
    'ext: "//:ext.bzl", "ext" [1]',
    'ext: "@visible_repo" [2]',
  })
  lacks(out, { '"@generated_repo"' })
end)

case("bazel_module_use_repo_dict_splat_alias", function()
  local src = [==[ext = use_extension("//:ext.bzl", "ext")
use_repo(ext, **{"foo.2": "generated_repo", "bar": "generated_bar"})]==]
  local out = idx(src, "bazel_module")
  has(out, {
    'ext: "@foo.2", "@bar" [2]',
  })
  lacks(out, {
    '**{"foo.2": "generated_repo", "bar": "generated_bar"}',
    '"@generated_repo"',
  })
end)

case("bazel_module_use_repo_proxy_must_be_positional", function()
  local src = [==[ext = use_extension("//:ext.bzl", "ext")
use_repo(extension_proxy = ext, "name")]==]
  local out = idx(src, "bazel_module")
  lacks(out, { "repos:", '"@name"' })
end)

case("bazel_module_invalid_alias_assignments", function()
  local src = [==[ext = use_extension()
ext.tag(name = "bad")
use_repo(ext, "bad")

repo_rule = use_repo_rule()
repo_rule(name = "bad")]==]
  local out = idx(src, "bazel_module")
  lacks(out, {
    "module_extensions.tags:",
    "repos:",
    'ext.tag: "bad"',
    'ext: "@bad"',
    'repo_rule: "@bad"',
  })
end)

case("bazel_module_reassigned_aliases_are_cleared", function()
  local src = [==[ext = use_extension("//:ext.bzl", "ext")
ext = "not_extension"
ext.tag(name = "bad")
use_repo(ext, "bad")

repo_rule = use_repo_rule("//:repo.bzl", "repo")
repo_rule = "not_repo_rule"
repo_rule(name = "bad")]==]
  local out = idx(src, "bazel_module")
  has(out, {
    "module_extensions:",
    'ext: "//:ext.bzl", "ext" [1]',
  })
  lacks(out, {
    "module_extensions.tags:",
    "repos:",
    'ext.tag: "bad"',
    'ext: "@bad"',
    'repo_rule: "@bad"',
  })
end)

case("bazel_module_invalid_reassignment_clears_stale_alias", function()
  local src = [==[ext = use_extension("//:ext.bzl", "ext")
ext = use_extension()
ext.tag(name = "bad")
use_repo(ext, "bad")

repo_rule = use_repo_rule("//:repo.bzl", "repo")
repo_rule = use_repo_rule()
repo_rule(name = "bad")]==]
  local out = idx(src, "bazel_module")
  lacks(out, {
    "module_extensions.tags:",
    "repos:",
    'ext.tag: "bad"',
    'ext: "@bad"',
    'repo_rule: "@bad"',
  })
end)

case("bazel_module_multiple_target_args", function()
  local src = [==[register_toolchains(
    "@a//:all",
    "@b//:all",
    dev_dependency = True,
)
register_execution_platforms("@p1", "@p2")
include("//a:MODULE.bazel", "//b:MODULE.bazel")]==]
  local out = idx(src, "bazel_module")
  has(out, {
    "register_toolchains:",
    '"@a//:all": dev=True [1-5]',
    '"@b//:all": dev=True [1-5]',
    "register_execution_platforms:",
    '"@p1": [6]',
    '"@p2": [6]',
    "includes:",
    '"//a:MODULE.bazel": [7]',
    '"//b:MODULE.bazel": [7]',
  })
end)

case("bazel_module_register_execution_platforms_dev_dependency", function()
  local src = [==[register_execution_platforms(
    "@p1",
    "@p2",
    dev_dependency = True,
)
register_execution_platforms("@p3")]==]
  local out = idx(src, "bazel_module")
  has(out, {
    "register_execution_platforms:",
    '"@p1": dev=True [1-5]',
    '"@p2": dev=True [1-5]',
    '"@p3": [6]',
  })
end)

case("bazel_module_include_never_renders_dev", function()
  local src = [==[include("//a:MODULE.bazel", dev_dependency = True)]==]
  local out = idx(src, "bazel_module")
  has(out, { '"//a:MODULE.bazel": [1]' })
  lacks(out, { "dev=True" })
end)

-- analyze.lua's value records expose `text` as the unquoted contents of a
-- string literal. Callers that render targets must use util.render_value
-- to re-add quotes; otherwise "@a//:all" leaks as @a//:all.
case("bazel_module_string_positional_args_keep_quotes", function()
  local src = [==[ext = use_extension("//:ext.bzl", "ext")
register_toolchains("@a//:all")
register_execution_platforms("@p1")
include("//a:MODULE.bazel")
use_repo(ext, "alias")]==]
  local out = idx(src, "bazel_module")
  has(out, {
    '"@a//:all": [2]',
    '"@p1": [3]',
    '"//a:MODULE.bazel": [4]',
    'ext: "@alias" [5]',
  })
  lacks(out, {
    "@a//:all: ",
    "@p1: ",
    "//a:MODULE.bazel: ",
  })
end)

case("bazel_module_repo_rule_dev_dependency", function()
  local src = [==[http_file = use_repo_rule("@bazel_tools//tools/build_defs/repo:http.bzl", "http_file")
http_file(name = "fixture", dev_dependency = True)]==]
  local out = idx(src, "bazel_module")
  has(out, {
    'http_file: "@fixture", dev=True [2]',
  })
end)

case("bazel_module_uppercase_alias_no_var_dup", function()
  local src = [==[EXT = use_extension("//:ext.bzl", "ext")
RULE = use_repo_rule("//:repo.bzl", "rule")]==]
  local out = idx(src, "bazel_module")
  has(out, {
    "module_extensions:",
    'EXT: "//:ext.bzl", "ext"',
  })
  lacks(out, {
    "vars:",
    "EXT: [",
    "RULE: [",
  })
end)

case("bazel_module_private_and_numbered_vars", function()
  local src = [==[_PRIVATE_VERSION = "1.0"
VERSION2 = "2.0"]==]
  local out = idx(src, "bazel_module")
  has(out, {
    "vars:",
    "_PRIVATE_VERSION: [1]",
    "VERSION2: [2]",
  })
end)

case("bazel_module_module_doc", function()
  local src = [==["""Bzlmod manifest for foo."""

module(name = "foo")]==]
  local out = idx(src, "bazel_module")
  has(out, { "module doc: [1]" })
end)

case("bazel_bzl_module_doc", function()
  local src = [==["""Module-level docstring."""

def helper():
    pass]==]
  local out = idx(src, "bazel_bzl")
  has(out, { "module doc:" })
end)

case("bazel_bzl_single_quote_module_doc", function()
  local src = [==['''Module-level docstring.'''

def helper():
    pass]==]
  local out = idx(src, "bazel_bzl")
  has(out, { "module doc: [1]" })
end)

case("bazel_bzl_raw_string_load", function()
  local src = [==[load(r"//foo:defs.bzl", "rule")]==]
  local out = idx(src, "bazel_bzl")
  has(out, {
    "loads:",
    '"//foo:defs.bzl": rule [1]',
  })
end)

case("bazel_bzl_function_params", function()
  local src = [==[def my_rule(name, srcs = [], deps = [], visibility = None):
    pass]==]
  local out = idx(src, "bazel_bzl")
  has(out, { "my_rule(name, srcs=[], deps=[], visibility=None)" })
end)

case("bazel_bzl_function_params_keep_string_default_equals", function()
  local src = [==[def my_rule(
    message = "a = b",
    raw = r"c = d",
    single = 'e = f',
    same = x == y,
    min_ok = x >= y,
):
    pass]==]
  local out = idx(src, "bazel_bzl")
  has(out, { [[my_rule(message="a = b", raw=r"c = d", single='e = f', same=x == y, min_ok=x >= y)]] })
end)

case("bazel_bzl_binding_truncation", function()
  local src = [==[LONG_VALUE = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]==]
  local out = idx(src, "bazel_bzl")
  has(out, { "[truncated]" })
end)

case("bazel_module_inline_comment_in_register_toolchains", function()
  local src = [==[register_toolchains(
    "@a//:all",
    # this is a comment
    "@b//:all",
)]==]
  local out = idx(src, "bazel_module")
  has(out, {
    '"@a//:all":',
    '"@b//:all":',
  })
  lacks(out, { "this is a comment" })
end)

case("bazel_module_attr_call_with_line_continuation", function()
  local src = [==[pip = use_extension("//:ext.bzl", "ext")
pip.\
parse(name = "foo")]==]
  local out = idx(src, "bazel_module")
  has(out, {
    "module_extensions.tags:",
    'pip.parse: "foo"',
  })
end)

case("bazel_module_inline_comment_in_use_repo", function()
  local src = [==[ext = use_extension("//:ext.bzl", "ext")
use_repo(
    ext,
    # comment between args
    "first_repo",
    "second_repo",
)]==]
  local out = idx(src, "bazel_module")
  has(out, {
    'ext: "@first_repo", "@second_repo"',
  })
  lacks(out, { "comment between args" })
end)

case("bazel_build_inline_comment_in_load", function()
  local src = [==[load(
    # the bzl file
    "@foo//bar.bzl",
    "rule",
)]==]
  local out = idx(src, "bazel_build")
  has(out, {
    '"@foo//bar.bzl": rule',
  })
  lacks(out, { "the bzl file" })
end)

case("bazel_module_inline_comment_in_bazel_dep", function()
  local src = [==[bazel_dep(
    # use latest version
    name = "protobuf",
    version = "31.1",
)]==]
  local out = idx(src, "bazel_module")
  has(out, { '"@protobuf": "31.1"' })
  lacks(out, { "use latest version" })
end)
