//! `lector commands` (the registry) and `lector run` (one command).

use std::io::Read as _;

use anyhow::{anyhow, bail, Context as _};
use lector_app::commands::CommandError;
use lector_workspace::Project;
use serde_json::{json, Value};

use crate::args::{CommandsArgs, RunArgs};
use crate::out::{fit, print_json, Io};
use crate::session::{context, open_core, open_project, resolve_dir};

/// Runs `lector commands`.
pub fn run_commands(args: &CommandsArgs, io: &mut Io) -> anyhow::Result<()> {
    let core = open_core();
    let wanted = args.category.as_deref().map(str::to_lowercase);
    let commands: Vec<_> = core
        .registry()
        .list()
        .into_iter()
        .filter(|c| {
            wanted
                .as_deref()
                .is_none_or(|w| c.spec().category.to_lowercase() == w)
        })
        .collect();
    if args.json {
        let list: Vec<Value> = commands.iter().map(|c| c.spec().to_json()).collect();
        return Ok(print_json(io.out, json!({ "commands": list }))?);
    }
    let s = io.style;
    let id_width = commands
        .iter()
        .map(|c| c.spec().id.len())
        .max()
        .unwrap_or(0);
    let mut category = "";
    for c in &commands {
        let spec = c.spec();
        if spec.category != category {
            category = spec.category;
            writeln!(io.out, "{}", s.bold(category))?;
        }
        let flags = match (spec.writes, spec.destructive) {
            (false, _) => String::new(),
            (true, false) => s.yellow(" [writes]"),
            (true, true) => s.red(" [writes, destructive]"),
        };
        let args_hint = arg_hint(&spec.args_schema);
        writeln!(
            io.out,
            "  {}  {}{flags}{}",
            fit(spec.id, id_width),
            spec.title,
            if args_hint.is_empty() {
                String::new()
            } else {
                format!("  {}", s.dim(&args_hint))
            }
        )?;
    }
    writeln!(
        io.out,
        "\n{}",
        s.dim("`lector commands --json` has the argument schemas; `lector run <id> --args '{...}'` runs one.")
    )?;
    Ok(())
}

/// `{path*, line}` style summary of an object schema (`*` = required).
fn arg_hint(schema: &Value) -> String {
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return String::new();
    };
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut keys: Vec<&String> = props.keys().collect();
    keys.sort();
    let names: Vec<String> = keys
        .into_iter()
        .map(|k| {
            if required.contains(&k.as_str()) {
                format!("{k}*")
            } else {
                k.clone()
            }
        })
        .collect();
    if names.is_empty() {
        String::new()
    } else {
        format!("{{{}}}", names.join(", "))
    }
}

/// The `--args` value: inline JSON, `@file`, or `-` for stdin. Must be a JSON object.
fn parse_args_value(raw: Option<&str>) -> anyhow::Result<Value> {
    let Some(raw) = raw else {
        return Ok(json!({}));
    };
    let text = if raw == "-" {
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .context("cannot read the arguments from stdin")?;
        s
    } else if let Some(path) = raw.strip_prefix('@') {
        std::fs::read_to_string(path).with_context(|| format!("cannot read {path}"))?
    } else {
        raw.to_string()
    };
    let value: Value =
        serde_json::from_str(&text).map_err(|e| anyhow!("--args is not valid JSON: {e}"))?;
    match value {
        Value::Null => Ok(json!({})),
        Value::Object(_) => Ok(value),
        other => bail!("--args must be a JSON object, got {}", type_of(&other)),
    }
}

fn type_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Runs `lector run`.
pub fn run_command(args: &RunArgs, io: &mut Io) -> anyhow::Result<()> {
    let call = parse_args_value(args.args.as_deref())?;
    let core = open_core();
    let command = core
        .registry()
        .get(&args.id)
        .ok_or_else(|| anyhow!("unknown command `{}` (see `lector commands`)", args.id))?;
    let spec = command.spec();
    if spec.writes && !args.allow_writes {
        bail!(
            "`{}` writes to your files or repository; pass --allow-writes to run it{}",
            spec.id,
            if spec.destructive {
                " (it is destructive: what it overwrites is backed up and can be restored with `lector undo`)"
            } else {
                ""
            }
        );
    }
    let dir = resolve_dir(args.directory.as_deref())?;
    let project: Option<Project> = Some(open_project(&dir)?);
    let cx = context(&core, project);
    let outcome = core
        .registry()
        .invoke(&args.id, call, &cx)
        .map_err(|e| match e {
            CommandError::Failed(err) => err,
            other => anyhow!("{other}"),
        })?;
    if args.json {
        let mut v = json!({
            "ok": true,
            "command": spec.id,
            "summary": outcome.summary,
            "data": outcome.data,
            "paths": outcome.paths.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>(),
        });
        if let Some(id) = outcome.journal_id {
            v["journal_id"] = json!(id);
            v["undoable"] = json!(outcome.undo.is_some());
        }
        return Ok(print_json(io.out, v)?);
    }
    if !outcome.summary.is_empty() {
        writeln!(io.out, "{}", io.style.bold(&outcome.summary))?;
    }
    if !outcome.data.is_null() {
        let text = serde_json::to_string_pretty(&outcome.data).map_err(std::io::Error::other)?;
        writeln!(io.out, "{text}")?;
    }
    if let Some(id) = outcome.journal_id {
        writeln!(
            io.out,
            "{}",
            io.style.dim(&format!(
                "journal #{id}{}",
                if outcome.undo.is_some() {
                    format!(" (undo with `lector undo {id}`)")
                } else {
                    String::new()
                }
            ))
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints() {
        let schema = json!({"type":"object","properties":{"path":{"type":"string"},"line":{}},"required":["path"]});
        assert_eq!(arg_hint(&schema), "{line, path*}");
        assert_eq!(arg_hint(&json!({"type":"object"})), "");
    }

    #[test]
    fn args_parsing() {
        assert_eq!(parse_args_value(None).unwrap(), json!({}));
        assert_eq!(
            parse_args_value(Some("{\"a\":1}")).unwrap(),
            json!({"a": 1})
        );
        assert_eq!(parse_args_value(Some("null")).unwrap(), json!({}));
        assert!(parse_args_value(Some("[1]")).is_err());
        assert!(parse_args_value(Some("{oops")).is_err());
    }
}
