//! Fish history sync module
//!
//! This module handles syncing Atuin history entries to Fish shell's history file,
//! enabling Fish's autosuggestions (ghost text) to work with Atuin history.

use atuin_client::database::Database;
use atuin_client::history::History;
use atuin_client::settings::Settings;
use eyre::{Context, Result};
use fs_err as fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use time::OffsetDateTime;
use tracing::{debug, error, info, warn};

/// Format a history entry for Fish's history file format
///
/// Fish history format:
/// ```text
/// - cmd:git status
///   when:1737097200
/// - cmd:ls -la
///   when:1737097205
/// ```
fn format_fish_entry(history: &History) -> String {
    // Escape backslashes and newlines in the command
    let escaped_cmd = history
        .command
        .replace('\\', "\\\\")
        .replace('\n', "\\n");

    let timestamp = history.timestamp.unix_timestamp();

    format!("- cmd:{}\n  when:{}\n", escaped_cmd, timestamp)
}

/// Sync a history entry to Fish's history file
pub fn sync_entry(history: &History, settings: &Settings) -> Result<()> {
    if !settings.fish_sync.enabled {
        return Ok(());
    }

    let fish_history_path = &settings.fish_sync.history_path;

    debug!(
        id = history.id.0.as_str(),
        path = fish_history_path.as_str(),
        "syncing history to fish"
    );

    // Ensure the parent directory exists
    if let Some(parent) = Path::new(fish_history_path).parent() {
        if !parent.exists() {
            fs::create_dir_all(parent)
                .context("failed to create fish history directory")?;
        }
    }

    // Format the entry
    let entry = format_fish_entry(history);

    // Append to the file
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(fish_history_path)
        .context("failed to open fish history file")?;

    file.write_all(entry.as_bytes())
        .context("failed to write to fish history file")?;

    file.flush().context("failed to flush fish history file")?;

    debug!(
        id = history.id.0.as_str(),
        "synced history to fish"
    );

    // Trim the file if it exceeds max_entries
    trim_fish_history(fish_history_path, settings.fish_sync.max_entries)?;

    Ok(())
}

/// Sync multiple history entries to Fish's history file
pub fn sync_entries(entries: &[History], settings: &Settings) -> Result<()> {
    if !settings.fish_sync.enabled || entries.is_empty() {
        return Ok(());
    }

    info!(
        count = entries.len(),
        "syncing multiple history entries to fish"
    );

    for entry in entries {
        if let Err(e) = sync_entry(entry, settings) {
            error!(
                id = entry.id.0.as_str(),
                error = %e,
                "failed to sync entry to fish"
            );
        }
    }

    Ok(())
}

/// Trim the Fish history file to keep only the most recent N entries
///
/// Fish history files can grow indefinitely, so we need to trim them
/// to prevent performance issues.
fn trim_fish_history(path: &str, max_entries: usize) -> Result<()> {
    if max_entries == 0 {
        return Ok(()); // 0 means no limit
    }

    let path = Path::new(path);
    if !path.exists() {
        return Ok(());
    }

    // Read the file
    let content = fs::read_to_string(path).context("failed to read fish history file")?;

    // Parse entries
    let entries: Vec<&str> = content.split("- cmd:").skip(1).collect();

    if entries.len() <= max_entries {
        return Ok(());
    }

    warn!(
        path = path.display().to_string(),
        current = entries.len(),
        max = max_entries,
        "trimming fish history file"
    );

    // Keep only the most recent entries
    let to_keep = &entries[entries.len() - max_entries..];

    // Rebuild the file
    let mut trimmed = String::new();
    for entry in to_keep {
        trimmed.push_str("- cmd:");
        trimmed.push_str(entry);
    }

    // Write back
    fs::write(path, trimmed).context("failed to write trimmed fish history file")?;

    info!(
        path = path.display().to_string(),
        removed = entries.len() - max_entries,
        remaining = max_entries,
        "trimmed fish history file"
    );

    Ok(())
}

/// Get the last synced timestamp from Fish history
///
/// This can be used to avoid syncing duplicate entries.
pub fn get_last_synced_timestamp(path: &str) -> Result<Option<i64>> {
    let path = Path::new(path);
    if !path.exists() {
        return Ok(None);
    }

    let content = fs::read_to_string(path).context("failed to read fish history file")?;

    // Find the last "when:" timestamp
    let last_timestamp = content
        .lines()
        .rev()
        .find(|line| line.starts_with("  when:"))
        .and_then(|line| line.trim_start_matches("  when:").parse::<i64>().ok());

    Ok(last_timestamp)
}

/// Bootstrap Fish history with recent entries from Atuin
///
/// This should be called when the daemon first starts up to populate
/// Fish's history with the most recent Atuin entries.
pub async fn bootstrap_fish_history(
    settings: &Settings,
    history_db: &atuin_client::database::Sqlite,
) -> Result<()> {
    if !settings.fish_sync.enabled {
        return Ok(());
    }

    info!("bootstrapping fish history with recent atuin entries");

    let fish_history_path = &settings.fish_sync.history_path;

    // Get the last synced timestamp from Fish history
    let last_synced = get_last_synced_timestamp(fish_history_path)?;
    let last_synced_time = last_synced.and_then(|ts| OffsetDateTime::from_unix_timestamp(ts).ok());

    // Fetch recent entries from Atuin database
    let filters = &[];
    let context = &atuin_client::database::current_context();
    let max = Some(settings.fish_sync.max_entries);

    let entries = history_db
        .list(filters, context, max, false, false)
        .await
        .context("failed to fetch history from database")?;

    // Filter entries that are newer than the last synced timestamp
    let new_entries: Vec<_> = entries
        .into_iter()
        .filter(|entry| {
            // Only include entries that are newer than the last synced timestamp
            if let Some(last_synced) = last_synced_time {
                entry.timestamp > last_synced
            } else {
                true
            }
        })
        .collect();

    if new_entries.is_empty() {
        info!("no new entries to bootstrap to fish history");
        return Ok(());
    }

    info!(
        count = new_entries.len(),
        "bootstrapping fish history with new entries"
    );

    // Sync the entries
    for entry in &new_entries {
        if let Err(e) = sync_entry(entry, settings) {
            error!(
                id = entry.id.0.as_str(),
                error = %e,
                "failed to bootstrap entry to fish"
            );
        }
    }

    info!(
        count = new_entries.len(),
        "bootstrapped fish history with atuin entries"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_fish_entry() {
        let history = History::new(
            OffsetDateTime::UNIX_EPOCH,
            "git status".to_string(),
            "/home/user".to_string(),
            0,
            0,
            None,
            None,
            None,
        );

        let formatted = format_fish_entry(&history);
        assert!(formatted.contains("- cmd:git status"));
        assert!(formatted.contains("  when:0"));
    }

    #[test]
    fn test_format_fish_entry_with_special_chars() {
        let history = History::new(
            OffsetDateTime::UNIX_EPOCH,
            "echo \"hello\\nworld\"".to_string(),
            "/home/user".to_string(),
            0,
            0,
            None,
            None,
            None,
        );

        let formatted = format_fish_entry(&history);
        assert!(formatted.contains("- cmd:echo \"hello\\nworld\""));
    }
}
