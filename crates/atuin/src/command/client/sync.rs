use clap::Subcommand;
use eyre::{Result, WrapErr};

use atuin_client::{
    database::{Database, Sqlite},
    encryption,
    fish_sync,
    history::store::HistoryStore,
    record::{sqlite_store::SqliteStore, store::Store, sync},
    settings::Settings,
};

mod status;

use std::path::Path;

use crate::command::client::account;

#[derive(Subcommand, Debug)]
#[command(infer_subcommands = true)]
pub enum Cmd {
    /// Sync with the configured server
    Sync {
        /// Force re-download everything
        #[arg(long, short)]
        force: bool,

        /// Check if fish_sync startup is enabled (exits with 0 if true, 1 if false)
        #[arg(long, conflicts_with = "force")]
        should_fish_sync: bool,
    },

    /// Login to the configured server
    Login(account::login::Cmd),

    /// Log out
    Logout,

    /// Register with the configured server
    Register(account::register::Cmd),

    /// Print the encryption key for transfer to another machine
    Key {
        /// Switch to base64 output of the key
        #[arg(long)]
        base64: bool,
    },

    /// Display the sync status
    Status,
}

impl Cmd {
    pub async fn run(
        self,
        settings: Settings,
        db: &Sqlite,
        store: SqliteStore,
    ) -> Result<()> {
        match self {
            Self::Sync {
                force,
                should_fish_sync,
            } => {
                if should_fish_sync {
                    // Check if fish_sync.sync_on_startup is enabled
                    if settings.fish_sync.sync_on_startup {
                        Ok(())
                    } else {
                        Err(eyre::eyre!("fish sync startup is disabled"))
                    }
                } else {
                    run(&settings, force, db, store).await
                }
            }
            Self::Login(l) => l.run(&settings, &store).await,
            Self::Logout => account::logout::run(&settings),
            Self::Register(r) => r.run(&settings).await,
            Self::Status => status::run(&settings, db).await,
            Self::Key { base64 } => {
                use atuin_client::encryption::{encode_key, load_key};
                let key = load_key(&settings).wrap_err("could not load encryption key")?;

                if base64 {
                    let encode = encode_key(&key).wrap_err("could not encode encryption key")?;
                    println!("{encode}");
                } else {
                    let mnemonic = bip39::Mnemonic::from_entropy(&key, bip39::Language::English)
                        .map_err(|_| eyre::eyre!("invalid key"))?;
                    println!("{mnemonic}");
                }
                Ok(())
            }
        }
    }
}

async fn run(
    settings: &Settings,
    force: bool,
    db: &Sqlite,
    store: SqliteStore,
) -> Result<()> {
    // Handle PID file if set via environment variable
    if let Ok(pid_file) = std::env::var("ATUIN_SYNC_PID_FILE") {
        // Ensure directory exists
        if let Some(parent) = Path::new(&pid_file).parent() {
            fs_err::create_dir_all(parent)?;
        }

        // Check if sync is already running
        if Path::new(&pid_file).exists() {
            if let Ok(pid_str) = fs_err::read_to_string(&pid_file) {
                if let Ok(pid) = pid_str.trim().parse::<u32>() {
                    // Check if process is still running using kill -0
                    if cfg!(unix) {
                        if let Ok(output) = std::process::Command::new("kill")
                            .arg("-0")
                            .arg(pid.to_string())
                            .output()
                        {
                            if output.status.success() {
                                return Ok(()); // Sync already running
                            }
                        }
                    }
                }
            }
        }

        // Write our PID
        fs_err::write(&pid_file, std::process::id().to_string())?;
    }

    if settings.sync.records {
        let encryption_key: [u8; 32] = encryption::load_key(settings)
            .context("could not load encryption key")?
            .into();

        let host_id = Settings::host_id().expect("failed to get host_id");
        let history_store = HistoryStore::new(store.clone(), host_id, encryption_key);

        let (uploaded, downloaded) = sync::sync(settings, &store).await?;

        crate::sync::build(settings, &store, db, Some(&downloaded)).await?;

        println!("{uploaded}/{} up/down to record store", downloaded.len());

        let history_length = db.history_count(true).await?;
        let store_history_length = store.len_tag("history").await?;

        #[allow(clippy::cast_sign_loss)]
        if history_length as u64 > store_history_length {
            println!(
                "{history_length} in history index, but {store_history_length} in history store"
            );
            println!("Running automatic history store init...");

            // Internally we use the global filter mode, so this context is ignored.
            // don't recurse or loop here.
            history_store.init_store(db).await?;

            println!("Re-running sync due to new records locally");

            // we'll want to run sync once more, as there will now be stuff to upload
            let (uploaded, downloaded) = sync::sync(settings, &store).await?;

            crate::sync::build(settings, &store, db, Some(&downloaded)).await?;

            println!("{uploaded}/{} up/down to record store", downloaded.len());

            // Sync downloaded remote entries to Fish history after second sync
            if !downloaded.is_empty() && settings.fish_sync.enabled {
                println!("Syncing {} remote entries to Fish history...", downloaded.len());
                if let Err(e) = fish_sync::sync_downloaded_entries(settings, db, &downloaded).await {
                    eprintln!("Failed to sync to fish history: {}", e);
                }
            }
        } else {
            // Sync downloaded remote entries to Fish history after first sync
            if !downloaded.is_empty() && settings.fish_sync.enabled {
                println!("Syncing {} remote entries to Fish history...", downloaded.len());
                if let Err(e) = fish_sync::sync_downloaded_entries(settings, db, &downloaded).await {
                    eprintln!("Failed to sync to fish history: {}", e);
                }
            }
        }

        // Check if we should sync all local entries
        if settings.fish_sync.sync_all_on_cli {
            println!("Syncing all local Atuin entries to Fish history...");
            match fish_sync::sync_all_entries(settings, db).await {
                Ok(count) => {
                    if count > 0 {
                        println!("Synced {} local entries to Fish history", count);
                    }
                }
                Err(e) => eprintln!("Failed to sync all local entries to fish history: {}", e),
            }
        }
    } else {
        atuin_client::sync::sync(settings, force, db).await?;
    }

    println!(
        "Sync complete! {} items in history database, force: {}",
        db.history_count(true).await?,
        force
    );

    // Clean up PID file on completion
    if let Ok(pid_file) = std::env::var("ATUIN_SYNC_PID_FILE") {
        let _ = fs_err::remove_file(&pid_file);
    }

    Ok(())
}
