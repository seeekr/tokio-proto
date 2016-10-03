use {Message, Body, Error};
use super::{Frame, RequestId, Transport};
use super::frame_buf::{FrameBuf, FrameDeque};
use sender::Sender;
use futures::{Future, Poll, Async};
use futures::stream::{self, Stream};
use std::io;
use std::collections::{HashMap, VecDeque};
use std::collections::hash_map::Entry;

/*
 * TODO:
 *
 * - Handle errors correctly
 *    * When the FramedIo returns an error, how is it handled?
 *    * Is it sent to the dispatch?
 *    * Is it sent to the body?
 *    * What happens if there are in-flight *in* bodies
 *    * What happens if the out message is buffered?
 * - [BUG] Can only poll from body sender FutureSender in `flush`
 * - Move constants to configuration settings
 *
 */

/// The max number of buffered frames that the connection can support. Once
/// this number is reached.
///
/// See module docs for more detail
const MAX_BUFFERED_FRAMES: usize = 128;

/// Task that drives multiplexed protocols
///
/// Provides protocol multiplexing functionality in a generic way over clients
/// and servers. Used internally by `multiplex::Client` and
/// `multiplex::Server`.
pub struct Multiplex<T> where T: Dispatch {
    // True as long as the connection has more request frames to read.
    run: bool,

    // Glues the service with the pipeline task
    dispatch: T,

    // Tracks in-progress exchanges
    exchanges: HashMap<RequestId, Exchange<T>>,

    // True when the transport is fully flushed
    is_flushed: bool,

    // RequestIds of exchanges that have not yet been dispatched
    dispatch_deque: VecDeque<RequestId>,

    // Storage for buffered frames
    frame_buf: FrameBuf<Option<Result<T::BodyOut, T::Error>>>,

    // Temporary storage for RequestIds...
    scratch: Vec<RequestId>,
}

/// Manages the state of a single in / out exchange
struct Exchange<T: Dispatch> {
    // Tracks the direction of the request as well as potentially buffers the
    // request message.
    //
    // The request message is only buffered when the dispatch is at capacity.
    // This case *shouldn't* happen and if it does it indicates a poorly
    // configured multiplex protocol or something a bit weird is happening.
    //
    // However, the world is full of multiplex protocols that don't have proper
    // flow control, so the case needs to be handled.
    request: Request<T>,

    // True indicates that the response has been handled
    responded: bool,

    // The outbound body stream sender
    out_body: Option<Sender<T::BodyOut, T::Error>>,

    // Buffers outbound body chunks until the sender is ready
    out_deque: FrameDeque<Option<Result<T::BodyOut, T::Error>>>,

    // Tracks if the sender is ready. This value is computed on each tick when
    // the senders are flushed and before new frames are read.
    //
    // The reason readiness is tracked here is because if readiness changes
    // during the progress of the multiplex tick, an outbound body chunk can't
    // simply be dispatched. Order must be maintained, so any buffered outbound
    // chunks must be dispatched first.
    out_is_ready: bool,

    // The inbound body stream receiver
    in_body: Option<T::Stream>,
}

enum Request<T: Dispatch> {
    In, // TODO: Handle inbound message buffering?
    Out(Option<Message<T::Out, Body<T::BodyOut, T::Error>>>),
}

/// Message used to communicate through the multiplex dispatch
pub type MultiplexMessage<T, B, E> = (RequestId, Result<Message<T, B>, E>);

/// Dispatch messages from the transport to the service
pub trait Dispatch: 'static {

    /// Messages written to the transport
    type In: 'static;

    /// Inbound body frame
    type BodyIn: 'static;

    /// Messages read from the transport
    type Out: 'static;

    /// Outbound body frame
    type BodyOut: 'static;

    /// Transport error
    type Error: From<Error<Self::Error>> + 'static;

    /// Inbound body stream type
    type Stream: Stream<Item = Self::BodyIn, Error = Self::Error> + 'static;

    /// Transport type
    type Transport: Transport<In = Self::In,
                          BodyIn = Self::BodyIn,
                             Out = Self::Out,
                         BodyOut = Self::BodyOut,
                           Error = Self::Error>;

    /// Mutable reference to the transport
    fn transport(&mut self) -> &mut Self::Transport;

    /// Poll the next available message
    fn poll(&mut self) -> Poll<Option<MultiplexMessage<Self::In, Self::Stream, Self::Error>>, io::Error>;

    /// The `Dispatch` is ready to accept another message
    fn poll_ready(&self) -> Async<()>;

    /// Process an out message
    fn dispatch(&mut self, message: MultiplexMessage<Self::Out, Body<Self::BodyOut, Self::Error>, Self::Error>) -> io::Result<()>;

    /// Cancel interest in the exchange identified by RequestId
    fn cancel(&mut self, request_id: RequestId) -> io::Result<()>;
}

/*
 *
 * ===== impl Multiplex =====
 *
 */

impl<T> Multiplex<T> where T: Dispatch {
    /// Create a new pipeline `Multiplex` dispatcher with the given service and
    /// transport
    pub fn new(dispatch: T) -> Multiplex<T> {
        let frame_buf = FrameBuf::with_capacity(MAX_BUFFERED_FRAMES);

        Multiplex {
            run: true,
            dispatch: dispatch,
            exchanges: HashMap::new(),
            is_flushed: true,
            dispatch_deque: VecDeque::new(),
            frame_buf: frame_buf,
            scratch: vec![],
        }
    }

    /// Returns true if the multiplexer has nothing left to do
    fn is_done(&self) -> bool {
        !self.run && self.is_flushed && self.exchanges.len() == 0
    }

    /// Attempt to dispatch any outbound request messages
    fn flush_dispatch_deque(&mut self) -> io::Result<()> {
        while self.dispatch.poll_ready().is_ready() {
            let id = match self.dispatch_deque.pop_front() {
                Some(id) => id,
                None => return Ok(()),
            };

            // Take the buffered inbound request
            let message = self.exchanges.get_mut(&id)
                .and_then(|exchange| exchange.take_buffered_out_request());

            // If `None`, continue the loop
            let message = match message {
                Some(message) => message,
                _ => continue,
            };

            try!(self.dispatch.dispatch((id, Ok(message))));
        }

        Ok(())
    }

    /// Dispatch any buffered outbound body frames to the sender
    fn flush_out_bodies(&mut self) -> io::Result<()> {
        trace!("flush out bodies");

        self.scratch.clear();

        for (id, exchange) in self.exchanges.iter_mut() {
            trace!("   --> request={}", id);
            try!(exchange.flush_out_body());

            // If the exchange is complete, track it for removal
            if exchange.is_complete() {
                self.scratch.push(*id);
            }
        }

        // Purge the scratch
        for id in &self.scratch {
            trace!("drop exchange; id={}", id);
            self.exchanges.remove(id);
        }

        Ok(())
    }

    /// Read and process frames from transport
    fn read_out_frames(&mut self) -> io::Result<()> {
        while self.run {
            // TODO: Only read frames if there is available space in the frame
            // buffer
            if let Async::Ready(frame) = try!(self.dispatch.transport().read()) {
                try!(self.process_out_frame(frame));
            } else {
                break;
            }
        }

        Ok(())
    }

    /// Process outbound frame
    fn process_out_frame(&mut self, frame: Frame<T::Out, T::BodyOut, T::Error>) -> io::Result<()> {
        trace!("Multiplex::process_out_frame");

        match frame {
            Frame::Message { id, message, body } => {
                if body {
                    let (tx, rx) = stream::channel();
                    let tx = Sender::new(tx);
                    let message = Message::WithBody(message, rx);

                    try!(self.process_out_message(id, message, Some(tx)));
                } else {
                    let message = Message::WithoutBody(message);

                    try!(self.process_out_message(id, message, None));
                }
            }
            Frame::Body { id, chunk } => {
                trace!("   --> read out body chunk");
                self.process_out_body_chunk(id, Ok(chunk));
            }
            Frame::Error { id, error } => {
                try!(self.process_out_err(id, error));
            }
            Frame::Done => {
                trace!("read Frame::Done");
                // TODO: Ensure all bodies have been completed
                self.run = false;
            }
        }

        Ok(())
    }

    /// Process an outbound message
    fn process_out_message(&mut self,
                           id: RequestId,
                           message: Message<T::Out, Body<T::BodyOut, T::Error>>,
                           body: Option<Sender<T::BodyOut, T::Error>>)
                           -> io::Result<()>
    {
        trace!("   --> process message; body={:?}", body.is_some());

        match self.exchanges.entry(id) {
            Entry::Occupied(mut e) => {
                assert!(!e.get().responded, "invalid exchange state");
                assert!(e.get().is_inbound());

                // Dispatch the message
                try!(self.dispatch.dispatch((id, Ok(message))));

                // Track that the exchange has been responded to
                e.get_mut().responded = true;

                // Set the body sender
                e.get_mut().out_body = body;

                // If the exchange is complete, clean up resources
                if e.get().is_complete() {
                    e.remove();
                }
            }
            Entry::Vacant(e) => {
                if self.dispatch.poll_ready().is_ready() {
                    trace!("   --> dispatch ready -- dispatching");

                    // Only should be here if there are no queued messages
                    assert!(self.dispatch_deque.is_empty());

                    // Dispatch the message
                    try!(self.dispatch.dispatch((id, Ok(message))));

                    // Create the exchange state
                    let mut exchange = Exchange::new(
                        Request::Out(None),
                        self.frame_buf.deque());

                    exchange.out_body = body;

                    // Track the exchange
                    e.insert(exchange);
                } else {
                    trace!("   --> dispatch not ready");

                    // Create the exchange state, including the buffered message
                    let mut exchange = Exchange::new(
                        Request::Out(Some(message)),
                        self.frame_buf.deque());

                    exchange.out_body = body;

                    // Track the exchange state
                    e.insert(exchange);

                    // Track the request ID as pending dispatch
                    self.dispatch_deque.push_back(id);
                }
            }
        }

        Ok(())
    }

    // Process an error
    fn process_out_err(&mut self, id: RequestId, err: T::Error) -> io::Result<()> {
        trace!("   --> process error frame");

        let mut remove = false;

        if let Some(exchange) = self.exchanges.get_mut(&id) {
            if !exchange.is_dispatched() {
                // The exchange is buffered and hasn't exited the multiplexer.
                // At this point it is safe to just drop the state
                remove = true;

                assert!(exchange.out_body.is_none());
                assert!(exchange.in_body.is_none());
            } else if exchange.is_outbound() {
                // Outbound exchanges can only have errors dispatched via the
                // body
                exchange.send_out_chunk(Err(err));

                // The downstream dispatch has not provided a response to the
                // exchange, indicate that interest has been canceled.
                if !exchange.responded {
                    try!(self.dispatch.cancel(id));
                }

                remove = exchange.is_complete();
            } else {
                if !exchange.responded {
                    // A response has not been provided yet, send the error via
                    // the dispatch
                    let message = (id, Err(err));
                    try!(self.dispatch.dispatch(message));

                    exchange.responded = true;
                } else {
                    // A response has already been sent, send the error via the
                    // body stream
                    exchange.send_out_chunk(Err(err));
                }

                remove = exchange.is_complete();
            }
        } else {
            trace!("   --> no in-flight exchange; dropping error");
        }

        if remove {
            self.exchanges.remove(&id);
        }

        Ok(())
    }

    fn process_out_body_chunk(&mut self, id: RequestId, chunk: Result<Option<T::BodyOut>, T::Error>) {
        trace!("process out body chunk; id={:?}", id);

        {
            let exchange = match self.exchanges.get_mut(&id) {
                Some(v) => v,
                _ => {
                    trace!("   --> exchange previously aborted; id={:?}", id);
                    return;
                }
            };

            exchange.send_out_chunk(chunk);

            if !exchange.is_complete() {
                return;
            }
        }

        trace!("dropping out body handle; id={:?}", id);
        self.exchanges.remove(&id);
    }

    fn write_in_frames(&mut self) -> io::Result<()> {
        try!(self.write_in_messages());
        try!(self.write_in_body());
        Ok(())
    }

    fn write_in_messages(&mut self) -> io::Result<()> {
        trace!("write in messages");

        while self.dispatch.transport().poll_write().is_ready() {
            trace!("   --> polling for in frame");

            match try!(self.dispatch.poll()) {
                Async::Ready(Some((id, Ok(message)))) => {
                    trace!("   --> got message");
                    try!(self.write_in_message(id, message));
                }
                Async::Ready(Some((id, Err(error)))) => {
                    trace!("   --> got error");
                    try!(self.write_in_error(id, error));
                }
                Async::Ready(None) => {
                    trace!("   --> got None");
                    // The service is done with the connection. In this case, a
                    // `Done` frame should be written to the transport and the
                    // transport should start shutting down.
                    //
                    // However, the `Done` frame should only be written once
                    // all the in-flight bodies have been written.
                    //
                    // For now, do nothing...
                    break;
                }
                // Nothing to dispatch
                Async::NotReady => break,
            }
        }

        trace!("   --> transport not ready");

        Ok(())
    }

    fn write_in_message(&mut self,
                        id: RequestId,
                        message: Message<T::In, T::Stream>)
                        -> io::Result<()>
    {
        let (message, body) = match message {
            Message::WithBody(message, rx) => (message, Some(rx)),
            Message::WithoutBody(message) => (message, None),
        };

        // Create the frame
        let frame = Frame::Message {
            id: id,
            message: message,
            body: body.is_some()
        };

        // Write the frame
        try!(self.dispatch.transport().write(frame));

        match self.exchanges.entry(id) {
            Entry::Occupied(mut e) => {
                assert!(!e.get().responded, "invalid exchange state");
                assert!(e.get().is_outbound());

                // Track that the exchange has been responded to
                e.get_mut().responded = true;

                // Set the body receiver
                e.get_mut().in_body = body;

                // If the exchange is complete, clean up the resources
                if e.get().is_complete() {
                    e.remove();
                }
            }
            Entry::Vacant(e) => {
                // Create the exchange state
                let exchange = Exchange::new(
                    Request::In,
                    self.frame_buf.deque());

                // Track the exchange
                e.insert(exchange);
            }
        }

        Ok(())
    }

    fn write_in_error(&mut self,
                      id: RequestId,
                      error: T::Error)
                      -> io::Result<()>
    {
        if let Entry::Occupied(mut e) = self.exchanges.entry(id) {
            assert!(e.get().is_outbound(), "invalid state");
            assert!(!e.get().responded, "exchange already responded");

            // TODO: should the outbound body be canceled? In theory, if the
            // consuming end doesn't want it anymore, it should drop interest
            e.get_mut().out_body = None;
            e.get_mut().out_deque.clear();

            assert!(e.get().is_complete());

            // Write the error frame
            let frame = Frame::Error { id: id, error: error };
            try!(self.dispatch.transport().write(frame));

            e.remove();
        }

        Ok(())
    }

    fn write_in_body(&mut self) -> io::Result<()> {
        trace!("write in body chunks");

        self.scratch.clear();

        // Now, write the ready streams
        'outer:
        for (&id, exchange) in &mut self.exchanges {
            trace!("   --> checking request {:?}", id);

            while self.dispatch.transport().poll_write().is_ready() {
                match exchange.try_poll_in_body() {
                    Ok(Async::Ready(Some(chunk))) => {
                        trace!("   --> got chunk");

                        let frame = Frame::Body { id: id, chunk: Some(chunk) };
                        try!(self.dispatch.transport().write(frame));
                    }
                    Ok(Async::Ready(None)) => {
                        trace!("   --> end of stream");

                        let frame = Frame::Body { id: id, chunk: None };
                        try!(self.dispatch.transport().write(frame));

                        // in_body is fully written.
                        exchange.in_body = None;
                        break;
                    }
                    Err(error) => {
                        // Write the error frame
                        let frame = Frame::Error { id: id, error: error };
                        try!(self.dispatch.transport().write(frame));

                        exchange.responded = true;
                        exchange.in_body = None;
                        exchange.out_body = None;
                        exchange.out_deque.clear();

                        debug_assert!(exchange.is_complete());
                        break;
                    }
                    Ok(Async::NotReady) => {
                        trace!("   --> no pending chunks");
                        continue 'outer;
                    }
                }
            }

            if exchange.is_complete() {
                self.scratch.push(id);
            }
        }

        for id in &self.scratch {
            trace!("dropping in body handle; id={:?}", id);
            self.exchanges.remove(id);
        }

        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.is_flushed = try!(self.dispatch.transport().flush()).is_ready();
        Ok(())
    }
}

impl<T> Future for Multiplex<T>
    where T: Dispatch,
{
    type Item = ();
    type Error = io::Error;

    // Tick the pipeline state machine
    fn poll(&mut self) -> Poll<(), io::Error> {
        trace!("Multiplex::tick ~~~~~~~~~~~~~~~~~~~~~~~~~~~");

        // Always flush the transport first
        try!(self.flush());

        // Try to dispatch any buffered messages
        try!(self.flush_dispatch_deque());

        // Try to send any buffered body chunks on their senders
        try!(self.flush_out_bodies());

        // First read off data from the socket
        try!(self.read_out_frames());

        // Handle completed responses
        try!(self.write_in_frames());

        // Since writing frames could un-block the dispatch, attempt to flush
        // the dispatch queue again.
        // TODO: This is a hack and really shouldn't be relied on
        try!(self.flush_dispatch_deque());

        // Try flushing buffered writes
        try!(self.flush());

        // Clean shutdown of the pipeline server can happen when
        //
        // 1. The server is done running, this is signaled by Transport::read()
        //    returning Frame::Done.
        //
        // 2. The transport is done writing all data to the socket, this is
        //    signaled by Transport::flush() returning Ok(Some(())).
        //
        // 3. There are no further responses to write to the transport.
        //
        // It is necessary to perfom these three checks in order to handle the
        // case where the client shuts down half the socket.
        //
        if self.is_done() {
            trace!("multiplex done; terminating");
            return Ok(Async::Ready(()));
        }

        trace!("tick done; waiting for wake-up");

        // Tick again later
        Ok(Async::NotReady)
    }
}

impl<T: Dispatch> Exchange<T> {
    fn new(request: Request<T>, deque: FrameDeque<Option<Result<T::BodyOut, T::Error>>>) -> Exchange<T> {
        Exchange {
            request: request,
            responded: false,
            out_body: None,
            out_deque: deque,
            out_is_ready: false,
            in_body: None,
        }
    }

    fn is_inbound(&self) -> bool {
        match self.request {
            Request::In => true,
            Request::Out(_) => false,
        }
    }

    fn is_outbound(&self) -> bool {
        !self.is_inbound()
    }

    fn is_dispatched(&self) -> bool {
        match self.request {
            Request::Out(Some(_)) => false,
            _ => true,
        }
    }

    /// Returns true if the exchange is complete
    fn is_complete(&self) -> bool {
        // The exchange is completed if the response has been seen and bodies
        // in both directions are fully flushed
        self.responded && self.out_body.is_none() && self.in_body.is_none()
    }

    /// Takes the buffered out request out of the value and returns it
    fn take_buffered_out_request(&mut self) -> Option<Message<T::Out, Body<T::BodyOut, T::Error>>> {
        match self.request {
            Request::Out(ref mut request) => request.take(),
            _ => None,
        }
    }

    fn send_out_chunk(&mut self, chunk: Result<Option<T::BodyOut>, T::Error>) {
        // Reverse Result & Option
        let chunk = match chunk {
            Ok(Some(v)) => Some(Ok(v)),
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        };

        // Get a reference to the sender
        {
            let sender = match self.out_body {
                Some(ref mut v) => v,
                _ =>  {
                    return;
                }
            };

            if self.out_is_ready {
                trace!("   --> send chunk; end-of-stream={:?}", chunk.is_none());

                // If there is a chunk (vs. None which represents end of
                // stream)
                if let Some(chunk) = chunk {
                    // Send the chunk
                    sender.send(chunk);

                    // See if the sender is ready again
                    match sender.poll_ready() {
                        Ok(Async::Ready(_)) => {
                            trace!("   --> ready for more");
                            // The sender is ready for another message
                            return;
                        }
                        Ok(Async::NotReady) => {
                            // The sender is not ready for another message
                            self.out_is_ready = false;
                            return;
                        }
                        Err(_) => {
                            // The sender is complete, it should be removed
                        }
                    }
                }

                assert!(self.out_deque.is_empty());
            } else {
                trace!("   --> queueing chunk");

                self.out_deque.push(chunk);
                return;
            }
        }

        self.out_is_ready = false;
        self.out_body = None;
    }

    fn try_poll_in_body(&mut self) -> Poll<Option<T::BodyIn>, T::Error> {
        match self.in_body {
            Some(ref mut b) => b.poll(),
            _ => Ok(Async::NotReady),
        }
    }

    /// Write as many buffered body chunks to the sender
    fn flush_out_body(&mut self) -> io::Result<()> {
        {
            let sender = match self.out_body {
                Some(ref mut sender) => sender,
                None => {
                    assert!(self.out_deque.is_empty(), "pending out frames but no sender");
                    return Ok(());
                }
            };

            self.out_is_ready = true;

            loop {
                match sender.poll_ready() {
                    Ok(Async::Ready(())) => {
                        // Pop a pending frame
                        match self.out_deque.pop() {
                            Some(Some(Ok(chunk))) => {
                                sender.send(Ok(chunk));
                            }
                            Some(Some(Err(e))) => {
                                // Send the error then break as it is the final
                                // chunk
                                sender.send(Err(e));
                                break;
                            }
                            Some(None) => {
                                break;
                            }
                            None => {
                                // No more frames to flush
                                return Ok(());
                            }
                        }
                    }
                    Ok(Async::NotReady) => {
                        trace!("   --> not ready");
                        // Sender not ready
                        self.out_is_ready = false;
                        return Ok(());
                    }
                    Err(_) => {
                        // The receiving end dropped interest in the body
                        // stream. In this case, the sender and the frame
                        // buffer is dropped. If future body frames are
                        // received, the sender will be gone and the frames
                        // will be dropped.
                        break;
                    }
                }
            }
        }

        // At this point, the outbound body is complete.
        self.out_deque.clear();
        self.out_body.take();
        Ok(())
    }
}
