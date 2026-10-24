//! Dedicated PTY lanes. The owner retains the actual child through reap;
//! output/input workers never own copied process IDs or signal another target.

use std::io::{Read, Write};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use super::*;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

const POLL: Duration = Duration::from_millis(25);

struct Child {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    reaped: bool,
}
impl Child {
    fn reap(&mut self) -> u32 {
        let pid = self.child.process_id();
        loop {
            match self.child.wait() {
                Ok(status) => {
                    self.reaped = true;
                    if let Some(pid) = pid {
                        crate::tools::terminate_reaped_child_discipline(pid);
                    }
                    return status.exit_code();
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => {
                    let _ = self.child.kill();
                    std::thread::sleep(POLL);
                }
            }
        }
    }
    fn monitor(&mut self, terminal: &Terminal, connected: &Connected) -> u32 {
        let pid = self.child.process_id();
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.reaped = true;
                    if let Some(pid) = pid {
                        crate::tools::terminate_reaped_child_discipline(pid);
                    }
                    return status.exit_code();
                }
                Ok(None) => {}
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    let _ = self.child.kill();
                    return self.reap();
                }
            }
            if terminal.stop.load(Ordering::SeqCst) || !connected() {
                terminal.stop.store(true, Ordering::SeqCst);
                crate::tools::kill_process_group_tree(self.child.process_id());
                let _ = self.child.kill();
                return self.reap();
            }
            std::thread::sleep(POLL);
        }
    }
}
impl Drop for Child {
    fn drop(&mut self) {
        if !self.reaped {
            crate::tools::kill_process_group_tree(self.child.process_id());
            let _ = self.child.kill();
            self.reap();
        }
    }
}

// Field order stops the child before dropping the terminal owner.
struct Running {
    child: Child,
    _master: Box<dyn MasterPty + Send>,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn run(
    launch: Launch,
    workspace: PathBuf,
    owner: AgentCx,
    connected: Connected,
    terminal: Arc<Terminal>,
    inputs: Receiver<Input>,
    started: futures::channel::oneshot::Sender<std::result::Result<u32, String>>,
    capacity: Arc<Capacity>,
) {
    let result = spawn(
        &launch, &workspace, &owner, &connected, &terminal, inputs, &capacity,
    );
    match result {
        Ok(mut running) => {
            let pid = running
                .child
                .child
                .process_id()
                .expect("spawn checks process id");
            *lock(&terminal.info) = json!({"state":"running","pid":pid});
            if terminal.stop.load(Ordering::SeqCst)
                || !connected()
                || owner.checkpoint().is_err()
                || started.send(Ok(pid)).is_err()
            {
                terminal.stop.store(true, Ordering::SeqCst);
            }
            let code = running.child.monitor(&terminal, &connected);
            terminal.ended.store(true, Ordering::SeqCst);
            *lock(&terminal.info) = json!({"state":"settled","pid":pid,"exitCode":code});
        }
        Err(error) => {
            terminal.stop.store(true, Ordering::SeqCst);
            terminal.ended.store(true, Ordering::SeqCst);
            *lock(&terminal.info) = json!({"state":"failed","error":error.to_string()});
            let _ = started.send(Err(error.to_string()));
        }
    }
}

fn spawn(
    launch: &Launch,
    workspace: &Path,
    owner: &AgentCx,
    connected: &Connected,
    terminal: &Arc<Terminal>,
    inputs: Receiver<Input>,
    capacity: &Arc<Capacity>,
) -> Result<Running> {
    check_owner(owner, true)?;
    if terminal.stop.load(Ordering::SeqCst) || !connected() {
        return Err(tool_err(
            "DAP_TERMINAL_CLOSED",
            "terminal launch revoked before allocation",
        ));
    }
    let cwd = launch.directory(workspace)?;
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| {
            tool_err(
                "DAP_TERMINAL_PTY",
                format!("cannot allocate terminal: {error}"),
            )
        })?;
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| tool_err("DAP_TERMINAL_PTY", format!("cannot read terminal: {error}")))?;
    let writer = pair.master.take_writer().map_err(|error| {
        tool_err(
            "DAP_TERMINAL_PTY",
            format!("cannot write terminal: {error}"),
        )
    })?;
    let mut command = CommandBuilder::new(&launch.args[0]);
    command.args(&launch.args[1..]);
    command.cwd(&cwd);
    if let Some(env) = &launch.env {
        for (key, value) in env {
            match value {
                Some(value) => command.env(key, value),
                None => command.env_remove(key),
            }
        }
    }
    // No implicit shell and no permission inherited from a reader thread.
    check_owner(owner, true)?;
    if terminal.stop.load(Ordering::SeqCst) || !connected() {
        return Err(tool_err(
            "DAP_TERMINAL_CLOSED",
            "terminal launch revoked before spawn",
        ));
    }
    let native = {
        let _owner = owner.cx().clone().set_current_restricted();
        pair.slave.spawn_command(command).map_err(|error| {
            tool_err(
                "DAP_TERMINAL_SPAWN",
                format!("cannot launch debuggee: {error}"),
            )
        })?
    };
    let child = Child {
        child: native,
        reaped: false,
    };
    drop(pair.slave);
    if child.child.process_id().is_none() {
        return Err(tool_err(
            "DAP_TERMINAL_SPAWN",
            "terminal child has no process identity",
        ));
    }
    let output_terminal = Arc::clone(terminal);
    let output_capacity = Arc::clone(capacity);
    std::thread::Builder::new()
        .name("dap-terminal-output".into())
        .spawn(move || {
            let _capacity = output_capacity;
            output(reader, &output_terminal);
        })
        .map_err(|error| {
            tool_err(
                "DAP_TERMINAL_PTY",
                format!("cannot start output lane: {error}"),
            )
        })?;
    let input_terminal = Arc::clone(terminal);
    let input_capacity = Arc::clone(capacity);
    std::thread::Builder::new()
        .name("dap-terminal-input".into())
        .spawn(move || {
            let _capacity = input_capacity;
            input(writer, inputs, &input_terminal);
        })
        .map_err(|error| {
            tool_err(
                "DAP_TERMINAL_PTY",
                format!("cannot start input lane: {error}"),
            )
        })?;
    Ok(Running {
        child,
        _master: pair.master,
    })
}

fn output(mut reader: Box<dyn Read + Send>, terminal: &Terminal) {
    let mut bytes = [0_u8; 4096];
    loop {
        match reader.read(&mut bytes) {
            Ok(0) => {
                lock(&terminal.tail).complete = true;
                return;
            }
            Ok(count) => lock(&terminal.tail).push(&bytes[..count]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                // Some PTY implementations normalize the Unix EIO-at-close
                // condition to EOF themselves. Other errors remain explicit.
                lock(&terminal.tail).error = Some(error.to_string());
                return;
            }
        }
        if terminal.stop.load(Ordering::SeqCst) {
            // This is a received prefix, not a claim that the pipe reached EOF.
            return;
        }
    }
}

fn input(mut writer: Box<dyn Write + Send>, inputs: Receiver<Input>, terminal: &Terminal) {
    while !terminal.stop.load(Ordering::SeqCst) && !terminal.ended.load(Ordering::SeqCst) {
        let input = match inputs.recv_timeout(POLL) {
            Ok(input) => input,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        if terminal.stop.load(Ordering::SeqCst) || terminal.ended.load(Ordering::SeqCst) {
            return;
        }
        if input
            .phase
            .compare_exchange(QUEUED, WRITING, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            continue;
        }
        let result = writer
            .write_all(&input.bytes)
            .and_then(|()| writer.flush())
            .map(|()| input.bytes.len())
            .map_err(|error| error.to_string());
        let failed = result.is_err();
        let _ = input.reply.try_send(result);
        if failed {
            return;
        }
    }
}
