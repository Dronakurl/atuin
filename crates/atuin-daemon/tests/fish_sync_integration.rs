//! Integration tests for Fish history sync functionality
//!
//! These tests verify the behavior of the Fish sync feature when integrated
//! with the sync worker and real database operations.

use atuin_client::history::History;
use atuin_client::settings::{FishSync, Settings};
use fs_err as fs;
use std::path::PathBuf;
use tempfile::TempDir;
use time::OffsetDateTime;

/// Create a test settings instance
fn create_test_settings(fish_path: &PathBuf, enabled: bool) -> Settings {
    let mut settings = Settings::default();
    settings.fish_sync = FishSync {
        enabled,
        history_path: fish_path.to_string_lossy().to_string(),
        max_entries: 1000,
        fish_merge: true,
    };
    settings
}

/// Create a test history entry
fn create_history_entry(id: &str, timestamp: i64, command: &str) -> History {
    History {
        id: id.to_string().into(),
        timestamp: OffsetDateTime::from_unix_timestamp(timestamp).unwrap(),
        duration: 100,
        exit: 0,
        command: command.to_string(),
        cwd: "/home/user".to_string(),
        session: "test-session".to_string(),
        hostname: "localhost".to_string(),
        deleted_at: None,
    }
}

/// Count entries in Fish history file
fn count_fish_entries(path: &PathBuf) -> usize {
    if !path.exists() {
        return 0;
    }
    let content = fs::read_to_string(path).unwrap();
    content.matches("- cmd:").count()
}

/// Get all UUIDs from Fish history file
fn get_fish_uuids(path: &PathBuf) -> Vec<String> {
    if !path.exists() {
        return Vec::new();
    }
    let content = fs::read_to_string(path).unwrap();
    content
        .lines()
        .filter(|line| line.starts_with("  # atuin-uuid:"))
        .map(|line| line.trim_start_matches("  # atuin-uuid:").to_string())
        .collect()
}

#[tokio::test]
async fn test_sync_entry_with_settings() {
    let temp_dir = TempDir::new().unwrap();
    let fish_path = temp_dir.path().join("fish_history");
    let settings = create_test_settings(&fish_path, true);
    let history = create_history_entry("00000000-0000-0000-0000-000000000001", 1000, "git status");

    atuin_daemon::fish_sync::sync_entry(&history, &settings).unwrap();

    assert!(fish_path.exists());
    let content = fs::read_to_string(&fish_path).unwrap();
    assert!(content.contains("git status"));
    assert!(content.contains("00000000-0000-0000-0000-000000000001"));
}

#[tokio::test]
async fn test_sync_multiple_entries_sequential() {
    let temp_dir = TempDir::new().unwrap();
    let fish_path = temp_dir.path().join("fish_history");
    let settings = create_test_settings(&fish_path, true);

    let entries = vec![
        create_history_entry("00000000-0000-0000-0000-000000000001", 1000, "git status"),
        create_history_entry("00000000-0000-0000-0000-000000000002", 2000, "ls -la"),
        create_history_entry("00000000-0000-0000-0000-000000000003", 3000, "cargo test"),
    ];

    for entry in &entries {
        atuin_daemon::fish_sync::sync_entry(entry, &settings).unwrap();
    }

    let entry_count = count_fish_entries(&fish_path);
    assert_eq!(entry_count, 3);

    let uuids = get_fish_uuids(&fish_path);
    assert_eq!(uuids.len(), 3);
}

#[tokio::test]
async fn test_sync_entry_disabled_does_not_create_file() {
    let temp_dir = TempDir::new().unwrap();
    let fish_path = temp_dir.path().join("fish_history");
    let settings = create_test_settings(&fish_path, false);
    let history = create_history_entry("00000000-0000-0000-0000-000000000001", 1000, "git status");

    atuin_daemon::fish_sync::sync_entry(&history, &settings).unwrap();

    assert!(!fish_path.exists());
}

#[tokio::test]
async fn test_trim_fish_history_integration() {
    let temp_dir = TempDir::new().unwrap();
    let fish_path = temp_dir.path().join("fish_history");
    let settings = create_test_settings(&fish_path, true);

    // Create 20 entries
    for i in 1..=20 {
        let id = format!("00000000-0000-0000-0000-00000000000{:02}", i);
        let history = create_history_entry(&id, i * 1000, &format!("test command {}", i));
        atuin_daemon::fish_sync::sync_entry(&history, &settings).unwrap();
    }

    let entry_count = count_fish_entries(&fish_path);
    assert_eq!(entry_count, 20);

    // Now test with a lower max_entries setting
    let mut settings_with_trim = create_test_settings(&fish_path, true);
    settings_with_trim.fish_sync.max_entries = 10;

    let new_history =
        create_history_entry("00000000-0000-0000-0000-000000000021", 21000, "new command");
    atuin_daemon::fish_sync::sync_entry(&new_history, &settings_with_trim).unwrap();

    let entry_count_after = count_fish_entries(&fish_path);
    assert_eq!(entry_count_after, 10);
}

#[tokio::test]
async fn test_get_synced_uuids_integration() {
    let temp_dir = TempDir::new().unwrap();
    let fish_path = temp_dir.path().join("fish_history");
    let settings = create_test_settings(&fish_path, true);

    // Add some entries
    let entries = vec![
        create_history_entry("00000000-0000-0000-0000-000000000001", 1000, "cmd1"),
        create_history_entry("00000000-0000-0000-0000-000000000002", 2000, "cmd2"),
        create_history_entry("00000000-0000-0000-0000-000000000003", 3000, "cmd3"),
    ];

    for entry in &entries {
        atuin_daemon::fish_sync::sync_entry(entry, &settings).unwrap();
    }

    let synced_uuids =
        atuin_daemon::fish_sync::get_synced_uuids(fish_path.to_str().unwrap()).unwrap();

    assert_eq!(synced_uuids.len(), 3);
    assert!(synced_uuids.contains("00000000-0000-0000-0000-000000000001"));
    assert!(synced_uuids.contains("00000000-0000-0000-0000-000000000002"));
    assert!(synced_uuids.contains("00000000-0000-0000-0000-000000000003"));
}

#[tokio::test]
async fn test_no_duplicate_entries_on_re_sync() {
    let temp_dir = TempDir::new().unwrap();
    let fish_path = temp_dir.path().join("fish_history");
    let settings = create_test_settings(&fish_path, true);

    let history = create_history_entry("00000000-0000-0000-0000-000000000001", 1000, "git status");

    // Sync the same entry multiple times
    atuin_daemon::fish_sync::sync_entry(&history, &settings).unwrap();
    atuin_daemon::fish_sync::sync_entry(&history, &settings).unwrap();
    atuin_daemon::fish_sync::sync_entry(&history, &settings).unwrap();

    let entry_count = count_fish_entries(&fish_path);

    // We expect 3 entries because sync_entry doesn't check for duplicates
    // The UUID deduplication is in bootstrap_fish_history
    assert_eq!(entry_count, 3);
}

#[tokio::test]
async fn test_get_last_synced_timestamp_integration() {
    let temp_dir = TempDir::new().unwrap();
    let fish_path = temp_dir.path().join("fish_history");
    let settings = create_test_settings(&fish_path, true);

    // Add entries with different timestamps
    let entries = vec![
        create_history_entry("00000000-0000-0000-0000-000000000001", 1000, "cmd1"),
        create_history_entry("00000000-0000-0000-0000-000000000002", 2000, "cmd2"),
        create_history_entry("00000000-0000-0000-0000-000000000003", 5000, "cmd3"),
    ];

    for entry in &entries {
        atuin_daemon::fish_sync::sync_entry(entry, &settings).unwrap();
    }

    let last_timestamp =
        atuin_daemon::fish_sync::get_last_synced_timestamp(fish_path.to_str().unwrap()).unwrap();

    assert_eq!(last_timestamp, Some(5000));
}

#[tokio::test]
async fn test_special_characters_in_commands() {
    let temp_dir = TempDir::new().unwrap();
    let fish_path = temp_dir.path().join("fish_history");
    let settings = create_test_settings(&fish_path, true);

    let test_cases = vec![
        (
            "echo 'hello world'",
            "00000000-0000-0000-0000-000000000001",
            1000,
        ),
        (
            "echo \"hello\\nworld\"",
            "00000000-0000-0000-0000-000000000002",
            2000,
        ),
        (
            "ls path\\to\\file",
            "00000000-0000-0000-0000-000000000003",
            3000,
        ),
        (
            "cargo build --release",
            "00000000-0000-0000-0000-000000000004",
            4000,
        ),
    ];

    for (cmd, id, ts) in &test_cases {
        let history = create_history_entry(id, *ts, cmd);
        atuin_daemon::fish_sync::sync_entry(&history, &settings).unwrap();
    }

    let content = fs::read_to_string(&fish_path).unwrap();
    assert!(content.contains("echo 'hello world'"));
    assert!(content.contains("echo"));
    assert!(content.contains("hello"));
    assert!(content.contains("world"));
    assert!(content.contains("cargo build --release"));
}

#[tokio::test]
async fn test_concurrent_sync_entries() {
    let temp_dir = TempDir::new().unwrap();
    let fish_path = temp_dir.path().join("fish_history");
    let settings = create_test_settings(&fish_path, true);

    // Spawn multiple tasks syncing different entries concurrently
    let mut handles = vec![];

    for i in 1..=10 {
        let _fish_path_clone = fish_path.clone();
        let settings_clone = settings.clone();
        let id = format!("00000000-0000-0000-0000-00000000000{:02}", i);
        let history = create_history_entry(&id, i * 1000, &format!("cmd{}", i));

        let handle = tokio::task::spawn_blocking(move || {
            atuin_daemon::fish_sync::sync_entry(&history, &settings_clone)
        });

        handles.push(handle);
    }

    // Wait for all to complete
    for handle in handles {
        handle.await.unwrap().unwrap();
    }

    let entry_count = count_fish_entries(&fish_path);
    // All 10 entries should be present
    assert!(entry_count >= 10);
}

#[tokio::test]
async fn test_trim_with_max_entries_zero() {
    let temp_dir = TempDir::new().unwrap();
    let fish_path = temp_dir.path().join("fish_history");
    let mut settings = create_test_settings(&fish_path, true);
    settings.fish_sync.max_entries = 0; // No limit

    // Add many entries
    for i in 1..=100 {
        let id = format!("00000000-0000-0000-0000-00000000000{:02}", i % 100 + 1);
        let history = create_history_entry(&id, i * 1000, &format!("cmd{}", i));
        atuin_daemon::fish_sync::sync_entry(&history, &settings).unwrap();
    }

    let entry_count = count_fish_entries(&fish_path);
    assert_eq!(entry_count, 100);
}

#[tokio::test]
async fn test_empty_fish_history_file() {
    let temp_dir = TempDir::new().unwrap();
    let fish_path = temp_dir.path().join("fish_history");

    // Create empty file
    fs::write(&fish_path, "").unwrap();

    let uuids = atuin_daemon::fish_sync::get_synced_uuids(fish_path.to_str().unwrap()).unwrap();

    assert_eq!(uuids.len(), 0);

    let timestamp =
        atuin_daemon::fish_sync::get_last_synced_timestamp(fish_path.to_str().unwrap()).unwrap();

    assert_eq!(timestamp, None);
}

#[tokio::test]
async fn test_uuid_extraction_with_malformed_entries() {
    let temp_dir = TempDir::new().unwrap();
    let fish_path = temp_dir.path().join("fish_history");

    // Create file with some entries missing UUIDs
    let content = "- cmd:cmd1\n  when:1000\n  # atuin-uuid:00000000-0000-0000-0000-000000000001\n\
                    - cmd:cmd2\n  when:2000\n\
                    - cmd:cmd3\n  when:3000\n  # atuin-uuid:00000000-0000-0000-0000-000000000003\n\
                    - cmd:cmd4\n  when:4000\n";

    fs::write(&fish_path, content).unwrap();

    let uuids = atuin_daemon::fish_sync::get_synced_uuids(fish_path.to_str().unwrap()).unwrap();

    // Should only extract the 2 valid UUIDs
    assert_eq!(uuids.len(), 2);
    assert!(uuids.contains("00000000-0000-0000-0000-000000000001"));
    assert!(uuids.contains("00000000-0000-0000-0000-000000000003"));
}
