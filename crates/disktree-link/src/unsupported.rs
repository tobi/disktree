//! Off Unix there is no socket yet: the agent works headless, and `show`
//! and `review` say so. A named pipe would bring the window along on
//! Windows; the protocol and these types are ready for it, so the window
//! and the server compile unchanged.

use std::io;
use std::path::Path;

use crate::{Request, Response};

/// What a connection sent; nothing ever does here.
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

/// Where an answer would go.
#[derive(Debug)]
pub struct Reply;

impl Reply {
    #[allow(
        clippy::unused_self,
        reason = "the Unix reply consumes itself; this one has the same shape"
    )]
    pub fn send(self, _response: Response) {}
}

/// A listener that cannot be bound here.
#[derive(Debug)]
pub struct Listener;

impl Listener {
    pub fn bind() -> io::Result<Self> {
        Err(unsupported())
    }

    pub fn bind_at(_path: &Path) -> io::Result<Self> {
        Err(unsupported())
    }

    pub fn path(&self) -> &Path {
        Path::new("")
    }

    pub const fn connections(&self) -> usize {
        0
    }

    pub const fn drain(&self) -> Vec<Incoming> {
        Vec::new()
    }
}

/// A client that cannot connect here.
#[derive(Debug)]
pub struct Link;

impl Link {
    pub fn connect() -> io::Result<Self> {
        Err(unsupported())
    }

    pub fn connect_at(_path: &Path) -> io::Result<Self> {
        Err(unsupported())
    }

    pub fn call(&mut self, _request: &Request) -> io::Result<Response> {
        Err(unsupported())
    }
}

fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "the window link is not available on this system yet",
    )
}
