use std::io::{self, BufRead, IsTerminal, Read, Write};
use tokio::sync::mpsc;

const MAX_LINE: u64 = 1024 * 1024;

/// One stdin owner serves both chat and approvals; cancelled approvals cannot steal later input.
pub struct Input {
    lines: mpsc::Receiver<Result<String, String>>,
    terminal: bool,
}

impl Input {
    pub fn stdin() -> Self {
        let terminal = io::stdin().is_terminal();
        let (sender, lines) = mpsc::channel(16);
        std::thread::spawn(move || {
            let mut reader = io::stdin().lock();
            loop {
                let mut line = String::new();
                match reader.by_ref().take(MAX_LINE + 1).read_line(&mut line) {
                    Ok(0) => break,
                    Ok(size) if size as u64 > MAX_LINE => {
                        let _ = sender.blocking_send(Err("Input exceeds the 1 MiB limit.".into()));
                        break;
                    }
                    Ok(_) => {
                        if sender.blocking_send(Ok(line)).is_err() {
                            break;
                        }
                    }
                    Err(_) => {
                        let _ = sender.blocking_send(Err("Could not read terminal input.".into()));
                        break;
                    }
                }
            }
        });
        Self { lines, terminal }
    }

    pub fn prompt(&self, prompt: &str) -> Result<(), String> {
        if self.terminal {
            print!("{prompt}");
            io::stdout()
                .flush()
                .map_err(|_| "Could not write terminal output.")?;
        }
        Ok(())
    }

    pub async fn next(&mut self) -> Result<Option<String>, String> {
        tokio::select! {
            result = self.read() => result,
            _ = tokio::signal::ctrl_c() => Ok(None),
        }
    }

    pub async fn read(&mut self) -> Result<Option<String>, String> {
        self.lines.recv().await.transpose()
    }
}
