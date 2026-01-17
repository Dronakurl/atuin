//! Benchmark suite for Fish history sync functionality
//!
//! These benchmarks measure the performance of critical operations in the Fish sync module.
//! Run with: `cargo bench -p atuin-daemon --bench fish_sync`

use atuin_client::history::History;
use atuin_client::settings::{FishSync, Settings};
use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};
use fs_err as fs;
use std::path::PathBuf;
use tempfile::TempDir;
use time::OffsetDateTime;

/// Create a test settings instance
fn create_test_settings(fish_path: &PathBuf) -> Settings {
    let mut settings = Settings::default();
    settings.fish_sync = FishSync {
        enabled: true,
        history_path: fish_path.to_string_lossy().to_string(),
        max_entries: 10000,
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

/// Create a Fish history file with the specified number of entries
fn create_fish_history_file(path: &PathBuf, count: usize) {
    let mut content = String::new();
    for i in 0..count {
        let uuid = uuid::Uuid::new_v4();
        content.push_str(&format!(
            "- cmd:test_command_{}\n  when:{}\n  # atuin-uuid:{}\n",
            i,
            i * 1000,
            uuid
        ));
    }
    fs::write(path, content).unwrap();
}

fn benchmark_sync_entry(c: &mut Criterion) {
    let temp_dir = TempDir::new().unwrap();
    let fish_path = temp_dir.path().join("fish_history");
    let settings = create_test_settings(&fish_path);
    let history = create_history_entry("00000000-0000-0000-0000-000000000001", 1000, "git status");

    c.bench_function("sync_entry", |b| {
        b.iter(|| {
            atuin_daemon::fish_sync::sync_entry(black_box(&history), black_box(&settings)).unwrap()
        })
    });
}

fn benchmark_format_fish_entry(c: &mut Criterion) {
    let history = create_history_entry("00000000-0000-0000-0000-000000000001", 1000, "git status");

    c.bench_function("format_fish_entry", |b| {
        b.iter(|| {
            // We need to call the internal function, but it's private
            // So we'll test sync_entry which calls it
            let formatted = format!(
                "- cmd:{}\n  when:{}\n  # atuin-uuid:{}\n",
                history.command.replace('\\', "\\\\").replace('\n', "\\n"),
                history.timestamp.unix_timestamp(),
                history.id.0.as_str()
            );
            black_box(formatted)
        })
    });
}

fn benchmark_get_synced_uuids(c: &mut Criterion) {
    let mut group = c.benchmark_group("get_synced_uuids");

    for count in [10, 100, 500, 1000].iter() {
        let temp_dir = TempDir::new().unwrap();
        let fish_path = temp_dir.path().join("fish_history");
        create_fish_history_file(&fish_path, *count);
        let path_str = fish_path.to_string_lossy().to_string();

        group.bench_with_input(BenchmarkId::from_parameter(count), count, |b, _| {
            b.iter(|| {
                atuin_daemon::fish_sync::get_synced_uuids(black_box(&path_str)).unwrap()
            })
        });
    }

    group.finish();
}

fn benchmark_trim_fish_history(c: &mut Criterion) {
    let mut group = c.benchmark_group("trim_fish_history");

    for count in [100, 500, 1000, 5000].iter() {
        let temp_dir = TempDir::new().unwrap();
        let fish_path = temp_dir.path().join("fish_history");
        create_fish_history_file(&fish_path, *count);
        let path_str = fish_path.to_string_lossy().to_string();
        let max_entries = count / 2; // Trim to half

        group.bench_with_input(BenchmarkId::from_parameter(count), count, |b, _| {
            b.iter(|| {
                atuin_daemon::fish_sync::trim_fish_history(black_box(&path_str), black_box(max_entries)).unwrap()
            })
        });
    }

    group.finish();
}

fn benchmark_sync_entries_batch(c: &mut Criterion) {
    let mut group = c.benchmark_group("sync_entries_batch");

    for count in [1, 10, 50, 100].iter() {
        let mut entries = Vec::new();
        for i in 0..*count {
            let id = format!("00000000-0000-0000-0000-00000000000{:02}", i % 100);
            entries.push(create_history_entry(&id, i * 1000, &format!("cmd {}", i)));
        }

        group.bench_with_input(BenchmarkId::from_parameter(count), count, |b, _| {
            b.iter(|| {
                // Create a new temp file for each iteration
                let temp_dir = TempDir::new().unwrap();
                let fish_path = temp_dir.path().join("fish_history");
                let settings = create_test_settings(&fish_path);
                atuin_daemon::fish_sync::sync_entries(black_box(&entries), black_box(&settings)).unwrap()
            })
        });
    }

    group.finish();
}

fn benchmark_get_last_synced_timestamp(c: &mut Criterion) {
    let mut group = c.benchmark_group("get_last_synced_timestamp");

    for count in [10, 100, 1000].iter() {
        let temp_dir = TempDir::new().unwrap();
        let fish_path = temp_dir.path().join("fish_history");
        create_fish_history_file(&fish_path, *count);
        let path_str = fish_path.to_string_lossy().to_string();

        group.bench_with_input(BenchmarkId::from_parameter(count), count, |b, _| {
            b.iter(|| {
                atuin_daemon::fish_sync::get_last_synced_timestamp(black_box(&path_str)).unwrap()
            })
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    benchmark_sync_entry,
    benchmark_format_fish_entry,
    benchmark_get_synced_uuids,
    benchmark_trim_fish_history,
    benchmark_sync_entries_batch,
    benchmark_get_last_synced_timestamp
);

criterion_main!(benches);
