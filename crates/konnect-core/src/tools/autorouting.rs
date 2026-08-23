//! Safe, revision-bound Freerouting bridge.
//!
//! KiCad's supported production round-trip is Specctra DSN/SES.  The SWIG
//! Python module owns that conversion; Konnect owns the mutation boundary.
//! Routing is performed in a fresh run directory and imported into a scratch
//! board first.  The requested board is replaced atomically only when it is
//! still byte-for-byte identical to the revision approved by the caller.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{get_path, ToolContext, ToolDef};
use konnect_sexp::writer::{read_consistent, write_atomic, write_atomic_if_unchanged};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;
use tokio::process::Command;

const EXPORT_DSN_SCRIPT: &str = r#"
import json
import pcbnew
import sys

board = pcbnew.LoadBoard(sys.argv[1])
if board is None:
    raise RuntimeError("KiCad could not load the source board")
if not pcbnew.ExportSpecctraDSN(board, sys.argv[2]):
    raise RuntimeError("KiCad failed to export the Specctra DSN")
print(json.dumps({"footprints": len(board.Footprints()), "tracks": len(board.Tracks())}))
"#;

const IMPORT_SES_SCRIPT: &str = r#"
import json
import pcbnew
import sys

board = pcbnew.LoadBoard(sys.argv[1])
if board is None:
    raise RuntimeError("KiCad could not load the source board")
before_footprints = len(board.Footprints())
before_tracks = len(board.Tracks())
if not pcbnew.ImportSpecctraSES(board, sys.argv[2]):
    raise RuntimeError("KiCad failed to import the Specctra session")
if len(board.Footprints()) != before_footprints:
    raise RuntimeError("Specctra import changed the footprint count")
if not pcbnew.SaveBoard(sys.argv[3], board):
    raise RuntimeError("KiCad failed to save the routed scratch board")
check = pcbnew.LoadBoard(sys.argv[3])
if check is None:
    raise RuntimeError("KiCad could not reload the routed scratch board")
if len(check.Footprints()) != before_footprints:
    raise RuntimeError("Routed scratch board failed the footprint-count readback")
print(json.dumps({
    "footprints": len(check.Footprints()),
    "tracks_before": before_tracks,
    "tracks_after": len(check.Tracks()),
}))
"#;

pub fn tools() -> Vec<ToolDef> {
    vec![tool!(
        "autoroute_with_freerouting",
        "Safely autoroute a closed KiCad board through the production DSN/SES flow. Call with dry_run=true first, then repeat with dry_run=false and the returned board_sha256. Konnect exports through KiCad, routes in an isolated run directory, imports into a scratch board, reloads it, and atomically replaces the source only if its bytes are unchanged.",
        json!({
            "type": "object",
            "properties": {
                "board": { "type": "string", "description": "Path to the .kicad_pcb board" },
                "work_dir": { "type": "string", "description": "Parent directory for persistent, isolated routing runs" },
                "freerouting_executable": { "type": "string", "description": "Optional Freerouting executable; auto-detects the macOS application launcher" },
                "kicad_python": { "type": "string", "description": "Optional Python interpreter containing KiCad's pcbnew module" },
                "max_passes": { "type": "integer", "minimum": 1, "maximum": 200, "default": 40 },
                "threads": { "type": "integer", "minimum": 0, "maximum": 64, "default": 4 },
                "timeout_seconds": { "type": "integer", "minimum": 30, "maximum": 7200, "default": 1800 },
                "copper_to_edge_clearance_um": { "type": "integer", "minimum": 0, "maximum": 10000, "default": 500 },
                "strict_drc": { "type": "boolean", "default": true },
                "dry_run": { "type": "boolean", "default": true },
                "expected_board_sha256": { "type": "string", "description": "Required for dry_run=false; copy board_sha256 from the dry run" }
            },
            "required": ["board", "work_dir"]
        }),
        |args, ctx| async move { handle_autoroute(args, ctx).await }
    )]
}

fn sha256_text(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

fn existing_file(path: PathBuf, label: &str) -> anyhow::Result<PathBuf> {
    if !path.is_file() {
        anyhow::bail!("{label} was not found at {}", path.display());
    }
    Ok(path)
}

fn resolve_kicad_python(args: &serde_json::Value, ctx: &ToolContext) -> anyhow::Result<PathBuf> {
    if let Some(path) = args["kicad_python"].as_str() {
        return existing_file(PathBuf::from(path), "KiCad Python");
    }

    let configured_cli = Path::new(&ctx.config.kicad_cli);
    if let Some(contents) = configured_cli.parent().and_then(Path::parent) {
        let bundled = contents
            .join("Frameworks")
            .join("Python.framework")
            .join("Versions")
            .join("3.9")
            .join("bin")
            .join("python3.9");
        if bundled.is_file() {
            return Ok(bundled);
        }
    }

    #[cfg(target_os = "macos")]
    {
        let bundled = PathBuf::from(
            "/Applications/KiCad/KiCad.app/Contents/Frameworks/Python.framework/Versions/3.9/bin/python3.9",
        );
        if bundled.is_file() {
            return Ok(bundled);
        }
    }

    anyhow::bail!("KiCad Python with the pcbnew module was not found; pass kicad_python explicitly")
}

fn resolve_freerouting(args: &serde_json::Value) -> anyhow::Result<PathBuf> {
    if let Some(path) = args["freerouting_executable"].as_str() {
        return existing_file(PathBuf::from(path), "Freerouting executable");
    }

    #[cfg(target_os = "macos")]
    {
        let launcher = PathBuf::from("/Applications/freerouting.app/Contents/MacOS/freerouting");
        if launcher.is_file() {
            return Ok(launcher);
        }
    }

    anyhow::bail!("Freerouting executable was not found; pass freerouting_executable explicitly")
}

fn command_text(output: &Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    [stdout.trim(), stderr.trim()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

async fn run_with_timeout(
    command: &mut Command,
    label: &str,
    timeout: Duration,
) -> anyhow::Result<Output> {
    let output = tokio::time::timeout(timeout, command.output())
        .await
        .map_err(|_| anyhow::anyhow!("{label} timed out after {} seconds", timeout.as_secs()))??;
    if !output.status.success() {
        anyhow::bail!("{label} failed: {}", command_text(&output));
    }
    Ok(output)
}

fn parse_u64(
    args: &serde_json::Value,
    key: &str,
    default: u64,
    min: u64,
    max: u64,
) -> Result<u64, CallToolResult> {
    let value = args[key].as_u64().unwrap_or(default);
    if !(min..=max).contains(&value) {
        return Err(CallToolResult::error(format!(
            "Argument '{key}' must be between {min} and {max}"
        )));
    }
    Ok(value)
}

async fn handle_autoroute(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let work_dir = get_path(args, "work_dir")?;
    if board.extension().and_then(|value| value.to_str()) != Some("kicad_pcb") {
        return Ok(CallToolResult::error("board must end in .kicad_pcb"));
    }
    if !board.is_file() {
        return Ok(CallToolResult::error(format!(
            "Board not found: {}",
            board.display()
        )));
    }

    let original = read_consistent(&board)?;
    let root = konnect_sexp::parse_sexp(&original)?;
    if root.head() != Some("kicad_pcb") {
        return Ok(CallToolResult::error(
            "The requested file is not one complete KiCad PCB document",
        ));
    }
    let board_sha256 = sha256_text(&original);
    let dry_run = args["dry_run"].as_bool().unwrap_or(true);
    let max_passes = match parse_u64(args, "max_passes", 40, 1, 200) {
        Ok(value) => value,
        Err(error) => return Ok(error),
    };
    let threads = match parse_u64(args, "threads", 4, 0, 64) {
        Ok(value) => value,
        Err(error) => return Ok(error),
    };
    let timeout_seconds = match parse_u64(args, "timeout_seconds", 1800, 30, 7200) {
        Ok(value) => value,
        Err(error) => return Ok(error),
    };
    let edge_clearance = match parse_u64(args, "copper_to_edge_clearance_um", 500, 0, 10_000) {
        Ok(value) => value,
        Err(error) => return Ok(error),
    };
    let strict_drc = args["strict_drc"].as_bool().unwrap_or(true);
    let kicad_python = resolve_kicad_python(args, ctx)?;
    let freerouting = resolve_freerouting(args)?;

    if dry_run {
        return Ok(CallToolResult::json(&json!({
            "dry_run": true,
            "board": board,
            "board_sha256": board_sha256,
            "work_dir": work_dir,
            "kicad_python": kicad_python,
            "freerouting_executable": freerouting,
            "max_passes": max_passes,
            "threads": threads,
            "timeout_seconds": timeout_seconds,
            "copper_to_edge_clearance_um": edge_clearance,
            "strict_drc": strict_drc,
            "next": "Repeat with dry_run=false and expected_board_sha256 set to board_sha256"
        })));
    }

    let expected_hash = match args["expected_board_sha256"].as_str() {
        Some(value) => value,
        None => {
            return Ok(CallToolResult::error(
                "dry_run=false requires expected_board_sha256 from a dry run",
            ))
        }
    };
    if expected_hash != board_sha256 {
        return Ok(CallToolResult::error(format!(
            "Board revision changed: expected {expected_hash}, current {board_sha256}. Run a new dry run; nothing was modified."
        )));
    }

    if let Some(refusal) = crate::tools::pcb_board::refuse_if_board_open_in_kicad(
        ctx.config.ipc_address.clone(),
        &board,
        "Specctra autoroute import",
    )
    .await?
    {
        return Ok(refusal);
    }

    std::fs::create_dir_all(&work_dir)?;
    let run_dir = tempfile::Builder::new()
        .prefix("freerouting-")
        .tempdir_in(&work_dir)?
        .keep();
    let stem = board
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("board");
    let dsn = run_dir.join(format!("{stem}.dsn"));
    let ses = run_dir.join(format!("{stem}.ses"));
    let routed_board = run_dir.join(format!("{stem}.routed.kicad_pcb"));
    let original_backup = run_dir.join(format!("{stem}.original.kicad_pcb"));
    let freerouting_drc = run_dir.join(format!("{stem}.freerouting-drc.json"));
    let freerouting_data = run_dir.join("freerouting-data");
    std::fs::create_dir_all(&freerouting_data)?;
    write_atomic(&original_backup, &original)?;

    let timeout = Duration::from_secs(timeout_seconds);
    let mut export = Command::new(&kicad_python);
    export
        .env("PYTHONNOUSERSITE", "1")
        .arg("-c")
        .arg(EXPORT_DSN_SCRIPT)
        .arg(&board)
        .arg(&dsn);
    let export_output = run_with_timeout(&mut export, "KiCad Specctra export", timeout).await?;
    if !dsn.is_file() || std::fs::metadata(&dsn)?.len() == 0 {
        anyhow::bail!("KiCad reported success but produced no Specctra DSN");
    }

    let mut route = Command::new(&freerouting);
    route
        .arg("--gui.enabled=false")
        .arg("--api_server.enabled=false")
        .arg("-da")
        .arg("-de")
        .arg(&dsn)
        .arg("-do")
        .arg(&ses)
        .arg("-mp")
        .arg(max_passes.to_string())
        .arg("-mt")
        .arg(threads.to_string())
        .arg(format!("--router.strict_drc={strict_drc}"))
        .arg(format!(
            "--router.copper_to_edge_clearance_um={edge_clearance}"
        ))
        .arg(format!("--user_data_path={}", freerouting_data.display()))
        .arg("--logging.console.enabled=true")
        .arg("--logging.console.level=INFO");
    let route_output = run_with_timeout(&mut route, "Freerouting", timeout).await?;
    if !ses.is_file() || std::fs::metadata(&ses)?.len() == 0 {
        anyhow::bail!("Freerouting reported success but produced no Specctra session");
    }

    // `-drc` is a dedicated DRC-only mode in Freerouting 2.3: combining it
    // with `-do` exits cleanly after the report and never routes.  Validate the
    // completed DSN+SES in a second process instead.
    let mut freerouting_check = Command::new(&freerouting);
    freerouting_check
        .arg("--gui.enabled=false")
        .arg("--api_server.enabled=false")
        .arg("-da")
        .arg("-de")
        .arg(format!("{}+{}", dsn.display(), ses.display()))
        .arg("-drc")
        .arg(&freerouting_drc)
        .arg(format!("--router.strict_drc={strict_drc}"))
        .arg(format!(
            "--router.copper_to_edge_clearance_um={edge_clearance}"
        ))
        .arg(format!("--user_data_path={}", freerouting_data.display()))
        .arg("--logging.console.enabled=true")
        .arg("--logging.console.level=INFO");
    let freerouting_check_output =
        run_with_timeout(&mut freerouting_check, "Freerouting DRC", timeout).await?;
    if !freerouting_drc.is_file() || std::fs::metadata(&freerouting_drc)?.len() == 0 {
        anyhow::bail!("Freerouting DRC reported success but produced no report");
    }

    let mut import = Command::new(&kicad_python);
    import
        .env("PYTHONNOUSERSITE", "1")
        .arg("-c")
        .arg(IMPORT_SES_SCRIPT)
        .arg(&board)
        .arg(&ses)
        .arg(&routed_board);
    let import_output = run_with_timeout(&mut import, "KiCad Specctra import", timeout).await?;
    let routed = read_consistent(&routed_board)?;
    let routed_root = konnect_sexp::parse_sexp(&routed)?;
    if routed_root.head() != Some("kicad_pcb") {
        anyhow::bail!("KiCad's routed scratch output is not a complete PCB document");
    }

    write_atomic_if_unchanged(&board, &original, &routed)?;
    let readback = read_consistent(&board)?;
    if readback != routed {
        anyhow::bail!("Atomic board write failed readback verification");
    }

    Ok(CallToolResult::json(&json!({
        "dry_run": false,
        "board": board,
        "source_board_sha256": board_sha256,
        "routed_board_sha256": sha256_text(&routed),
        "run_dir": run_dir,
        "dsn": dsn,
        "ses": ses,
        "original_backup": original_backup,
        "routed_scratch_board": routed_board,
        "freerouting_drc": freerouting_drc,
        "export_readback": command_text(&export_output),
        "import_readback": command_text(&import_output),
        "freerouting_drc_readback": command_text(&freerouting_check_output),
        "freerouting_log_tail": command_text(&route_output)
            .lines()
            .rev()
            .take(80)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n"),
        "saved_and_reloaded": true,
        "next": "Run KiCad DRC and design review before accepting the route"
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_is_revision_bound_and_dry_by_default() {
        let definition = tools().pop().unwrap();
        assert_eq!(definition.name, "autoroute_with_freerouting");
        assert_eq!(
            definition.input_schema["properties"]["dry_run"]["default"],
            true
        );
        assert!(definition.description.contains("board_sha256"));
    }

    #[test]
    fn bounded_integer_parser_rejects_out_of_range_values() {
        assert!(parse_u64(&json!({"passes": 0}), "passes", 40, 1, 200).is_err());
        assert_eq!(
            parse_u64(&json!({"passes": 12}), "passes", 40, 1, 200).unwrap(),
            12
        );
    }

    #[test]
    fn python_readback_avoids_kicads_broken_swig_list_helpers() {
        assert!(!EXPORT_DSN_SCRIPT.contains("GetTracks"));
        assert!(!EXPORT_DSN_SCRIPT.contains("GetFootprints"));
        assert!(!IMPORT_SES_SCRIPT.contains("GetTracks"));
        assert!(!IMPORT_SES_SCRIPT.contains("GetFootprints"));
    }
}
