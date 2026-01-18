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
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

/// Cached check for Fish shell installation
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
pub fn get_synced_uuids(path: &str) -> Result<HashSet<String>> {
    let path = Path::new(path);
    if !path.exists() {
        return Ok(HashSet::new());
    }

    let content = fs_err::read_to_string(path)
        .context("failed to read fish history file")?;

    // Extract UUIDs from comments (format: # atuin-uuid:...)
    let uuids: HashSet<String> = content
        .lines()
        .filter(|line| line.starts_with("  # atuin-uuid:"))
        .map(|line| line.trim_start_matches("  # atuin-uuid:").to_string())
        .collect();

    log::debug!("found {} synced uuids in fish history", uuids.len());

    Ok(uuids)
}

/// Check if an entry (by command+timestamp) already exists in Fish history
///
/// This handles the case where Fish itself writes entries without UUID comments.
/// Fish writes entries with format like: "- cmd: command\n  when:123"
/// (with optional spaces after "cmd:" and "when:")
fn entry_exists_in_fish_history(path: &str, command: &str, timestamp: i64) -> Result<bool> {
    let path = Path::new(path);
    if !path.exists() {
        return Ok(false);
    }

    let content = fs_err::read_to_string(path)
        .context("failed to read fish history file")?;

    // Normalize the command for comparison (Fish may add spaces)
    // We need to check for both formats:
    // "- cmd:command" and "- cmd: command" (with space)
    let cmd_pattern1 = format!("- cmd:{}", command);
    let cmd_pattern2 = format!("- cmd: {}", command);
    let timestamp_str = timestamp.to_string();

    // Parse entries and check for match
    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim();

        // Check if this is an entry start (begins with "- cmd:")
        if line.starts_with("- cmd:") {
            // Extract command from this line
            let entry_cmd = if line.len() > 6 {
                line[6..].trim().to_string()
            } else {
                String::new()
            };

            // Check next line for timestamp
            if i + 1 < lines.len() {
                let when_line = lines[i + 1].trim();
                if when_line.starts_with("when:") {
                    let entry_timestamp = when_line[5..].trim();

                    // Check if both command and timestamp match
                    // (handling both Fish and Atuin formats)
                    if (entry_cmd == command || line[6..].trim_start() == command)
                        && entry_timestamp == timestamp_str
                    {
                        return Ok(true);
                    }
                }
            }
        }
        i += 1;
    }

    Ok(false)
}

/// Trim the Fish history file to keep only the most recent N entries
pub fn trim_fish_history(path: &str, max_entries: usize) -> Result<()> {
    if max_entries == 0 {
        return Ok(()); // 0 means no limit
    }

    let path = Path::new(path);
    if !path.exists() {
        return Ok(());
    }

    let content = fs_err::read_to_string(path)
        .context("failed to read fish history file")?;

    // Parse entries
    let entries: Vec<&str> = content.split("- cmd:").skip(1).collect();

    if entries.len() <= max_entries {
        return Ok(());
    }

    log::info!(
        "trimming fish history file from {} to {} entries",
        entries.len(),
        max_entries
    );

    // Keep only the most recent entries
    let to_keep = &entries[entries.len() - max_entries..];

    // Rebuild the file
    let mut trimmed = String::new();
    for entry in to_keep {
        trimmed.push_str("- cmd:");
        trimmed.push_str(entry);
    }

    fs_err::write(path, trimmed).context("failed to write trimmed fish history file")?;

    Ok(())
}

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
    let uuid = &history.id.0;

    // Add UUID as a comment for deduplication
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
        log::debug!("fish shell not installed, skipping sync");
        return Ok(());
    }

    let fish_history_path = shellexpand::tilde(&settings.fish_sync.history_path);

    // Ensure parent directory exists
    if let Some(parent) = Path::new(fish_history_path.as_ref()).parent() {
        if !parent.exists() {
            fs_err::create_dir_all(parent).context("failed to create fish history directory")?;
        }
    }

    // Check if this entry is already synced (UUID deduplication)
    let uuid_str = history.id.0.as_str();
    if Path::new(fish_history_path.as_ref()).exists() {
        let synced_uuids = get_synced_uuids(fish_history_path.as_ref())?;
        if synced_uuids.contains(uuid_str) {
            log::debug!("entry {} already synced (UUID found), skipping", uuid_str);
            return Ok(());
        }

        // Also check if entry exists by command+timestamp (for entries written by Fish)
        let timestamp = history.timestamp.unix_timestamp();
        if entry_exists_in_fish_history(fish_history_path.as_ref(), &history.command, timestamp)?
        {
            log::debug!(
                "entry '{}' @ {} already exists in fish history (no UUID), skipping",
                history.command,
                timestamp
            );
            return Ok(());
        }
    }

    // Format the entry
    let entry = format_fish_entry(history);

    // Open file and acquire exclusive lock to prevent concurrent write corruption
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(fish_history_path.as_ref())
        .context("failed to open fish history file")?;

    file.lock_exclusive()
        .context("failed to acquire lock on fish history file")?;

    file.write_all(entry.as_bytes())
        .context("failed to write to fish history file")?;

    file.flush().context("failed to flush fish history file")?;

    // Lock is automatically released when file is dropped

    // Trim if needed
    trim_fish_history(
        fish_history_path.as_ref(),
        settings.fish_sync.max_entries,
    )?;

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

/// Sync all local Atuin history entries to Fish history file
///
/// Uses UUID-based deduplication to avoid syncing entries that are already
/// present in the Fish history file.
pub async fn sync_all_entries(
    settings: &Settings,
    history_db: &crate::database::Sqlite,
) -> Result<usize> {
    if !settings.fish_sync.enabled {
        return Ok(0);
    }

    if !is_fish_installed() {
        log::debug!("fish shell not installed, skipping sync");
        return Ok(0);
    }

    let fish_history_path = shellexpand::tilde(&settings.fish_sync.history_path);

    // Get already synced UUIDs from Fish history metadata
    let synced_uuids = get_synced_uuids(fish_history_path.as_ref())?;

    // Fetch recent entries from Atuin database (limit by max_entries)
    let host_id = Settings::host_id()
        .map(|h| h.0.to_string())
        .unwrap_or_default();
    let context = crate::database::Context {
        cwd: "/".to_string(),
        hostname: host_id.clone(),
        host_id,
        session: "fish_sync_all".to_string(),
        git_root: None,
    };

    let filters = &[];
    let entries = history_db
        .list(filters, &context, Some(settings.fish_sync.max_entries), false, false)
        .await?;

    // Filter out entries that have already been synced (by UUID)
    let new_entries: Vec<_> = entries
        .into_iter()
        .filter(|entry| !synced_uuids.contains(entry.id.0.as_str()))
        .collect();

    if new_entries.is_empty() {
        log::info!("no new entries to sync to fish history");
        return Ok(0);
    }

    log::info!(
        "syncing {} new entries to fish history ({} already synced)",
        new_entries.len(),
        synced_uuids.len()
    );

    let mut synced = 0;
    for entry in &new_entries {
        if let Err(e) = sync_entry(entry, settings) {
            log::warn!(
                "id={}, error={}: failed to sync entry to fish",
                entry.id.0.as_str(),
                e
            );
        } else {
            synced += 1;
        }
    }

    log::info!("synced {}/{} new entries to fish history", synced, new_entries.len());
    Ok(synced)
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
            sync_all_on_cli: false,
            sync_all_on_daemon: false,
            sync_on_startup: false,
            max_entries: 10000,
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
        assert!(formatted.contains("  # atuin-uuid:"));
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

        // Create different history entries for each thread with unique UUIDs
        let histories: Vec<_> = (0..10)
            .map(|i| {
                let mut h = create_test_history();
                h.id = format!("{:032}", i).into(); // Unique UUID for each entry
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
            30,
            "Expected 30 lines (10 entries × 3 lines each)"
        );

        // Verify all commands are present and not interleaved
        for i in 0..10 {
            assert!(content.contains(&format!("test command {}", i)));
        }
    }
}
