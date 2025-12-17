use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use clap::Parser;
use scorpio::antares::fuse::AntaresFuse;
use scorpio::dicfuse::Dicfuse;
use scorpio::util::config;
use tokio::process::Command;
use tokio::time::{sleep, Duration};
use uuid::Uuid;

/// Mount an Antares overlay, run a Buck2 build inside the mount, then unmount.
#[derive(Parser, Debug)]
#[command(author, version, about = "Mount overlay and run buck2 build inside it", long_about = None)]
struct Cli {
    /// Path to scorpio config (defaults to scorpio.toml)
    #[arg(long, default_value = "scorpio.toml")]
    config_path: String,

    /// Relative path inside the mount to run the build (default: third-party/buck-hello)
    #[arg(long, default_value = "third-party/buck-hello")]
    build_rel: String,

    /// Buck2 target to build (default: //...)
    #[arg(long, default_value = "//...")]
    target: String,
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
    let base = PathBuf::from(format!("/tmp/antares_build_{run_id}"));
    let mount = base.join("mnt");
    let upper = base.join("upper");
    let cl = base.join("cl");
    let store_path = base.join("store");

    std::fs::create_dir_all(&store_path)?;

    // Build Dicfuse with a dedicated store and load the directory tree eagerly to reduce
    // latency for the subsequent Buck build.
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

    // Give the kernel a brief moment to settle before running heavy I/O.
    sleep(Duration::from_millis(200)).await;

    // Run buck2 build inside the mount.
    let workdir = mount.join(&cli.build_rel);
    println!(
        "Running buck2 build '{}' in {}",
        cli.target,
        workdir.display()
    );

    // Put buck2 daemon/state outside FUSE to avoid sqlite shm issues on FUSE mounts.
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

    let mut cmd = Command::new("buck2");
    cmd.arg("build")
        .arg(&cli.target)
        .current_dir(&workdir)
        .env("HOME", "/root") // buck2 rejects root when $HOME is non-root-owned
        .env("BUCK2_ALLOW_ROOT", "1") // explicit opt-in for root execution
        .env("BUCK2_DAEMON_DIR", &buck2_daemon_dir)
        .env("BUCK2_ISOLATION_DIR", &buck2_isolation_dir)
        .env("TMPDIR", &buck2_tmp_dir)
        .env("BUCK_OUT", &buck2_out_dir)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let status = match cmd.status().await {
        Ok(st) => st,
        Err(err) => {
            eprintln!("Failed to spawn buck2: {err}");
            cleanup(&mut fuse, &base).await;
            std::process::exit(1);
        }
    };

    println!("buck2 exited with status: {}", status);

    // Unmount regardless of build result to avoid leaving a busy mount.
    cleanup(&mut fuse, &base).await;

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }

    Ok(())
}

async fn cleanup(fuse: &mut AntaresFuse, base: &PathBuf) {
    println!("Unmounting {}...", fuse.mountpoint.display());
    if let Err(e) = fuse.unmount().await {
        eprintln!("Warning: unmount failed: {e}");
    }

    if let Err(e) = std::fs::remove_dir_all(base) {
        eprintln!("Warning: failed to remove {}: {e}", base.display());
    }
}
