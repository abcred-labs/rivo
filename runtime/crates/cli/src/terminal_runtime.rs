#![allow(dead_code)]

use anyhow::Result;
use crossterm::event::Event;
#[cfg(test)]
use std::collections::VecDeque;
use std::io::IsTerminal;
use std::time::Duration;
#[cfg(windows)]
use std::{fs::File, os::windows::io::AsRawHandle};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub(crate) struct TerminalCapabilities {
    pub interactive_input: bool,
    pub interactive_output: bool,
    pub unicode: bool,
    pub color: bool,
    pub bracketed_paste: bool,
    pub alternate_screen: bool,
    pub resize_events: bool,
}

pub(crate) fn detect_terminal_capabilities() -> TerminalCapabilities {
    let input = std::io::stdin().is_terminal() || windows_console_input();
    let output = std::io::stdout().is_terminal() || windows_console_output();
    let term = std::env::var("TERM").unwrap_or_default();
    let unicode = std::env::var("LC_ALL")
        .or_else(|_| std::env::var("LANG"))
        .map(|value| !value.eq_ignore_ascii_case("C"))
        .unwrap_or(true);
    let color = std::env::var_os("NO_COLOR").is_none()
        && (std::env::var_os("COLORTERM").is_some() || !term.contains("dumb"));
    TerminalCapabilities {
        interactive_input: input,
        interactive_output: output,
        unicode,
        color,
        bracketed_paste: input && output,
        alternate_screen: input && output,
        resize_events: input && output,
    }
}

pub(crate) trait TerminalRuntime {
    fn capabilities(&self) -> &TerminalCapabilities;
    fn enter(&mut self) -> Result<()>;
    fn leave(&mut self) -> Result<()>;
    fn read_event(&mut self, timeout: Duration) -> Result<Option<Event>>;
    fn size(&self) -> Result<(u16, u16)>;
}

#[cfg(test)]
pub(crate) struct ScriptedRuntime {
    capabilities: TerminalCapabilities,
    events: VecDeque<Event>,
    size: (u16, u16),
}

#[cfg(test)]
impl ScriptedRuntime {
    pub(crate) fn new(events: impl IntoIterator<Item = Event>, size: (u16, u16)) -> Self {
        Self {
            capabilities: TerminalCapabilities {
                interactive_input: true,
                interactive_output: true,
                unicode: true,
                color: true,
                bracketed_paste: true,
                alternate_screen: true,
                resize_events: true,
            },
            events: events.into_iter().collect(),
            size,
        }
    }
}

#[cfg(test)]
impl TerminalRuntime for ScriptedRuntime {
    fn capabilities(&self) -> &TerminalCapabilities {
        &self.capabilities
    }

    fn enter(&mut self) -> Result<()> {
        Ok(())
    }

    fn leave(&mut self) -> Result<()> {
        Ok(())
    }

    fn read_event(&mut self, _timeout: Duration) -> Result<Option<Event>> {
        Ok(self.events.pop_front())
    }

    fn size(&self) -> Result<(u16, u16)> {
        Ok(self.size)
    }
}

pub(crate) struct CrosstermRuntime {
    capabilities: TerminalCapabilities,
    entered: bool,
    screen_started: bool,
    #[cfg(not(windows))]
    raw_mode_enabled: bool,
    #[cfg(windows)]
    console_modes: Vec<ConsoleModeSnapshot>,
    #[cfg(windows)]
    console_input: Option<StandardHandleBinding>,
    #[cfg(windows)]
    console_output: Option<StandardHandleBinding>,
}

impl CrosstermRuntime {
    pub(crate) fn new() -> Self {
        Self {
            capabilities: detect_terminal_capabilities(),
            entered: false,
            screen_started: false,
            #[cfg(not(windows))]
            raw_mode_enabled: false,
            #[cfg(windows)]
            console_modes: Vec::new(),
            #[cfg(windows)]
            console_input: None,
            #[cfg(windows)]
            console_output: None,
        }
    }
}

impl TerminalRuntime for CrosstermRuntime {
    fn capabilities(&self) -> &TerminalCapabilities {
        &self.capabilities
    }

    fn enter(&mut self) -> Result<()> {
        if self.entered {
            return Ok(());
        }
        enter_transaction(
            self,
            |runtime| runtime.enter_inner(),
            |runtime| runtime.leave(),
        )
    }

    fn leave(&mut self) -> Result<()> {
        if !self.entered {
            return Ok(());
        }
        use crossterm::{
            cursor::Show, event::DisableBracketedPaste, execute, terminal::LeaveAlternateScreen,
        };
        // Each cleanup is attempted even when an earlier stage fails. Keep failed
        // stages owned so Drop can retry, including partially completed enter().
        let screen_result = if self.screen_started {
            let result = execute!(
                std::io::stdout(),
                LeaveAlternateScreen,
                DisableBracketedPaste,
                Show
            );
            if result.is_ok() {
                self.screen_started = false;
            }
            result.map_err(anyhow::Error::from)
        } else {
            Ok(())
        };
        #[cfg(windows)]
        let mode_result = {
            let mut result = Ok(());
            self.console_modes.retain_mut(|snapshot| {
                if let Err(error) = snapshot.restore() {
                    if result.is_ok() {
                        result = Err(error);
                    }
                    true
                } else {
                    false
                }
            });
            result
        };
        #[cfg(not(windows))]
        let mode_result = if self.raw_mode_enabled {
            let result = crossterm::terminal::disable_raw_mode();
            if result.is_ok() {
                self.raw_mode_enabled = false;
            }
            result.map_err(anyhow::Error::from)
        } else {
            Ok(())
        };
        #[cfg(windows)]
        let handle_result = {
            let input = restore_standard_handle(&mut self.console_input);
            let output = restore_standard_handle(&mut self.console_output);
            input.and(output)
        };
        #[cfg(not(windows))]
        let handle_result = Ok(());
        let result = screen_result.and(mode_result).and(handle_result);
        if result.is_ok() {
            self.entered = false;
        }
        result
    }

    fn read_event(&mut self, timeout: Duration) -> Result<Option<Event>> {
        read_terminal_event(timeout)
    }

    fn size(&self) -> Result<(u16, u16)> {
        Ok(crossterm::terminal::size()?)
    }
}

// Shared by real entry and terminal-free failure injection tests.
fn enter_transaction<T>(
    state: &mut T,
    enter: impl FnOnce(&mut T) -> Result<()>,
    rollback: impl FnOnce(&mut T) -> Result<()>,
) -> Result<()> {
    if let Err(error) = enter(state) {
        return match rollback(state) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(error.context(format!("终端进入失败，回滚也失败: {cleanup:#}"))),
        };
    }
    Ok(())
}

impl CrosstermRuntime {
    fn enter_inner(&mut self) -> Result<()> {
        use crossterm::{
            cursor::Hide,
            event::EnableBracketedPaste,
            execute,
            terminal::{enable_raw_mode, EnterAlternateScreen},
        };
        // Own cleanup before the first side effect, not after the last one.
        self.entered = true;
        #[cfg(windows)]
        {
            self.bind_conpty_standard_handles()?;
            self.console_modes
                .push(ConsoleModeSnapshot::capture("CONIN$")?);
            let output = ConsoleModeSnapshot::capture("CONOUT$")?;
            // Crossterm caches ANSI support process-wide. Since we restore the
            // original output mode, re-enable VT on every entry, not only once.
            output.set_mode(
                output.original
                    | windows_sys::Win32::System::Console::ENABLE_VIRTUAL_TERMINAL_PROCESSING,
            )?;
            self.console_modes.push(output);
        }
        enable_raw_mode()?;
        #[cfg(not(windows))]
        {
            self.raw_mode_enabled = true;
        }
        self.screen_started = true;
        execute!(
            std::io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste,
            Hide
        )?;
        Ok(())
    }
}

#[cfg(windows)]
impl CrosstermRuntime {
    fn bind_conpty_standard_handles(&mut self) -> Result<()> {
        use windows_sys::Win32::System::Console::{STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
        if !std::io::stdin().is_terminal() {
            self.console_input = Some(StandardHandleBinding::bind(STD_INPUT_HANDLE, "CONIN$")?);
        }
        if !std::io::stdout().is_terminal() {
            self.console_output = Some(StandardHandleBinding::bind(STD_OUTPUT_HANDLE, "CONOUT$")?);
        }
        Ok(())
    }
}

#[cfg(windows)]
struct ConsoleModeSnapshot {
    file: File,
    original: u32,
    restored: bool,
}

#[cfg(windows)]
impl ConsoleModeSnapshot {
    fn capture(device: &str) -> Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(device)?;
        let mut original = 0;
        if unsafe {
            windows_sys::Win32::System::Console::GetConsoleMode(file.as_raw_handle(), &mut original)
        } == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self {
            file,
            original,
            restored: false,
        })
    }

    fn set_mode(&self, mode: u32) -> Result<()> {
        if unsafe {
            windows_sys::Win32::System::Console::SetConsoleMode(self.file.as_raw_handle(), mode)
        } == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }

    fn restore(&mut self) -> Result<()> {
        if !self.restored {
            self.set_mode(self.original)?;
            self.restored = true;
        }
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for ConsoleModeSnapshot {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(windows)]
struct StandardHandleBinding {
    slot: u32,
    original: windows_sys::Win32::Foundation::HANDLE,
    replacement: Option<File>,
}

#[cfg(windows)]
impl StandardHandleBinding {
    fn bind(slot: u32, device: &str) -> Result<Self> {
        use windows_sys::Win32::System::Console::{GetStdHandle, SetStdHandle};
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(device)?;
        let original = unsafe { GetStdHandle(slot) };
        if unsafe { SetStdHandle(slot, file.as_raw_handle()) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self {
            slot,
            original,
            replacement: Some(file),
        })
    }

    fn restore(&mut self) -> Result<()> {
        if self.replacement.is_some() {
            if unsafe {
                windows_sys::Win32::System::Console::SetStdHandle(self.slot, self.original)
            } == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            // Never close a File while its handle is still a process standard handle.
            self.replacement.take();
        }
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for StandardHandleBinding {
    fn drop(&mut self) {
        if self.restore().is_err() {
            // A failed OS restore must not leave a dangling process handle. The
            // exceptional leak lasts until process exit; normal leave reports it.
            if let Some(file) = self.replacement.take() {
                std::mem::forget(file);
            }
        }
    }
}

#[cfg(windows)]
fn restore_standard_handle(binding: &mut Option<StandardHandleBinding>) -> Result<()> {
    if let Some(value) = binding.as_mut() {
        value.restore()?;
    }
    binding.take();
    Ok(())
}

fn read_terminal_event(timeout: Duration) -> Result<Option<Event>> {
    if !crossterm::event::poll(timeout)? {
        return Ok(None);
    }
    Ok(Some(crossterm::event::read()?))
}

pub(crate) struct TerminalInputReader {
    receiver: tokio::sync::mpsc::Receiver<Result<Event>>,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TerminalInputReader {
    pub(crate) fn new() -> Self {
        Self::spawn(read_terminal_event)
    }

    fn spawn(mut read: impl FnMut(Duration) -> Result<Option<Event>> + Send + 'static) -> Self {
        use std::sync::{atomic::Ordering, Arc};
        let (sender, receiver) = tokio::sync::mpsc::channel(64);
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader_shutdown = Arc::clone(&shutdown);
        let thread = std::thread::spawn(move || {
            while !reader_shutdown.load(Ordering::Relaxed) {
                let event = match read(Duration::from_millis(40)) {
                    Ok(Some(event)) => Ok(event),
                    Ok(None) => continue,
                    Err(error) => Err(error),
                };
                let failed = event.is_err();
                if sender.blocking_send(event).is_err() || failed {
                    break;
                }
            }
        });
        Self {
            receiver,
            shutdown,
            thread: Some(thread),
        }
    }

    pub(crate) async fn recv(&mut self) -> Result<Event> {
        self.receiver
            .recv()
            .await
            .unwrap_or_else(|| Err(anyhow::anyhow!("终端输入线程意外结束")))
    }

    pub(crate) fn stop(&mut self) -> Result<()> {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // close() wakes blocking_send even if all 64 queue slots are occupied.
        self.receiver.close();
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| anyhow::anyhow!("终端输入线程 panic"))?;
        }
        Ok(())
    }
}

impl Drop for TerminalInputReader {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

impl Drop for CrosstermRuntime {
    fn drop(&mut self) {
        let _ = self.leave();
    }
}

#[cfg(windows)]
fn windows_console_input() -> bool {
    std::fs::OpenOptions::new()
        .read(true)
        .open("CONIN$")
        .is_ok_and(|file| file.is_terminal())
}

#[cfg(not(windows))]
fn windows_console_input() -> bool {
    false
}

#[cfg(windows)]
fn windows_console_output() -> bool {
    std::fs::OpenOptions::new()
        .write(true)
        .open("CONOUT$")
        .is_ok_and(|file| file.is_terminal())
}

#[cfg(not(windows))]
fn windows_console_output() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn enter_failure_rolls_back_each_partially_completed_stage() {
        for fail_after in 1..=3 {
            let mut stages = Vec::new();
            let error = enter_transaction(
                &mut stages,
                |stages| {
                    for stage in 1..=fail_after {
                        stages.push(stage);
                    }
                    anyhow::bail!("injected entry failure")
                },
                |stages| {
                    stages.clear();
                    Ok(())
                },
            )
            .unwrap_err();
            assert_eq!(error.to_string(), "injected entry failure");
            assert!(stages.is_empty());
        }
    }

    #[test]
    fn enter_success_does_not_roll_back_and_failure_keeps_both_errors() {
        let mut active = false;
        enter_transaction(
            &mut active,
            |active| {
                *active = true;
                Ok(())
            },
            |_| panic!("successful entry must not roll back"),
        )
        .unwrap();
        assert!(active);
        let error = enter_transaction(
            &mut active,
            |_| anyhow::bail!("entry failure"),
            |_| anyhow::bail!("cleanup failure"),
        )
        .unwrap_err();
        let chain = format!("{error:#}");
        assert!(chain.contains("entry failure"));
        assert!(chain.contains("cleanup failure"));
    }

    fn assert_full_input_queue_stops(on_error: bool) {
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let owner = std::thread::spawn(move || {
            let (full_tx, full_rx) = std::sync::mpsc::channel();
            let mut count = 0;
            let mut reader = TerminalInputReader::spawn(move |_| {
                count += 1;
                if count == 65 {
                    // The previous 64 blocking_send calls have filled the queue.
                    full_tx.send(()).unwrap();
                }
                Ok(Some(Event::Resize(80, 24)))
            });
            full_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            if on_error {
                let fail = || -> Result<()> {
                    let _reader = reader;
                    anyhow::bail!("injected render/rebind failure")
                };
                assert!(fail().is_err());
            } else {
                reader.stop().unwrap();
                reader.stop().unwrap();
            }
            done_tx.send(()).unwrap();
        });
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("full queue must not deadlock join");
        owner.join().unwrap();
    }

    #[test]
    fn input_reader_closes_full_queue_before_join() {
        assert_full_input_queue_stops(false);
    }

    #[test]
    fn input_reader_error_exit_drops_and_joins_full_queue() {
        assert_full_input_queue_stops(true);
    }

    #[tokio::test]
    async fn input_reader_propagates_read_errors_and_closed_channel() {
        let mut reader = TerminalInputReader::spawn(|_| anyhow::bail!("injected read failure"));
        assert_eq!(
            reader.recv().await.unwrap_err().to_string(),
            "injected read failure"
        );
        assert!(reader
            .recv()
            .await
            .unwrap_err()
            .to_string()
            .contains("意外结束"));
        reader.stop().unwrap();
    }

    // All real console mutation runs in a dedicated ConPTY child, never the
    // unit-test runner's inherited console (even with parallel cargo tests).
    #[cfg(windows)]
    #[test]
    fn windows_mode_and_fallback_guards_in_isolated_conpty() {
        use portable_pty::{native_pty_system, CommandBuilder, PtySize};
        use std::io::{Read, Write};
        let pair = native_pty_system().openpty(PtySize::default()).unwrap();
        let mut command = CommandBuilder::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            "terminal_runtime::tests::windows_console_guard_child",
            "--ignored",
            "--nocapture",
        ]);
        command.env("RIVO_TEST_ISOLATED_CONSOLE_GUARD", "1");
        let mut child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let mut output = pair.master.try_clone_reader().unwrap();
        let mut input = pair.master.take_writer().unwrap();
        let drain = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let mut chunk = [0_u8; 4096];
            while let Ok(size) = output.read(&mut chunk) {
                if size == 0 {
                    break;
                }
                // Like the integration fixture, act as the terminal host and
                // answer ConPTY's cursor query. Keep overlap for split queries.
                let start = bytes.len().saturating_sub(3);
                bytes.extend_from_slice(&chunk[..size]);
                for query in bytes[start..].windows(4) {
                    if query == b"\x1b[6n" {
                        let _ = input.write_all(b"\x1b[1;1R");
                    }
                }
            }
            bytes
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status);
            }
            if std::time::Instant::now() >= deadline {
                // This handle belongs only to the test child, never a user process.
                let _ = child.kill();
                break None;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        drop(pair.master);
        let bytes = drain.join().unwrap();
        let status = status.unwrap_or_else(|| {
            panic!(
                "isolated console guard child timed out: {}",
                String::from_utf8_lossy(&bytes)
            )
        });
        assert!(status.success(), "{}", String::from_utf8_lossy(&bytes));
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "child-only: invoked by windows_mode_and_fallback_guards_in_isolated_conpty"]
    fn windows_console_guard_child() {
        use windows_sys::Win32::System::Console::{
            GetConsoleMode, GetStdHandle, SetConsoleMode, SetStdHandle, ENABLE_ECHO_INPUT,
            ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
            STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        };
        // Refuse to touch any console unless the isolated parent opted in.
        assert_eq!(
            std::env::var("RIVO_TEST_ISOLATED_CONSOLE_GUARD").as_deref(),
            Ok("1")
        );
        let original_input = ConsoleModeSnapshot::capture("CONIN$").unwrap();
        let original_output = ConsoleModeSnapshot::capture("CONOUT$").unwrap();
        let input_handle = original_input.file.as_raw_handle();
        let output_handle = original_output.file.as_raw_handle();
        let read_mode = |handle| {
            let mut mode = 0;
            assert_ne!(unsafe { GetConsoleMode(handle, &mut mode) }, 0);
            mode
        };
        // A non-default mode proves exact restore, not disable_raw_mode's OR-mask.
        let expected = original_input.original & !ENABLE_ECHO_INPUT;
        let expected_output = original_output.original & !ENABLE_VIRTUAL_TERMINAL_PROCESSING;
        assert_ne!(unsafe { SetConsoleMode(input_handle, expected) }, 0);
        assert_ne!(unsafe { SetConsoleMode(output_handle, expected_output) }, 0);
        for _ in 0..2 {
            let mut runtime = CrosstermRuntime::new();
            runtime.enter().unwrap();
            assert_eq!(
                read_mode(input_handle)
                    & (ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT),
                0
            );
            assert_ne!(
                read_mode(output_handle) & ENABLE_VIRTUAL_TERMINAL_PROCESSING,
                0
            );
            runtime.enter().unwrap();
            runtime.leave().unwrap();
            runtime.leave().unwrap();
            assert_eq!(read_mode(input_handle), expected);
            assert_eq!(read_mode(output_handle), expected_output);
        }
        // Simulate failure after raw mode was enabled, before screen entry.
        let mut runtime = CrosstermRuntime::new();
        let error = enter_transaction(
            &mut runtime,
            |runtime| {
                runtime.entered = true;
                runtime
                    .console_modes
                    .push(ConsoleModeSnapshot::capture("CONIN$")?);
                crossterm::terminal::enable_raw_mode()?;
                anyhow::bail!("injected screen entry failure")
            },
            |runtime| runtime.leave(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("injected"));
        assert!(!runtime.entered);
        assert_eq!(read_mode(input_handle), expected);

        let stdin = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        let stdout = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
        // Null standards force only this child's conditional fallback branch.
        assert_ne!(
            unsafe { SetStdHandle(STD_INPUT_HANDLE, std::ptr::null_mut()) },
            0
        );
        assert_ne!(
            unsafe { SetStdHandle(STD_OUTPUT_HANDLE, std::ptr::null_mut()) },
            0
        );
        let mut runtime = CrosstermRuntime::new();
        runtime.enter().unwrap();
        assert!(runtime.console_input.is_some());
        assert!(runtime.console_output.is_some());
        runtime.leave().unwrap();
        assert!(unsafe { GetStdHandle(STD_INPUT_HANDLE) }.is_null());
        assert!(unsafe { GetStdHandle(STD_OUTPUT_HANDLE) }.is_null());
        assert_ne!(unsafe { SetStdHandle(STD_INPUT_HANDLE, stdin) }, 0);
        assert_ne!(unsafe { SetStdHandle(STD_OUTPUT_HANDLE, stdout) }, 0);
        assert_eq!(read_mode(input_handle), expected);
        assert!(StandardHandleBinding::bind(u32::MAX - 100, "CONIN$").is_err());
    }

    #[test]
    fn capability_detection_is_consistent_for_noninteractive_streams() {
        let caps = detect_terminal_capabilities();
        if !caps.interactive_input || !caps.interactive_output {
            assert!(!caps.alternate_screen);
            assert!(!caps.bracketed_paste);
            assert!(!caps.resize_events);
        }
    }

    #[test]
    fn scripted_runtime_replays_events_and_size_without_a_terminal() {
        let mut runtime = ScriptedRuntime::new(
            [Event::Key(KeyEvent::new(
                KeyCode::Char('x'),
                KeyModifiers::NONE,
            ))],
            (80, 24),
        );
        runtime.enter().unwrap();
        assert_eq!(runtime.size().unwrap(), (80, 24));
        assert!(matches!(
            runtime.read_event(Duration::ZERO).unwrap(),
            Some(Event::Key(KeyEvent {
                code: KeyCode::Char('x'),
                ..
            }))
        ));
        assert!(runtime.read_event(Duration::ZERO).unwrap().is_none());
        runtime.leave().unwrap();
    }
}
