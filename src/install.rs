use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

/// Where we expect a user's Claude Code settings file to live. Hosts the
/// SessionEnd hook.
fn settings_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not resolve $HOME")?;
    Ok(home.join(".claude").join("settings.json"))
}

/// Where global MCP servers live. Anthropic's CLI maintains this file and
/// reads `mcpServers` from it across every Claude Code session.
fn global_mcp_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not resolve $HOME")?;
    Ok(home.join(".claude.json"))
}

/// Per-project MCP config file. Lives next to the repo root.
fn project_mcp_path(dir: &Path) -> PathBuf {
    dir.join(".mcp.json")
}

#[derive(Debug, Default, Clone)]
pub struct InstallOpts {
    pub no_hook: bool,
    pub no_mcp: bool,
    pub project_mcp: Option<PathBuf>,
    pub dry_run: bool,
}

/// Install the SessionEnd hook and the sonar MCP server entry. Both files
/// are backed up to `<file>.pre-sonar` on first modification so an
/// uninstall can put them back exactly.
pub fn install(opts: InstallOpts) -> Result<()> {
    let bin = std::env::current_exe()?;
    let bin_str = bin.to_string_lossy().to_string();

    if !opts.no_hook {
        let path = settings_path()?;
        let cmd = hook_command(&bin_str);
        let summary = mutate_json(&path, opts.dry_run, |doc| {
            ensure_object(doc, "settings.json");
            let hooks = doc
                .as_object_mut()
                .unwrap()
                .entry("hooks".to_string())
                .or_insert_with(|| json!({}));
            ensure_object(hooks, "settings.json/hooks");
            let session_end = hooks
                .as_object_mut()
                .unwrap()
                .entry("SessionEnd".to_string())
                .or_insert_with(|| json!([]));
            let arr = session_end
                .as_array_mut()
                .context("SessionEnd is not an array")?;
            // idempotent: if any existing entry already runs our binary, skip.
            let already = arr.iter().any(|entry| {
                entry
                    .pointer("/hooks/0/command")
                    .and_then(|v| v.as_str())
                    .map(|s| s.contains(&bin_str))
                    .unwrap_or(false)
            });
            if already {
                Ok(MutateOutcome::Unchanged("already installed".into()))
            } else {
                arr.push(json!({
                    "matcher": "",
                    "hooks": [{ "type": "command", "command": cmd }]
                }));
                Ok(MutateOutcome::Changed(
                    "added SessionEnd hook for sonar".into(),
                ))
            }
        })?;
        println!("hook  ({}): {}", path.display(), summary);
    }

    if !opts.no_mcp {
        let path = match &opts.project_mcp {
            Some(dir) => project_mcp_path(dir),
            None => global_mcp_path()?,
        };
        let summary = mutate_json(&path, opts.dry_run, |doc| {
            ensure_object(doc, "mcp config");
            let servers = doc
                .as_object_mut()
                .unwrap()
                .entry("mcpServers".to_string())
                .or_insert_with(|| json!({}));
            // If a previous run left this as null (some Claude Code installs
            // ship with mcpServers: null), coerce it to an empty object.
            if servers.is_null() {
                *servers = json!({});
            }
            ensure_object(servers, "mcpServers");
            let map = servers.as_object_mut().unwrap();
            let new_entry = json!({
                "command": bin_str,
                "args": ["mcp"]
            });
            let already = map.get("sonar").map(|v| v == &new_entry).unwrap_or(false);
            if already {
                Ok(MutateOutcome::Unchanged("already installed".into()))
            } else {
                map.insert("sonar".to_string(), new_entry);
                Ok(MutateOutcome::Changed("registered sonar MCP server".into()))
            }
        })?;
        println!("mcp   ({}): {}", path.display(), summary);
    }

    if opts.dry_run {
        println!("\n(dry-run — nothing written. omit --dry-run to apply.)");
    } else {
        println!("\nDone. Restart Claude Code so the MCP server loads. The hook works without restart.");
    }
    Ok(())
}

/// Restore `<file>.pre-sonar` backups for both settings.json and the chosen
/// MCP config. Idempotent: if a backup is missing we just skip that file.
pub fn uninstall(project_mcp: Option<PathBuf>) -> Result<()> {
    for path in [
        Some(settings_path()?),
        Some(match project_mcp {
            Some(dir) => project_mcp_path(&dir),
            None => global_mcp_path()?,
        }),
    ]
    .into_iter()
    .flatten()
    {
        let backup = backup_path(&path);
        if !backup.exists() {
            println!("skip  ({}): no backup", path.display());
            continue;
        }
        fs::copy(&backup, &path)
            .with_context(|| format!("restoring {}", path.display()))?;
        println!("restored {}", path.display());
    }
    Ok(())
}

fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".pre-sonar");
    path.with_file_name(name)
}

enum MutateOutcome {
    Changed(String),
    Unchanged(String),
}

fn mutate_json<F>(path: &Path, dry_run: bool, mutate: F) -> Result<String>
where
    F: FnOnce(&mut Value) -> Result<MutateOutcome>,
{
    let mut doc: Value = if path.exists() {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        if raw.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&raw)
                .with_context(|| format!("parsing JSON in {}", path.display()))?
        }
    } else {
        json!({})
    };

    let outcome = mutate(&mut doc)?;
    match outcome {
        MutateOutcome::Unchanged(s) => Ok(s),
        MutateOutcome::Changed(s) => {
            if dry_run {
                return Ok(format!("would: {} (dry-run)", s));
            }
            if path.exists() {
                let backup = backup_path(path);
                if !backup.exists() {
                    fs::copy(path, &backup).with_context(|| {
                        format!("backing up {} -> {}", path.display(), backup.display())
                    })?;
                }
            } else if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            let pretty = serde_json::to_string_pretty(&doc)?;
            // Write to a temp file in the same dir, then rename — atomic on
            // POSIX so a crash mid-write can't corrupt the original.
            let tmp = path.with_extension("json.sonar-tmp");
            fs::write(&tmp, pretty)?;
            fs::rename(&tmp, path)?;
            Ok(s)
        }
    }
}

fn ensure_object(v: &mut Value, label: &str) {
    if !v.is_object() {
        // Coerce non-objects (null, missing) to {} so we can mutate.
        *v = json!({});
    }
    let _ = label;
}

fn hook_command(bin: &str) -> String {
    format!(
        r#"P=$(jq -r .transcript_path 2>/dev/null) && [ -n "$P" ] && "{}" index --file "$P" >/dev/null 2>&1 || true"#,
        bin
    )
}
