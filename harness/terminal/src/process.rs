use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::mpsc,
};

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);
pub const MAX_OUTPUT_BYTES: usize = 2 * 1024 * 1024;

const NON_INTERACTIVE_ENV: &[(&str, &str)] = &[
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
    ("LESS", "FRX"),
    ("EDITOR", "true"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GH_PROMPT_DISABLED", "1"),
    ("SSH_ASKPASS", "false"),
    ("SSH_ASKPASS_REQUIRE", "force"),
];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Output { stream: String, data: String },
    Exit { code: Option<i32> },
}

pub fn command(program: &str, args: &[String], cwd: &Path) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in NON_INTERACTIVE_ENV {
        cmd.env(key, value);
    }
    for name in [
        "VENTICLI_TOKEN",
        "VENTIHARNESS_TOKEN",
        "VENTICODE_HARNESS_TOKEN",
        "OPEN_TERMINAL_API_KEY",
    ] {
        cmd.env_remove(name);
    }
    cmd
}

pub fn spawn(cmd: Command) -> Result<Box<dyn ChildWrapper>> {
    let mut command = CommandWrap::from(cmd);
    command.wrap(KillOnDrop);
    #[cfg(unix)]
    command.wrap(process_wrap::tokio::ProcessSession);
    #[cfg(windows)]
    command.wrap(process_wrap::tokio::JobObject);
    Ok(command.spawn()?)
}

pub fn shell(cwd: &Path, text: &str) -> Result<Command> {
    ensure!(
        !text.is_empty() && text.len() <= 32768 && !text.contains('\0'),
        "Invalid command"
    );
    #[cfg(unix)]
    let cmd = command("/bin/sh", &["-c".into(), text.into()], cwd);
    #[cfg(windows)]
    let cmd = command(
        "powershell.exe",
        &[
            "-NoLogo".into(),
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            text.into(),
        ],
        cwd,
    );
    Ok(cmd)
}

#[derive(Default)]
struct OutputBudget {
    total: usize,
    truncated: bool,
}

async fn output(
    mut reader: impl AsyncRead + Unpin,
    stream: &'static str,
    tx: mpsc::Sender<Event>,
    budget: Arc<Mutex<OutputBudget>>,
) {
    let mut bytes = [0u8; 8192];
    loop {
        let read = match reader.read(&mut bytes).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        let (to_send, emit_notice) = {
            let mut guard = budget.lock().unwrap_or_else(|e| e.into_inner());
            if guard.truncated {
                (0usize, false)
            } else {
                let remaining = MAX_OUTPUT_BYTES.saturating_sub(guard.total);
                if read <= remaining {
                    guard.total += read;
                    (read, false)
                } else {
                    guard.total = MAX_OUTPUT_BYTES;
                    guard.truncated = true;
                    (remaining, true)
                }
            }
        };
        if to_send > 0
            && tx
                .send(Event::Output {
                    stream: stream.into(),
                    data: STANDARD.encode(&bytes[..to_send]),
                })
                .await
                .is_err()
        {
            return;
        }
        if emit_notice {
            let notice = format!(
                "\n[output truncated after {} MiB]\n",
                MAX_OUTPUT_BYTES / (1024 * 1024)
            );
            let _ = tx
                .send(Event::Output {
                    stream: "stderr".into(),
                    data: STANDARD.encode(notice.as_bytes()),
                })
                .await;
        }
    }
}

#[cfg(unix)]
struct GroupGuard(Option<libc::pid_t>);

#[cfg(unix)]
impl GroupGuard {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

#[cfg(unix)]
impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(pgid) = self.0.filter(|&p| p > 1) {
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
    }
}

#[cfg(unix)]
async fn terminate_group(pgid: Option<libc::pid_t>) {
    let Some(pgid) = pgid.filter(|&p| p > 1) else {
        return;
    };
    if unsafe { libc::kill(-pgid, libc::SIGTERM) } != 0 {
        // Group already exited (ESRCH) or inaccessible.
        return;
    }
    for _ in 0..10 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if unsafe { libc::kill(-pgid, 0) } != 0 {
            return;
        }
    }
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
}

struct AbortOnDrop(Option<tokio::task::JoinHandle<()>>);

impl AbortOnDrop {
    async fn finish(&mut self, timeout: Duration) {
        if let Some(mut handle) = self.0.take()
            && tokio::time::timeout(timeout, &mut handle).await.is_err()
        {
            handle.abort();
        }
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}

pub async fn execute(cwd: &Path, text: &str, tx: mpsc::Sender<Event>) -> Result<()> {
    execute_with_timeout(cwd, text, tx, DEFAULT_TIMEOUT).await
}

pub async fn execute_with_timeout(
    cwd: &Path,
    text: &str,
    tx: mpsc::Sender<Event>,
    timeout: Duration,
) -> Result<()> {
    let mut child = spawn(shell(cwd, text)?)?;
    #[cfg(unix)]
    let pgid = child
        .id()
        .and_then(|id| libc::pid_t::try_from(id).ok())
        .filter(|&id| id > 1);
    #[cfg(unix)]
    let mut group_guard = GroupGuard(pgid);

    child.stdin().take();
    let stdout = child.stdout().take().context("Missing stdout")?;
    let stderr = child.stderr().take().context("Missing stderr")?;
    let budget = Arc::new(Mutex::new(OutputBudget::default()));

    let mut stdout_task = AbortOnDrop(Some(tokio::spawn(output(
        stdout,
        "stdout",
        tx.clone(),
        budget.clone(),
    ))));
    let mut stderr_task = AbortOnDrop(Some(tokio::spawn(output(
        stderr,
        "stderr",
        tx.clone(),
        budget,
    ))));

    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(wait_res) => {
            let status = wait_res?;
            // Give normal commands a brief chance to flush remaining pipe bytes before signaling.
            let _ = tokio::time::timeout(Duration::from_millis(50), async {
                if let Some(h) = stdout_task.0.as_mut() {
                    let _ = h.await;
                    stdout_task.0 = None;
                }
                if let Some(h) = stderr_task.0.as_mut() {
                    let _ = h.await;
                    stderr_task.0 = None;
                }
            })
            .await;
            #[cfg(unix)]
            {
                terminate_group(pgid).await;
                group_guard.disarm();
            }
            stdout_task.finish(Duration::from_millis(300)).await;
            stderr_task.finish(Duration::from_millis(300)).await;
            let _ = tx
                .send(Event::Exit {
                    code: status.code(),
                })
                .await;
            Ok(())
        }
        Err(_) => {
            #[cfg(unix)]
            {
                terminate_group(pgid).await;
                group_guard.disarm();
            }
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_millis(500), child.wait()).await;
            stdout_task.finish(Duration::from_millis(200)).await;
            stderr_task.finish(Duration::from_millis(200)).await;
            bail!("Command timed out after {}s", timeout.as_secs());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn streams_output_and_exit() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = mpsc::channel(32);
        let task = tokio::spawn(async move { execute(dir.path(), "echo hello", tx).await });
        let mut output = Vec::new();
        let mut exit = false;
        while let Some(event) = rx.recv().await {
            match event {
                Event::Output { data, .. } => output.extend(STANDARD.decode(data).unwrap()),
                Event::Exit { code } => {
                    assert_eq!(code, Some(0));
                    exit = true;
                }
            }
        }
        task.await.unwrap().unwrap();
        assert!(String::from_utf8(output).unwrap().contains("hello"));
        assert!(exit);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn injects_non_interactive_env() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = mpsc::channel(32);
        let task = tokio::spawn(async move {
            execute(
                dir.path(),
                "printf '%s|%s|%s|%s|%s|%s|%s|%s' \"$PAGER\" \"$GIT_PAGER\" \"$LESS\" \"$EDITOR\" \"$GIT_TERMINAL_PROMPT\" \"$GH_PROMPT_DISABLED\" \"$SSH_ASKPASS\" \"$SSH_ASKPASS_REQUIRE\"",
                tx,
            )
            .await
        });
        let mut output = Vec::new();
        while let Some(event) = rx.recv().await {
            if let Event::Output { data, .. } = event {
                output.extend(STANDARD.decode(data).unwrap());
            }
        }
        task.await.unwrap().unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "cat|cat|FRX|true|0|1|false|force"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_stops_descendants() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("late");
        let (tx, mut rx) = mpsc::channel(32);
        let task = tokio::spawn(async move {
            execute(
                dir.path(),
                "(sleep 1; echo bad > late) & echo started; wait",
                tx,
            )
            .await
        });
        rx.recv().await.unwrap();
        task.abort();
        let _ = task.await;
        tokio::time::sleep(std::time::Duration::from_millis(1300)).await;
        assert!(!destination.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn background_descendant_does_not_hang_or_leak_after_shell_exit() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("orphan-marker");
        let (tx, mut rx) = mpsc::channel(32);
        let started = tokio::time::Instant::now();
        let task = tokio::spawn(async move {
            execute(
                dir.path(),
                "(sleep 2; echo leaked > orphan-marker) & echo done",
                tx,
            )
            .await
        });
        let mut output = Vec::new();
        let mut exit_code = None;
        while let Some(event) = rx.recv().await {
            match event {
                Event::Output { data, .. } => output.extend(STANDARD.decode(data).unwrap()),
                Event::Exit { code } => exit_code = code,
            }
        }
        task.await.unwrap().unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(exit_code, Some(0));
        assert!(String::from_utf8(output).unwrap().contains("done"));
        tokio::time::sleep(Duration::from_millis(2200)).await;
        assert!(
            !marker.exists(),
            "background descendant should be terminated"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wall_clock_timeout_terminates_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("after-timeout");
        let (tx, mut rx) = mpsc::channel(32);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let err = execute_with_timeout(
            dir.path(),
            "(sleep 2; echo late > after-timeout) & wait",
            tx,
            Duration::from_millis(200),
        )
        .await
        .unwrap_err();
        let _ = drain.await;
        assert!(err.to_string().contains("timed out"));
        tokio::time::sleep(Duration::from_millis(2200)).await;
        assert!(
            !marker.exists(),
            "timed-out descendant should be terminated"
        );
    }
}
