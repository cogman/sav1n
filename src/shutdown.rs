use std::collections::HashSet;
use std::io;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;
use tokio::process::{Child, Command};

const GRACEFUL_TIMEOUT: Duration = Duration::from_secs(5);

static ACTIVE_CHILDREN: LazyLock<Mutex<HashSet<u32>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));
static TEMP_DIRS: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(|| Mutex::new(HashSet::new()));
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

pub struct TrackedChild {
    child: Option<Child>,
    pid: Option<u32>,
}

impl TrackedChild {
    pub fn spawn(mut command: Command) -> io::Result<Self> {
        #[cfg(unix)]
        command.process_group(0);
        command.kill_on_drop(true);
        let child = command.spawn()?;
        let pid = child.id();
        if let Some(pid) = pid {
            ACTIVE_CHILDREN.lock().unwrap().insert(pid);
            if is_shutting_down() {
                #[cfg(unix)]
                signal_process(pid, libc::SIGTERM);
            }
        }
        Ok(TrackedChild {
            child: Some(child),
            pid,
        })
    }

    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        let status = self
            .child
            .as_mut()
            .expect("child was already collected")
            .wait()
            .await?;
        self.unregister();
        Ok(status)
    }

    fn unregister(&mut self) {
        if let Some(pid) = self.pid.take() {
            ACTIVE_CHILDREN.lock().unwrap().remove(&pid);
        }
    }

    pub async fn wait_with_output(mut self) -> io::Result<Output> {
        self.child
            .take()
            .expect("child was already collected")
            .wait_with_output()
            .await
    }
}

impl Deref for TrackedChild {
    type Target = Child;

    fn deref(&self) -> &Child {
        self.child.as_ref().expect("child was already collected")
    }
}

impl DerefMut for TrackedChild {
    fn deref_mut(&mut self) -> &mut Child {
        self.child.as_mut().expect("child was already collected")
    }
}

impl Drop for TrackedChild {
    fn drop(&mut self) {
        self.unregister();
    }
}

pub fn install() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if !is_shutting_down() {
            default_hook(info);
        }
    }));

    tokio::spawn(async move {
        let (name, exit_code) = wait_for_signal().await;
        shut_down(name, exit_code).await;
    });
}

pub fn is_shutting_down() -> bool {
    SHUTTING_DOWN.load(Ordering::SeqCst)
}

pub fn track_temp_dir(path: PathBuf) -> bool {
    let mut dirs = TEMP_DIRS.lock().unwrap();
    if is_shutting_down() {
        false
    } else {
        dirs.insert(path);
        true
    }
}

pub fn untrack_temp_dir(path: &Path) {
    TEMP_DIRS.lock().unwrap().remove(path);
}

async fn wait_for_signal() -> (&'static str, i32) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        let mut interrupt = signal(SignalKind::interrupt()).expect("failed to listen for SIGINT");
        let mut terminate = signal(SignalKind::terminate()).expect("failed to listen for SIGTERM");
        tokio::select! {
            _ = interrupt.recv() => ("SIGINT", 130),
            _ = terminate.recv() => ("SIGTERM", 143),
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        ("SIGINT", 130)
    }
}

async fn shut_down(name: &str, exit_code: i32) -> ! {
    {
        let _guard = TEMP_DIRS.lock().unwrap();
        SHUTTING_DOWN.store(true, Ordering::SeqCst);
    }
    eprintln!("Received {name}, stopping encoders and cleaning up temporary files");

    terminate_children().await;
    remove_temp_dirs().await;

    std::process::exit(exit_code);
}

async fn terminate_children() {
    #[cfg(unix)]
    {
        let mut signaled = HashSet::new();
        let deadline = tokio::time::Instant::now() + GRACEFUL_TIMEOUT;
        loop {
            let pids = active_children();
            if pids.is_empty() {
                break;
            }
            for pid in pids {
                if signaled.insert(pid) {
                    signal_process(pid, libc::SIGTERM);
                }
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        for pid in active_children() {
            signal_process(pid, libc::SIGKILL);
        }
    }
}

fn active_children() -> Vec<u32> {
    ACTIVE_CHILDREN.lock().unwrap().iter().copied().collect()
}

#[cfg(unix)]
fn signal_process(pid: u32, signal: i32) {
    let pid = pid as i32;
    // Every child starts its own process group so the whole tree gets the signal.
    if unsafe { libc::kill(-pid, signal) } != 0 {
        unsafe { libc::kill(pid, signal) };
    }
}

async fn remove_temp_dirs() {
    let dirs: Vec<PathBuf> = TEMP_DIRS.lock().unwrap().drain().collect();
    for dir in dirs {
        let _ = tokio::fs::remove_dir_all(dir).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::Mutex as AsyncMutex;

    static TEST_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());

    #[cfg(unix)]
    #[tokio::test]
    async fn sends_sigterm_to_tracked_process_groups() {
        let _guard = TEST_LOCK.lock().await;
        let mut command = Command::new("sleep");
        command.arg("30");
        let mut child = TrackedChild::spawn(command).unwrap();
        let pid = child.id().expect("child should have a pid");
        assert!(active_children().contains(&pid));

        signal_process(pid, libc::SIGTERM);
        let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .expect("child ignored SIGTERM")
            .unwrap();
        assert!(!status.success());
        assert!(!active_children().contains(&pid));
        drop(child);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wait_with_output_unregisters_child() {
        let _guard = TEST_LOCK.lock().await;
        let output = TrackedChild::spawn(Command::new("true"))
            .unwrap()
            .wait_with_output()
            .await
            .unwrap();
        assert!(output.status.success());
        assert!(active_children().is_empty());
    }

    #[tokio::test]
    async fn tracks_and_untracks_temp_dirs() {
        let dir = std::env::temp_dir().join(format!("sav1n-shutdown-{}", std::process::id()));
        track_temp_dir(dir.clone());
        assert!(TEMP_DIRS.lock().unwrap().contains(&dir));
        untrack_temp_dir(&dir);
        assert!(!TEMP_DIRS.lock().unwrap().contains(&dir));
    }
}
