use crate::error::Result;
use bytes::Buf;
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::broadcast::Receiver;
use tokio_rustls::server::TlsStream;

macro_rules! write_stream_buf {
    ($id:expr, $writer:expr, $buf:expr, $addr:expr) => {
        let n = $buf.remaining();
        $writer
            .write_all_buf(&mut $buf)
            .await
            .map_err(|e| format!("session={id} error writing to socket: {e}", id = $id))?;
        tracing::debug!(
            session = %$id,
            "wrote {n} bytes to {peer_addr:?}",
            n = n,
            peer_addr = $addr
        );
    };
}

macro_rules! flush_stream {
    ($id:expr, $writer:expr, $addr:expr) => {
        $writer
            .flush()
            .await
            .map_err(|e| format!("session={id} error flushing stream: {e}", id = $id))?;
        tracing::debug!(
            session = %$id,
            "flushed stream to {peer_addr:?}",
            peer_addr = $addr
        );
    };
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub enum ProtoOp {
    Get { key: String },
    Set { key: String, value: Vec<u8> },
    Echo { msg: Vec<u8> },
    SysClose,
    Cancelled,
}

#[derive(Clone, Eq, PartialEq, Debug)]
enum ProtoRead {
    Read(usize),
    Eof,
    Cancelled,
}

#[derive(Debug, Eq, PartialEq)]
enum Op {
    Get,
    Set,
    Echo,
}

enum State {
    Start,
    ReadOp,
    ReadKeyLen,
    ReadKey,
    ReadEcho,
    ReadValueLen,
    ReadValue,
    Done,
}

const MIN_BUF_SIZE: usize = 4;
const BUF_SIZE: usize = 256;

/// A basic wire protocol reader/writer.
/// See `read` method below for more details.
pub struct Proto {
    // The connection/session ID this proto is being used for
    id: String,
    // The peer/client's address
    addr: std::net::SocketAddr,
    // The read-half of the client's connection
    reader: ReadHalf<TlsStream<TcpStream>>,
    // Internal buffer used to read into
    buf: Vec<u8>,
    // Flag denoting whether this proto is newly constructed
    // or whether is has been used to read before. This is
    // used to signal whether we want to preserve the existing
    // contents of `self.buf`
    fresh: bool,
    // Broadcast receiver to signal shutdown
    kill: Receiver<bool>,
}
impl Proto {
    pub fn new(
        id: &str,
        addr: std::net::SocketAddr,
        reader: ReadHalf<TlsStream<TcpStream>>,
        kill: Receiver<bool>,
    ) -> Self {
        let buf = Vec::with_capacity(BUF_SIZE);
        // big enough to read the initial `Op` string
        assert!(buf.capacity() >= MIN_BUF_SIZE);
        Self {
            id: id.to_string(),
            addr,
            reader,
            buf,
            fresh: true,
            kill,
        }
    }

    pub async fn flush(&self, writer: &mut WriteHalf<TlsStream<TcpStream>>) -> Result<()> {
        flush_stream!(self.id, writer, self.addr);
        Ok(())
    }

    pub async fn write_null(&self, writer: &mut WriteHalf<TlsStream<TcpStream>>) -> Result<()> {
        tracing::trace!(session = %self.id, "writing null");
        let mut bytes = b"null\n".reader();
        write_stream_buf!(self.id, writer, bytes.get_mut(), self.addr);
        Ok(())
    }

    pub async fn write_echo(
        &self,
        writer: &mut WriteHalf<TlsStream<TcpStream>>,
        data: &[u8],
    ) -> Result<()> {
        tracing::trace!(session = %self.id, "writing echo");
        let data_len = data.len().to_string();
        let mut bytes = Buf::chain(data_len.as_bytes(), &b":"[..])
            .chain(data)
            .chain(&b"\n"[..]);
        write_stream_buf!(self.id, writer, bytes, self.addr);
        Ok(())
    }

    pub async fn write_get_result(
        &self,
        writer: &mut WriteHalf<TlsStream<TcpStream>>,
        data: &[u8],
    ) -> Result<()> {
        // todo: accept async reader instead of straight data
        tracing::trace!(session = %self.id, "writing get result");
        let data_len = data.len().to_string();
        let mut bytes = Buf::chain(data_len.as_bytes(), &b":"[..])
            .chain(data)
            .chain(&b"\n"[..]);
        write_stream_buf!(self.id, writer, bytes, self.addr);
        Ok(())
    }

    pub async fn write_set_result(
        &self,
        writer: &mut WriteHalf<TlsStream<TcpStream>>,
        data: &[u8],
    ) -> Result<()> {
        tracing::trace!(session = %self.id, "writing set result");
        let len_v = data.len().to_string();
        let len_v_len = len_v.len().to_string();
        let mut bytes = Buf::chain(len_v_len.as_bytes(), &b":"[..])
            .chain(len_v.as_bytes())
            .chain(&b"\n"[..]);
        write_stream_buf!(self.id, writer, bytes, self.addr);
        Ok(())
    }

    /// read to the internal buffer
    async fn read_buf(&mut self) -> Result<ProtoRead> {
        tracing::trace!(session = %self.id, "reading to buffer");
        tokio::select! {
            _ = self.kill.recv() => {
                tracing::info!(session = %self.id, "connection cancelled by server shutdown");
                Ok(ProtoRead::Cancelled)
            }
            res = self.reader.read_buf(&mut self.buf) => {
                // match self.reader.read_buf(&mut self.buf).await {
                match res {
                    Ok(n) => Ok(ProtoRead::Read(n)),
                    Err(e) => {
                        use std::io::ErrorKind::*;
                        match e.kind() {
                            UnexpectedEof => Ok(ProtoRead::Eof),
                            _ => Err(format!("session={} error reading from socket: {e}", self.id).into()),
                        }
                    }
                }
            }
        }
    }

    /// Read from `self.reader` (into `self.buf`) to construct a single valid `ProtoOp`
    /// TODO: Add max limits to number of bytes read for lengths/keys/values
    /// TODO: Better handling of client errors - malformed or malicious inputs
    ///
    /// This is a really basic wire protocol to communicate utf8 keys and raw byte values.
    /// There are 3 commands:
    ///   GET key       => GET:3:key\n           => 9:the_value\n   ;; returning the found bytes
    ///   SET key value => SET:3:key:5:value\n   => 1:5\n           ;; returning the number of bytes saved
    ///   ECHO msg      => ECHO:7:message\n      => 7:message\n     ;; returning the bytes sent
    ///
    /// - `key`, `value`, `msg` denote variable length byte arguments
    /// - `key` bytes must be a valid utf8 string
    /// - Every variable length byte argument is prefixed by a "length" surrounded by colons `:`
    ///   which denotes how many bytes must be read to consume the following argument.
    /// - Every command must end with a newline `\n`. These act as a secondary separator,
    ///   with the "lengths" being the primary means of separation. Any bytes found between
    ///   the "end" of a "length" and the trailing newline are discarded.
    /// - Every result has a trailing newline to denote the end of the result message.
    /// - Lack of existence is represented by `null\n`
    ///
    /// Examples:
    /// - Get non existent key:
    ///     send=> GET:9:unset_key\n
    ///     recv=> null\n
    ///
    /// - Get an existing key:
    ///     send=> GET:7:set_key\n
    ///     recv=> 11:found_value\n
    ///
    /// - Set a key/value pair:
    ///     send=> SET:6:my_key:8:my_value\n
    ///     recv=> 1:8\n
    ///
    /// - Echo a message:
    ///     send=> ECHO:11:hello world\n
    ///     recv=> 11:hello world\n
    ///
    pub async fn read(&mut self) -> Result<ProtoOp> {
        // --------
        // --- Starting defaults
        // --------
        let mut state = State::Start;
        let mut op = Op::Get;
        // Flag used when reading length integers
        let mut between_colons = false;
        // Whether a "read from socket" is required. This will clear
        // and refill the internal `self.buf`.
        // When a `fresh` Proto is being used, we want to start
        // off reading from the socket, but when a `!fresh` Proto
        // is being re-used for subsequent reads of `ProtoOp`s, then
        // we _don't_ want to start with a read since we want to
        // preserve whatever may be in the existing `self.buf`
        let mut needs_read = self.fresh;
        // Pointer to the internal `self.buf` buffer
        let mut ptr = 0;

        // --------
        // --- Buffers for reading distinct parts of the proto-op
        // --------
        // Buf to read the key length integer, 8 chars should cover most numbers
        let mut key_len_buf = Vec::with_capacity(8);
        // Eventual parsed length in bytes of the key
        let mut key_len = 0;
        let mut key = Vec::with_capacity(BUF_SIZE);

        // Buf to read message to be echo'd
        let mut echo = Vec::with_capacity(BUF_SIZE);

        // Buf to read the key length integer, 8 chars should cover most numbers
        let mut value_len_buf = Vec::with_capacity(8);
        // Eventual parsed length in bytes of the value
        let mut value_len = 0;
        let mut value = Vec::with_capacity(BUF_SIZE);

        // Buf to hold residual bytes - these are bytes found
        // in `self.buf` after an "end of message" newline.
        // Any residual bytes will be prepended to `self.buf`
        // after the next read.
        let mut residual = Vec::with_capacity(BUF_SIZE);

        'state_loop: loop {
            if needs_read {
                // Before reading, empty the read buffer and make sure
                // it's sized to the expected BUF_SIZE.
                // Clearing ensures there's space to fill, and shrinking
                // ensures that the buffer hasn't grown due to previously
                // prepended residual bytes.
                self.buf.clear();
                self.buf.shrink_to(BUF_SIZE);

                match self.read_buf().await? {
                    ProtoRead::Eof => return Ok(ProtoOp::SysClose),
                    ProtoRead::Cancelled => return Ok(ProtoOp::Cancelled),
                    ProtoRead::Read(n) => {
                        tracing::debug!(session = %self.id, "read {} bytes", n);
                    }
                }
                if !residual.is_empty() {
                    residual.append(&mut self.buf);
                    std::mem::swap(&mut residual, &mut self.buf);
                    // residual should now be empty and have self.buf's capacity
                    assert!(residual.is_empty());
                    assert!(residual.capacity() >= BUF_SIZE);
                }
                ptr = 0;
                needs_read = false;
            }

            match state {
                State::Start => {
                    tracing::debug!(session = %self.id, fresh= %self.fresh, "handling State::Start");
                    if self.fresh {
                        // this is a new proto, just continue to reading
                        state = State::ReadOp;
                        self.fresh = false;
                    } else {
                        // This is an existing proto so there may be residual data in `self.buf`.
                        // Clear anything remaining on the stream up to and including a b'\n'.
                        // If there's anything after that newline, then save it to the residual buffer.
                        while ptr < self.buf.len() {
                            tracing::trace!(session = %self.id, ptr=%ptr, "clearing residual bytes up to newline");
                            if self.buf[ptr] == b'\n' {
                                ptr += 1;
                                state = State::ReadOp;
                                if ptr < self.buf.len() {
                                    // save the rest to a residual buffer that will be prepended
                                    // to the next read buffer
                                    residual.append(&mut self.buf[ptr..].to_vec());
                                }
                                continue 'state_loop;
                            } else {
                                ptr += 1;
                            }
                        }
                        needs_read = true;
                    }
                }
                State::ReadOp => {
                    tracing::debug!(session = %self.id, "handling State::ReadOp");
                    let read_op_end_ptr = ptr + MIN_BUF_SIZE;
                    if read_op_end_ptr > self.buf.len() {
                        if ptr == 0 {
                            // We're at the start of a read buffer and there's not enough bytes
                            // so there must have been a malformed write from a client.
                            // Note: This assumption isn't _really_ valid. It's _possible_
                            //       that the client is slowly writing the initial "op" (GET/SET)
                            //       bytes, and it might be better if we kept reading and
                            //       prepending our current byes using the residual buffer.
                            //       We can add that if we see this error happening...
                            return Err(format!(
                                "error reading start of operation, buffer-len {:?} shorter than expected {:?}",
                                self.buf.len(),
                                String::from_utf8(self.buf.clone()).unwrap_or_else(|_| format!("{:?}", &self.buf))
                            )
                            .into());
                        } else {
                            // we were previously clearing residual bytes and
                            // are mid-buffer (ptr > 0). Instead of blowing up,
                            // try reading more bytes (prepending the residual bytes)
                            needs_read = true;
                            continue 'state_loop;
                        }
                    }
                    op = match &self.buf[ptr..read_op_end_ptr] {
                        b"GET:" => {
                            ptr = 3;
                            Op::Get
                        }
                        b"SET:" => {
                            ptr = 3;
                            Op::Set
                        }
                        b"ECHO" => {
                            ptr = 4;
                            Op::Echo
                        }
                        _ => {
                            return Err(format!(
                                "error reading start of operation, unknown operation {:?}",
                                String::from_utf8(self.buf[ptr..read_op_end_ptr].to_vec())
                                    .unwrap_or_else(|_| format!(
                                        "{:?}",
                                        &self.buf[ptr..read_op_end_ptr]
                                    ))
                            )
                            .into())
                        }
                    };
                    tracing::debug!(session = %self.id, "read op {:?}", op);
                    needs_read = false;
                    // transition next to read-key-len, even if the op is `Echo`
                    // since we need to read a length regardless
                    state = State::ReadKeyLen;
                }
                State::ReadKeyLen => {
                    tracing::debug!(session = %self.id, ptr = %ptr, buf_len = %self.buf.len(), "handling State::ReadKeyLen");
                    // read between `:` and `:`
                    while ptr < self.buf.len() {
                        if !between_colons {
                            if self.buf[ptr] != b':' {
                                return Err(format!(
                                    "reading key_len, expected ':' found {:?}",
                                    self.buf[ptr] as char
                                )
                                .into());
                            }
                            between_colons = true;
                            ptr += 1;
                        } else if self.buf[ptr] == b':' {
                            between_colons = false;
                            ptr += 1;
                            key_len = std::str::from_utf8(&key_len_buf)
                                .map_err(|e| format!("key length is invalid utf8: {e}"))?
                                .parse::<usize>()?;

                            // if we're echoing, then we want to read into the echo buffer
                            if op == Op::Echo {
                                state = State::ReadEcho;
                            } else {
                                state = State::ReadKey;
                            }
                            continue 'state_loop;
                        } else {
                            key_len_buf.push(self.buf[ptr]);
                            ptr += 1;
                        }
                    }
                    needs_read = true;
                }
                State::ReadEcho => {
                    tracing::debug!(session = %self.id, ptr = %ptr, buf_len = %self.buf.len(), "handling State::ReadEcho");
                    while ptr < self.buf.len() && echo.len() < key_len {
                        echo.push(self.buf[ptr]);
                        ptr += 1;
                    }
                    if echo.len() >= key_len {
                        state = State::Done;
                        continue 'state_loop;
                    }
                    needs_read = true;
                }
                State::ReadKey => {
                    tracing::debug!(session = %self.id, ptr = %ptr, buf_len = %self.buf.len(), "handling State::ReadKey");
                    while ptr < self.buf.len() && key.len() < key_len {
                        key.push(self.buf[ptr]);
                        ptr += 1;
                    }
                    if key.len() >= key_len {
                        match op {
                            Op::Get => {
                                state = State::Done;
                            }
                            Op::Set => {
                                state = State::ReadValueLen;
                            }
                            Op::Echo => {
                                unreachable!();
                            }
                        }
                        continue 'state_loop;
                    }
                    needs_read = true;
                }
                State::ReadValueLen => {
                    tracing::debug!(session = %self.id, ptr = %ptr, buf_len = %self.buf.len(), "handling State::ReadValueLen");
                    // read between `:` and `:`
                    while ptr < self.buf.len() {
                        if !between_colons {
                            if self.buf[ptr] != b':' {
                                return Err(format!(
                                    "reading value_len, expected ':' found {:?}",
                                    &self.buf[ptr]
                                )
                                .into());
                            }
                            between_colons = true;
                            ptr += 1;
                        } else if self.buf[ptr] == b':' {
                            between_colons = false;
                            ptr += 1;
                            value_len = std::str::from_utf8(&value_len_buf)
                                .map_err(|e| format!("value length is invalid utf8: {e}"))?
                                .parse::<usize>()?;
                            state = State::ReadValue;
                            continue 'state_loop;
                        } else {
                            value_len_buf.push(self.buf[ptr]);
                            ptr += 1;
                        }
                    }
                    needs_read = true;
                }
                State::ReadValue => {
                    tracing::debug!(session = %self.id, ptr = %ptr, buf_len = %self.buf.len(), "handling State::ReadValue");
                    while ptr < self.buf.len() && value.len() < value_len {
                        value.push(self.buf[ptr]);
                        ptr += 1;
                    }
                    if value.len() >= value_len {
                        state = State::Done;
                        continue 'state_loop;
                    }
                    needs_read = true;
                }
                State::Done => {
                    tracing::debug!(session = %self.id, ptr = %ptr, buf_len = %self.buf.len(), "handling State::Done");
                    let key =
                        String::from_utf8(key).map_err(|e| format!("key is invalid utf8: {e}"))?;
                    tracing::debug!(session = %self.id, "handling State::Done: {:?} {:?}", op, key);
                    match op {
                        Op::Echo => return Ok(ProtoOp::Echo { msg: echo }),
                        Op::Get => return Ok(ProtoOp::Get { key }),
                        // todo: return a ProtoOp::Set that can stream the value from the socket reader
                        Op::Set => return Ok(ProtoOp::Set { key, value }),
                    }
                }
            }
        }
    }
}
