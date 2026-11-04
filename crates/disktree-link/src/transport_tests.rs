use crate::{Incoming, Link, Listener, Request, Response, decode};
use crate::{Mark, State, VERSION};
use std::io::{self, BufRead as _, BufReader, Write as _};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

fn socket() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::TempDir::new().expect("tempdir");
    #[cfg(unix)]
    let path = dir.path().join("l.sock");
    #[cfg(windows)]
    let path = PathBuf::from(format!(
        r"\\.\pipe\pwrdisk-test-{}",
        dir.path().file_name().expect("temp name").to_string_lossy()
    ));
    (dir, path)
}

#[cfg(unix)]
fn raw(path: &Path) -> UnixStream {
    let raw = UnixStream::connect(path).expect("connect");
    raw.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    raw
}

#[cfg(windows)]
fn raw(path: &Path) -> std::fs::File {
    use std::os::windows::fs::OpenOptionsExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
    };
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(SECURITY_IDENTIFICATION | SECURITY_SQOS_PRESENT)
        .open(path)
        .expect("raw pipe")
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
        app_version: "0.13.0".into(),
        root: "/r".into(),
        current: "/r".into(),
        screen: "explore".into(),
        scanning: false,
        scanned: true,
        estimate: None,
        marks: vec![Mark {
            path: "/r/x".into(),
            bytes: 1,
            is_dir: false,
        }],
        space: None,
        removal_operation: None,
        removal: None,
        removals: None,
        tree: None,
        insights: Vec::new(),
        remote: None,
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
    let mut raw = raw(&path);
    raw.write_all(b"not json\n").expect("write");
    let mut reader = BufReader::new(raw);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read");
    let response: Response = decode(&line).expect("decode");
    assert!(matches!(response, Response::Error { .. }));
    drop(listener);
}

#[test]
fn dropping_a_reply_closes_only_that_client() {
    let (_dir, path) = socket();
    let listener = Listener::bind_at(&path).expect("bind");
    let mut link = Link::connect_at(&path).expect("connect");
    let answer = thread::spawn(move || link.call(&Request::State));
    let connection = match pump(&listener) {
        Incoming::Request {
            connection, reply, ..
        } => {
            drop(reply);
            connection
        }
        Incoming::Closed { .. } => panic!("closed first"),
    };
    assert!(answer.join().expect("client").is_err());
    assert!(
        matches!(pump(&listener), Incoming::Closed { connection: id } if id == connection)
    );
    assert_eq!(listener.connections(), 0);
    assert!(Link::connect_at(&path).is_ok());
}

#[test]
fn two_clients_keep_their_replies_and_connection_ids() {
    let (_dir, path) = socket();
    let listener = Listener::bind_at(&path).expect("bind");
    let mut first = Link::connect_at(&path).expect("first");
    let mut second = Link::connect_at(&path).expect("second");
    let a = thread::spawn(move || first.call(&Request::State));
    let mut ids = Vec::new();
    if let Incoming::Request {
        connection, reply, ..
    } = pump(&listener)
    {
        ids.push(connection);
        reply.send(Response::error("first reply"));
    } else {
        panic!("closed first");
    }
    assert_eq!(
        a.join().expect("first thread").expect("first reply"),
        Response::error("first reply")
    );
    // Drain the first client's close before issuing the second request.
    assert!(matches!(pump(&listener), Incoming::Closed { .. }));
    let b = thread::spawn(move || second.call(&Request::State));
    if let Incoming::Request {
        connection, reply, ..
    } = pump(&listener)
    {
        ids.push(connection);
        reply.send(Response::Ok);
    } else {
        panic!("closed second");
    }
    assert_eq!(
        b.join().expect("second thread").expect("second reply"),
        Response::Ok
    );
    assert_ne!(ids[0], ids[1]);
}

#[test]
fn an_oversized_unterminated_request_is_dropped() {
    use std::io::Read as _;
    let (_dir, path) = socket();
    let listener = Listener::bind_at(&path).expect("bind");
    let mut raw = raw(&path);
    raw.write_all(&vec![b'x'; crate::MAX_LINE])
        .expect("write oversized line");
    let mut byte = [0];
    // Windows can report either a broken pipe or PIPE_NOT_CONNECTED after
    // the server disconnects it; Unix reports EOF.
    match raw.read(&mut byte) {
        Ok(0) => {}
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {}
        #[cfg(windows)]
        Err(error)
            if error.raw_os_error()
                == Some(
                    windows_sys::Win32::Foundation::ERROR_PIPE_NOT_CONNECTED
                        .cast_signed(),
                ) => {}
        result => panic!("expected disconnection, got {result:?}"),
    }
    assert!(matches!(pump(&listener), Incoming::Closed { .. }));
}

#[test]
fn hanging_up_is_reported_and_the_listener_can_be_rebound() {
    let (_dir, path) = socket();
    let listener = Listener::bind_at(&path).expect("bind");
    drop(Link::connect_at(&path).expect("connect"));
    assert!(matches!(pump(&listener), Incoming::Closed { .. }));
    assert_eq!(listener.connections(), 0);
    drop(listener);
    let again = Listener::bind_at(&path).expect("rebind");
    assert!(Link::connect_at(&path).is_ok());
    drop(again);
}
