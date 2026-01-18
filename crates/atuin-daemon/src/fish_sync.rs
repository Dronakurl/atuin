//! Fish history sync module
//!
//! This module handles syncing Atuin history entries to Fish shell's history file,
//! enabling Fish's autosuggestions (ghost text) to work with Atuin history.
//!
//! **Note:** This is a temporary workaround until Fish adds native API support.
//! See: https://github.com/fish-shell/fish-shell/issues/2186

use atuin_client::database::Database;
use atuin_client::history::History;
use atuin_client::settings::Settings;
use eyre::{Context, Result};
use fs_err as fs;
use std::fs::OpenOptions;
use std::io::Write;
use tracing::{info, warn};

/// Format a history entry for Fish's history file format
///
/// Fish history format:
/// ```text
/// - cmd:git status
///   when:1737097200
/// ```
fn format_fish_entry(history: &History) -> String {
    // Escape backslashes and newlines in the command
    let escaped_cmd = history.command.replace('\\', "\\\\").replace('\n', "\\n");
    let timestamp = history.timestamp.unix_timestamp();

    format!("- cmd:{}\n  when:{}\n", escaped_cmd, timestamp)
}

/// Sync a history entry to Fish's history file
pub fn sync_entry(history: &History, settings: &Settings) -> Result<()> {
    let fish_history_path = &settings.fish_sync.history_path;

    // Format the entry
    let entry = format_fish_entry(history);

    // Append to the file (let Fish handle directory creation)
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(fish_history_path)
        .context("failed to open fish history file")?;

    file.write_all(entry.as_bytes())
        .context("failed to write to fish history file")?;

    file.flush().context("failed to flush fish history file")?;

    Ok(())
}

/// Bootstrap Fish history with recent entries from Atuin
///
/// This should be called when the daemon first starts up to populate
/// Fish's history with the most recent Atuin entries.
///
/// Simple implementation: fetch recent entries and append to Fish history file.
/// New Fish sessions will automatically pick up all entries.
pub async fn bootstrap_fish_history(
    settings: &Settings,
    history_db: &atuin_client::database::Sqlite,
) -> Result<()> {
    if !settings.fish_sync.enabled {
        return Ok(());
    }

    info!("bootstrapping fish history with recent atuin entries");

    let fish_history_path = &settings.fish_sync.history_path;

    // Fetch recent entries from Atuin database
    let filters = &[];
    let context = &atuin_client::database::current_context();
    let max = if settings.fish_sync.max_entries == 0 {
        None
    } else {
        Some(settings.fish_sync.max_entries)
    };

    let entries = history_db
        .list(filters, context, max, false, false)
        .await
        .context("failed to fetch history from database")?;

    if entries.is_empty() {
        info!("no entries to bootstrap to fish history");
        return Ok(());
    }

    info!(
        count = entries.len(),
        "bootstrapping fish history with entries"
    );

    // Check if file exists and is not empty to avoid duplicates on restart
    let need_bootstrap = if fs::metadata(fish_history_path).is_ok() {
        let content = fs::read_to_string(fish_history_path)?;
        content.trim().is_empty()
    } else {
        true
    };

    if !need_bootstrap {
        info!("fish history file already populated, skipping bootstrap");
        return Ok(());
    }

    // Sync the entries
    let mut synced = 0;
    for entry in &entries {
        if let Err(e) = sync_entry(entry, settings) {
            warn!(
                id = entry.id.0.as_str(),
                error = %e,
                "failed to bootstrap entry to fish"
            );
        } else {
            synced += 1;
        }
    }

    info!(
        count = synced,
        "bootstrapped fish history with atuin entries"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use atuin_client::settings::FishSync;
    use std::path::PathBuf;
    use time::OffsetDateTime;

    fn create_test_settings(fish_path: &PathBuf) -> atuin_client::settings::Settings {
        let mut settings = atuin_client::settings::Settings::default();
        settings.fish_sync = FishSync {
            enabled: true,
            history_path: fish_path.to_string_lossy().to_string(),
            max_entries: 10000,
        };
        settings
    }

    fn create_test_history() -> History {
        History {
            id: "00000000-0000-0000-0000-000000000001".to_string().into(),
            timestamp: OffsetDateTime::UNIX_EPOCH,
            duration: 100,
            exit: 0,
            command: "git status".to_string(),
            cwd: "/home/user".to_string(),
            session: "test-session".to_string(),
            hostname: "localhost".to_string(),
            deleted_at: None,
        }
    }

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
    }

    #[test]
    fn test_format_fish_entry_with_newlines() {
        let history = History {
            id: "00000000-0000-0000-0000-000000000003".to_string().into(),
            timestamp: OffsetDateTime::UNIX_EPOCH,
            duration: 0,
            exit: 0,
            command: "echo 'line1\nline2'".to_string(),
            cwd: "/home/user".to_string(),
            session: "test".to_string(),
            hostname: "localhost".to_string(),
            deleted_at: None,
        };

        let formatted = format_fish_entry(&history);
        assert!(formatted.contains("echo 'line1\\nline2'"));
    }

    #[test]
    fn test_sync_entry_creates_file_if_not_exists() {
        let temp_dir = tempfile::tempdir().unwrap();
        let fish_path = temp_dir.path().join("fish_history");
        let settings = create_test_settings(&fish_path);
        let history = create_test_history();

        sync_entry(&history, &settings).unwrap();

        assert!(fish_path.exists());
        let content = fs::read_to_string(&fish_path).unwrap();
        assert!(content.contains(&history.command));
    }
}
