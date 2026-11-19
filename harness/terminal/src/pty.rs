use anyhow::{Result, ensure};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::{
    io::{Read, Write},
    path::Path,
};

pub struct Terminal {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
}

impl Terminal {
    pub fn start(cwd: &Path, rows: u16, cols: u16) -> Result<(Self, Box<dyn Read + Send>)> {
        let size = size(rows, cols)?;
        let pair = native_pty_system().openpty(size)?;
        #[cfg(unix)]
        let mut cmd = {
            let shell = std::env::var("SHELL")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "/bin/sh".into());
            let mut cmd = CommandBuilder::new(shell);
            if std::env::var("TERM")
                .ok()
                .is_none_or(|t| t.trim().is_empty())
            {
                cmd.env("TERM", "xterm-256color");
            }
            cmd
        };
        #[cfg(windows)]
        let mut cmd = CommandBuilder::new("powershell.exe");
        cmd.cwd(cwd);
        for name in [
            "VENTICLI_TOKEN",
            "VENTIHARNESS_TOKEN",
            "VENTICODE_HARNESS_TOKEN",
            "OPEN_TERMINAL_API_KEY",
        ] {
            cmd.env_remove(name);
        }
        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);
        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        Ok((
            Self {
                master: pair.master,
                child,
                writer,
            },
            reader,
        ))
    }
    pub fn input(&mut self, bytes: &[u8]) -> Result<()> {
        ensure!(bytes.len() <= 32768, "Terminal input too large");
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }
    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        self.master.resize(size(rows, cols)?)
    }
    pub fn status(&mut self) -> Result<Option<u32>> {
        Ok(self.child.try_wait()?.map(|s| s.exit_code()))
    }
}

fn size(rows: u16, cols: u16) -> Result<PtySize> {
    ensure!(
        (1..=1000).contains(&rows) && (1..=1000).contains(&cols),
        "Invalid terminal size"
    );
    Ok(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })
}

impl Drop for Terminal {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(group) = self.master.process_group_leader() {
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
        }
        #[cfg(unix)]
        if let Some(pid) = self.child.process_id() {
            // The PTY child creates its own session; stop the shell process group.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
        #[cfg(windows)]
        if let Some(pid) = self.child.process_id() {
            let _ = std::process::Command::new("taskkill.exe")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_terminal_reads_input_and_resizes() {
        let dir = tempfile::tempdir().unwrap();
        let (mut terminal, mut reader) = Terminal::start(dir.path(), 24, 80).unwrap();
        terminal.resize(40, 120).unwrap();
        terminal.input(b"echo ventic-pty-test\r\nexit\r\n").unwrap();
        let mut output = [0; 4096];
        let count = reader.read(&mut output).unwrap();
        assert!(count > 0);
        assert!(terminal.resize(0, 80).is_err());
    }
}
