use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use serde::{Deserialize, Serialize};
use std::{path::Path, process::Stdio};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::mpsc,
};

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

async fn output(
    mut reader: impl AsyncRead + Unpin,
    stream: &str,
    tx: &mpsc::Sender<Event>,
) -> Result<()> {
    let mut bytes = [0; 8192];
    loop {
        let read = reader.read(&mut bytes).await?;
        if read == 0 {
            return Ok(());
        }
        tx.send(Event::Output {
            stream: stream.into(),
            data: STANDARD.encode(&bytes[..read]),
        })
        .await?;
    }
}

pub async fn execute(cwd: &Path, text: &str, tx: mpsc::Sender<Event>) -> Result<()> {
    let mut child = spawn(shell(cwd, text)?)?;
    child.stdin().take();
    let stdout = child.stdout().take().context("Missing stdout")?;
    let stderr = child.stderr().take().context("Missing stderr")?;
    let status = tokio::try_join!(
        output(stdout, "stdout", &tx),
        output(stderr, "stderr", &tx),
        async { Ok::<_, anyhow::Error>(child.wait().await?) }
    )?
    .2;
    tx.send(Event::Exit {
        code: status.code(),
    })
    .await?;
    Ok(())
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
}
