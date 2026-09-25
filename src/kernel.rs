//! Kernel lifecycle + wire protocol. The UI owns a `Kernel` handle and an
//! `mpsc::Receiver<Event>`; three small tokio tasks pump the sockets.

use anyhow::{Context, Result, anyhow};
use jupyter_protocol::messaging::{
    CompleteRequest, ExecuteRequest, ExecutionState, InputReply, InspectRequest, InterruptRequest,
    JupyterMessage, JupyterMessageContent,
};
use jupyter_protocol::{ConnectionInfo, Transport};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::sync::mpsc;

/// Kernel-side happenings, applied to app state in the main loop.
pub enum Event {
    /// Background launch finished.
    Ready(Box<Kernel>),
    /// Background launch `launch` failed (reason for the status line).
    LaunchFailed { launch: u64, reason: String },
    /// nbformat-shaped output object for the cell that sent `parent`.
    Output { parent: String, output: Value },
    /// `execute_input` arrived: the kernel assigned an execution count.
    ExecutionCount { parent: String, count: i64 },
    /// update_display_data: replace every output carrying this display_id.
    UpdateDisplay { display_id: String, output: Value },
    /// complete_reply: `matches` replace the code from char `start` to the
    /// request's cursor;
    /// `types` maps a match to its (kind, signature) where IPython said
    /// (`_jupyter_types_experimental`; jedi fills signatures, the live
    /// completer only kinds).
    Complete {
        parent: String,
        matches: Vec<String>,
        start: usize,
        types: HashMap<String, (String, String)>,
    },
    /// inspect_reply: `text` is the text/plain doc (may carry ANSI colors).
    Inspect { parent: String, text: Option<String> },
    /// clear_output arrived; with `wait` the clear is deferred to next output.
    Clear { parent: String, wait: bool },
    /// The kernel asked for user input (`input()`); reply via `Kernel::reply_input`.
    Input {
        request: Box<JupyterMessage>,
        prompt: String,
        password: bool,
    },
    /// iopub status: kernel busy/idle while handling request `parent`.
    Status { parent: String, busy: bool },
    /// execute_reply arrived (the execution ends once its idle status is in too).
    Done { parent: String },
    /// The kernel process exited on its own (reason includes last stderr line).
    Dead(String),
    /// Human-readable status (launch errors, ...).
    Info(String),
}

pub struct Kernel {
    /// Resolved kernelspec name (may differ from the requested one).
    pub name: String,
    /// Directory the kernelspec was found in — disambiguates venv vs global.
    pub spec_dir: PathBuf,
    /// kernelspec display name and language (notebook metadata, highlighting).
    pub display_name: String,
    pub language: String,
    /// Which `App::spawn_kernel` call produced this kernel (stale-launch guard).
    pub launch: u64,
    session: String,
    shell_tx: mpsc::UnboundedSender<JupyterMessage>,
    control_tx: mpsc::UnboundedSender<JupyterMessage>,
    stdin_tx: mpsc::UnboundedSender<JupyterMessage>,
    pid: Option<u32>,
    /// Tells the monitor task to kill the child (fired on Drop).
    kill_tx: Option<tokio::sync::oneshot::Sender<()>>,
    /// kernelspec interrupt_mode == "message"; otherwise SIGINT the process.
    interrupt_via_message: bool,
    connection_file: PathBuf,
}

/// Kernelspec resolution ladder. For each candidate name (`requested`, then
/// `python3`): project venv kernels next to the notebook first, then global
/// dirs (JUPYTER_PATH, standard paths, `jupyter --paths` when available).
async fn resolve_kernelspec(
    requested: &str,
    notebook_dir: &std::path::Path,
) -> Result<jupyter_zmq_client::KernelspecDir> {
    // Activated env first ($VIRTUAL_ENV: uv/poetry/pdm/venv; $CONDA_PREFIX:
    // conda/mamba/pixi), then .venv/venv next to the notebook.
    let venv_dirs: Vec<PathBuf> = ["VIRTUAL_ENV", "CONDA_PREFIX"]
        .iter()
        .filter_map(std::env::var_os)
        .map(|p| PathBuf::from(p).join("share/jupyter"))
        .chain(
            [".venv", "venv"]
                .iter()
                .map(|v| notebook_dir.join(v).join("share/jupyter")),
        )
        .filter(|d| d.is_dir())
        .collect();
    let mut candidates = vec![requested.to_string()];
    if requested != "python3" {
        candidates.push("python3".into());
    }
    for name in &candidates {
        for dir in &venv_dirs {
            let specs = jupyter_zmq_client::read_kernelspec_jsons(dir).await;
            if let Some(mut spec) = specs.into_iter().find(|s| &s.kernel_name == name) {
                // ipykernel ships argv[0] = "python" (relative); jupyter clients
                // substitute their own interpreter. Ours is the venv's python.
                if let Some(arg0) = spec.kernelspec.argv.first_mut()
                    && !arg0.contains('/')
                    && arg0.starts_with("python")
                    && let Some(venv) = dir.parent().and_then(|p| p.parent())
                {
                    *arg0 = venv.join("bin/python").to_string_lossy().into_owned();
                }
                return Ok(spec);
            }
        }
        if let Ok(spec) = jupyter_zmq_client::find_kernelspec_with_jupyter_paths(name).await {
            return Ok(spec);
        }
    }
    Err(anyhow!(
        "no kernelspec found (tried {candidates:?}); pip install ipykernel in your venv, or pass --kernel"
    ))
}

impl Kernel {
    /// Resolve + launch a kernel and wire all channels. Run in the background;
    /// reports back as `Event::Ready` / `Event::Info` on `tx`.
    pub async fn launch(
        name: String,
        notebook_dir: PathBuf,
        tx: mpsc::UnboundedSender<Event>,
        launch: u64,
    ) {
        match Self::launch_inner(&name, &notebook_dir, tx.clone()).await {
            Ok(mut kernel) => {
                kernel.launch = launch;
                let _ = tx.send(Event::Ready(Box::new(kernel)));
            }
            Err(e) => {
                let reason = format!("kernel '{name}' failed: {e:#}");
                let _ = tx.send(Event::LaunchFailed { launch, reason });
            }
        }
    }

    async fn launch_inner(
        name: &str,
        notebook_dir: &std::path::Path,
        tx: mpsc::UnboundedSender<Event>,
    ) -> Result<Kernel> {
        // Absolute notebook dir: venv-relative argv stays valid, and the kernel
        // runs with cwd = notebook dir so relative paths in cells resolve like
        // they do in jupyterlab.
        let notebook_dir = notebook_dir
            .canonicalize()
            .unwrap_or_else(|_| notebook_dir.to_path_buf());
        let spec = resolve_kernelspec(name, &notebook_dir).await?;
        let resolved_name = spec.kernel_name.clone();
        let spec_dir = spec.path.clone();
        let display_name = spec.kernelspec.display_name.clone();
        let language = spec.kernelspec.language.clone();
        log::info!(
            "kernelspec '{resolved_name}' from {}, argv {:?}",
            spec_dir.display(),
            spec.kernelspec.argv
        );
        let interrupt_via_message = spec.kernelspec.interrupt_mode.as_deref() == Some("message");

        let ip: std::net::IpAddr = "127.0.0.1".parse().unwrap();
        let ports = jupyter_zmq_client::peek_ports(ip, 5)
            .await
            .map_err(|e| anyhow!("free ports: {e}"))?;
        let session = uuid::Uuid::new_v4().to_string();
        let info = ConnectionInfo {
            ip: "127.0.0.1".into(),
            transport: Transport::TCP,
            shell_port: ports[0],
            iopub_port: ports[1],
            stdin_port: ports[2],
            control_port: ports[3],
            hb_port: ports[4],
            key: uuid::Uuid::new_v4().to_string(),
            signature_scheme: "hmac-sha256".into(),
            kernel_name: Some(name.into()),
        };

        let dir = jupyter_zmq_client::runtime_dir();
        tokio::fs::create_dir_all(&dir).await.ok();
        let connection_file = dir.join(format!("jotter-{session}.json"));
        tokio::fs::write(&connection_file, serde_json::to_string(&info)?)
            .await
            .with_context(|| format!("writing {}", connection_file.display()))?;

        let mut cmd = spec
            .command(&connection_file, Some(std::process::Stdio::piped()), None)
            .map_err(|e| anyhow!("kernel argv: {e}"))?;
        cmd.current_dir(&notebook_dir);
        // Orphan protection for exits that skip Drop (signals, panics, the
        // runtime tearing down the monitor task): ipykernel's parent poller
        // exits once its parent is gone — what jupyter_client sets too.
        cmd.env("JPY_PARENT_PID", std::process::id().to_string());
        cmd.kill_on_drop(true);
        let mut child = cmd.spawn().context("spawning kernel process")?;
        let pid = child.id();
        log::info!(
            "kernel spawned, pid {pid:?}, connection file {}",
            connection_file.display()
        );

        // Keep the last stderr line: it's the diagnosis when the kernel dies
        // (and the pipe must be drained anyway or the kernel could block).
        let last_stderr = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        if let Some(stderr) = child.stderr.take() {
            let last = last_stderr.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let mut lines = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if !line.trim().is_empty() {
                        *last.lock().unwrap() = line;
                    }
                }
            });
        }

        // Monitor: report unexpected exit; kill on demand (Kernel::drop).
        let (kill_tx, mut kill_rx) = tokio::sync::oneshot::channel::<()>();
        let monitor_tx = tx.clone();
        tokio::spawn(async move {
            let status = tokio::select! {
                status = child.wait() => Some(status),
                _ = &mut kill_rx => None,
            };
            match status {
                Some(status) => {
                    let code = status.map_or_else(|e| e.to_string(), |s| s.to_string());
                    let stderr = last_stderr.lock().unwrap().clone();
                    log::warn!("kernel died ({code}): {stderr}");
                    let _ = monitor_tx.send(Event::Dead(format!("kernel died ({code}): {stderr}")));
                }
                None => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                }
            }
        });

        let mut iopub = jupyter_zmq_client::create_client_iopub_connection(&info, "", &session)
            .await
            .map_err(|e| anyhow!("iopub connect: {e}"))?;
        let identity =
            jupyter_zmq_client::peer_identity_for_session(&session).map_err(|e| anyhow!("{e}"))?;
        let shell = jupyter_zmq_client::create_client_shell_connection_with_identity(
            &info,
            &session,
            identity.clone(),
        )
        .await
        .map_err(|e| anyhow!("shell connect: {e}"))?;
        // stdin must share the shell's identity so input_request routes to us
        let stdin = jupyter_zmq_client::create_client_stdin_connection_with_identity(
            &info, &session, identity,
        )
        .await
        .map_err(|e| anyhow!("stdin connect: {e}"))?;
        let control = jupyter_zmq_client::create_client_control_connection(&info, &session)
            .await
            .map_err(|e| anyhow!("control connect: {e}"))?;

        // iopub pump: kernel outputs/status -> UI events
        let iopub_tx = tx.clone();
        tokio::spawn(async move {
            loop {
                match iopub.read().await {
                    Ok(msg) => {
                        if let Some(ev) = translate_iopub(msg)
                            && iopub_tx.send(ev).is_err()
                        {
                            break; // UI gone
                        }
                    }
                    Err(e) => {
                        log::warn!("iopub read failed: {e}");
                        let _ = iopub_tx.send(Event::Info(format!("kernel connection lost: {e}")));
                        break;
                    }
                }
            }
        });

        // shell: sender task + reply pump
        let (mut shell_send, mut shell_recv) = shell.split();
        let (shell_tx, mut shell_rx) = mpsc::unbounded_channel::<JupyterMessage>();
        tokio::spawn(async move {
            while let Some(msg) = shell_rx.recv().await {
                if shell_send.send(msg).await.is_err() {
                    break;
                }
            }
        });
        let reply_tx = tx.clone();
        tokio::spawn(async move {
            while let Ok(msg) = shell_recv.read().await {
                let parent = parent_id(&msg);
                let event = match msg.content {
                    JupyterMessageContent::ExecuteReply(_) => Event::Done { parent },
                    JupyterMessageContent::CompleteReply(x) => Event::Complete {
                        parent,
                        types: completion_types(&x.metadata),
                        matches: x.matches,
                        start: x.cursor_start,
                    },
                    JupyterMessageContent::InspectReply(x) => Event::Inspect {
                        parent,
                        text: x
                            .found
                            .then(|| serde_json::to_value(&x.data).ok())
                            .flatten()
                            .and_then(|d| d.get("text/plain")?.as_str().map(String::from)),
                    },
                    _ => continue,
                };
                if reply_tx.send(event).is_err() {
                    break;
                }
            }
        });

        // stdin: input_request -> UI event; input_reply queued back from the UI
        let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<JupyterMessage>();
        let (mut stdin_send, mut stdin_recv) = stdin.split();
        let input_tx = tx.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    msg = stdin_recv.read() => {
                        let Ok(msg) = msg else { break };
                        if let JupyterMessageContent::InputRequest(x) = &msg.content {
                            let (prompt, password) = (x.prompt.clone(), x.password);
                            if input_tx
                                .send(Event::Input { request: Box::new(msg), prompt, password })
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                    reply = stdin_rx.recv() => match reply {
                        Some(msg) => {
                            if stdin_send.send(msg).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    },
                }
            }
        });

        // control: sender task (replies are not interesting yet; the recv half
        // must stay alive or the socket closes, so the task holds both)
        let (control_tx, mut control_rx) = mpsc::unbounded_channel::<JupyterMessage>();
        let (mut control_send, control_recv) = control.split();
        tokio::spawn(async move {
            let _keep_alive = control_recv;
            while let Some(msg) = control_rx.recv().await {
                if control_send.send(msg).await.is_err() {
                    break;
                }
            }
        });

        Ok(Kernel {
            name: resolved_name,
            spec_dir,
            display_name,
            language,
            launch: 0,
            session,
            shell_tx,
            control_tx,
            stdin_tx,
            pid,
            kill_tx: Some(kill_tx),
            interrupt_via_message,
            connection_file,
        })
    }

    /// Queue an execute_request; returns its msg_id for routing outputs.
    pub fn execute(&self, code: String) -> String {
        let msg = JupyterMessage::new(
            ExecuteRequest {
                code,
                silent: false,
                store_history: true,
                user_expressions: None,
                allow_stdin: true,
                stop_on_error: true,
            },
            None,
        );
        self.shell(msg)
    }

    /// Run `code` silently (no outputs recorded, no history, no count):
    /// kernel setup like switching the completer.
    pub fn execute_silent(&self, code: &str) {
        let msg = JupyterMessage::new(
            ExecuteRequest {
                code: code.into(),
                silent: true,
                store_history: false,
                user_expressions: None,
                allow_stdin: false,
                stop_on_error: false,
            },
            None,
        );
        self.shell(msg);
    }

    /// Queue a complete_request; `cursor_pos` counts unicode code points.
    pub fn complete(&self, code: String, cursor_pos: usize) -> String {
        self.shell(JupyterMessage::new(CompleteRequest { code, cursor_pos }, None))
    }

    /// Queue an inspect_request (Shift+Tab docs) at `cursor_pos`.
    pub fn inspect(&self, code: String, cursor_pos: usize) -> String {
        let req = InspectRequest {
            code,
            cursor_pos,
            detail_level: Some(0),
        };
        self.shell(JupyterMessage::new(req, None))
    }

    /// Send on shell under our session; returns the msg_id replies carry.
    fn shell(&self, msg: JupyterMessage) -> String {
        let msg = msg.with_session(&self.session);
        let id = msg.header.msg_id.clone();
        let _ = self.shell_tx.send(msg);
        id
    }

    /// Answer an `input_request` (must be a child of the request message).
    pub fn reply_input(&self, request: &JupyterMessage, value: String) {
        let reply = JupyterMessage::new(
            InputReply {
                value,
                ..Default::default()
            },
            Some(request),
        )
        .with_session(&self.session);
        let _ = self.stdin_tx.send(reply);
    }

    pub fn interrupt(&self) {
        if self.interrupt_via_message {
            let msg = JupyterMessage::new(InterruptRequest {}, None).with_session(&self.session);
            let _ = self.control_tx.send(msg);
        } else if let Some(pid) = self.pid {
            // jupyter-client behavior for interrupt_mode=signal (ipykernel default)
            unsafe { libc::kill(pid as i32, libc::SIGINT) };
        }
    }
}

impl Drop for Kernel {
    fn drop(&mut self) {
        if let Some(kill) = self.kill_tx.take() {
            let _ = kill.send(()); // monitor task kills + reaps the child
        }
        let _ = std::fs::remove_file(&self.connection_file);
    }
}

/// `_jupyter_types_experimental` of a complete_reply: text -> (type, signature).
fn completion_types(meta: &serde_json::Map<String, Value>) -> HashMap<String, (String, String)> {
    let str_of = |e: &Value, k: &str| e.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    meta.get("_jupyter_types_experimental")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|e| (str_of(e, "text"), (str_of(e, "type"), str_of(e, "signature"))))
        .collect()
}

fn parent_id(msg: &JupyterMessage) -> String {
    msg.parent_header
        .as_ref()
        .map(|h| h.msg_id.clone())
        .unwrap_or_default()
}

fn translate_iopub(msg: JupyterMessage) -> Option<Event> {
    let parent = parent_id(&msg);
    use JupyterMessageContent as C;
    match msg.content {
        C::StreamContent(x) => nb_output(parent, "stream", x),
        C::ExecuteResult(x) => nb_output(parent, "execute_result", x),
        C::DisplayData(x) => nb_output(parent, "display_data", x),
        C::UpdateDisplayData(x) => {
            let display_id = x.transient.display_id.clone()?;
            match nb_output(parent, "display_data", x)? {
                Event::Output { output, .. } => Some(Event::UpdateDisplay { display_id, output }),
                _ => None,
            }
        }
        C::ErrorOutput(x) => nb_output(parent, "error", x),
        C::ClearOutput(x) => Some(Event::Clear {
            parent,
            wait: x.wait,
        }),
        C::ExecuteInput(x) => {
            let count = serde_json::to_value(x.execution_count).ok()?.as_i64()?;
            Some(Event::ExecutionCount { parent, count })
        }
        C::Status(x) => Some(Event::Status {
            parent,
            busy: matches!(x.execution_state, ExecutionState::Busy),
        }),
        _ => None,
    }
}

/// Serialize a protocol content struct and stamp the nbformat output_type.
fn nb_output<T: Serialize>(parent: String, ty: &str, x: T) -> Option<Event> {
    // `transient` (display_id) stays for update_display_data routing;
    // Notebook::save strips it
    let mut output = serde_json::to_value(x).ok()?;
    output["output_type"] = ty.into();
    Some(Event::Output { parent, output })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end against a real python kernel. Ignored by default because it
    /// needs ipykernel installed; run with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "spawns a real python kernel (needs ipykernel)"]
    async fn execute_roundtrip_on_real_kernel() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        Kernel::launch("python3".into(), std::env::temp_dir(), tx, 1).await;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
        async fn next(
            rx: &mut mpsc::UnboundedReceiver<Event>,
            deadline: tokio::time::Instant,
        ) -> Event {
            tokio::time::timeout_at(deadline, rx.recv())
                .await
                .expect("timed out waiting for kernel event")
                .expect("event channel closed")
        }
        let kernel = loop {
            match next(&mut rx, deadline).await {
                Event::Ready(k) => break *k,
                Event::LaunchFailed { reason, .. } => panic!("launch failed: {reason}"),
                _ => {}
            }
        };
        let msg_id = kernel.execute("print(21 * 2)".into());
        let mut out = String::new();
        loop {
            match next(&mut rx, deadline).await {
                Event::Output { parent, output } if parent == msg_id => {
                    out.push_str(&crate::notebook::join_multiline(&output["text"]));
                }
                Event::Done { parent } if parent == msg_id => break,
                Event::Dead(reason) => panic!("kernel died: {reason}"),
                _ => {}
            }
        }
        assert!(out.contains("42"), "expected 42 in stdout, got: {out:?}");
    }

    /// complete/inspect replies route back (ignored: needs ipykernel).
    #[tokio::test]
    #[ignore = "spawns a real python kernel (needs ipykernel)"]
    async fn complete_and_inspect_on_real_kernel() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        Kernel::launch("python3".into(), std::env::temp_dir(), tx, 1).await;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut next = async || {
            tokio::time::timeout_at(deadline, rx.recv())
                .await
                .expect("timed out waiting for kernel event")
                .expect("event channel closed")
        };
        let kernel = loop {
            if let Event::Ready(k) = next().await {
                break *k;
            }
        };
        let code = "import os\nos.getc";
        let id = kernel.complete(code.into(), code.chars().count());
        loop {
            if let Event::Complete { parent, matches, start, .. } = next().await
                && parent == id
            {
                assert!(matches.iter().any(|m| m.trim_start_matches('.') == "getcwd"), "{matches:?}");
                assert!(start == 13 || start == 12, "token start {start}");
                break;
            }
        }
        let id = kernel.inspect("len".into(), 3);
        loop {
            if let Event::Inspect { parent, text } = next().await
                && parent == id
            {
                assert!(text.is_some_and(|t| t.contains("len")));
                break;
            }
        }
    }
}
