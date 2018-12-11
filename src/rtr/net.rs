//! Listener and connections.
//!
//! This module implements the RTR listener socket and the high-level
//! connection handling.

use std::mem;
use std::net::SocketAddr;
use std::time::SystemTime;
use futures::future;
use futures::{Async, Future, IntoFuture, Stream};
use tokio;
use tokio::io::{AsyncRead, ReadHalf, WriteHalf};
use tokio::net::{TcpListener, TcpStream};
use ::config::Config;
use ::origins::OriginsHistory;
use super::send::{Sender, Timing};
use super::query::{Input, InputStream, Query};
use super::notify::{Dispatch, NotifyReceiver, NotifySender};


//------------ rtr_listener --------------------------------------------------

/// Returns a future for the RTR server.
///
/// The server will be configured according to `config` including creating
/// listener sockets for all the listen addresses mentioned.
/// The data exchanged with the clients is taken from `history`.
///
/// In order to be able to send notifications, the function also creates a
/// channel. It returns the sending half of that channel together with the
/// future.
pub fn rtr_listener(
    history: OriginsHistory,
    config: &Config,
) -> (NotifySender, impl Future<Item=(), Error=()>) {
    let session = session_id();
    let (dispatch, dispatch_fut) = Dispatch::new();
    let timing = Timing::new(config);
    let fut = dispatch_fut.select(
        future::select_all(
            config.tcp_listen.iter().map(|addr| {
                single_listener(
                    *addr, session, history.clone(),
                    dispatch.clone(), timing
                )
            })
        ).then(|_| Ok(()))
    ).then(|_| Ok(()));
    (dispatch.get_sender(), fut)
}

/// Creates a session ID based on the current Unix time.
fn session_id() -> u16 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH).unwrap()
        .as_secs() as u16
}

/// Creates the future for a single RTR TCP listener.
///
/// The future binds to `addr` and then spawns a new `Connection` for every
/// incoming connection on the default Tokio runtime.
fn single_listener(
    addr: SocketAddr,
    session: u16,
    history: OriginsHistory,
    mut dispatch: Dispatch,
    timing: Timing,
) -> impl Future<Item=(), Error=()> {
    TcpListener::bind(&addr).into_future()
    .then(move |res| {
        match res {
            Ok(some) => {
                info!("RTR: Listening on {}.", addr);
                Ok(some)
            }
            Err(err) => {
                error!("Failed to bind RTR listener {}: {}", addr, err);
                Err(())
            }
        }
    })
    .and_then(move |listener| {
        listener.incoming()
        .map_err(|err| error!("Failed to accept connection: {}", err))
        .for_each(move |sock| {
            let notify = dispatch.get_receiver();
            tokio::spawn(
                Connection::new(
                    sock, session, history.clone(), notify,
                    timing,
                )
            )
        })
    })
}


//------------ Connection ----------------------------------------------------

/// The future for an RTR connection.
struct Connection {
    /// The input stream.
    ///
    /// Contains both the input half of the TCP stream and the notifier.
    input: InputStream<ReadHalf<TcpStream>>,

    /// The output half of the socket as well as the state of the connection.
    output: OutputState,

    /// The session ID to be used in the RTR PDUs.
    session: u16,

    /// The validated RPKI data.
    history: OriginsHistory,

    /// The timing information for the End-of-data PDU.
    timing: Timing,
}

/// The output state of the connection.
///
/// This enum also determines where we are in the cycle.
enum OutputState {
    /// Not currently sending.
    ///
    /// Which means that we are actually waiting to receive something or
    /// being notified.
    Idle(WriteHalf<TcpStream>),

    /// We are currently sending something.
    Sending(Sender<WriteHalf<TcpStream>>),

    /// We are, like, totally done.
    Done
}

impl Connection {
    /// Creates a new connection for the given socket.
    pub fn new(
        sock: TcpStream,
        session: u16,
        history: OriginsHistory,
        notify: NotifyReceiver,
        timing: Timing,
    ) -> Self {
        let (read, write) = sock.split();
        Connection {
            input: InputStream::new(read, notify),
            output: OutputState::Idle(write),
            session, history, timing
        }
    }

    fn send(&mut self, input: Input) {
        let sock = match mem::replace(&mut self.output, OutputState::Done) {
            OutputState::Idle(sock) => sock,
            _ => panic!("illegal output state"),
        };
        let send = match input {
            Input::Query(Query::Serial { session, serial }) => {
                let diff = if session == self.session {
                    self.history.get(serial)
                }
                else { None };
                match diff {
                    Some(diff) => {
                        Sender::diff(
                            sock, self.input.version(), session, diff,
                            self.timing
                        ) 
                    }
                    None => {
                        Sender::reset(sock, self.input.version())
                    }
                }
            }
            Input::Query(Query::Reset) => {
                let (current, serial) = self.history.current_and_serial();
                Sender::full(
                    sock, self.input.version(), self.session, serial, current,
                    self.timing
                )
            }
            Input::Query(Query::Error(err)) => Sender::error(sock, err),
            Input::Notify => {
                let serial = self.history.serial();
                Sender::notify(
                    sock, self.input.version(),self.session, serial
                )
            }
        };
        self.output = OutputState::Sending(send);
    }
}

impl Future for Connection {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Result<Async<Self::Item>, Self::Error> {
        loop {
            let next = match self.output {
                OutputState::Sending(ref mut send) => {
                    let sock = try_ready!(send.poll());
                    Err(sock)
                }
                OutputState::Idle(_) => {
                    // We need to wait for input.
                    match try_ready!(self.input.poll()) {
                        Some(input) => Ok(input),
                        None => return Ok(Async::Ready(()))
                    }
                }
                OutputState::Done => panic!("illegal output state")
            };
            match next {
                Err(sock) => {
                    self.output = OutputState::Idle(sock);
                }
                Ok(input) => {
                    self.send(input)
                }
            }
        }
    }
}

