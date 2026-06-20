use std::sync::OnceLock;

use serde_json::{json, Value};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

static LONG_VERSION: OnceLock<String> = OnceLock::new();

pub fn version() -> &'static str {
    VERSION
}

pub fn long_version() -> &'static str {
    LONG_VERSION
        .get_or_init(|| {
            let mut version = VERSION.to_string();
            if let Some(sha) = git_sha() {
                version.push_str(" (git ");
                version.push_str(sha);
                if git_dirty() == Some(true) {
                    version.push_str("-dirty");
                }
                version.push(')');
            }
            version
        })
        .as_str()
}

pub fn git_sha() -> Option<&'static str> {
    non_empty(option_env!("TALES_GIT_SHA"))
}

pub fn git_dirty() -> Option<bool> {
    match non_empty(option_env!("TALES_GIT_DIRTY")) {
        Some("true") => Some(true),
        Some("false") => Some(false),
        _ => None,
    }
}

pub fn build_target() -> Option<&'static str> {
    non_empty(option_env!("TALES_BUILD_TARGET"))
}

pub fn build_profile() -> Option<&'static str> {
    non_empty(option_env!("TALES_BUILD_PROFILE"))
}

pub fn json() -> Value {
    json!({
        "version": VERSION,
        "git_sha": git_sha(),
        "git_dirty": git_dirty(),
        "target": build_target(),
        "profile": build_profile(),
    })
}

fn non_empty(value: Option<&'static str>) -> Option<&'static str> {
    value.filter(|value| !value.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_includes_package_version() {
        assert_eq!(json()["version"], VERSION);
    }
}
