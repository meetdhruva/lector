//! `lector mcp`: the MCP server on stdin / stdout.

use std::io;

use lector_mcp::{McpServer, ServerOptions};

use crate::args::McpArgs;
use crate::session::{open_core, open_project, resolve_dir};

/// Serves MCP until stdin closes. Nothing but protocol messages goes to stdout; a short
/// banner goes to stderr.
pub fn run(args: &McpArgs) -> anyhow::Result<()> {
    let dir = resolve_dir(args.path.as_deref())?;
    let project = open_project(&dir)?;
    let core = open_core();
    let server = McpServer::new(
        core,
        Some(project),
        ServerOptions {
            allow_writes: args.allow_writes,
            version: env!("CARGO_PKG_VERSION").into(),
            ..ServerOptions::default()
        },
    );
    eprintln!(
        "lector mcp: serving {} ({}) on stdio",
        dir.display(),
        if args.allow_writes {
            "writes allowed, journaled as origin mcp"
        } else {
            "read-only"
        }
    );
    server.serve(io::stdin().lock(), io::stdout())?;
    Ok(())
}
