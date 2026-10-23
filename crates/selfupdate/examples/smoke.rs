//! Offline smoke driver. All inputs and state belong to the caller's temporary directory.
use codecraft_selfupdate::{Store, build, source::ManagedSource};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, atomic::AtomicBool};
fn main() -> std::io::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let base = PathBuf::from(args.next().ok_or_else(|| std::io::Error::other("base required"))?);
    let remote = PathBuf::from(args.next().ok_or_else(|| std::io::Error::other("local remote required"))?);
    let launcher = PathBuf::from(args.next().ok_or_else(|| std::io::Error::other("launcher required"))?);
    let source = ManagedSource::new(base.clone(), format!("file://{}", remote.display()))?;
    let path = source.prepare(None, |s| println!("{s}"))?;
    std::fs::write(path.join("selfupdate-smoke.txt"), "local feedback smoke\n")?;
    let main = path.join("apps/codecraft/src/main.rs");
    let mut text = std::fs::read_to_string(&main)?;
    text.push_str("\n// Managed feedback smoke.\n");
    std::fs::write(&main, &text)?;
    let commit =
        source.commit(&["selfupdate-smoke.txt".into(), "apps/codecraft/src/main.rs".into()], "Local smoke feedback")?;
    let mut request = build::Request {
        source: path,
        base: base.clone(),
        cargo: "cargo".into(),
        profile: "selfupdate".into(),
        commit: Some(commit.clone()),
    };
    let cold = std::time::Instant::now();
    let payload = build::run(&request, Arc::new(AtomicBool::new(false)), |s| print!("{s}"))?;
    println!("COLD_BUILD_SECONDS={:.2}", cold.elapsed().as_secs_f64());
    text.push_str("// A warm rebuild of accepted Rust source.\n");
    std::fs::write(&main, &text)?;
    let commit = source.commit(&["apps/codecraft/src/main.rs".into()], "Warm local feedback")?;
    request.commit = Some(commit.clone());
    let warm = std::time::Instant::now();
    let warm_payload = build::run(&request, Arc::new(AtomicBool::new(false)), |s| print!("{s}"))?;
    println!("WARM_BUILD_SECONDS={:.2}", warm.elapsed().as_secs_f64());
    let store = Store::new(base.clone(), payload.clone());
    let id = store.stage(&warm_payload, "0.3.0", &commit, "codecraft-user")?;
    store.promote_pending(&id)?;
    let version = Command::new(store.executable(&id)?).arg("--version").output()?;
    if !version.status.success()
        || !String::from_utf8_lossy(&version.stdout).contains(commit.get(..10).unwrap_or_default())
    {
        return Err(std::io::Error::other("payload does not report its accepted commit"));
    }
    println!("PAYLOAD_VERSION={}", String::from_utf8_lossy(&version.stdout).trim());
    let run = |flags: &[&str]| -> std::io::Result<()> {
        let status = Command::new(&launcher)
            .args(flags)
            .env("CODECRAFT_HOME", &base)
            .env("CODECRAFT_FACTORY_DIR", &payload)
            .env("CODECRAFT_HEALTHY_AFTER", "0")
            .env("CODECRAFT_BOOT_TIMEOUT", "3")
            .status()?;
        if !status.success() {
            return Err(std::io::Error::other(format!("launcher failed: {status}")));
        }
        Ok(())
    };
    run(&["--status"])?;
    run(&["--version-id", &id, "--selfupdate-health-check"])?;
    if store.state().last_good.as_deref() != Some(&id) {
        return Err(std::io::Error::other("real payload did not become healthy"));
    }
    let broken = base.join("broken");
    std::fs::create_dir_all(&broken)?;
    std::fs::write(broken.join("codecraft"), "deliberately invalid executable")?;
    let bad = store.stage(&broken, "0.3.0", "badbadbad", "smoke")?;
    store.promote_pending(&bad)?;
    run(&["--selfupdate-health-check"])?;
    if store.take_notice()?.is_none()
        || codecraft_selfupdate::choose(&store.boot_state(), codecraft_selfupdate::now()).id != id
    {
        return Err(std::io::Error::other("rollback assertion failed"));
    }
    println!("SMOKE_OK real payload healthy; broken payload rolled back to {id}");
    Ok(())
}
