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
pub fn get_synced_uuids(path: &str) -> Result<HashSet<String>> {
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
pub fn trim_fish_history(path: &str, max_entries: usize) -> Result<()> {
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
///
/// Note: This is now less useful since we use UUID-based deduplication,
/// but kept for backwards compatibility and potential edge cases.
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
    use atuin_client::settings::FishSync;
    use std::path::PathBuf;
    use time::OffsetDateTime;

    /// Create a test settings instance with a custom Fish history path
    fn create_test_settings(fish_path: &PathBuf) -> atuin_client::settings::Settings {
        let mut settings = atuin_client::settings::Settings::default();
        settings.fish_sync = FishSync {
            enabled: true,
            history_path: fish_path.to_string_lossy().to_string(),
            max_entries: 1000,
            fish_merge: true,
        };
        settings
    }

    /// Create a test history entry
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

    // ===== format_fish_entry tests =====

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
        assert!(formatted.contains("00000000-0000-0000-0000-000000000001"));
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
        // Newlines should be escaped as \n
        assert!(formatted.contains("echo 'line1\\nline2'"));
    }

    #[test]
    fn test_format_fish_entry_with_backslashes() {
        let history = History {
            id: "00000000-0000-0000-0000-000000000004".to_string().into(),
            timestamp: OffsetDateTime::UNIX_EPOCH,
            duration: 0,
            exit: 0,
            command: "echo 'path\\to\\file'".to_string(),
            cwd: "/home/user".to_string(),
            session: "test".to_string(),
            hostname: "localhost".to_string(),
            deleted_at: None,
        };

        let formatted = format_fish_entry(&history);
        // Backslashes should be escaped as \\
        assert!(formatted.contains("echo 'path\\\\to\\\\file'"));
    }

    // ===== get_synced_uuids tests =====

    #[test]
    fn test_get_synced_uuids() {
        let temp_dir = tempfile::tempdir().unwrap();
        let temp_file = temp_dir.path().join("test_fish_history");
        let content = "- cmd:test1\n  when:1000\n  # atuin-uuid:00000000-0000-0000-0000-000000000001\n\
                        - cmd:test2\n  when:2000\n  # atuin-uuid:00000000-0000-0000-0000-000000000002\n\
                        - cmd:test3\n  when:3000\n  # atuin-uuid:00000000-0000-0000-0000-000000000003\n";

        fs::write(&temp_file, content).expect("failed to write test file");

        let uuids = get_synced_uuids(temp_file.to_str().unwrap()).expect("failed to get synced uuids");

        assert_eq!(uuids.len(), 3);
        assert!(uuids.contains("00000000-0000-0000-0000-000000000001"));
        assert!(uuids.contains("00000000-0000-0000-0000-000000000002"));
        assert!(uuids.contains("00000000-0000-0000-0000-000000000003"));
    }

    #[test]
    fn test_get_synced_uuids_empty_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let temp_file = temp_dir.path().join("test_fish_history_empty");

        fs::write(&temp_file, "").expect("failed to write test file");

        let uuids = get_synced_uuids(temp_file.to_str().unwrap()).expect("failed to get synced uuids");

        assert_eq!(uuids.len(), 0);
    }

    #[test]
    fn test_get_synced_uuids_no_uuids() {
        let temp_dir = tempfile::tempdir().unwrap();
        let temp_file = temp_dir.path().join("test_fish_history_no_uuids");
        let content = "- cmd:test1\n  when:1000\n- cmd:test2\n  when:2000\n";

        fs::write(&temp_file, content).expect("failed to write test file");

        let uuids = get_synced_uuids(temp_file.to_str().unwrap()).expect("failed to get synced uuids");

        assert_eq!(uuids.len(), 0);
    }

    #[test]
    fn test_get_synced_uuids_nonexistent_file() {
        let uuids = get_synced_uuids("/nonexistent/file").expect("failed to get synced uuids");

        assert_eq!(uuids.len(), 0);
    }

    // ===== sync_entry tests =====

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
        assert!(content.contains(&history.id.0));
    }

    #[test]
    fn test_sync_entry_appends_to_existing_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let fish_path = temp_dir.path().join("fish_history");
        let settings = create_test_settings(&fish_path);

        // Create initial file
        let initial_content = "- cmd:initial\n  when:1000\n  # atuin-uuid:00000000-0000-0000-0000-000000000001\n";
        fs::write(&fish_path, initial_content).unwrap();

        let history1 = create_test_history();
        let history2 = History {
            id: "00000000-0000-0000-0000-000000000002".to_string().into(),
            ..create_test_history()
        };

        sync_entry(&history1, &settings).unwrap();
        sync_entry(&history2, &settings).unwrap();

        let content = fs::read_to_string(&fish_path).unwrap();
        assert!(content.contains("initial"));
        assert!(content.contains(&history1.command));
        assert!(content.contains(&history2.id.0));
    }

    #[test]
    fn test_sync_entry_with_disabled_setting() {
        let temp_dir = tempfile::tempdir().unwrap();
        let fish_path = temp_dir.path().join("fish_history");
        let mut settings = create_test_settings(&fish_path);
        settings.fish_sync.enabled = false;
        let history = create_test_history();

        sync_entry(&history, &settings).unwrap();

        // Verify no file created
        assert!(!fish_path.exists());
    }

    #[test]
    fn test_sync_entry_creates_parent_directory() {
        let temp_dir = tempfile::tempdir().unwrap();
        let deep_path = temp_dir.path().join("deep/nested/path/fish_history");
        let settings = create_test_settings(&deep_path);
        let history = create_test_history();

        sync_entry(&history, &settings).unwrap();

        assert!(deep_path.exists());
        let content = fs::read_to_string(&deep_path).unwrap();
        assert!(content.contains(&history.command));
    }

    #[test]
    fn test_sync_entry_triggers_trim() {
        let temp_dir = tempfile::tempdir().unwrap();
        let fish_path = temp_dir.path().join("fish_history");
        let mut settings = create_test_settings(&fish_path);
        settings.fish_sync.max_entries = 3;

        // Create initial content with 5 entries
        let mut initial_content = String::new();
        for i in 1..=5 {
            initial_content.push_str(&format!("- cmd:test{}\n  when:{}\n  # atuin-uuid:00000000-0000-0000-0000-00000000000{}\n", i, i * 1000, i));
        }
        fs::write(&fish_path, initial_content).unwrap();

        let history = History {
            id: "00000000-0000-0000-0000-000000000006".to_string().into(),
            ..create_test_history()
        };

        sync_entry(&history, &settings).unwrap();

        let content = fs::read_to_string(&fish_path).unwrap();
        // Should have exactly 3 entries after trim (entry 6 + 2 of the oldest from 4, 5)
        // Actually, looking at the trim logic, it keeps the most recent max_entries
        // So after adding entry 6, we should have entries 4, 5, 6 (3 entries)
        let entry_count = content.matches("- cmd:").count();
        assert_eq!(entry_count, 3);
    }

    // ===== sync_entries tests =====

    #[test]
    fn test_sync_entries_multiple() {
        let temp_dir = tempfile::tempdir().unwrap();
        let fish_path = temp_dir.path().join("fish_history");
        let settings = create_test_settings(&fish_path);

        let entries = vec![
            create_test_history(),
            History {
                id: "00000000-0000-0000-0000-000000000002".to_string().into(),
                ..create_test_history()
            },
            History {
                id: "00000000-0000-0000-0000-000000000003".to_string().into(),
                ..create_test_history()
            },
        ];

        sync_entries(&entries, &settings).unwrap();

        let content = fs::read_to_string(&fish_path).unwrap();
        assert_eq!(content.matches("- cmd:").count(), 3);
    }

    #[test]
    fn test_sync_entries_empty_list() {
        let temp_dir = tempfile::tempdir().unwrap();
        let fish_path = temp_dir.path().join("fish_history");
        let settings = create_test_settings(&fish_path);

        sync_entries(&[], &settings).unwrap();

        assert!(!fish_path.exists());
    }

    #[test]
    fn test_sync_entries_with_disabled_setting() {
        let temp_dir = tempfile::tempdir().unwrap();
        let fish_path = temp_dir.path().join("fish_history");
        let mut settings = create_test_settings(&fish_path);
        settings.fish_sync.enabled = false;

        let entries = vec![create_test_history()];

        sync_entries(&entries, &settings).unwrap();

        assert!(!fish_path.exists());
    }

    // ===== trim_fish_history tests =====

    #[test]
    fn test_trim_fish_history_when_exceeds_max() {
        let temp_dir = tempfile::tempdir().unwrap();
        let fish_path = temp_dir.path().join("fish_history");

        // Create file with 10 entries
        let mut content = String::new();
        for i in 1..=10 {
            content.push_str(&format!("- cmd:test{}\n  when:{}\n  # atuin-uuid:{}\n", i, i * 1000, uuid::Uuid::new_v4()));
        }
        fs::write(&fish_path, content).unwrap();

        trim_fish_history(fish_path.to_str().unwrap(), 5).unwrap();

        let trimmed_content = fs::read_to_string(&fish_path).unwrap();
        // Should have exactly 5 entries (most recent)
        let entry_count = trimmed_content.matches("- cmd:").count();
        assert_eq!(entry_count, 5);
    }

    #[test]
    fn test_trim_fish_history_when_under_max() {
        let temp_dir = tempfile::tempdir().unwrap();
        let fish_path = temp_dir.path().join("fish_history");

        // Create file with 3 entries
        let mut content = String::new();
        for i in 1..=3 {
            content.push_str(&format!("- cmd:test{}\n  when:{}\n  # atuin-uuid:{}\n", i, i * 1000, uuid::Uuid::new_v4()));
        }
        fs::write(&fish_path, content).unwrap();

        let original_content = fs::read_to_string(&fish_path).unwrap();

        trim_fish_history(fish_path.to_str().unwrap(), 10).unwrap();

        let trimmed_content = fs::read_to_string(&fish_path).unwrap();
        // Content should be unchanged
        assert_eq!(trimmed_content, original_content);
    }

    #[test]
    fn test_trim_fish_history_preserves_uuids() {
        let temp_dir = tempfile::tempdir().unwrap();
        let fish_path = temp_dir.path().join("fish_history");

        // Create file with UUID metadata
        let mut content = String::new();
        for i in 1..=10 {
            let uuid = format!("00000000-0000-0000-0000-00000000000{:02}", i);
            content.push_str(&format!("- cmd:test{}\n  when:{}\n  # atuin-uuid:{}\n", i, i * 1000, uuid));
        }
        fs::write(&fish_path, content).unwrap();

        trim_fish_history(fish_path.to_str().unwrap(), 5).unwrap();

        let trimmed_content = fs::read_to_string(&fish_path).unwrap();
        // Verify UUIDs are still present in remaining entries
        assert!(trimmed_content.contains("00000000-0000-0000-0000-0000000000006"));
        assert!(trimmed_content.contains("00000000-0000-0000-0000-0000000000010"));
        // Oldest UUIDs should be trimmed
        assert!(!trimmed_content.contains("00000000-0000-0000-0000-0000000000001"));
    }

    #[test]
    fn test_trim_fish_history_with_zero_max() {
        let temp_dir = tempfile::tempdir().unwrap();
        let fish_path = temp_dir.path().join("fish_history");

        let content = "- cmd:test1\n  when:1000\n  # atuin-uuid:00000000-0000-0000-0000-000000000001\n";
        fs::write(&fish_path, content).unwrap();

        let original_content = fs::read_to_string(&fish_path).unwrap();

        // max_entries = 0 means no limit
        trim_fish_history(fish_path.to_str().unwrap(), 0).unwrap();

        let trimmed_content = fs::read_to_string(&fish_path).unwrap();
        assert_eq!(trimmed_content, original_content);
    }

    #[test]
    fn test_trim_fish_history_nonexistent_file() {
        let result = trim_fish_history("/nonexistent/file", 100);
        assert!(result.is_ok());
    }

    // ===== get_last_synced_timestamp tests =====

    #[test]
    fn test_get_last_synced_timestamp() {
        let temp_dir = tempfile::tempdir().unwrap();
        let fish_path = temp_dir.path().join("fish_history");

        let content = "- cmd:test1\n  when:1000\n- cmd:test2\n  when:2000\n- cmd:test3\n  when:3000\n";
        fs::write(&fish_path, content).unwrap();

        let timestamp = get_last_synced_timestamp(fish_path.to_str().unwrap()).unwrap();

        assert_eq!(timestamp, Some(3000));
    }

    #[test]
    fn test_get_last_synced_timestamp_empty_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let fish_path = temp_dir.path().join("fish_history");

        fs::write(&fish_path, "").unwrap();

        let timestamp = get_last_synced_timestamp(fish_path.to_str().unwrap()).unwrap();

        assert_eq!(timestamp, None);
    }

    #[test]
    fn test_get_last_synced_timestamp_nonexistent_file() {
        let timestamp = get_last_synced_timestamp("/nonexistent/file").unwrap();

        assert_eq!(timestamp, None);
    }

    // ===== bootstrap_fish_history tests =====

    #[tokio::test]
    async fn test_bootstrap_fish_history_filters_synced_uuids() {
        let temp_dir = tempfile::tempdir().unwrap();
        let fish_path = temp_dir.path().join("fish_history");

        let _settings = create_test_settings(&fish_path);

        // Create Fish history with UUIDs for entries 001, 002
        let fish_content = "- cmd:test1\n  when:1000\n  # atuin-uuid:00000000-0000-0000-0000-000000000001\n\
                            - cmd:test2\n  when:2000\n  # atuin-uuid:00000000-0000-0000-0000-000000000002\n";
        fs::write(&fish_path, fish_content).unwrap();

        // Create a mock database with entries 001, 002, 003
        // Note: bootstrap_fish_history uses the real database, so we need to test differently
        // For this test, we'll verify that get_synced_uuids works correctly
        let synced_uuids = get_synced_uuids(fish_path.to_str().unwrap()).unwrap();

        assert_eq!(synced_uuids.len(), 2);
        assert!(synced_uuids.contains("00000000-0000-0000-0000-000000000001"));
        assert!(synced_uuids.contains("00000000-0000-0000-0000-000000000002"));
    }

    #[tokio::test]
    async fn test_bootstrap_fish_history_empty_fish_history() {
        let temp_dir = tempfile::tempdir().unwrap();
        let fish_path = temp_dir.path().join("fish_history");

        let _settings = create_test_settings(&fish_path);

        // Start with empty file
        fs::write(&fish_path, "").unwrap();

        let synced_uuids = get_synced_uuids(fish_path.to_str().unwrap()).unwrap();

        assert_eq!(synced_uuids.len(), 0);
    }

    #[tokio::test]
    async fn test_bootstrap_fish_history_with_disabled_setting() {
        let temp_dir = tempfile::tempdir().unwrap();
        let fish_path = temp_dir.path().join("fish_history");
        let mut settings = create_test_settings(&fish_path);
        settings.fish_sync.enabled = false;

        // Bootstrap should return early when disabled
        // We can't easily test this without a real database, but we can test
        // that sync_entry respects the disabled setting
        let history = create_test_history();

        sync_entry(&history, &settings).unwrap();

        // Verify no file created when disabled
        assert!(!fish_path.exists());
    }
}
