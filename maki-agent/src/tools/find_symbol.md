Find all references to a symbol across a project. Scope-aware: locals search enclosing function, private items search the file.

- Supports Rust, C, and C++.
- No type info: may include false positives for common names.
- Returns file:line:col, kind (def/call/read/write/type_ref/field_ref), and context.