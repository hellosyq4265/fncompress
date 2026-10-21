//! Running external tools (ffprobe, ffmpeg, dovi_tool). Every call has a
//! timeout and can be cancelled; on Unix the child gets its own process group
//! so a kill also takes down anything it spawned (ADR 0006).

use std::{
    io::{BufRead, BufReader, Read},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

/// How much of stderr is kept for error messages.
const STDERR_TAIL: usize = 16 * 1024;

/// Shared flag that asks a running tool to stop. Cloning shares the flag.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("无法启动 {program}：{source}")]
    Spawn {
        program: String,
        source: std::io::Error,
    },
    #[error("{program} 超时（{}s），已终止", .after.as_secs())]
    Timeout { program: String, after: Duration },
    #[error("{program} 已取消")]
    Cancelled { program: String },
    #[error("{program} 失败（{status}）：{stderr}")]
    Failed {
        program: String,
        status: ExitStatus,
        stderr: String,
    },
    #[error("{program} 输出异常：{message}")]
    Unexpected { program: String, message: String },
    #[error("读取 {program} 输出失败：{source}")]
    Io {
        program: String,
        source: std::io::Error,
    },
}

const RUNNING: u8 = 0;
const TIMED_OUT: u8 = 1;
const CANCELLED: u8 = 2;

/// Runs `cmd`, calling `on_line` for each stdout line (without the newline),
/// and returns the stderr tail on success. A non-zero exit is an error.
pub fn run_lines(
    cmd: &mut Command,
    timeout: Duration,
    cancel: Option<&CancelToken>,
    mut on_line: impl FnMut(&str),
) -> Result<String, ToolError> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|source| ToolError::Spawn {
        program: program.clone(),
        source,
    })?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let child = Arc::new(Mutex::new(child));

    let stderr_reader = thread::spawn(move || read_tail(stderr));

    let state = Arc::new(AtomicU8::new(RUNNING));
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let watchdog = {
        let child = Arc::clone(&child);
        let state = Arc::clone(&state);
        let cancel = cancel.cloned();
        let deadline = Instant::now() + timeout;
        thread::spawn(move || loop {
            match done_rx.recv_timeout(Duration::from_millis(50)) {
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                _ => return,
            }
            let reason = if cancel.as_ref().is_some_and(CancelToken::is_cancelled) {
                CANCELLED
            } else if Instant::now() >= deadline {
                TIMED_OUT
            } else {
                continue;
            };
            state.store(reason, Ordering::SeqCst);
            kill(&mut child.lock().unwrap());
            return;
        })
    };

    let mut reader = BufReader::new(stdout);
    let mut buf = Vec::new();
    let read_result = loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => break Ok(()),
            Ok(_) => {
                let line = String::from_utf8_lossy(&buf);
                on_line(line.trim_end_matches(['\n', '\r']));
            }
            Err(e) => break Err(e),
        }
    };
    if read_result.is_err() {
        kill(&mut child.lock().unwrap());
    }

    let status = loop {
        match child.lock().unwrap().try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {}
            Err(e) => break Err(e),
        }
        thread::sleep(Duration::from_millis(10));
    };
    let _ = done_tx.send(());
    let _ = watchdog.join();
    let stderr = stderr_reader.join().unwrap_or_default();

    match state.load(Ordering::SeqCst) {
        TIMED_OUT => {
            return Err(ToolError::Timeout {
                program,
                after: timeout,
            })
        }
        CANCELLED => return Err(ToolError::Cancelled { program }),
        _ => {}
    }
    let io_err = |source| ToolError::Io {
        program: program.clone(),
        source,
    };
    read_result.map_err(io_err)?;
    let status = status.map_err(io_err)?;
    if !status.success() {
        return Err(ToolError::Failed {
            program,
            status,
            stderr,
        });
    }
    Ok(stderr)
}

/// Runs `producer | consumer` and returns the consumer's stdout. Both share
/// the timeout and the cancel token; when the consumer fails, its error is
/// reported (the producer then usually fails on the broken pipe).
pub fn run_piped(
    producer: &mut Command,
    consumer: &mut Command,
    timeout: Duration,
    cancel: Option<&CancelToken>,
) -> Result<String, ToolError> {
    let first = producer.get_program().to_string_lossy().into_owned();
    let second = consumer.get_program().to_string_lossy().into_owned();
    for cmd in [&mut *producer, &mut *consumer] {
        cmd.stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
    }
    producer.stdin(Stdio::null()).stdout(Stdio::piped());
    let mut p = producer.spawn().map_err(|source| ToolError::Spawn {
        program: first.clone(),
        source,
    })?;
    let pipe = p.stdout.take().expect("stdout is piped");
    consumer.stdin(Stdio::from(pipe)).stdout(Stdio::piped());
    let mut c = match consumer.spawn() {
        Ok(c) => c,
        Err(source) => {
            kill(&mut p);
            let _ = p.wait();
            return Err(ToolError::Spawn {
                program: second,
                source,
            });
        }
    };
    let p_err = p.stderr.take().expect("stderr is piped");
    let c_err = c.stderr.take().expect("stderr is piped");
    let c_out = c.stdout.take().expect("stdout is piped");
    let p_err = thread::spawn(move || read_tail(p_err));
    let c_err = thread::spawn(move || read_tail(c_err));
    let c_out = thread::spawn(move || read_tail(c_out));

    let deadline = Instant::now() + timeout;
    let mut state = RUNNING;
    let (mut p_status, mut c_status) = (None, None);
    while p_status.is_none() || c_status.is_none() {
        if state == RUNNING {
            if cancel.is_some_and(CancelToken::is_cancelled) {
                state = CANCELLED;
            } else if Instant::now() >= deadline {
                state = TIMED_OUT;
            }
            if state != RUNNING {
                kill(&mut p);
                kill(&mut c);
            }
        }
        let io_err = |program: &str, source| ToolError::Io {
            program: program.to_owned(),
            source,
        };
        if p_status.is_none() {
            p_status = p.try_wait().map_err(|e| io_err(&first, e))?;
        }
        if c_status.is_none() {
            c_status = c.try_wait().map_err(|e| io_err(&second, e))?;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let (p_err, c_err) = (
        p_err.join().unwrap_or_default(),
        c_err.join().unwrap_or_default(),
    );
    let out = c_out.join().unwrap_or_default();
    match state {
        TIMED_OUT => {
            return Err(ToolError::Timeout {
                program: second,
                after: timeout,
            })
        }
        CANCELLED => return Err(ToolError::Cancelled { program: second }),
        _ => {}
    }
    let (p_status, c_status) = (p_status.unwrap(), c_status.unwrap());
    if !c_status.success() {
        return Err(ToolError::Failed {
            program: second,
            status: c_status,
            stderr: if c_err.is_empty() { out } else { c_err },
        });
    }
    if !p_status.success() {
        return Err(ToolError::Failed {
            program: first,
            status: p_status,
            stderr: p_err,
        });
    }
    Ok(out)
}

/// Runs `cmd` and returns its whole stdout.
pub fn run_capture(
    cmd: &mut Command,
    timeout: Duration,
    cancel: Option<&CancelToken>,
) -> Result<String, ToolError> {
    let mut out = String::new();
    run_lines(cmd, timeout, cancel, |line| {
        out.push_str(line);
        out.push('\n');
    })?;
    Ok(out)
}

#[cfg(unix)]
fn kill(child: &mut Child) {
    // The child leads its own process group; take the whole group down.
    let pid = child.id() as libc::pid_t;
    // SAFETY: plain syscall on a pid we spawned and have not reaped yet.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill(child: &mut Child) {
    let _ = child.kill();
}

fn read_tail(mut r: impl Read) -> String {
    let mut tail: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    while let Ok(n) = r.read(&mut chunk) {
        if n == 0 {
            break;
        }
        tail.extend_from_slice(&chunk[..n]);
        if tail.len() > 2 * STDERR_TAIL {
            tail.drain(..tail.len() - STDERR_TAIL);
        }
    }
    if tail.len() > STDERR_TAIL {
        tail.drain(..tail.len() - STDERR_TAIL);
    }
    String::from_utf8_lossy(&tail).trim().to_owned()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut c = Command::new("sh");
        c.args(["-c", script]);
        c
    }

    #[test]
    fn collects_lines_and_reports_failure() {
        let out = run_capture(&mut sh("echo a; echo b"), Duration::from_secs(5), None).unwrap();
        assert_eq!(out, "a\nb\n");
        let err = run_capture(
            &mut sh("echo oops >&2; exit 3"),
            Duration::from_secs(5),
            None,
        )
        .unwrap_err();
        assert!(matches!(err, ToolError::Failed { ref stderr, .. } if stderr == "oops"));
    }

    #[test]
    fn timeout_kills_the_process_group() {
        let started = Instant::now();
        let err =
            run_capture(&mut sh("sleep 30 & wait"), Duration::from_millis(200), None).unwrap_err();
        assert!(matches!(err, ToolError::Timeout { .. }));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn cancel_stops_the_tool() {
        let token = CancelToken::default();
        let t = token.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            t.cancel();
        });
        let err =
            run_capture(&mut sh("sleep 30"), Duration::from_secs(30), Some(&token)).unwrap_err();
        assert!(matches!(err, ToolError::Cancelled { .. }));
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    fn cmd(script: &str) -> Command {
        let mut c = Command::new("cmd");
        c.args(["/C", script]);
        c
    }

    #[test]
    fn collects_lines_and_reports_failure() {
        let out = run_capture(&mut cmd("echo a&echo b"), Duration::from_secs(5), None).unwrap();
        assert_eq!(out, "a\nb\n");
        let err = run_capture(&mut cmd("exit 3"), Duration::from_secs(5), None).unwrap_err();
        assert!(matches!(err, ToolError::Failed { .. }));
    }

    #[test]
    fn timeout_kills_the_process() {
        let started = Instant::now();
        // Spawn ping directly: killing `cmd` would leave its ping child holding stdout.
        let mut ping = Command::new("ping");
        ping.args(["-n", "30", "127.0.0.1"]);
        let err = run_capture(&mut ping, Duration::from_millis(300), None).unwrap_err();
        assert!(matches!(err, ToolError::Timeout { .. }));
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
