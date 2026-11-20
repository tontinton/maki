use std::env;
use std::process::Command;

/// Set on the targets where reqwest's platform verifier is webpki over the
/// system store, which `crate::tls` extends.
const WEBPKI_STORE_CFG: &str = "webpki_store";

fn main() {
    let hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8(o.stdout).ok()
            } else {
                None
            }
        })
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=GIT_SHORT_HASH={hash}");

    println!("cargo::rustc-check-cfg=cfg({WEBPKI_STORE_CFG})");
    let target_is = |key: &str, value: &str| env::var(key).is_ok_and(|v| v == value);
    if env::var_os("CARGO_CFG_UNIX").is_some()
        && !target_is("CARGO_CFG_TARGET_OS", "android")
        && !target_is("CARGO_CFG_TARGET_VENDOR", "apple")
    {
        println!("cargo::rustc-cfg={WEBPKI_STORE_CFG}");
    }
}
