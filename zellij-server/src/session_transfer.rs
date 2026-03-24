use std::borrow::Cow;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

#[cfg(windows)]
use interprocess::local_socket::Stream as LocalSocketStream;
use serde::{Deserialize, Serialize};
use zellij_utils::consts::session_transfer_socket_file_name;
use zellij_utils::data::{PaneContents, PaneInfo};
use zellij_utils::input::layout::Run;

#[cfg(windows)]
use zellij_utils::consts::{ipc_bind, ipc_connect};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransferKind {
    Pane,
    Tab,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TransferredPane {
    pub pane_info: PaneInfo,
    pub invoked_with: Option<Run>,
    pub pane_contents: PaneContents,
    pub child_pid: Option<u32>,
    pub windows_pty_handles: Option<WindowsTransferredPtyHandles>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowsTransferredPtyHandles {
    pub conpty_handle: u64,
    pub input_write_handle: u64,
    pub output_read_handle: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionTransferRequest {
    pub transfer_kind: TransferKind,
    pub new_session: bool,
    pub target_session_name: String,
    pub target_tab_id: Option<usize>,
    pub source_tab_id: Option<usize>,
    pub panes: Vec<TransferredPane>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTransferResponse {
    pub success: bool,
    pub error: Option<String>,
}

pub fn seed_bytes_from_pane_contents(pane_contents: &PaneContents) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"\x1b[2J\x1b[H");

    let lines: Cow<'_, [String]> = if pane_contents.lines_below_viewport.is_empty() {
        Cow::Owned(
            pane_contents
                .lines_above_viewport
                .iter()
                .chain(pane_contents.viewport.iter())
                .cloned()
                .collect(),
        )
    } else {
        Cow::Borrowed(&pane_contents.viewport)
    };

    for (line_index, line) in lines.iter().enumerate() {
        if line_index > 0 {
            bytes.push(b'\n');
        }
        bytes.extend_from_slice(line.as_bytes());
    }
    if !lines.is_empty() {
        bytes.push(b'\n');
    }
    bytes
}

#[cfg(any(unix, windows))]
pub fn session_transfer_socket_path(session_name: &str) -> PathBuf {
    session_transfer_socket_file_name(session_name)
}

#[cfg(unix)]
pub fn bind_session_transfer_socket(
    session_name: &str,
) -> io::Result<std::os::unix::net::UnixListener> {
    use std::os::unix::net::UnixListener;

    let socket_path = session_transfer_socket_path(session_name);
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(&socket_path);
    UnixListener::bind(socket_path)
}

#[cfg(windows)]
pub fn bind_session_transfer_socket(
    session_name: &str,
) -> io::Result<interprocess::local_socket::Listener> {
    let socket_path = session_transfer_socket_path(session_name);
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(&socket_path);
    ipc_bind(&socket_path)
}

#[cfg(unix)]
pub fn connect_session_transfer_socket(
    session_name: &str,
    timeout: Duration,
) -> io::Result<std::os::unix::net::UnixStream> {
    use std::os::unix::net::UnixStream;
    let socket_path = session_transfer_socket_path(session_name);
    let started = Instant::now();
    loop {
        match UnixStream::connect(&socket_path) {
            Ok(stream) => return Ok(stream),
            Err(err) if started.elapsed() >= timeout => return Err(err),
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

#[cfg(windows)]
pub fn connect_session_transfer_socket(
    session_name: &str,
    timeout: Duration,
) -> io::Result<LocalSocketStream> {
    let socket_path = session_transfer_socket_path(session_name);
    let started = Instant::now();
    loop {
        match ipc_connect(&socket_path) {
            Ok(stream) => return Ok(stream),
            Err(err) if started.elapsed() >= timeout => return Err(err),
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

#[cfg(windows)]
pub fn session_transfer_target_pid(session_name: &str) -> io::Result<u32> {
    let marker_path = session_transfer_socket_path(session_name);
    let pid = std::fs::read_to_string(marker_path)?;
    pid.trim()
        .parse()
        .map_err(|err| io::Error::other(format!("invalid session transfer pid marker: {err}")))
}

fn write_len_prefixed_json<W: Write, T: Serialize>(writer: &mut W, value: &T) -> io::Result<()> {
    let payload = serde_json::to_vec(value).map_err(io::Error::other)?;
    let len_bytes = (payload.len() as u64).to_be_bytes();
    writer.write_all(&len_bytes)?;
    writer.write_all(&payload)?;
    writer.flush()?;
    Ok(())
}

fn read_len_prefixed_json<R: Read, T: for<'de> Deserialize<'de>>(reader: &mut R) -> io::Result<T> {
    let mut len_bytes = [0u8; 8];
    reader.read_exact(&mut len_bytes)?;
    let len = u64::from_be_bytes(len_bytes) as usize;
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload)?;
    serde_json::from_slice(&payload).map_err(io::Error::other)
}

#[cfg(unix)]
pub fn send_request_with_fds(
    stream: &std::os::unix::net::UnixStream,
    request: &SessionTransferRequest,
    fds: &[i32],
) -> io::Result<()> {
    use nix::sys::socket::{sendmsg, ControlMessage, MsgFlags};
    use nix::sys::uio::IoVec;
    use std::os::unix::io::AsRawFd;

    let payload = serde_json::to_vec(request).map_err(io::Error::other)?;
    let len_bytes = (payload.len() as u64).to_be_bytes();
    let cmsgs = if fds.is_empty() {
        Vec::new()
    } else {
        vec![ControlMessage::ScmRights(fds)]
    };
    let iov = [IoVec::from_slice(&len_bytes)];
    sendmsg(stream.as_raw_fd(), &iov, &cmsgs, MsgFlags::empty(), None).map_err(io::Error::other)?;
    let mut stream = stream;
    stream.write_all(&payload)?;
    stream.flush()?;
    Ok(())
}

#[cfg(windows)]
pub fn send_request_with_fds(
    stream: &mut LocalSocketStream,
    request: &SessionTransferRequest,
    _fds: &[i32],
) -> io::Result<()> {
    write_len_prefixed_json(stream, request)
}

#[cfg(unix)]
pub fn recv_request_with_fds(
    stream: &std::os::unix::net::UnixStream,
) -> io::Result<(SessionTransferRequest, Vec<i32>)> {
    use nix::sys::socket::{recvmsg, ControlMessageOwned, MsgFlags};
    use nix::sys::uio::IoVec;
    use std::os::unix::io::AsRawFd;

    let mut len_bytes = [0u8; 8];
    let mut len_read = 0usize;
    let mut received_fds = Vec::new();

    while len_read < len_bytes.len() {
        let mut cmsgspace = nix::cmsg_space!([i32; 256]);
        let iov = [IoVec::from_mut_slice(&mut len_bytes[len_read..])];
        let msg = recvmsg(
            stream.as_raw_fd(),
            &iov,
            Some(&mut cmsgspace),
            MsgFlags::empty(),
        )
        .map_err(io::Error::other)?;
        if msg.bytes == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "session transfer socket closed before length prefix was read",
            ));
        }
        len_read += msg.bytes;
        for cmsg in msg.cmsgs() {
            if let ControlMessageOwned::ScmRights(mut rights) = cmsg {
                received_fds.append(&mut rights);
            }
        }
    }

    let payload_len = u64::from_be_bytes(len_bytes) as usize;
    let mut payload = vec![0u8; payload_len];
    let mut stream = stream;
    stream.read_exact(&mut payload)?;
    let request = serde_json::from_slice(&payload).map_err(io::Error::other)?;
    Ok((request, received_fds))
}

#[cfg(windows)]
pub fn recv_request_with_fds(
    stream: &mut LocalSocketStream,
) -> io::Result<(SessionTransferRequest, Vec<i32>)> {
    let request = read_len_prefixed_json(stream)?;
    Ok((request, vec![]))
}

pub fn send_response<W: Write>(
    writer: &mut W,
    response: &SessionTransferResponse,
) -> io::Result<()> {
    write_len_prefixed_json(writer, response)
}

pub fn recv_response<R: Read>(reader: &mut R) -> io::Result<SessionTransferResponse> {
    read_len_prefixed_json(reader)
}

pub fn build_detached_session_command(session_name: &str) -> io::Result<Command> {
    let mut command = Command::new(std::env::current_exe()?);
    command.arg("attach");
    command.arg(session_name);
    command.arg("-b");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use std::process::Stdio;
        use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};

        command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
        command.stdin(Stdio::null());
        command.stdout(Stdio::null());
        command.stderr(Stdio::null());
    }
    Ok(command)
}

pub fn spawn_detached_session(session_name: &str) -> io::Result<()> {
    let status = build_detached_session_command(session_name)?.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "failed to start detached session {session_name:?}: {status}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::io::AsRawFd;
    #[cfg(unix)]
    use std::os::unix::net::UnixStream;

    #[test]
    fn seed_bytes_prefers_viewport_when_scrolled_up() {
        let contents = PaneContents {
            lines_above_viewport: vec!["above".to_string()],
            lines_below_viewport: vec!["below".to_string()],
            viewport: vec!["visible".to_string()],
            selected_text: None,
        };
        let bytes = seed_bytes_from_pane_contents(&contents);
        assert!(bytes.ends_with(b"visible\n"));
        assert!(!bytes.windows(5).any(|window| window == b"above"));
    }

    #[test]
    fn seed_bytes_includes_scrollback_at_bottom() {
        let contents = PaneContents {
            lines_above_viewport: vec!["above".to_string()],
            lines_below_viewport: vec![],
            viewport: vec!["visible".to_string()],
            selected_text: None,
        };
        let bytes = seed_bytes_from_pane_contents(&contents);
        let rendered = String::from_utf8(bytes).unwrap();
        assert!(rendered.contains("above"));
        assert!(rendered.contains("visible"));
    }

    #[test]
    fn build_detached_session_command_uses_attach_background_args() {
        let command = build_detached_session_command("transfer-test")
            .expect("expected detached session command to build");
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert_eq!(args, vec!["attach", "transfer-test", "-b"]);
    }

    #[cfg(unix)]
    #[test]
    fn request_round_trip_preserves_fds() {
        let (left, right) = UnixStream::pair().unwrap();
        let temp = tempfile::tempfile().unwrap();
        let fd = temp.as_raw_fd();
        let request = SessionTransferRequest {
            transfer_kind: TransferKind::Pane,
            new_session: true,
            target_session_name: "test-session".to_string(),
            target_tab_id: Some(7),
            source_tab_id: Some(3),
            panes: vec![TransferredPane {
                pane_info: PaneInfo::default(),
                invoked_with: None,
                pane_contents: PaneContents::default(),
                child_pid: Some(42),
                windows_pty_handles: None,
            }],
        };

        send_request_with_fds(&left, &request, &[fd]).unwrap();
        let (decoded, fds) = recv_request_with_fds(&right).unwrap();

        assert_eq!(decoded, request);
        assert_eq!(fds.len(), 1);
    }

    #[test]
    fn response_round_trip() {
        let response = SessionTransferResponse {
            success: true,
            error: None,
        };

        let mut bytes = Vec::new();
        send_response(&mut bytes, &response).unwrap();
        let decoded = recv_response(&mut bytes.as_slice()).unwrap();
        assert_eq!(decoded, response);
    }
}
