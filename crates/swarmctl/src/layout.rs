//! Session layouts: a set of tabs, each with one or more panes, each a
//! session on this machine or a host, in a folder, maybe running a command.
//! `swarmctl up <layout>` starts them; the Swarm app opens them as tabs with
//! their panes split. Layouts live in `~/.config/swarm/layouts/<name>.toml`:
//!
//! ```toml
//! [[tab]]
//! name = "server"
//! host = "devbox"          # optional: this machine without it
//! cwd = "~/src/web"
//! run = "npm run dev"      # optional; also run again after a reboot
//!
//! [[tab.pane]]             # more panes in the same tab
//! name = "tests"
//! cwd = "~/src/web"
//! run = "npm test -- --watch"
//! split = "right"          # or "down"
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// UI metadata keys the app reads to put a layout's sessions together.
pub const META_LAYOUT: &str = "layout";
pub const META_LAYOUT_TAB: &str = "layout.tab";
pub const META_LAYOUT_PANE: &str = "layout.pane";
pub const META_LAYOUT_SPLIT: &str = "layout.split";
/// The `swarmctl up` run that started the session: a layout's tabs from two
/// runs are different tabs.
pub const META_LAYOUT_RUN: &str = "layout.run";
/// Set when `swarmctl up` makes the session: the app opens these as tabs
/// right away (all sessions with the same value together), then clears it.
pub const META_LAYOUT_OPEN: &str = "layout.open";

const MAX_TABS: usize = 32;
const MAX_PANES: usize = 8;

#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct Layout {
    #[serde(default, rename = "tab")]
    pub tabs: Vec<Tab>,
}

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct Tab {
    pub name: Option<String>,
    pub host: Option<String>,
    pub cwd: Option<String>,
    pub run: Option<String>,
    #[serde(default, rename = "pane")]
    pub panes: Vec<Pane>,
}

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct Pane {
    pub name: Option<String>,
    /// The tab's host if not given (a tab's panes are on one host).
    pub cwd: Option<String>,
    pub run: Option<String>,
    pub split: Option<String>,
}

/// One session to start: where, and how the app places it.
#[derive(Debug, Clone, PartialEq)]
pub struct Planned {
    pub host: Option<String>,
    pub name: Option<String>,
    pub cwd: Option<String>,
    pub run: Option<String>,
    pub tab: usize,
    pub pane: usize,
    pub split: &'static str,
}

pub fn dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config/swarm/layouts"))
}

/// `name` in the layouts folder, or a path to a `.toml` file.
pub fn path_of(name: &str) -> Result<PathBuf> {
    if name.contains('/') || name.ends_with(".toml") {
        return Ok(PathBuf::from(name));
    }
    if name.is_empty() || name.starts_with('.') {
        bail!("{name:?} isn't a layout name");
    }
    Ok(dir()?.join(format!("{name}.toml")))
}

/// The layouts in the layouts folder, by name.
pub fn names() -> Vec<String> {
    let mut names: Vec<String> = dir()
        .ok()
        .and_then(|d| std::fs::read_dir(d).ok())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.strip_suffix(".toml").map(str::to_string))
        .collect();
    names.sort();
    names
}

pub fn parse(text: &str) -> Result<Layout> {
    toml::from_str(text).context("the layout isn't valid")
}

/// The sessions `layout` describes, checked. `host`: put every tab on this
/// host instead of the ones the layout names.
pub fn plan(layout: &Layout, host: Option<&str>) -> Result<Vec<Planned>> {
    if layout.tabs.is_empty() {
        bail!("the layout has no [[tab]]");
    }
    if layout.tabs.len() > MAX_TABS {
        bail!("a layout has at most {MAX_TABS} tabs");
    }
    let text = |what: &str, v: &Option<String>| -> Result<Option<String>> {
        match v {
            Some(s) if s.chars().any(swarm_proto::is_unsafe_char) => bail!("{what} {s:?} has control characters"),
            Some(s) if s.trim().is_empty() => Ok(None),
            Some(s) => Ok(Some(s.clone())),
            None => Ok(None),
        }
    };
    let run = |v: &Option<String>| -> Result<Option<String>> {
        match v {
            Some(cmd) => {
                swarm_proto::check_restart_command(cmd).map_err(|why| anyhow::anyhow!("run {cmd:?}: {why}"))?;
                Ok(Some(cmd.clone()))
            }
            None => Ok(None),
        }
    };
    let mut out = Vec::new();
    for (t, tab) in layout.tabs.iter().enumerate() {
        if tab.panes.len() + 1 > MAX_PANES {
            bail!("a tab has at most {MAX_PANES} panes");
        }
        let host = match host.map(str::to_string).or_else(|| tab.host.clone()) {
            Some(h) => {
                swarm_client::check_ssh_host(&h)?;
                Some(h)
            }
            None => None,
        };
        out.push(Planned {
            host: host.clone(),
            name: text("name", &tab.name)?,
            cwd: text("cwd", &tab.cwd)?,
            run: run(&tab.run)?,
            tab: t,
            pane: 0,
            split: "right",
        });
        for (p, pane) in tab.panes.iter().enumerate() {
            let split = match pane.split.as_deref() {
                None | Some("right") => "right",
                Some("down") => "down",
                Some(other) => bail!("split {other:?}: use \"right\" or \"down\""),
            };
            out.push(Planned {
                host: host.clone(),
                name: text("name", &pane.name)?,
                cwd: text("cwd", &pane.cwd)?.or(text("cwd", &tab.cwd)?),
                run: run(&pane.run)?,
                tab: t,
                pane: p + 1,
                split,
            });
        }
    }
    Ok(out)
}

/// The session metadata for `p`, started as part of layout `name` by `swarmctl up` run `token`.
pub fn metadata(p: &Planned, name: &str, token: &str) -> BTreeMap<String, String> {
    let mut ui = BTreeMap::new();
    ui.insert("auto_attach".into(), "true".into());
    ui.insert("group".into(), name.into());
    ui.insert("tab_order".into(), format!("{}", p.tab * MAX_PANES + p.pane));
    ui.insert(META_LAYOUT.into(), name.into());
    ui.insert(META_LAYOUT_TAB.into(), p.tab.to_string());
    ui.insert(META_LAYOUT_PANE.into(), p.pane.to_string());
    ui.insert(META_LAYOUT_SPLIT.into(), p.split.into());
    ui.insert(META_LAYOUT_RUN.into(), token.into());
    ui.insert(META_LAYOUT_OPEN.into(), token.into());
    if let Some(cmd) = &p.run {
        ui.insert(swarm_proto::META_RESTART.into(), cmd.clone());
    }
    ui
}

#[cfg(test)]
mod tests {
    use super::*;

    const WEB: &str = r#"
[[tab]]
name = "server"
cwd = "~/src/web"
run = "npm run dev"

[[tab.pane]]
name = "tests"
run = "npm test"
split = "down"

[[tab]]
host = "devbox"
"#;

    #[test]
    fn a_layout_becomes_sessions_by_tab_and_pane() {
        let plan = plan(&parse(WEB).unwrap(), None).unwrap();
        assert_eq!(plan.len(), 3);
        assert_eq!((plan[0].tab, plan[0].pane, plan[0].run.as_deref()), (0, 0, Some("npm run dev")));
        assert_eq!(plan[1].cwd.as_deref(), Some("~/src/web"), "a pane gets its tab's folder");
        assert_eq!((plan[1].tab, plan[1].pane, plan[1].split), (0, 1, "down"));
        assert_eq!(plan[2].host.as_deref(), Some("devbox"));
        let ui = metadata(&plan[1], "web", "t1");
        assert_eq!(ui.get("layout.tab").map(String::as_str), Some("0"));
        assert_eq!(ui.get("restart").map(String::as_str), Some("npm test"));
        assert_eq!(ui.get("group").map(String::as_str), Some("web"));
    }

    #[test]
    fn bad_layouts_are_refused() {
        assert!(parse("[[tab]]\nnmae = \"x\"").is_err(), "unknown keys are typos");
        assert!(plan(&Layout::default(), None).is_err());
        assert!(plan(&parse("[[tab]]\nrun = \"a\\nb\"").unwrap(), None).is_err());
        assert!(plan(&parse("[[tab]]\nhost = \"-oProxyCommand=x\"").unwrap(), None).is_err());
        assert!(plan(&parse("[[tab]]\n[[tab.pane]]\nsplit = \"left\"").unwrap(), None).is_err());
        assert!(path_of("../x").unwrap().ends_with("../x"));
        assert!(path_of(".hidden").is_err());
    }
}
