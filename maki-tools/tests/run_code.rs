//! End-to-end code execution through the default child workload.

#![cfg(all(feature = "sandbox", target_os = "linux"))]

use std::path::PathBuf;
use std::sync::Arc;

use maki_sandbox::Sandbox;
use maki_sandbox::namespace::NamespaceConfig;

const SKIP_NO_NS: &str = "sandbox tests require user namespace support (CLONE_NEWUSER)";

fn try_sandbox() -> Option<Arc<Sandbox>> {
    let dir = tempfile::TempDir::new().unwrap();
    let config = NamespaceConfig::new(
        vec![],
        vec![],
        PathBuf::from(dir.path()),
        "test".into(),
        vec![],
        vec![],
        vec![],
        vec![],
        vec![],
    );
    Sandbox::new(config).ok()
}

#[test]
fn sandbox_run_code_prints() {
    maki_tools::install_child_workload();
    let Some(sandbox) = try_sandbox() else {
        eprintln!("{SKIP_NO_NS}");
        return;
    };
    let result = match sandbox.run_code(
        "print('sandbox-ok')".into(),
        30,
        0,
        "{}".into(),
        |name, _, _| Err(format!("no tools in test: {name}")),
    ) {
        Ok(r) => r,
        Err(_) => {
            eprintln!("{SKIP_NO_NS}");
            return;
        }
    };
    assert!(
        result.error.is_none(),
        "run should not error: {:?}",
        result.error
    );
    assert!(
        result.stdout.contains("sandbox-ok"),
        "stdout should contain the printed line: {:?}",
        result.stdout
    );
}
