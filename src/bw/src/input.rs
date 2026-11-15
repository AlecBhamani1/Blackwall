use std::io::{self, IsTerminal, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

const MAX_LINE: u64 = 1024 * 1024;

/// A line tagged with the approval-prompt generation current when the reader received it.
type Line = (u64, Result<String, String>);

/// One stdin owner serves both chat and approvals; cancelled approvals cannot steal later input.
pub struct Input {
    lines: mpsc::Receiver<Line>,
    terminal: bool,
    generation: Arc<AtomicU64>,
    flush: Box<dyn Fn() + Send>,
}

impl Input {
    pub fn stdin() -> Self {
        let terminal = io::stdin().is_terminal();
        Self::spawn(
            || {
                // Read the descriptor directly: std's stdin buffer would hold type-ahead that
                // neither tcflush nor the generation stamp can see.
                #[cfg(unix)]
                {
                    use std::os::fd::AsFd;
                    io::stdin()
                        .as_fd()
                        .try_clone_to_owned()
                        .map(|fd| Box::new(std::fs::File::from(fd)) as Box<dyn Read>)
                        .unwrap_or_else(|_| Box::new(io::stdin()))
                }
                #[cfg(not(unix))]
                {
                    Box::new(io::stdin()) as Box<dyn Read>
                }
            },
            terminal,
            || {
                // Drop type-ahead the terminal holds that the reader has not consumed yet.
                #[cfg(unix)]
                let _ =
                    rustix::termios::tcflush(io::stdin(), rustix::termios::QueueSelector::IFlush);
            },
        )
    }

    /// Each line carries the generation current when the read that delivered its first byte
    /// returned, so bytes already in hand when a prompt opens are always stale.
    fn spawn<R: Read>(
        open: impl FnOnce() -> R + Send + 'static,
        terminal: bool,
        flush: impl Fn() + Send + 'static,
    ) -> Self {
        let generation = Arc::new(AtomicU64::new(0));
        let stamp = generation.clone();
        let (sender, lines) = mpsc::channel(16);
        std::thread::spawn(move || {
            let mut reader = open();
            let mut chunk = vec![0; 8 * 1024];
            let mut line = Vec::new();
            let mut started = 0;
            let error = loop {
                let size = match reader.read(&mut chunk) {
                    Ok(size) => size,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break Some("Could not read terminal input."),
                };
                let at = stamp.load(Ordering::SeqCst);
                if size == 0 {
                    if line.is_empty() {
                        break None;
                    }
                    break send(&sender, started, &mut line).err().flatten();
                }
                let mut stop = None;
                for piece in chunk[..size].split_inclusive(|&byte| byte == b'\n') {
                    if line.is_empty() {
                        started = at;
                    }
                    line.extend_from_slice(piece);
                    let result = if line.len() as u64 > MAX_LINE {
                        Err(Some("Input exceeds the 1 MiB limit."))
                    } else if piece.ends_with(b"\n") {
                        send(&sender, started, &mut line)
                    } else {
                        Ok(())
                    };
                    if let Err(error) = result {
                        stop = Some(error);
                        break;
                    }
                }
                if let Some(error) = stop {
                    break error;
                }
            };
            if let Some(error) = error {
                let _ = sender.blocking_send((started, Err(error.into())));
            }
        });
        Self {
            lines,
            terminal,
            generation,
            flush: Box::new(flush),
        }
    }

    /// Whether a person is typing, rather than a script piping input.
    pub fn is_terminal(&self) -> bool {
        self.terminal
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
        self.lines.recv().await.map(|(_, line)| line).transpose()
    }

    /// Opens an approval prompt: on a terminal, all type-ahead becomes stale, whether queued
    /// here, held by the reader, or still in the terminal. Pass the result to `read_answer`.
    /// Windows consoles are not flushed; queued and held lines are still discarded.
    pub fn discard_type_ahead(&self) -> u64 {
        if !self.terminal {
            return 0;
        }
        (self.flush)();
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Reads the first line typed after the prompt `generation` opened, so a stray buffered
    /// line cannot answer a prompt the person has not seen yet. Piped input is scripted ahead
    /// of time, so it is read in order.
    pub async fn read_answer(&mut self, generation: u64) -> Result<Option<String>, String> {
        loop {
            match self.lines.recv().await {
                Some((at, Ok(_))) if self.terminal && at < generation => continue,
                line => return line.map(|(_, line)| line).transpose(),
            }
        }
    }
}

/// Sends a finished line; `Err(None)` means the receiver is gone, `Err(Some)` a read error.
fn send(
    sender: &mpsc::Sender<Line>,
    at: u64,
    line: &mut Vec<u8>,
) -> Result<(), Option<&'static str>> {
    let text = String::from_utf8(std::mem::take(line))
        .map_err(|_| Some("Could not read terminal input."))?;
    sender.blocking_send((at, Ok(text))).map_err(|_| None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn answer(input: &mut Input, generation: u64) -> Result<Option<String>, String> {
        tokio::time::timeout(Duration::from_secs(5), input.read_answer(generation))
            .await
            .map_err(|_| "timed out".to_string())?
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn approval_prompts_on_a_terminal_ignore_everything_typed_before_them() {
        use rustix::pty::{grantpt, openpt, ptsname, unlockpt, OpenptFlags};
        use std::fs::{File, OpenOptions};

        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).unwrap();
        grantpt(&master).unwrap();
        unlockpt(&master).unwrap();
        let path = ptsname(&master, Vec::new()).unwrap();
        let terminal = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path.to_str().unwrap())
            .unwrap();
        let flushed = terminal.try_clone().unwrap();
        let mut input = Input::spawn(
            move || terminal,
            true,
            move || {
                rustix::termios::tcflush(&flushed, rustix::termios::QueueSelector::IFlush).unwrap();
            },
        );
        let mut keyboard = File::from(master);
        // A paste larger than the queue: the reader fills it, then holds one more line,
        // and the rest waits in the terminal.
        for _ in 0..20 {
            keyboard.write_all(b"y\n").unwrap();
        }
        keyboard
            .write_all(b"/settings profile unsandboxed\n")
            .unwrap();
        for _ in 0..500 {
            if input.lines.len() == input.lines.max_capacity() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(input.lines.len(), input.lines.max_capacity());
        tokio::time::sleep(Duration::from_millis(50)).await;

        let generation = input.discard_type_ahead();
        keyboard.write_all(b"n\n").unwrap();
        assert_eq!(
            answer(&mut input, generation).await.unwrap().as_deref(),
            Some("n\n")
        );
    }

    /// Delivers each sent chunk from one `read` call, like a raw descriptor.
    struct Chunks(std::sync::mpsc::Receiver<&'static [u8]>);

    impl Read for Chunks {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let chunk = self.0.recv().unwrap_or_default();
            buf[..chunk.len()].copy_from_slice(chunk);
            Ok(chunk.len())
        }
    }

    #[tokio::test]
    async fn lines_already_read_before_a_prompt_cannot_answer_it() {
        let (keyboard, chunks) = std::sync::mpsc::channel();
        let mut input = Input::spawn(move || Chunks(chunks), true, || {});
        // One read brings in more lines than the queue holds, as with a big paste or
        // `stty -icanon`, plus the start of a line that is finished after the prompt.
        let paste = "y\n".repeat(20) + "/settings profile unsandboxed\n/app";
        keyboard.send(paste.leak().as_bytes()).unwrap();
        for _ in 0..500 {
            if input.lines.len() == input.lines.max_capacity() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(input.lines.len(), input.lines.max_capacity());

        let generation = input.discard_type_ahead();
        keyboard.send(b"rove\nn\n").unwrap();
        assert_eq!(
            answer(&mut input, generation).await.unwrap().as_deref(),
            Some("n\n")
        );
    }

    #[tokio::test]
    async fn piped_approval_answers_are_read_in_order() {
        let mut input = Input::spawn(|| io::Cursor::new("yes\nno"), false, || {});
        let generation = input.discard_type_ahead();
        for expected in [Some("yes\n"), Some("no"), None] {
            assert_eq!(
                answer(&mut input, generation).await.unwrap().as_deref(),
                expected
            );
        }
    }

    #[tokio::test]
    async fn approval_reads_still_report_input_errors() {
        let long = "y".repeat(MAX_LINE as usize + 1);
        let mut input = Input::spawn(move || io::Cursor::new(long), true, || {});
        let generation = input.discard_type_ahead();
        assert_eq!(
            answer(&mut input, generation).await,
            Err("Input exceeds the 1 MiB limit.".into())
        );
    }
}
