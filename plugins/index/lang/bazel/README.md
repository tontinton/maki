# Bazel indexer

Bazel files are written in [Starlark], but they come in three distinct kinds
with different semantic models. Even though we parse them with the same
`starlark` tree-sitter grammar, they need their own extractors:

- [`MODULE.bazel`]: a [Bzlmod] manifest.
- [`BUILD`] / `BUILD.bazel`: package files that declare build targets.
- [`.bzl`]: Starlark module files.

## Architecture

The plugin is layered. Each extractor is file-kind specific and depends only on
the internal API: `analyze.lua` (records) and `render.lua` (output), plus the
layer-agnostic `util.lua`. Tree-sitter primitives live in `ast.lua`, private to
`analyze.lua`, so the AST shape never leaks into the rest of the plugin.

```
                     +----------------------------------------------+
                     |        Extractors (file-kind specific)       |
                     |  +-----------+  +------------+  +---------+  |
                     |  | build.lua |  | module.lua |  | bzl.lua |  |
                     |  +-----------+  +------------+  +---------+  |
                     +----------------------------------------------+
 +----------+                               |
 | util.lua | ← reachable from any layer    |
 +----------+                               |
                        +---------------------------------------+
                        |    Internal API (records + output)    |
                        |  +-------------+     +------------+   |
                        |  | analyze.lua |     | render.lua |   |
                        |  +-------------+     +------------+   |
                        |       |                               |
                        |  +---------+                          |
                        |  | ast.lua | ← tree-sitter primitives |
                        |  +---------+                          |
                        +---------------------------------------+
```

## Where to make changes

- **Recognise a new `MODULE.bazel` builtin call**: add it to `call_handlers`
  in `module.lua`.
- **Recognise a new BUILD rule shape**: add it to `call_handlers` in
  `build.lua`.
- **Skip a noisy call kind**: add it to `IGNORED_CALLS` in the relevant
  extractor.
- **Add a new top-level statement kind**: add a classifier to
  `classify_steps` in `analyze.lua`, plus a new record builder.
- **Change how a section renders**: edit the relevant `render_*` function in
  the extractor (or in `render.lua` if the change is shared).
- **Add a tree-sitter helper**: put it in `ast.lua` and surface it through
  an `analyze.lua` record field or method. Do not require `ast.lua` from an
  extractor.

[Starlark]: https://github.com/bazelbuild/starlark/blob/master/spec.md
[Bzlmod]: https://bazel.build/external/overview
[`MODULE.bazel`]: https://bazel.build/external/module
[`BUILD`]: https://bazel.build/concepts/build-files
[`.bzl`]: https://bazel.build/extending/concepts
