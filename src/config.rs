// config — parse env vars into a typed Config struct.
// SPDX-License-Identifier: GPL-3.0-only
//
// Public API:
//   pub struct Config { ... }
//   pub fn from_env() -> anyhow::Result<Config>      (Amendment A2: module-level fn, not impl)
//   pub fn from_env_source(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Config>
//
// Implemented in T-005 (Phase 1).

#![allow(dead_code)]

use std::time::Duration;

use anyhow::Context;
use regex::Regex;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceMode {
    NextFree,
    NewEach,
    Same,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub workspace_mode: WorkspaceMode,
    pub maximize: bool,
    pub switch_to_workspace: bool,
    /// None when SWITCH_VERIFY_TIMEOUT_MS=0 (verification disabled).
    pub switch_verify_timeout: Option<Duration>,
    /// Case-sensitive, comma-split, empty entries dropped.
    pub excluded_app_ids: Vec<String>,
    /// None when env var is absent or empty; compile error at startup on bad regex.
    pub excluded_title_regex: Option<Regex>,
    /// None when env var is absent or empty.
    pub workspace_output: Option<String>,
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Parse config from real environment variables.
/// Call sites: `crate::config::from_env()` (never `Config::from_env`).
pub fn from_env() -> anyhow::Result<Config> {
    from_env_source(|key| std::env::var(key).ok())
}

/// Parse config from an arbitrary env-var source.
/// Used by unit tests — no global env mutation required.
pub fn from_env_source(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Config> {
    let workspace_mode = parse_workspace_mode(get("WORKSPACE_MODE").as_deref())?;
    let maximize = parse_bool("MAXIMIZE", get("MAXIMIZE").as_deref())?;
    let switch_to_workspace =
        parse_bool("SWITCH_TO_WORKSPACE", get("SWITCH_TO_WORKSPACE").as_deref())?;
    let switch_verify_timeout =
        parse_verify_timeout(get("SWITCH_VERIFY_TIMEOUT_MS").as_deref())?;
    let excluded_app_ids = parse_excluded_app_ids(get("EXCLUDED_APP_IDS").as_deref());
    let excluded_title_regex =
        parse_excluded_title_regex(get("EXCLUDED_TITLE_REGEX").as_deref())?;
    let workspace_output = parse_workspace_output(get("WORKSPACE_OUTPUT").as_deref());

    Ok(Config {
        workspace_mode,
        maximize,
        switch_to_workspace,
        switch_verify_timeout,
        excluded_app_ids,
        excluded_title_regex,
        workspace_output,
    })
}

// ---------------------------------------------------------------------------
// Private parsers
// ---------------------------------------------------------------------------

fn parse_workspace_mode(val: Option<&str>) -> anyhow::Result<WorkspaceMode> {
    match val {
        None | Some("next-free") => Ok(WorkspaceMode::NextFree),
        Some("new-each") => Ok(WorkspaceMode::NewEach),
        Some("same") => Ok(WorkspaceMode::Same),
        Some(other) => anyhow::bail!(
            "WORKSPACE_MODE: unrecognized value {:?}; expected one of: next-free, new-each, same",
            other
        ),
    }
}

fn parse_bool(var_name: &str, val: Option<&str>) -> anyhow::Result<bool> {
    match val {
        None | Some("0") => Ok(false),
        Some("1") => Ok(true),
        Some(other) => anyhow::bail!(
            "{}: unrecognized value {:?}; expected \"0\" or \"1\"",
            var_name,
            other
        ),
    }
}

fn parse_verify_timeout(val: Option<&str>) -> anyhow::Result<Option<Duration>> {
    match val {
        None => Ok(Some(Duration::from_millis(250))),
        Some("0") => Ok(None),
        Some(s) => {
            let ms: u64 = s
                .parse()
                .with_context(|| format!("SWITCH_VERIFY_TIMEOUT_MS: {:?} is not a valid u64", s))?;
            Ok(Some(Duration::from_millis(ms)))
        }
    }
}

fn parse_excluded_app_ids(val: Option<&str>) -> Vec<String> {
    match val {
        None => Vec::new(),
        Some(s) => s
            .split(',')
            .map(str::trim)
            .filter(|e| !e.is_empty())
            .map(str::to_owned)
            .collect(),
    }
}

fn parse_excluded_title_regex(val: Option<&str>) -> anyhow::Result<Option<Regex>> {
    match val {
        None | Some("") => Ok(None),
        Some(s) => {
            let re = Regex::new(s)
                .with_context(|| format!("EXCLUDED_TITLE_REGEX: {:?} is not a valid regex", s))?;
            Ok(Some(re))
        }
    }
}

fn parse_workspace_output(val: Option<&str>) -> Option<String> {
    match val {
        None | Some("") => None,
        Some(s) => Some(s.to_owned()),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Helper: build a source from a map.
    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        let map: HashMap<&str, &str> = pairs.iter().copied().collect();
        move |key: &str| map.get(key).map(|v| v.to_string())
    }

    // --- defaults ---

    #[test]
    fn from_env_source_returns_defaults_when_no_env_vars_set() {
        let cfg = from_env_source(|_| None).unwrap();
        assert_eq!(cfg.workspace_mode, WorkspaceMode::NextFree);
        assert!(!cfg.maximize);
        assert!(!cfg.switch_to_workspace);
        assert_eq!(cfg.switch_verify_timeout, Some(Duration::from_millis(250)));
        assert!(cfg.excluded_app_ids.is_empty());
        assert!(cfg.excluded_title_regex.is_none());
        assert!(cfg.workspace_output.is_none());
    }

    // --- WorkspaceMode ---

    #[test]
    fn from_env_source_parses_workspace_mode_next_free() {
        let cfg = from_env_source(env(&[("WORKSPACE_MODE", "next-free")])).unwrap();
        assert_eq!(cfg.workspace_mode, WorkspaceMode::NextFree);
    }

    #[test]
    fn from_env_source_parses_workspace_mode_new_each() {
        let cfg = from_env_source(env(&[("WORKSPACE_MODE", "new-each")])).unwrap();
        assert_eq!(cfg.workspace_mode, WorkspaceMode::NewEach);
    }

    #[test]
    fn from_env_source_parses_workspace_mode_same() {
        let cfg = from_env_source(env(&[("WORKSPACE_MODE", "same")])).unwrap();
        assert_eq!(cfg.workspace_mode, WorkspaceMode::Same);
    }

    #[test]
    fn from_env_source_returns_err_when_workspace_mode_is_invalid() {
        let err = from_env_source(env(&[("WORKSPACE_MODE", "bogus")])).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("WORKSPACE_MODE"), "error should mention the var: {msg}");
        assert!(msg.contains("bogus"), "error should echo the bad value: {msg}");
    }

    // --- maximize / switch_to_workspace ---

    #[test]
    fn from_env_source_parses_maximize_true() {
        let cfg = from_env_source(env(&[("MAXIMIZE", "1")])).unwrap();
        assert!(cfg.maximize);
    }

    #[test]
    fn from_env_source_parses_maximize_false() {
        let cfg = from_env_source(env(&[("MAXIMIZE", "0")])).unwrap();
        assert!(!cfg.maximize);
    }

    #[test]
    fn from_env_source_returns_err_when_maximize_is_invalid() {
        let err = from_env_source(env(&[("MAXIMIZE", "yes")])).unwrap_err();
        assert!(err.to_string().contains("MAXIMIZE"));
    }

    #[test]
    fn from_env_source_parses_switch_to_workspace_true() {
        let cfg = from_env_source(env(&[("SWITCH_TO_WORKSPACE", "1")])).unwrap();
        assert!(cfg.switch_to_workspace);
    }

    // --- switch_verify_timeout ---

    #[test]
    fn from_env_source_parses_verify_timeout_zero_to_none() {
        let cfg = from_env_source(env(&[("SWITCH_VERIFY_TIMEOUT_MS", "0")])).unwrap();
        assert_eq!(cfg.switch_verify_timeout, None);
    }

    #[test]
    fn from_env_source_parses_verify_timeout_nonzero() {
        let cfg = from_env_source(env(&[("SWITCH_VERIFY_TIMEOUT_MS", "500")])).unwrap();
        assert_eq!(cfg.switch_verify_timeout, Some(Duration::from_millis(500)));
    }

    #[test]
    fn from_env_source_returns_err_when_verify_timeout_is_invalid() {
        let err =
            from_env_source(env(&[("SWITCH_VERIFY_TIMEOUT_MS", "not-a-number")])).unwrap_err();
        assert!(err.to_string().contains("SWITCH_VERIFY_TIMEOUT_MS"));
    }

    // --- excluded_app_ids ---

    #[test]
    fn from_env_source_parses_excluded_app_ids_comma_list() {
        let cfg =
            from_env_source(env(&[("EXCLUDED_APP_IDS", "org.kde.dolphin,foot")])).unwrap();
        assert_eq!(cfg.excluded_app_ids, vec!["org.kde.dolphin", "foot"]);
    }

    #[test]
    fn from_env_source_drops_empty_entries_from_excluded_app_ids() {
        let cfg = from_env_source(env(&[("EXCLUDED_APP_IDS", "foo,,bar,")])).unwrap();
        assert_eq!(cfg.excluded_app_ids, vec!["foo", "bar"]);
    }

    #[test]
    fn from_env_source_excluded_app_ids_trailing_comma_drops_empty() {
        let cfg =
            from_env_source(env(&[("EXCLUDED_APP_IDS", "foot,")])).unwrap();
        assert_eq!(cfg.excluded_app_ids, vec!["foot"]);
    }

    // --- excluded_title_regex ---

    #[test]
    fn from_env_source_parses_excluded_title_regex_when_set() {
        let cfg =
            from_env_source(env(&[("EXCLUDED_TITLE_REGEX", "^Picture-in-Picture")])).unwrap();
        assert!(cfg.excluded_title_regex.is_some());
    }

    #[test]
    fn from_env_source_excluded_title_regex_is_none_when_empty() {
        let cfg =
            from_env_source(env(&[("EXCLUDED_TITLE_REGEX", "")])).unwrap();
        assert!(cfg.excluded_title_regex.is_none());
    }

    #[test]
    fn from_env_source_returns_err_when_excluded_title_regex_is_invalid() {
        let err =
            from_env_source(env(&[("EXCLUDED_TITLE_REGEX", "[invalid")])).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("EXCLUDED_TITLE_REGEX"), "error should mention the var: {msg}");
    }

    // --- workspace_output ---

    #[test]
    fn from_env_source_parses_workspace_output_when_set() {
        let cfg =
            from_env_source(env(&[("WORKSPACE_OUTPUT", "HDMI-1")])).unwrap();
        assert_eq!(cfg.workspace_output, Some("HDMI-1".to_string()));
    }

    #[test]
    fn from_env_source_workspace_output_is_none_when_empty() {
        let cfg =
            from_env_source(env(&[("WORKSPACE_OUTPUT", "")])).unwrap();
        assert!(cfg.workspace_output.is_none());
    }

    #[test]
    fn from_env_source_workspace_output_is_none_when_absent() {
        let cfg = from_env_source(|_| None).unwrap();
        assert!(cfg.workspace_output.is_none());
    }
}
