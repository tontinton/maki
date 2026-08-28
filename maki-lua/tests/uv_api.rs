use std::sync::Arc;

use maki_agent::tools::ToolRegistry;
use maki_lua::{Permission, PluginHost, PluginPermissions};

const PERMISSION_DENIED_SUBSTR: &str = "permission denied";
const CWD: &str = "maki.uv.cwd()";
const HOMEDIR: &str = "maki.uv.os_homedir()";
const GETENV: &str = r#"maki.uv.os_getenv("HOME")"#;

fn setup() -> PluginHost {
    let reg = Arc::new(ToolRegistry::new());
    PluginHost::new(reg).unwrap()
}

#[test]
fn os_getenv_returns_nil_for_missing_var() {
    let host = setup();
    host.load_source(
        "getenv_missing",
        r#"
        local val = maki.uv.os_getenv("MAKI_TEST_VAR_DOES_NOT_EXIST_12345")
        assert(val == nil, "unset var should return nil, got: " .. tostring(val))
        "#,
    )
    .unwrap();
}

fn load_with(permission: Permission, chunk: &str) -> Result<(), maki_lua::PluginError> {
    let mut perms = PluginPermissions::denied();
    perms.set(permission, true);
    setup().load_source_with_permissions("uv_perm", chunk, perms)
}

#[test_case::test_case(Permission::FsRead, CWD ; "fs_read_cwd")]
#[test_case::test_case(Permission::FsRead, HOMEDIR ; "fs_read_homedir")]
#[test_case::test_case(Permission::Env, GETENV ; "env_getenv")]
fn the_permission_the_call_needs_is_enough(permission: Permission, call: &str) {
    let chunk = format!(
        r#"local value = {call}
        assert(type(value) == "string", "expected a string, got: " .. tostring(value))"#
    );
    load_with(permission, &chunk).unwrap();
}

/// Asking where a file lives must not cost a plugin the key to every secret in
/// the environment, so the two guards do not stand in for each other.
#[test_case::test_case(Permission::Env, CWD, Permission::FsRead ; "env_alone_misses_cwd")]
#[test_case::test_case(Permission::Env, HOMEDIR, Permission::FsRead ; "env_alone_misses_homedir")]
#[test_case::test_case(Permission::FsRead, GETENV, Permission::Env ; "fs_read_alone_misses_getenv")]
fn a_neighbouring_permission_does_not_carry_over(held: Permission, call: &str, needed: Permission) {
    let err = load_with(held, call)
        .expect_err("the guarded call must fail")
        .to_string();
    assert!(err.contains(PERMISSION_DENIED_SUBSTR), "got: {err}");
    assert!(err.contains(&format!("'{needed}'")), "got: {err}");
}
