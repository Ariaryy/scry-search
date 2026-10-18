use std::process::Command;

fn main() {
    // Re-evaluate for working-tree builds as well as commit changes.
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/index");
    println!("cargo:rerun-if-changed=../scry-client/src");
    println!("cargo:rerun-if-changed=../scry-core/src");
    println!("cargo:rerun-if-changed=../scry-ipc/src");
    println!("cargo:rerun-if-changed=../Cargo.toml");
    println!("cargo:rerun-if-changed=src");
    for (name, program, args) in [
        ("SCRY_BENCH_COMMIT", "git", vec!["rev-parse", "HEAD"]),
        ("SCRY_BENCH_RUSTC", "rustc", vec!["--version"]),
    ] {
        let value = Command::new(program)
            .args(args)
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
            .unwrap_or_else(|| "unknown".into());
        println!("cargo:rustc-env={name}={value}");
    }
    for name in ["PROFILE", "OPT_LEVEL", "TARGET", "CARGO_ENCODED_RUSTFLAGS"] {
        let value = std::env::var(name).unwrap_or_default();
        println!("cargo:rustc-env=SCRY_BENCH_{name}={value}");
    }
    let dirty = Command::new("git")
        .args(["diff", "--quiet", "HEAD", "--", "../"])
        .status()
        .map(|status| !status.success())
        .unwrap_or(true);
    println!("cargo:rustc-env=SCRY_BENCH_TRACKED_DIRTY={dirty}");
}
