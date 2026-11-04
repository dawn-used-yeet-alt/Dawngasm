//! Dawngasm TUI entry point.
//!
//! Sets up the terminal, runs the event loop, and restores the terminal even
//! when the loop returns an error. `dawngasm-tui extensions ...` is a small
//! diagnostic surface for the plugin system and never starts the TUI.

use dawngasm_tui::services::extensions::{Extensions, PLUGIN_PATH_ENV};
use dawngasm_tui::App;

fn main() -> anyhow::Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.first().map(String::as_str) == Some("extensions") {
        return extensions_command(&arguments[1..]);
    }

    // Built before the alternate screen so a startup failure (no data dir, no
    // audio device) reads as a normal error rather than a corrupted terminal.
    let mut app = App::new()?;
    let mut terminal = ratatui::init();
    let result = app.run(&mut terminal);
    ratatui::restore();
    result?;
    Ok(())
}

fn extensions_command(arguments: &[String]) -> anyhow::Result<()> {
    match arguments.first().map(String::as_str) {
        Some("call") => extensions_call(arguments),
        Some("list") | None => extensions_list(arguments),
        Some(other) => anyhow::bail!(
            "unknown `extensions` subcommand `{other}`; try `extensions list` or `extensions call`"
        ),
    }
}

fn extensions_list(arguments: &[String]) -> anyhow::Result<()> {
    let extensions = Extensions::global();
    if arguments.iter().any(|argument| argument == "--json") {
        let failures: Vec<_> = extensions
            .failures()
            .iter()
            .map(|failure| {
                serde_json::json!({
                    // Paths are not necessarily UTF-8, and `json!` panics
                    // when a value cannot be serialized.
                    "path": failure.path.to_string_lossy(),
                    "error": failure.error,
                })
            })
            .collect();
        let plugins: Vec<_> = extensions
            .plugins()
            .iter()
            .map(|plugin| {
                let mut value =
                    serde_json::to_value(&plugin.manifest).unwrap_or(serde_json::Value::Null);
                if let serde_json::Value::Object(fields) = &mut value {
                    fields.insert(
                        "path".to_string(),
                        serde_json::json!(plugin.path.to_string_lossy()),
                    );
                }
                value
            })
            .collect();
        let search_dirs: Vec<_> = Extensions::search_dirs()
            .into_iter()
            .map(|dir| dir.to_string_lossy().into_owned())
            .collect();
        let value = serde_json::json!({
            "search_dirs": search_dirs,
            "plugins": plugins,
            "failures": failures,
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }

    println!("Search path ({PLUGIN_PATH_ENV} first):");
    for dir in Extensions::search_dirs() {
        println!("  {}", dir.display());
    }
    println!();

    let plugins = extensions.plugins();
    if plugins.is_empty() {
        println!("No extensions found.");
    } else {
        println!("Loaded:");
        for plugin in plugins {
            let capabilities = plugin
                .manifest
                .capabilities
                .iter()
                .map(|capability| capability.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            println!(
                "  {:<16} {:<10} {}",
                plugin.manifest.id,
                plugin.manifest.version,
                plugin.path.display()
            );
            if !capabilities.is_empty() {
                println!("      {capabilities}");
            }
            for command in &plugin.manifest.commands {
                println!("      :{} — {}", command.id, command.display_title());
            }
        }
    }

    if !extensions.failures().is_empty() {
        println!();
        println!("Failed:");
        for failure in extensions.failures() {
            println!("  {}", failure.path.display());
            println!("      {}", failure.error);
        }
    }
    Ok(())
}

fn extensions_call(arguments: &[String]) -> anyhow::Result<()> {
    let usage = "usage: dawngasm-tui extensions call <id|capability> <method> [json]";
    let reference = arguments.get(1).ok_or_else(|| anyhow::anyhow!("{usage}"))?;
    let method = arguments.get(2).ok_or_else(|| anyhow::anyhow!("{usage}"))?;
    let params = match arguments.get(3) {
        Some(raw) => serde_json::from_str(raw)?,
        None => serde_json::json!({}),
    };

    let extensions = Extensions::global();
    let plugin = extensions
        .find_by_ref(reference)
        .ok_or_else(|| anyhow::anyhow!("no extension matches `{reference}`"))?;
    let result = plugin
        .call(method, params)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
