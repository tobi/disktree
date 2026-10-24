//! The socket on Unix: a listener the window owns, and the client an agent
//! holds.

use std::collections::VecDeque;
use std::io::{self, BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::{MAX_LINE, Request, Response, decode, encode, socket_path};

/// How often the accept loop looks for a connection or a stop. Also the
/// most a client waits to be heard; the window drains requests on a similar
/// tick, so this is not the slow part.
const ACCEPT_TICK: Duration = Duration::from_millis(40);

/// Something a connection sent, or the news that it is gone.
#[derive(Debug)]
pub enum Incoming {
    Request {
        connection: u64,
        request: Request,
        reply: Reply,
    },
    Closed {
        connection: u64,
    },
}

/// Where a request's answer goes. Dropping it unanswered closes the
/// connection, which the client sees as an error rather than a hang.
#[derive(Debug)]
pub struct Reply(mpsc::Sender<Response>);

impl Reply {
    pub fn send(self, response: Response) {
        // The connection may have hung up while the window was busy; then
        // there is nobody to tell.
        let _ = self.0.send(response);
    }
}

/// The window's end: accepts connections in the background and queues what
/// they send for the UI thread to answer on its own schedule.
#[derive(Debug)]
pub struct Listener {
    path: PathBuf,
    queue: Arc<Mutex<VecDeque<Incoming>>>,
    connections: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
}

impl Listener {
    /// Listen at [`socket_path`].
    pub fn bind() -> io::Result<Self> {
        Self::bind_at(&socket_path())
    }

    /// Listen at `path`, replacing a socket file nothing answers on: the
    /// usual leftover of a window that did not exit cleanly. A socket
    /// something does answer on is another window's, and this one stays
    /// unlinked rather than stealing it.
    pub fn bind_at(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let listener = match UnixListener::bind(path) {
            Ok(listener) => listener,
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
                if UnixStream::connect(path).is_ok() {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        "another disktree window is already linked",
                    ));
                }
                std::fs::remove_file(path)?;
                UnixListener::bind(path)?
            }
            Err(error) => return Err(error),
        };
        // The runtime directory is already the user's, but a socket in
        // `$TMPDIR` may not be: no other user gets to drive this window.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;

        let queue = Arc::new(Mutex::new(VecDeque::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let accept = Acceptor {
            listener,
            queue: Arc::clone(&queue),
            connections: Arc::clone(&connections),
            stop: Arc::clone(&stop),
            next_id: AtomicU64::new(1),
        };
        thread::Builder::new()
            .name("disktree-link".into())
            .spawn(move || accept.run())?;
        Ok(Self {
            path: path.to_path_buf(),
            queue,
            connections,
            stop,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Connections open right now.
    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::Relaxed)
    }

    /// Everything that arrived since the last drain, oldest first.
    pub fn drain(&self) -> Vec<Incoming> {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queue.drain(..).collect()
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // The file is this window's; a stale one would only make the next
        // window take the slow path.
        let _ = std::fs::remove_file(&self.path);
    }
}

struct Acceptor {
    listener: UnixListener,
    queue: Arc<Mutex<VecDeque<Incoming>>>,
    connections: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    next_id: AtomicU64,
}

impl Acceptor {
    fn run(self) {
        while !self.stop.load(Ordering::Relaxed) {
            match self.listener.accept() {
                Ok((stream, _)) => self.serve(stream),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(ACCEPT_TICK);
                }
                // A transient accept error is not worth stopping for; a
                // persistent one would spin, so the tick stays in.
                Err(_) => thread::sleep(ACCEPT_TICK),
            }
        }
    }

    fn serve(&self, stream: UnixStream) {
        // The listener is non-blocking so the loop above can see a stop;
        // a connection reads whole lines and wants to block.
        if stream.set_nonblocking(false).is_err() {
            return;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let connection = Connection {
            id,
            stream,
            queue: Arc::clone(&self.queue),
            connections: Arc::clone(&self.connections),
        };
        self.connections.fetch_add(1, Ordering::Relaxed);
        if thread::Builder::new()
            .name(format!("disktree-link-{id}"))
            .spawn(move || connection.run())
            .is_err()
        {
            self.connections.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

struct Connection {
    id: u64,
    stream: UnixStream,
    queue: Arc<Mutex<VecDeque<Incoming>>>,
    connections: Arc<AtomicUsize>,
}

impl Connection {
    fn run(mut self) {
        let _ = self.exchange();
        self.connections.fetch_sub(1, Ordering::Relaxed);
        self.push(Incoming::Closed {
            connection: self.id,
        });
    }

    fn exchange(&mut self) -> io::Result<()> {
        let mut reader = BufReader::new(self.stream.try_clone()?);
        let mut line = String::new();
        loop {
            line.clear();
            let read =
                (&mut reader).take(MAX_LINE as u64).read_line(&mut line)?;
            if read == 0 {
                return Ok(());
            }
            if !line.ends_with('\n') {
                // Longer than any request: not a client worth keeping.
                return Err(io::Error::other("request too long"));
            }
            let response = match decode::<Request>(&line) {
                Ok(request) => {
                    let (sender, receiver) = mpsc::channel();
                    self.push(Incoming::Request {
                        connection: self.id,
                        request,
                        reply: Reply(sender),
                    });
                    // The window answers when its UI thread gets to it; a
                    // dropped sender means it never will.
                    receiver
                        .recv()
                        .map_err(|_| io::Error::other("the window went away"))?
                }
                Err(error) => Response::error(format!("bad request: {error}")),
            };
            self.stream.write_all(encode(&response)?.as_bytes())?;
        }
    }

    fn push(&self, incoming: Incoming) {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queue.push_back(incoming);
    }
}

/// The agent's end: one connection, one request at a time.
#[derive(Debug)]
pub struct Link {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Link {
    /// Connect to the window at [`socket_path`].
    pub fn connect() -> io::Result<Self> {
        Self::connect_at(&socket_path())
    }

    pub fn connect_at(path: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        // The window answers from its UI thread between frames; seconds
        // means it is gone, not busy.
        stream.set_read_timeout(Some(Duration::from_secs(20)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Self { stream, reader })
    }

    /// Send `request` and wait for the answer.
    pub fn call(&mut self, request: &Request) -> io::Result<Response> {
        self.stream.write_all(encode(request)?.as_bytes())?;
        let mut line = String::new();
        let read = (&mut self.reader)
            .take(MAX_LINE as u64)
            .read_line(&mut line)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the window closed the link",
            ));
        }
        decode(&line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Mark, State, VERSION};

    fn socket() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("l.sock");
        (dir, path)
    }

    fn pump(listener: &Listener) -> Incoming {
        for _ in 0..200 {
            if let Some(incoming) = listener.drain().into_iter().next() {
                return incoming;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("nothing arrived");
    }

    #[test]
    fn a_request_reaches_the_queue_and_its_reply_reaches_the_client() {
        let (_dir, path) = socket();
        let listener = Listener::bind_at(&path).expect("bind");
        let mut link = Link::connect_at(&path).expect("connect");
        let answer = thread::spawn(move || {
            link.call(&Request::Hello {
                client: "test".into(),
                version: VERSION,
            })
        });
        let state = State {
            version: VERSION,
            root: "/r".into(),
            current: "/r".into(),
            screen: "explore".into(),
            scanning: false,
            scanned: true,
            marks: vec![Mark {
                path: "/r/x".into(),
                bytes: 1,
                is_dir: false,
            }],
            space: None,
            removal: None,
            tree: None,
            insights: Vec::new(),
        };
        match pump(&listener) {
            Incoming::Request { request, reply, .. } => {
                assert!(matches!(request, Request::Hello { .. }));
                assert_eq!(listener.connections(), 1);
                reply.send(Response::State(state.clone()));
            }
            Incoming::Closed { .. } => panic!("closed first"),
        }
        assert_eq!(
            answer.join().expect("thread").expect("reply"),
            Response::State(state)
        );
    }

    #[test]
    fn hanging_up_is_reported_and_a_stale_socket_is_replaced() {
        let (_dir, path) = socket();
        let listener = Listener::bind_at(&path).expect("bind");
        {
            let _link = Link::connect_at(&path).expect("connect");
            for _ in 0..200 {
                if listener.connections() == 1 {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(listener.connections(), 1);
        }
        assert!(matches!(pump(&listener), Incoming::Closed { .. }));
        drop(listener);
        // The file is gone with the listener; put a dead one back and a new
        // listener must take its place.
        assert!(!path.exists());
        drop(UnixListener::bind(&path).expect("stale"));
        assert!(path.exists());
        let again = Listener::bind_at(&path).expect("rebind over stale");
        assert!(Link::connect_at(&path).is_ok());
        drop(again);
    }

    #[test]
    fn a_live_socket_is_not_stolen() {
        let (_dir, path) = socket();
        let first = Listener::bind_at(&path).expect("bind");
        let second = Listener::bind_at(&path);
        assert_eq!(
            second.err().map(|e| e.kind()),
            Some(io::ErrorKind::AddrInUse)
        );
        drop(first);
    }

    #[test]
    fn garbage_gets_an_error_line_not_a_hang() {
        let (_dir, path) = socket();
        let listener = Listener::bind_at(&path).expect("bind");
        let mut raw = UnixStream::connect(&path).expect("connect");
        raw.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        raw.write_all(b"not json\n").expect("write");
        let mut reader = BufReader::new(raw);
        let mut line = String::new();
        reader.read_line(&mut line).expect("read");
        let response: Response = decode(&line).expect("decode");
        assert!(matches!(response, Response::Error { .. }));
        drop(listener);
    }
}
