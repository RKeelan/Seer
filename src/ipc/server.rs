//! The viewer's pipe server: accepts concurrent clients, reads one request
//! from each, hands it to the UI thread and writes back the reply.
//!
//! A listening thread accepts connections and gives each its own thread, so a
//! client waiting on `marks --wait` never holds up another. The connection
//! thread reads the request and passes it, with a [`Responder`], to the
//! server's [`Handler`]. The handler may answer at once or much later, from
//! any thread; the viewer's handler posts the request to the UI thread, which
//! never blocks on a pipe.
//!
//! The pipe admits only the user who started the viewer, refuses clients on
//! other machines, and is created with `FILE_FLAG_FIRST_PIPE_INSTANCE`, so a
//! second server on the same instance fails rather than sharing the name.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_PIPE_CONNECTED, HANDLE, HLOCAL, LocalFree,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, PeekNamedPipe,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::{HSTRING, PWSTR};

use crate::error::{Error, ErrorCode, Result};
use crate::ipc::client;
use crate::ipc::instance::InstanceName;
use crate::model::app::RequestId;
use crate::protocol::{self, Request, Response};

/// The longest request line the server reads, in bytes.
const MAX_REQUEST: u64 = 16 * 1024 * 1024;

/// The size of each pipe instance's buffers, in bytes.
const BUFFER_SIZE: u32 = 64 * 1024;

/// How often a connection waiting for its reply checks that its client is
/// still there.
const DISCONNECT_POLL: Duration = Duration::from_millis(100);

/// What the server does with requests.
pub trait Handler: Send + Sync + 'static {
    /// Handles a request. The handler answers through `responder`, now or
    /// later and from any thread. Dropping it unanswered answers with an
    /// error.
    fn request(&self, request: Request, responder: Responder);

    /// Says that the client of an unanswered request has gone. Answering it
    /// afterwards does nothing.
    fn disconnected(&self, id: RequestId) {
        let _ = id;
    }
}

impl<F> Handler for F
where
    F: Fn(Request, Responder) + Send + Sync + 'static,
{
    fn request(&self, request: Request, responder: Responder) {
        self(request, responder);
    }
}

/// The way back to the client that sent a request.
#[derive(Debug)]
pub struct Responder {
    id: RequestId,
    sender: Sender<Response>,
}

impl Responder {
    /// The request's id, unique for the life of the server.
    pub fn id(&self) -> RequestId {
        self.id
    }

    /// Answers the request. If the client has gone, the answer is dropped.
    pub fn respond(self, response: impl Into<Response>) {
        let _ = self.sender.send(response.into());
    }
}

/// A running pipe server. Dropping it stops accepting clients; connections
/// already accepted carry on until they are answered.
#[derive(Debug)]
pub struct Server {
    instance: InstanceName,
    stop: Arc<AtomicBool>,
    listener: Option<JoinHandle<()>>,
}

impl Server {
    /// Opens the instance's pipe and starts accepting clients.
    ///
    /// Fails with [`ErrorCode::AlreadyRunning`] if another server has the
    /// pipe.
    pub fn start(instance: &InstanceName, handler: impl Handler) -> Result<Server> {
        let security = CurrentUserOnly::new()?;
        let name = HSTRING::from(instance.pipe_name());
        let first = create_pipe(&name, &security, true).map_err(|error| {
            if error.code() == ERROR_ACCESS_DENIED.to_hresult() {
                Error::new(
                    ErrorCode::AlreadyRunning,
                    format!("a viewer already serves instance {instance}"),
                )
            } else {
                error.into()
            }
        })?;
        let stop = Arc::new(AtomicBool::new(false));
        let listener = {
            let stop = Arc::clone(&stop);
            let handler: Arc<dyn Handler> = Arc::new(handler);
            thread::Builder::new()
                .name("seer-pipe".into())
                .spawn(move || listen(first, &name, &security, &stop, &handler))?
        };
        Ok(Server {
            instance: instance.clone(),
            stop,
            listener: Some(listener),
        })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // The listener is blocked waiting for a client; be that client.
        let deadline = Instant::now() + Duration::from_secs(2);
        let woken = loop {
            match client::open_pipe(&self.instance.pipe_name()) {
                Ok(_) => break true,
                Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                Err(_) => break false,
            }
        };
        if let Some(listener) = self.listener.take()
            && woken
        {
            let _ = listener.join();
        }
    }
}

/// Accepts clients until told to stop, keeping one instance of the pipe
/// waiting at all times so the name never lapses while the server runs.
fn listen(
    mut waiting: File,
    name: &HSTRING,
    security: &CurrentUserOnly,
    stop: &AtomicBool,
    handler: &Arc<dyn Handler>,
) {
    let next_id = AtomicU64::new(1);
    loop {
        // SAFETY: the handle is the pipe instance `waiting` owns, and the call
        // is synchronous.
        let connected = match unsafe { ConnectNamedPipe(handle_of(&waiting), None) } {
            Ok(()) => true,
            Err(error) => error.code() == ERROR_PIPE_CONNECTED.to_hresult(),
        };
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let next = loop {
            match create_pipe(name, security, false) {
                Ok(pipe) => break pipe,
                Err(_) if stop.load(Ordering::SeqCst) => return,
                Err(_) => thread::sleep(Duration::from_millis(50)),
            }
        };
        let pipe = std::mem::replace(&mut waiting, next);
        if connected {
            let id = RequestId(next_id.fetch_add(1, Ordering::Relaxed));
            let handler = Arc::clone(handler);
            let _ = thread::Builder::new()
                .name("seer-client".into())
                .spawn(move || serve(&pipe, id, handler.as_ref()));
        }
    }
}

/// Reads one request from a connected client, has it handled, and writes the
/// response.
fn serve(pipe: &File, id: RequestId, handler: &dyn Handler) {
    let mut line = String::new();
    match BufReader::new(pipe.take(MAX_REQUEST)).read_line(&mut line) {
        Ok(0) | Err(_) => return,
        Ok(_) => {}
    }
    let response = match protocol::decode_request(&line) {
        Err(error) => Response::Error(error),
        Ok(request) => {
            let (sender, receiver) = mpsc::channel();
            handler.request(request, Responder { id, sender });
            match wait_for_reply(&receiver, pipe) {
                Some(response) => response,
                None => {
                    handler.disconnected(id);
                    return;
                }
            }
        }
    };
    let mut line = protocol::encode_response(&response).unwrap_or_else(|error| {
        protocol::encode_response(&Response::Error(error)).expect("an error always encodes")
    });
    line.push('\n');
    if (&*pipe).write_all(line.as_bytes()).is_ok() {
        // Wait for the client to read the response before the pipe closes.
        let _ = pipe.sync_all();
    }
}

/// Waits for the handler's answer, or returns `None` if the client goes away
/// first.
fn wait_for_reply(receiver: &Receiver<Response>, pipe: &File) -> Option<Response> {
    loop {
        match receiver.recv_timeout(DISCONNECT_POLL) {
            Ok(response) => return Some(response),
            Err(RecvTimeoutError::Disconnected) => {
                return Some(Response::Error(Error::unexpected(
                    "the viewer dropped the request without answering",
                )));
            }
            Err(RecvTimeoutError::Timeout) => {
                // SAFETY: the handle is the connected pipe `pipe` owns, and
                // PeekNamedPipe is asked for nothing but success or failure.
                let open = unsafe { PeekNamedPipe(handle_of(pipe), None, 0, None, None, None) };
                if open.is_err() {
                    return None;
                }
            }
        }
    }
}

/// Creates one instance of the pipe.
fn create_pipe(
    name: &HSTRING,
    security: &CurrentUserOnly,
    first: bool,
) -> windows::core::Result<File> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security.descriptor.0,
        bInheritHandle: false.into(),
    };
    let mut open_mode = PIPE_ACCESS_DUPLEX;
    if first {
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    // SAFETY: the name and the security attributes, with the descriptor they
    // point to, outlive the call.
    let handle = unsafe {
        CreateNamedPipeW(
            name,
            open_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            BUFFER_SIZE,
            BUFFER_SIZE,
            0,
            Some(&attributes),
        )
    };
    if handle.is_invalid() {
        return Err(windows::core::Error::from_thread());
    }
    // SAFETY: the handle is a valid pipe that nothing else owns.
    Ok(File::from(unsafe {
        OwnedHandle::from_raw_handle(handle.0)
    }))
}

fn handle_of(pipe: &File) -> HANDLE {
    HANDLE(pipe.as_raw_handle())
}

/// A security descriptor whose access list admits only the current user.
#[derive(Debug)]
struct CurrentUserOnly {
    descriptor: PSECURITY_DESCRIPTOR,
}

// SAFETY: the descriptor is allocated once, never changed, only read by
// CreateNamedPipeW, and freed once on drop, so any thread may hold it.
unsafe impl Send for CurrentUserOnly {}

impl CurrentUserOnly {
    fn new() -> Result<CurrentUserOnly> {
        let sddl = HSTRING::from(format!("D:P(A;;GA;;;{})", current_user_sid()?));
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: the string outlives the call, and `descriptor` receives a
        // LocalAlloc'd descriptor that `Drop` frees.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                &sddl,
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
        }?;
        Ok(CurrentUserOnly { descriptor })
    }
}

impl Drop for CurrentUserOnly {
    fn drop(&mut self) {
        // SAFETY: the descriptor was allocated with LocalAlloc and is freed
        // only here.
        unsafe { LocalFree(Some(HLOCAL(self.descriptor.0))) };
    }
}

/// The current user's security identifier, as a string such as `S-1-5-21-…`.
fn current_user_sid() -> Result<String> {
    let mut token = HANDLE::default();
    // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no closing,
    // and `token` outlives the call.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }?;
    // SAFETY: the token handle is valid and nothing else owns it.
    let token = unsafe { OwnedHandle::from_raw_handle(token.0) };
    let token = HANDLE(token.as_raw_handle());

    let mut length = 0;
    // SAFETY: with no buffer, the call only reports the size it needs; it
    // fails with ERROR_INSUFFICIENT_BUFFER, which is expected.
    let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut length) };
    // A buffer of u64 is aligned well enough for TOKEN_USER.
    let mut buffer = vec![0u64; (length as usize).div_ceil(8)];
    // SAFETY: the buffer holds at least `length` bytes and outlives the call.
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            length,
            &mut length,
        )
    }?;
    // SAFETY: the call filled the buffer with a TOKEN_USER, whose SID points
    // into the same buffer, which lives until the end of this function.
    let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut text = PWSTR::null();
    // SAFETY: the SID is valid, and `text` receives a LocalAlloc'd string that
    // is freed below.
    unsafe { ConvertSidToStringSidW(sid, &mut text) }?;
    // SAFETY: the string is valid and null-terminated.
    let result = unsafe { text.to_string() };
    // SAFETY: the string was allocated with LocalAlloc and is freed only here.
    unsafe { LocalFree(Some(HLOCAL(text.0.cast()))) };
    result.map_err(|_| Error::unexpected("the user's security identifier is not valid UTF-16"))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;

    use super::*;
    use crate::ipc::client::{self, connect};
    use crate::protocol::{OpenFile, Reply};
    use crate::testutil::unique_instance;

    fn instance() -> InstanceName {
        InstanceName::new(&unique_instance()).unwrap()
    }

    /// Answers `Quit` with `Done`, echoes an `Open`'s paths back, and answers
    /// everything else "not implemented".
    fn stub(request: Request, responder: Responder) {
        let response = match request {
            Request::Quit => Ok(Reply::Done),
            Request::Open { files, .. } => Ok(Reply::Opened(protocol::OpenReply {
                images: files
                    .into_iter()
                    .enumerate()
                    .map(|(index, file)| protocol::OpenedImage {
                        id: crate::model::ImageId(index as u32 + 1),
                        path: file.path,
                        existing: false,
                    })
                    .collect(),
                failed: vec![],
            })),
            _ => Err(Error::new(ErrorCode::NotImplemented, "not implemented")),
        };
        responder.respond(response);
    }

    fn open_one(path: &str) -> Request {
        Request::Open {
            files: vec![OpenFile {
                path: PathBuf::from(path),
                caption: None,
            }],
            group: None,
            compare: false,
            activate: false,
        }
    }

    #[test]
    fn answers_a_request() {
        let instance = instance();
        let _server = Server::start(&instance, stub).unwrap();
        assert_eq!(
            client::send(&instance, &Request::Quit).unwrap(),
            Reply::Done
        );
        let error = client::send(&instance, &Request::List).unwrap_err();
        assert_eq!(error.code, ErrorCode::NotImplemented);
    }

    #[test]
    fn twenty_clients_at_once_lose_nothing() {
        let instance = instance();
        let _server = Server::start(&instance, stub).unwrap();
        let clients: Vec<_> = (0..20)
            .map(|n| {
                let instance = instance.clone();
                thread::spawn(move || {
                    let path = format!(r"C:\images\{n}.png");
                    match client::send(&instance, &open_one(&path)).unwrap() {
                        Reply::Opened(reply) => reply.images[0].path.clone(),
                        other => panic!("unexpected reply {other:?}"),
                    }
                })
            })
            .collect();
        let mut paths: Vec<_> = clients.into_iter().map(|c| c.join().unwrap()).collect();
        paths.sort();
        let mut expected: Vec<_> = (0..20)
            .map(|n| PathBuf::from(format!(r"C:\images\{n}.png")))
            .collect();
        expected.sort();
        assert_eq!(paths, expected);
    }

    #[test]
    fn a_waiting_client_does_not_hold_up_others() {
        let instance = instance();
        let parked = Arc::new(Mutex::new(None::<Responder>));
        let server = {
            let parked = Arc::clone(&parked);
            Server::start(&instance, move |request, responder: Responder| {
                if matches!(request, Request::Marks { .. }) {
                    *parked.lock().unwrap() = Some(responder);
                } else {
                    stub(request, responder);
                }
            })
            .unwrap()
        };
        let waiter = {
            let instance = instance.clone();
            let marks = Request::Marks {
                wait: true,
                timeout_ms: None,
                clear: false,
            };
            thread::spawn(move || client::send(&instance, &marks))
        };
        while parked.lock().unwrap().is_none() {
            thread::sleep(Duration::from_millis(5));
        }
        // Other clients come and go while the first waits.
        for _ in 0..3 {
            assert_eq!(
                client::send(&instance, &Request::Quit).unwrap(),
                Reply::Done
            );
        }
        let responder = parked.lock().unwrap().take().unwrap();
        responder.respond(Ok(Reply::Marks { marks: vec![] }));
        assert_eq!(
            waiter.join().unwrap().unwrap(),
            Reply::Marks { marks: vec![] }
        );
        drop(server);
    }

    #[test]
    fn a_vanished_client_is_reported() {
        let instance = instance();
        let (sender, receiver) = mpsc::channel();
        struct Reporter(Mutex<Sender<RequestId>>, Mutex<Vec<Responder>>);
        impl Handler for Reporter {
            fn request(&self, _request: Request, responder: Responder) {
                self.1.lock().unwrap().push(responder);
            }
            fn disconnected(&self, id: RequestId) {
                self.0.lock().unwrap().send(id).unwrap();
            }
        }
        let _server = Server::start(
            &instance,
            Reporter(Mutex::new(sender), Mutex::new(Vec::new())),
        )
        .unwrap();
        {
            // Send a request, then go away without reading the answer.
            let pipe = client::open_pipe(&instance.pipe_name()).unwrap();
            let mut line = protocol::encode_request(&Request::List).unwrap();
            line.push('\n');
            (&pipe).write_all(line.as_bytes()).unwrap();
            thread::sleep(Duration::from_millis(50));
        }
        let id = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(id.0 >= 1);
    }

    #[test]
    fn the_server_reports_its_process() {
        let instance = instance();
        let _server = Server::start(&instance, stub).unwrap();
        let connection = connect(&instance).unwrap();
        assert_eq!(connection.server_process_id().unwrap(), std::process::id());
        connection.allow_foreground();
        let activate = Request::Open {
            files: vec![],
            group: None,
            compare: false,
            activate: true,
        };
        assert!(matches!(connection.send(&activate), Ok(Reply::Opened(_))));
    }

    #[test]
    fn a_dropped_request_is_answered_with_an_error() {
        let instance = instance();
        let _server = Server::start(&instance, |_request, _responder| {}).unwrap();
        let error = client::send(&instance, &Request::List).unwrap_err();
        assert_eq!(error.code, ErrorCode::Unexpected);
        assert!(error.message.contains("dropped"), "{error}");
    }

    #[test]
    fn a_malformed_request_is_answered_with_an_error() {
        let instance = instance();
        let _server = Server::start(&instance, stub).unwrap();
        let pipe = client::open_pipe(&instance.pipe_name()).unwrap();
        (&pipe).write_all(b"{\"v\":1,\"cmd\":\"dance\"}\n").unwrap();
        let mut line = String::new();
        BufReader::new(&pipe).read_line(&mut line).unwrap();
        let error = Result::<Reply>::from(protocol::decode_response(&line).unwrap()).unwrap_err();
        assert_eq!(error.code, ErrorCode::Protocol);
    }

    #[test]
    fn a_second_server_on_the_same_instance_fails_cleanly() {
        let instance = instance();
        let first = Server::start(&instance, stub).unwrap();
        let error = Server::start(&instance, stub).unwrap_err();
        assert_eq!(error.code, ErrorCode::AlreadyRunning);
        // The first is unharmed, and the instance is free once it stops.
        assert_eq!(
            client::send(&instance, &Request::Quit).unwrap(),
            Reply::Done
        );
        drop(first);
        let error = client::send(&instance, &Request::Quit).unwrap_err();
        assert_eq!(error.code, ErrorCode::NoViewer);
        let _second = Server::start(&instance, stub).unwrap();
        assert_eq!(
            client::send(&instance, &Request::Quit).unwrap(),
            Reply::Done
        );
    }

    #[test]
    fn a_client_that_starts_before_the_pipe_waits_for_it() {
        let instance = instance();
        // A starting viewer takes the mutex before it opens the pipe.
        let _mutex = crate::ipc::instance::InstanceMutex::acquire_or_fail(&instance).unwrap();
        let client = {
            let instance = instance.clone();
            thread::spawn(move || client::send(&instance, &Request::Quit))
        };
        thread::sleep(Duration::from_millis(300));
        let _server = Server::start(&instance, stub).unwrap();
        assert_eq!(client.join().unwrap().unwrap(), Reply::Done);
    }

    #[test]
    fn the_pipe_admits_only_the_current_user() {
        let sid = current_user_sid().unwrap();
        assert!(sid.starts_with("S-1-5-"), "{sid}");
        let security = CurrentUserOnly::new().unwrap();
        assert!(!security.descriptor.0.is_null());
    }
}
