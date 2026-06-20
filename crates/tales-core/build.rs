use std::env;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=TALES_GIT_SHA");
    println!("cargo:rerun-if-env-changed=TALES_GIT_DIRTY");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");

    set_env("TALES_BUILD_TARGET", env::var("TARGET").ok());
    set_env("TALES_BUILD_PROFILE", env::var("PROFILE").ok());

    let git_sha = env::var("TALES_GIT_SHA")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| git_output(&["rev-parse", "--short=12", "HEAD"]));
    set_env("TALES_GIT_SHA", git_sha);

    let git_dirty = env::var("TALES_GIT_DIRTY")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            git_output(&["status", "--porcelain"])
                .map(|status| (!status.trim().is_empty()).to_string())
        });
    set_env("TALES_GIT_DIRTY", git_dirty);
}

fn git_output(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn set_env(key: &str, value: Option<String>) {
    if let Some(value) = value {
        println!("cargo:rustc-env={key}={value}");
    }
}
