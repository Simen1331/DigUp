//! The panel's connections to the daemon, each on a thread of its own so the window never waits on the socket:
//! word matches and full searches on separate connections (word matches never queue behind an embedding), one for
//! the preview's text and other requests, and one that watches the status. Replies come back on the main loop.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use showme_core::proto::{Request, Response, Status, socket_path};

/// Which connection a reply came on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Line {
    Words,
    Full,
    Other,
}

/// What comes back to the main loop.
#[derive(Debug)]
pub enum Event {
    Reply(Line, Box<Response>),
    /// The daemon's status changed (or it went away: None).
    Status(Option<Box<Status>>),
}

#[derive(Clone)]
pub struct Daemon {
    words: mpsc::Sender<Request>,
    full: mpsc::Sender<Request>,
    other: mpsc::Sender<Request>,
}

impl Daemon {
    /// Starts the connections; `events` gets everything the daemon says.
    pub fn start(events: async_channel::Sender<Event>) -> Daemon {
        let line = |which: Line| {
            let (sender, requests) = mpsc::channel::<Request>();
            let events = events.clone();
            thread::Builder::new()
                .name(format!("daemon-{which:?}"))
                .spawn(move || connection(which, requests, events))
                .expect("can't start a thread");
            sender
        };
        let daemon = Daemon { words: line(Line::Words), full: line(Line::Full), other: line(Line::Other) };
        thread::Builder::new().name("daemon-status".into()).spawn(move || watch(events)).expect("can't start a thread");
        daemon
    }

    pub fn send(&self, line: Line, request: Request) {
        let sender = match line {
            Line::Words => &self.words,
            Line::Full => &self.full,
            Line::Other => &self.other,
        };
        let _ = sender.send(request);
    }
}

/// One connection: writes requests as they come, and reads replies on a second thread. A broken connection is made
/// again for the next request (the daemon restarted); what was in flight is answered with an error.
fn connection(which: Line, requests: mpsc::Receiver<Request>, events: async_channel::Sender<Event>) {
    let mut writer: Option<UnixStream> = None;
    for request in requests {
        if writer.is_none() {
            writer = UnixStream::connect(socket_path()).ok();
            if let Some(stream) = writer.as_ref().and_then(|stream| stream.try_clone().ok()) {
                let events = events.clone();
                thread::spawn(move || {
                    for line in BufReader::new(stream).lines() {
                        let Ok(line) = line else { break };
                        if let Ok(response) = serde_json::from_str::<Response>(&line)
                            && events.send_blocking(Event::Reply(which, Box::new(response))).is_err()
                        {
                            return;
                        }
                    }
                });
            }
        }
        let mut line = serde_json::to_string(&request).unwrap_or_default();
        line.push('\n');
        let sent = writer.as_mut().is_some_and(|stream| stream.write_all(line.as_bytes()).is_ok());
        if !sent {
            writer = None;
            let seq = match request {
                Request::Search { seq, .. } => seq,
                _ => None,
            };
            let response = Response { seq, ..Response::error("ShowMe's daemon isn't running") };
            if events.send_blocking(Event::Reply(which, Box::new(response))).is_err() {
                return;
            }
        }
    }
}

/// The status, as the daemon reports it; None while it isn't running. Tries again every 2 s.
fn watch(events: async_channel::Sender<Event>) {
    loop {
        if let Ok(stream) = UnixStream::connect(socket_path())
            && let Ok(mut writer) = stream.try_clone()
            && writer.write_all(b"{\"op\":\"watch\"}\n").is_ok()
        {
            for line in BufReader::new(stream).lines() {
                let Ok(line) = line else { break };
                let status = serde_json::from_str::<Response>(&line).ok().and_then(|response| response.status);
                if events.send_blocking(Event::Status(status)).is_err() {
                    return;
                }
            }
        }
        if events.send_blocking(Event::Status(None)).is_err() {
            return;
        }
        thread::sleep(Duration::from_secs(2));
    }
}
