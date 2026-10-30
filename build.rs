use std::env;

fn main() {
    println!("cargo::rerun-if-changed=build.rs");

    // aws-lc's assembly (from s2n-bignum) reads tables out of code memory,
    // which breaks with OpenBSD's default execute-only .text.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("openbsd")
        && env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("x86_64")
    {
        println!("cargo::rustc-link-arg-bins=-Wl,--no-execute-only");
    }
}
