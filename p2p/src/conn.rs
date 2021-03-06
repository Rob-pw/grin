// Copyright 2016 The Grin Developers
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Provides a connection wrapper that handles the lower level tasks in sending
//! or
//! receiving data from the TCP socket, as well as dealing with timeouts.

use std::iter;
use std::ops::Deref;
use std::sync::{Mutex, Arc};
use std::time::{Instant, Duration};

use futures;
use futures::{Stream, Future};
use futures::stream;
use futures::sync::mpsc::{Sender, UnboundedSender, UnboundedReceiver};
use tokio_core::io::{Io, WriteHalf, ReadHalf, write_all, read_exact};
use tokio_core::net::TcpStream;
use tokio_timer::{Timer, TimerError};

use core::core::hash::{Hash, ZERO_HASH};
use core::ser;
use msg::*;

/// Handler to provide to the connection, will be called back anytime a message
/// is received. The provided sender can be use to immediately send back
/// another message.
pub trait Handler: Sync + Send {
	/// Handle function to implement to process incoming messages. A sender to
	/// reply immediately as well as the message header and its unparsed body
	/// are provided.
	fn handle(&self,
	          sender: UnboundedSender<Vec<u8>>,
	          header: MsgHeader,
	          body: Vec<u8>)
	          -> Result<Option<Hash>, ser::Error>;
}

impl<F> Handler for F
	where F: Fn(UnboundedSender<Vec<u8>>, MsgHeader, Vec<u8>) -> Result<Option<Hash>, ser::Error>,
	      F: Sync + Send
{
	fn handle(&self,
	          sender: UnboundedSender<Vec<u8>>,
	          header: MsgHeader,
	          body: Vec<u8>)
	          -> Result<Option<Hash>, ser::Error> {
		self(sender, header, body)
	}
}

/// A higher level connection wrapping the TcpStream. Maintains the amount of
/// data transmitted and deals with the low-level task of sending and
/// receiving data, parsing message headers and timeouts.
pub struct Connection {
	// Channel to push bytes to the remote peer
	outbound_chan: UnboundedSender<Vec<u8>>,

	// Close the connection with the remote peer
	close_chan: Sender<()>,

	// Bytes we've sent.
	sent_bytes: Arc<Mutex<u64>>,

	// Bytes we've received.
	received_bytes: Arc<Mutex<u64>>,

	// Counter for read errors.
	error_count: Mutex<u64>,
}

impl Connection {
	/// Start listening on the provided connection and wraps it. Does not hang
	/// the current thread, instead just returns a future and the Connection
	/// itself.
	pub fn listen<F>(conn: TcpStream,
	                 handler: F)
	                 -> (Connection, Box<Future<Item = (), Error = ser::Error>>)
		where F: Handler + 'static
	{

		let (reader, writer) = conn.split();

		// prepare the channel that will transmit data to the connection writer
		let (tx, rx) = futures::sync::mpsc::unbounded();

		// same for closing the connection
		let (close_tx, close_rx) = futures::sync::mpsc::channel(1);
		let close_conn = close_rx.for_each(|_| Ok(())).map_err(|_| ser::Error::CorruptedData);

		let me = Connection {
			outbound_chan: tx.clone(),
			close_chan: close_tx,
			sent_bytes: Arc::new(Mutex::new(0)),
			received_bytes: Arc::new(Mutex::new(0)),
			error_count: Mutex::new(0),
		};

		// setup the reading future, getting messages from the peer and processing them
		let read_msg = me.read_msg(tx, reader, handler).map(|_| ());

		// setting the writing future, getting messages from our system and sending
		// them out
		let write_msg = me.write_msg(rx, writer).map(|_| ());

		// select between our different futures and return them
		let fut =
			Box::new(close_conn.select(read_msg.select(write_msg).map(|_| ()).map_err(|(e, _)| e))
				.map(|_| ())
				.map_err(|(e, _)| e));

		(me, fut)
	}

	/// Prepares the future that gets message data produced by our system and
	/// sends it to the peer connection
	fn write_msg(&self,
	             rx: UnboundedReceiver<Vec<u8>>,
	             writer: WriteHalf<TcpStream>)
	             -> Box<Future<Item = WriteHalf<TcpStream>, Error = ser::Error>> {

		let sent_bytes = self.sent_bytes.clone();
		let send_data = rx.map(move |data| {
        // add the count of bytes sent
				let mut sent_bytes = sent_bytes.lock().unwrap();
				*sent_bytes += data.len() as u64;
				data
			})
      // write the data and make sure the future returns the right types
			.fold(writer,
			      |writer, data| write_all(writer, data).map_err(|_| ()).map(|(writer, buf)| writer))
			.map_err(|_| ser::Error::CorruptedData);
		Box::new(send_data)
	}

	/// Prepares the future reading from the peer connection, parsing each
	/// message and forwarding them appropriately based on their type
	fn read_msg<F>(&self,
	               sender: UnboundedSender<Vec<u8>>,
	               reader: ReadHalf<TcpStream>,
	               handler: F)
	               -> Box<Future<Item = ReadHalf<TcpStream>, Error = ser::Error>>
		where F: Handler + 'static
	{

		// infinite iterator stream so we repeat the message reading logic until the
		// peer is stopped
		let iter = stream::iter(iter::repeat(()).map(Ok::<(), ser::Error>));

		// setup the reading future, getting messages from the peer and processing them
		let recv_bytes = self.received_bytes.clone();
		let handler = Arc::new(handler);

		let read_msg = iter.fold(reader, move |reader, _| {
			let recv_bytes = recv_bytes.clone();
			let handler = handler.clone();
			let sender_inner = sender.clone();

			// first read the message header
			read_exact(reader, vec![0u8; HEADER_LEN as usize])
				.map_err(|e| ser::Error::IOErr(e))
				.and_then(move |(reader, buf)| {
					let header = try!(ser::deserialize::<MsgHeader>(&mut &buf[..]));
					Ok((reader, header))
				})
				.and_then(move |(reader, header)| {
					// now that we have a size, proceed with the body
					read_exact(reader, vec![0u8; header.msg_len as usize])
						.map(|(reader, buf)| (reader, header, buf))
						.map_err(|e| ser::Error::IOErr(e))
				})
				.map(move |(reader, header, buf)| {
					// add the count of bytes received
					let mut recv_bytes = recv_bytes.lock().unwrap();
					*recv_bytes += header.serialized_len() + header.msg_len;

					// and handle the different message types
					let msg_type = header.msg_type;
					if let Err(e) = handler.handle(sender_inner.clone(), header, buf) {
						debug!("Invalid {:?} message: {}", msg_type, e);
					}

					reader
				})
		});
		Box::new(read_msg)
	}

	/// Utility function to send any Writeable. Handles adding the header and
	/// serialization.
	pub fn send_msg(&self, t: Type, body: &ser::Writeable) -> Result<(), ser::Error> {

		let mut body_data = vec![];
		try!(ser::serialize(&mut body_data, body));
		let mut data = vec![];
		try!(ser::serialize(&mut data, &MsgHeader::new(t, body_data.len() as u64)));
		data.append(&mut body_data);

		self.outbound_chan.send(data).map_err(|_| ser::Error::CorruptedData)
	}

	/// Bytes sent and received by this peer to the remote peer.
	pub fn transmitted_bytes(&self) -> (u64, u64) {
		let sent = *self.sent_bytes.lock().unwrap();
		let recv = *self.received_bytes.lock().unwrap();
		(sent, recv)
	}
}

/// Connection wrapper that handles a request/response oriented interaction with
/// a timeout.
pub struct TimeoutConnection {
	underlying: Connection,

	expected_responses: Arc<Mutex<Vec<(Type, Option<Hash>, Instant)>>>,
}

impl TimeoutConnection {
	/// Same as Connection
	pub fn listen<F>(conn: TcpStream,
	                 handler: F)
	                 -> (TimeoutConnection, Box<Future<Item = (), Error = ser::Error>>)
		where F: Handler + 'static
	{

		let expects = Arc::new(Mutex::new(vec![]));

		// Decorates the handler to remove the "subscription" from the expected
		// responses. We got our replies, so no timeout should occur.
		let exp = expects.clone();
		let (conn, fut) = Connection::listen(conn, move |sender, header: MsgHeader, data| {
			let msg_type = header.msg_type;
			let recv_h = try!(handler.handle(sender, header, data));

			let mut expects = exp.lock().unwrap();
			let filtered = expects.iter()
				.filter(|&&(typ, h, _): &&(Type, Option<Hash>, Instant)| {
					msg_type != typ || h.is_some() && recv_h != h
				})
				.map(|&x| x)
				.collect::<Vec<_>>();
			*expects = filtered;

			Ok(recv_h)
		});

		// Registers a timer with the event loop to regularly check for timeouts.
		let exp = expects.clone();
		let timer = Timer::default()
			.interval(Duration::new(2, 0))
			.fold((), move |_, _| {
				let exp = exp.lock().unwrap();
				for &(_, _, t) in exp.deref() {
					if Instant::now() - t > Duration::new(2, 0) {
						return Err(TimerError::TooLong);
					}
				}
				Ok(())
			})
			.map_err(|_| ser::Error::CorruptedData);

		let me = TimeoutConnection {
			underlying: conn,
			expected_responses: expects,
		};
		(me, Box::new(fut.select(timer).map(|_| ()).map_err(|(e1, e2)| e1)))
	}

	/// Sends a request and registers a timer on the provided message type and
	/// optionally the hash of the sent data.
	pub fn send_request(&self,
	                    t: Type,
	                    rt: Type,
	                    body: &ser::Writeable,
	                    expect_h: Option<(Hash)>)
	                    -> Result<(), ser::Error> {
		let sent = try!(self.underlying.send_msg(t, body));

		let mut expects = self.expected_responses.lock().unwrap();
		expects.push((rt, expect_h, Instant::now()));
		Ok(())
	}

	/// Same as Connection
	pub fn send_msg(&self, t: Type, body: &ser::Writeable) -> Result<(), ser::Error> {
		self.underlying.send_msg(t, body)
	}

	/// Same as Connection
	pub fn transmitted_bytes(&self) -> (u64, u64) {
		self.underlying.transmitted_bytes()
	}
}
