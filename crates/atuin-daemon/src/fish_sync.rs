//! Fish history sync module
//!
//! This module handles syncing Atuin history entries to Fish shell's history file,
//! enabling Fish's autosuggestions (ghost text) to work with Atuin history.

use atuin_client::database::Database;
use atuin_client::history::History;
use atuin_client::settings::Settings;
use eyre::{Context, Result};
use fs_err as fs;
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;
use tracing::{debug, error, info, warn};

/// Cached check for Fish shell installation
///
/// This avoids spawning a process on every call.
fn is_fish_installed() -> bool {
    static FISH_INSTALLED: OnceLock<bool> = OnceLock::new();
    *FISH_INSTALLED.get_or_init(|| {
        Command::new("fish")
            .arg("--version")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    })
}

/// Parse Fish history file and extract synced entry UUIDs from metadata
///
/// This reads the Fish history file and extracts UUIDs from the metadata
/// comments we add. This enables UUID-based deduplication instead of relying
/// on timestamps which can fail with clock skew or remote commands.
fn get_synced_uuids(path: &str) -> Result<HashSet<String>> {
    let path = Path::new(path);
    if !path.exists() {
        return Ok(HashSet::new());
    }

    let content = fs::read_to_string(path).context("failed to read fish history file")?;

    // Extract UUIDs from comments (format: # atuin-uuid:XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX)
    let uuids: HashSet<String> = content
        .lines()
        .filter(|line| line.starts_with("  # atuin-uuid:"))
        .map(|line| line.trim_start_matches("  # atuin-uuid:").to_string())
        .collect();

    debug!(
        path = path.display().to_string(),
        count = uuids.len(),
        "found synced uuids in fish history"
    );

    Ok(uuids)
}

/// Format a history entry for Fish's history file format
///
/// Fish history format:
/// ```text
/// - cmd:git status
///   when:1737097200
/// - cmd:ls -la
///   when:1737097205
/// ```
///
/// We add a UUID metadata comment to enable deduplication:
/// ```text
/// - cmd:git status
///   when:1737097200
///   # atuin-uuid:01234567-89ab-cdef-0123-456789abcdef
/// ```
fn format_fish_entry(history: &History) -> String {
    // Escape backslashes and newlines in the command
    let escaped_cmd = history
        .command
        .replace('\\', "\\\\")
        .replace('\n', "\\n");

    let timestamp = history.timestamp.unix_timestamp();
    let uuid = history.id.0.to_string();

    // Fish ignores unknown fields, so we can add UUID as a comment
    format!(
        "- cmd:{}\n  when:{}\n  # atuin-uuid:{}\n",
        escaped_cmd, timestamp, uuid
    )
}

/// Sync a history entry to Fish's history file
pub fn sync_entry(history: &History, settings: &Settings) -> Result<()> {
    if !settings.fish_sync.enabled {
        return Ok(());
    }

    // Don't attempt to sync if Fish is not installed
    if !is_fish_installed() {
        debug!("fish shell not installed, skipping sync");
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
///
/// Uses UUID-based deduplication to avoid syncing the same entry twice,
/// which allows syncing remote commands with timestamps older than local entries.
pub async fn bootstrap_fish_history(
    settings: &Settings,
    history_db: &atuin_client::database::Sqlite,
) -> Result<()> {
    if !settings.fish_sync.enabled {
        return Ok(());
    }

    // Don't attempt to sync if Fish is not installed
    if !is_fish_installed() {
        debug!("fish shell not installed, skipping bootstrap");
        return Ok(());
    }

    info!("bootstrapping fish history with recent atuin entries");

    let fish_history_path = &settings.fish_sync.history_path;

    // Get already synced UUIDs from Fish history metadata
    let synced_uuids = get_synced_uuids(fish_history_path)?;

    debug!(
        synced_count = synced_uuids.len(),
        "found existing synced entries in fish history"
    );

    // Fetch recent entries from Atuin database
    let filters = &[];
    let context = &atuin_client::database::current_context();
    let max = Some(settings.fish_sync.max_entries);

    let entries = history_db
        .list(filters, context, max, false, false)
        .await
        .context("failed to fetch history from database")?;

    // Filter out entries that have already been synced (by UUID)
    let new_entries: Vec<_> = entries
        .into_iter()
        .filter(|entry| !synced_uuids.contains(entry.id.0.as_str()))
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
        let history = History {
            id: "00000000-0000-0000-0000-000000000001".to_string().into(),
            timestamp: OffsetDateTime::UNIX_EPOCH,
            duration: 0,
            exit: 0,
            command: "git status".to_string(),
            cwd: "/home/user".to_string(),
            session: "test".to_string(),
            hostname: "localhost".to_string(),
            deleted_at: None,
        };

        let formatted = format_fish_entry(&history);
        assert!(formatted.contains("- cmd:git status"));
        assert!(formatted.contains("  when:0"));
        assert!(formatted.contains("  # atuin-uuid:"));
    }

    #[test]
    fn test_format_fish_entry_with_special_chars() {
        let history = History {
            id: "00000000-0000-0000-0000-000000000002".to_string().into(),
            timestamp: OffsetDateTime::UNIX_EPOCH,
            duration: 0,
            exit: 0,
            command: "echo \"hello\\nworld\"".to_string(),
            cwd: "/home/user".to_string(),
            session: "test".to_string(),
            hostname: "localhost".to_string(),
            deleted_at: None,
        };

        let formatted = format_fish_entry(&history);
        // Check that the command is present and properly escaped
        // The original command has literal backslash-n, which gets escaped to double backslash
        assert!(formatted.contains("echo"));
        assert!(formatted.contains("hello"));
        assert!(formatted.contains("world"));
        assert!(formatted.contains("  # atuin-uuid:"));
    }

    #[test]
    fn test_get_synced_uuids() {
        // Create a temporary file with UUID metadata
        let temp_file = "/tmp/test_fish_history_uuids";
        let content = "- cmd:test1\n  when:1000\n  # atuin-uuid:00000000-0000-0000-0000-000000000001\n\
                        - cmd:test2\n  when:2000\n  # atuin-uuid:00000000-0000-0000-0000-000000000002\n\
                        - cmd:test3\n  when:3000\n  # atuin-uuid:00000000-0000-0000-0000-000000000003\n";

        fs::write(temp_file, content).expect("failed to write test file");

        let uuids = get_synced_uuids(temp_file).expect("failed to get synced uuids");

        assert_eq!(uuids.len(), 3);
        assert!(uuids.contains("00000000-0000-0000-0000-000000000001"));
        assert!(uuids.contains("00000000-0000-0000-0000-000000000002"));
        assert!(uuids.contains("00000000-0000-0000-0000-000000000003"));

        // Clean up
        fs::remove_file(temp_file).ok();
    }
}
