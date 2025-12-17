use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use scorpio::antares::fuse::AntaresFuse;
use scorpio::dicfuse::Dicfuse;
use scorpio::util::config;
use tokio::process::Command;
use tokio::signal;
use tokio::time::{sleep, Duration};
use uuid::Uuid;

/// Mount an Antares overlay and keep it running for testing.
#[derive(Parser, Debug)]
#[command(author, version, about = "Mount overlay and keep it running for testing", long_about = None)]
struct Cli {
    /// Path to scorpio config (defaults to scorpio.toml)
    #[arg(long, default_value = "scorpio.toml")]
    config_path: String,

    /// Relative path inside the mount to run buck2 build (optional)
    #[arg(long, default_value = "", value_name = "REL_PATH")]
    buck_build_rel: String,

    /// Buck2 target to build (only used when buck_build_rel is set)
    #[arg(long, default_value = "//...", value_name = "TARGET")]
    buck_target: String,
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let cli = Cli::parse();

    if let Err(e) = config::init_config(&cli.config_path) {
        eprintln!("Failed to load config: {e}");
        std::process::exit(1);
    }

    // Prepare per-run isolated paths under /tmp to avoid clashes.
    let run_id = Uuid::new_v4();
    let base = PathBuf::from(format!("/tmp/antares_test_{run_id}"));
    let mount = base.join("mnt");
    let upper = base.join("upper");
    let cl = base.join("cl");
    let store_path = base.join("store");

    std::fs::create_dir_all(&store_path)?;

    // Build Dicfuse with a dedicated store and load the directory tree eagerly.
    let dic = Dicfuse::new_with_store_path(store_path.to_str().unwrap()).await;
    println!("Loading directory tree (Dicfuse import_arc)...");
    scorpio::dicfuse::store::import_arc(dic.store.clone()).await;
    println!("Directory tree loaded, mounting overlay...");

    let mut fuse = AntaresFuse::new(
        mount.clone(),
        Arc::new(dic),
        upper.clone(),
        Some(cl.clone()),
    )
    .await
    .map_err(|e| {
        eprintln!("Failed to create AntaresFuse: {e}");
        e
    })?;

    fuse.mount().await.map_err(|e| {
        eprintln!("Mount failed: {e}");
        e
    })?;
    println!("Mounted at {}", mount.display());

    // Give the kernel a brief moment to settle.
    sleep(Duration::from_millis(200)).await;

    println!("Mount is ready for testing.");
    println!("Mount point: {}", mount.display());
    println!("Upper dir: {}", upper.display());
    println!("CL dir: {}", cl.display());
    println!("Store dir: {}", store_path.display());
    println!();

    // Optional: start buck2 build in a separate async task
    let mut buck_handle = None;
    if !cli.buck_build_rel.is_empty() {
        let workdir = mount.join(&cli.buck_build_rel);
        println!(
            "Starting buck2 build '{}' in {} (separate task)...",
            cli.buck_target,
            workdir.display()
        );

        let buck2_daemon_dir = PathBuf::from("/tmp/buck2_daemon");
        let buck2_isolation_dir = buck2_daemon_dir.join("isolation");
        let buck2_tmp_dir = buck2_daemon_dir.join("tmp");
        let buck2_out_dir = buck2_daemon_dir.join("buck-out");
        for dir in [
            &buck2_daemon_dir,
            &buck2_isolation_dir,
            &buck2_tmp_dir,
            &buck2_out_dir,
        ] {
            if let Err(e) = std::fs::create_dir_all(dir) {
                eprintln!("Warning: failed to create dir {}: {}", dir.display(), e);
            }
        }

        buck_handle = Some(tokio::spawn(async move {
            let mut cmd = Command::new("buck2");
            cmd.arg("build")
                .arg(&cli.buck_target)
                .current_dir(&workdir)
                .env("HOME", "/root")
                .env("BUCK2_ALLOW_ROOT", "1")
                .env("BUCK2_DAEMON_DIR", &buck2_daemon_dir)
                .env("BUCK2_ISOLATION_DIR", &buck2_isolation_dir)
                .env("TMPDIR", &buck2_tmp_dir)
                .env("BUCK_OUT", &buck2_out_dir)
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit());

            match cmd.status().await {
                Ok(st) => println!("buck2 exited with status: {}", st),
                Err(err) => eprintln!("Failed to spawn buck2: {err}"),
            }
        }));
    }

    println!("Mount will stay alive until Ctrl+C is pressed.");
    println!("You can test the mount point in another terminal:");
    println!("  cd {}", mount.display());
    println!();

    // If buck task exists, run it in background and keep mount until Ctrl+C
    if let Some(handle) = buck_handle {
        tokio::spawn(async move {
            let _ = handle.await;
        });
    }

    // Keep running until interrupted
    signal::ctrl_c().await?;
    println!("\nUnmounting...");

    // Cleanup
    cleanup(&mut fuse, &base).await;

    println!("Done.");
    Ok(())
}

async fn cleanup(fuse: &mut AntaresFuse, base: &PathBuf) {
    if let Err(e) = fuse.unmount().await {
        eprintln!("Warning: unmount failed: {e}");
    }

    if let Err(e) = std::fs::remove_dir_all(base) {
        eprintln!("Warning: failed to remove {}: {e}", base.display());
    }
}
