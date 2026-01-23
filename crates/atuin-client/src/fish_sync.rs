//! Fish history sync module
//!
//! This module handles syncing remote Atuin history entries to Fish shell's history file,
//! enabling Fish's autosuggestions (ghost text) to work with commands from other machines.
//!
//! **Note:** This is a temporary workaround until Fish adds native API support.
//! See: https://github.com/fish-shell/fish-shell/issues/2186

use crate::database::Database;
use crate::history::History;
use crate::settings::Settings;
use atuin_common::record::RecordId;
use eyre::{Context, Result};
use fs2::FileExt;
use std::fs::OpenOptions;
use std::io::Write;

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

    // Open file and acquire exclusive lock to prevent concurrent write corruption
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(fish_history_path)
        .context("failed to open fish history file")?;

    file.lock_exclusive()
        .context("failed to acquire lock on fish history file")?;

    file.write_all(entry.as_bytes())
        .context("failed to write to fish history file")?;

    file.flush().context("failed to flush fish history file")?;

    // Lock is automatically released when file is dropped

    Ok(())
}

/// Sync downloaded remote entries to Fish history file
///
/// This should be called after sync with the server completes.
/// Only writes entries that were downloaded from the server (not local commands).
pub async fn sync_downloaded_entries(
    settings: &Settings,
    history_db: &crate::database::Sqlite,
    downloaded_ids: &[RecordId],
) -> Result<()> {
    if !settings.fish_sync.enabled || downloaded_ids.is_empty() {
        return Ok(());
    }

    // Fetch each entry by ID (database stores ULID as text without hyphens)
    let mut synced = 0;
    for record_id in downloaded_ids {
        // ULID is stored as 32-character text without hyphens (UUID format)
        // The database column is TEXT type, so we need to convert Uuid to simple format
        let id_str = record_id.0.simple().to_string();
        if let Ok(Some(entry)) = history_db.load(&id_str).await {
            if let Err(e) = sync_entry(&entry, settings) {
                log::warn!(
                    "id={}, error={}: failed to sync entry to fish",
                    entry.id.0.as_str(),
                    e
                );
            } else {
                synced += 1;
                log::info!("synced {} (:hostname: {})", entry.command, entry.hostname);
            }
        }
    }

    log::info!(
        "synced {}/{} remote entries to fish history",
        synced,
        downloaded_ids.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::FishSync;
    use std::path::PathBuf;
    use time::OffsetDateTime;

    fn create_test_settings(fish_path: &PathBuf) -> Settings {
        let mut settings = Settings::default();
        settings.fish_sync = FishSync {
            enabled: true,
            history_path: fish_path.to_string_lossy().to_string(),
        };
        settings
    }

    fn create_test_history() -> History {
        History {
            id: "00000000-0000-0000-000000000000001".to_string().into(),
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
            id: "00000000-0000-0000-000000000000001".to_string().into(),
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
    fn test_sync_entry_creates_file_if_not_exists() {
        let temp_dir = tempfile::tempdir().unwrap();
        let fish_path = temp_dir.path().join("fish_history");
        let settings = create_test_settings(&fish_path);
        let history = create_test_history();

        sync_entry(&history, &settings).unwrap();

        assert!(fish_path.exists());
        let content = fs_err::read_to_string(&fish_path).unwrap();
        assert!(content.contains(&history.command));
    }

    #[test]
    fn test_concurrent_write_safety() {
        use std::sync::Arc;
        use std::thread;

        let temp_dir = tempfile::tempdir().unwrap();
        let fish_path = Arc::new(temp_dir.path().join("fish_history"));
        let settings = Arc::new(create_test_settings(fish_path.as_ref()));

        // Create different history entries for each thread
        let histories: Vec<_> = (0..10)
            .map(|i| {
                let mut h = create_test_history();
                h.command = format!("test command {}", i);
                Arc::new(h)
            })
            .collect();

        // Spawn multiple threads writing to the same file
        let handles: Vec<_> = histories
            .into_iter()
            .map(|history| {
                let settings = settings.clone();
                let fish_path = fish_path.clone();
                thread::spawn(move || {
                    sync_entry(&history, &settings)?;
                    // Double-check: try to read what we wrote
                    let content = fs_err::read_to_string(&*fish_path)?;
                    eyre::ensure!(
                        content.contains(&history.command),
                        "Command not found in file"
                    );
                    Ok::<(), eyre::Report>(())
                })
            })
            .collect();

        // All should succeed without deadlocking or corrupting data
        for handle in handles {
            handle.join().unwrap().unwrap();
        }

        // File should have exactly 10 entries (no corruption)
        let content = fs_err::read_to_string(&*fish_path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(
            lines.len(),
            20,
            "Expected 20 lines (10 entries × 2 lines each)"
        );

        // Verify all commands are present and not interleaved
        for i in 0..10 {
            assert!(content.contains(&format!("test command {}", i)));
        }
    }

    #[test]
    fn test_format_fish_entry_with_newlines() {
        let history = History {
            id: "00000000-0000-0000-000000000000001".to_string().into(),
            timestamp: OffsetDateTime::UNIX_EPOCH,
            duration: 0,
            exit: 0,
            command: "echo \"line1\nline2\nline3\"".to_string(),
            cwd: "/home/user".to_string(),
            session: "test".to_string(),
            hostname: "localhost".to_string(),
            deleted_at: None,
        };

        let formatted = format_fish_entry(&history);
        // Newlines should be escaped as \n (backslash followed by n)
        assert!(formatted.contains(r#"echo "line1\nline2\nline3""#));
    }

    #[test]
    fn test_format_fish_entry_with_backslashes() {
        let history = History {
            id: "00000000-0000-0000-000000000000001".to_string().into(),
            timestamp: OffsetDateTime::UNIX_EPOCH,
            duration: 0,
            exit: 0,
            command: r"echo C:\Users\test".to_string(),
            cwd: "/home/user".to_string(),
            session: "test".to_string(),
            hostname: "localhost".to_string(),
            deleted_at: None,
        };

        let formatted = format_fish_entry(&history);
        // Backslashes should be escaped as \\
        assert!(formatted.contains(r"echo C:\\Users\\test"));
    }

    #[test]
    fn test_format_fish_entry_with_tabs() {
        let history = History {
            id: "00000000-0000-0000-000000000000001".to_string().into(),
            timestamp: OffsetDateTime::UNIX_EPOCH,
            duration: 0,
            exit: 0,
            command: "echo\thello\tworld".to_string(),
            cwd: "/home/user".to_string(),
            session: "test".to_string(),
            hostname: "localhost".to_string(),
            deleted_at: None,
        };

        let formatted = format_fish_entry(&history);
        // Tabs should be preserved
        assert!(formatted.contains("echo\thello\tworld"));
    }

    #[test]
    fn test_format_fish_entry_with_unicode() {
        let history = History {
            id: "00000000-0000-0000-000000000000001".to_string().into(),
            timestamp: OffsetDateTime::UNIX_EPOCH,
            duration: 0,
            exit: 0,
            command: "echo 'Hello 世界 🌍'".to_string(),
            cwd: "/home/user".to_string(),
            session: "test".to_string(),
            hostname: "localhost".to_string(),
            deleted_at: None,
        };

        let formatted = format_fish_entry(&history);
        // Unicode should be preserved
        assert!(formatted.contains("Hello 世界 🌍"));
    }

    #[test]
    fn test_format_fish_entry_empty_command() {
        let history = History {
            id: "00000000-0000-0000-000000000000001".to_string().into(),
            timestamp: OffsetDateTime::UNIX_EPOCH,
            duration: 0,
            exit: 0,
            command: "".to_string(),
            cwd: "/home/user".to_string(),
            session: "test".to_string(),
            hostname: "localhost".to_string(),
            deleted_at: None,
        };

        let formatted = format_fish_entry(&history);
        assert!(formatted.contains("- cmd:"));
    }

    #[test]
    fn test_format_fish_entry_very_long_command() {
        let long_command = "a".repeat(2000);
        let history = History {
            id: "00000000-0000-0000-000000000000001".to_string().into(),
            timestamp: OffsetDateTime::UNIX_EPOCH,
            duration: 0,
            exit: 0,
            command: long_command.clone(),
            cwd: "/home/user".to_string(),
            session: "test".to_string(),
            hostname: "localhost".to_string(),
            deleted_at: None,
        };

        let formatted = format_fish_entry(&history);
        assert!(formatted.contains(&long_command[..100]));
    }

    #[test]
    fn test_format_fish_entry_special_characters() {
        let history = History {
            id: "00000000-0000-0000-000000000000001".to_string().into(),
            timestamp: OffsetDateTime::UNIX_EPOCH,
            duration: 0,
            exit: 0,
            command: r#"echo 'test with "quotes" and `backticks` and $dollar'"#.to_string(),
            cwd: "/home/user".to_string(),
            session: "test".to_string(),
            hostname: "localhost".to_string(),
            deleted_at: None,
        };

        let formatted = format_fish_entry(&history);
        assert!(formatted.contains(r#"echo 'test with "quotes" and `backticks` and $dollar'"#));
    }
}
