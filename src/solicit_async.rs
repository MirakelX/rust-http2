use std::io;
use std::io::Read;
use std::net::SocketAddr;

use bytes::Bytes;

use futures::future;
use futures::future::done;
use futures::future::Loop;
use futures::future::loop_fn;
use futures::future::Future;
use futures::future::BoxFuture;
use futures::stream::Stream;
use futures::stream::BoxStream;

use tokio_io::io::read_exact;
use tokio_io::io::write_all;
use tokio_core::net::TcpStream;
use tokio_core::reactor;
use tokio_io::AsyncWrite;
use tokio_io::AsyncRead;

use error;
use error::Error;
use error::ErrorCode;
use result::Result;

use solicit::StreamId;
use solicit::frame::FRAME_HEADER_LEN;
use solicit::frame::RawFrame;
use solicit::frame::RawFrameRef;
use solicit::frame::FrameIR;
use solicit::frame::headers::HeadersFlag;
use solicit::frame::headers::HeadersFrame;
use solicit::frame::push_promise::PushPromiseFrame;
use solicit::frame::push_promise::PushPromiseFlag;
use solicit::frame::unpack_header;
use solicit::frame::settings::SettingsFrame;
use solicit::frame::settings::HttpSetting;
use solicit::connection::HttpFrame;

use misc::BsDebug;


pub type HttpFuture<T> = Box<Future<Item=T, Error=Error>>;
// Type is called `HttpFutureStream`, not just `HttpStream`
// to avoid confusion with streams from HTTP/2 spec
pub type HttpFutureStream<T> = Box<Stream<Item=T, Error=Error>>;

pub type HttpFutureSend<T> = BoxFuture<T, Error>;
pub type HttpFutureStreamSend<T> = BoxStream<T, Error>;


struct VecWithPos<T> {
    vec: Vec<T>,
    pos: usize,
}

impl<T> AsMut<[T]> for VecWithPos<T> {
    fn as_mut(&mut self) -> &mut [T] {
        &mut self.vec[self.pos..]
    }
}

pub fn recv_raw_frame<'r, R : AsyncRead + 'r>(read: R, max_frame_size: u32)
    -> Box<Future<Item=(R, RawFrame), Error=error::Error> + 'r>
{
    let header = read_exact(read, [0; FRAME_HEADER_LEN]).map_err(error::Error::from);
    let frame_buf = header.and_then(move |(read, raw_header)| -> Box<Future<Item=_, Error=_> + 'r> {
        let header = unpack_header(&raw_header);

        if header.length > max_frame_size {
            warn!("closing conn because peer sent frame with size: {}, max_frame_size: {}",
                header.length, max_frame_size);
            return Box::new(future::err(error::Error::CodeError(ErrorCode::FrameSizeError)));
        }

        let total_len = FRAME_HEADER_LEN + header.length as usize;
        let mut full_frame = VecWithPos {
            vec: Vec::with_capacity(total_len),
            pos: 0,
        };

        full_frame.vec.extend(&raw_header);
        full_frame.vec.resize(total_len, 0);
        full_frame.pos = FRAME_HEADER_LEN;

        Box::new(read_exact(read, full_frame).map_err(error::Error::from))
    });
    let frame = frame_buf.map(|(read, frame_buf)| {
        (read, RawFrame::from(frame_buf.vec))
    });
    Box::new(frame)
}

struct SyncRead<'r, R : Read + ?Sized + 'r>(&'r mut R);

impl<'r, R : Read + ?Sized + 'r> Read for SyncRead<'r, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl<'r, R : Read + ?Sized + 'r> AsyncRead for SyncRead<'r, R> {
}


pub fn recv_raw_frame_sync(read: &mut Read, max_frame_size: u32) -> Result<RawFrame> {
    Ok(recv_raw_frame(SyncRead(read), max_frame_size).wait()?.1)
}

/// Recieve HTTP frame from reader.
pub fn recv_http_frame<'r, R : AsyncRead + 'r>(read: R, max_frame_size: u32)
    -> Box<Future<Item=(R, HttpFrame), Error=Error> + 'r>
{
    Box::new(recv_raw_frame(read, max_frame_size).and_then(|(read, raw_frame)| {
        Ok((read, HttpFrame::from_raw(&raw_frame)?))
    }))
}

/// Recieve HTTP frame, joining CONTINUATION frame with preceding HEADER frames.
pub fn recv_http_frame_join_cont<'r, R : AsyncRead + 'r>(read: R, max_frame_size: u32)
    -> Box<Future<Item=(R, HttpFrame), Error=Error> + 'r>
{
    enum ContinuableFrame {
        Headers(HeadersFrame),
        PushPromise(PushPromiseFrame),
    }

    impl ContinuableFrame {
        fn into_frame(self) -> HttpFrame {
            match self {
                ContinuableFrame::Headers(headers) => HttpFrame::Headers(headers),
                ContinuableFrame::PushPromise(push_promise) => HttpFrame::PushPromise(push_promise),
            }
        }

        fn extend_header_fragment(&mut self, bytes: Bytes) {
            let header_fragment = match self {
                &mut ContinuableFrame::Headers(ref mut headers) => &mut headers.header_fragment,
                &mut ContinuableFrame::PushPromise(ref mut push_promise) => &mut push_promise.header_fragment,
            };
            header_fragment.extend_from_slice(&bytes);
        }

        fn set_end_headers(&mut self) {
            match self {
                &mut ContinuableFrame::Headers(ref mut headers) =>
                    headers.flags.set(HeadersFlag::EndHeaders),
                &mut ContinuableFrame::PushPromise(ref mut push_promise) =>
                    push_promise.flags.set(PushPromiseFlag::EndHeaders),
            }
        }

        fn get_stream_id(&self) -> StreamId {
            match self {
                &ContinuableFrame::Headers(ref headers) => headers.stream_id,
                &ContinuableFrame::PushPromise(ref push_promise) => push_promise.stream_id,
            }
        }
    }

    Box::new(loop_fn::<(R, Option<ContinuableFrame>), _, _, _>((read, None), move |(read, header_opt)| {
        recv_http_frame(read, max_frame_size).and_then(move |(read, frame)| {
            match frame {
                HttpFrame::Headers(h) => {
                    if let Some(_) = header_opt {
                        Err(Error::Other("expecting CONTINUATION frame, got HEADERS"))
                    } else {
                        if h.flags.is_set(HeadersFlag::EndHeaders) {
                            Ok(Loop::Break((read, HttpFrame::Headers(h))))
                        } else {
                            Ok(Loop::Continue((read, Some(ContinuableFrame::Headers(h)))))
                        }
                    }
                }
                HttpFrame::PushPromise(p) => {
                    if let Some(_) = header_opt {
                        Err(Error::Other("expecting CONTINUATION frame, got PUSH_PROMISE"))
                    } else {
                        if p.flags.is_set(PushPromiseFlag::EndHeaders) {
                            Ok(Loop::Break((read, HttpFrame::PushPromise(p))))
                        } else {
                            Ok(Loop::Continue((read, Some(ContinuableFrame::PushPromise(p)))))
                        }
                    }
                }
                HttpFrame::Continuation(c) => {
                    if let Some(mut h) = header_opt {
                        if h.get_stream_id() != c.stream_id {
                            Err(Error::Other("CONTINUATION frame with different stream id"))
                        } else {
                            let header_end = c.is_headers_end();
                            h.extend_header_fragment(c.header_fragment);
                            if header_end {
                                h.set_end_headers();
                                Ok(Loop::Break((read, h.into_frame())))
                            } else {
                                Ok(Loop::Continue((read, Some(h))))
                            }
                        }
                    } else {
                        Err(Error::Other("CONTINUATION frame without headers"))
                    }
                }
                f => {
                    if let Some(_) = header_opt {
                        Err(Error::Other("expecting CONTINUATION frame"))
                    } else {
                        Ok(Loop::Break((read, f)))
                    }
                },
            }
        })
    }))
}

pub fn recv_settings_frame<'r, R : AsyncRead + 'r>(read: R, max_frame_size: u32)
    -> Box<Future<Item=(R, SettingsFrame), Error=Error> + 'r>
{
    Box::new(recv_http_frame(read, max_frame_size)
        .and_then(|(read, http_frame)| {
            match http_frame {
                HttpFrame::Settings(f) => {
                    Ok((read, f))
                }
                f => {
                    Err(Error::InvalidFrame(format!("unexpected frame, expected SETTINGS, got {:?}", f.frame_type())))
                }
            }
        }))
}

pub fn recv_settings_frame_ack<R : AsyncRead + Send + 'static>(read: R, max_frame_size: u32)
    -> HttpFuture<(R, SettingsFrame)>
{
    Box::new(recv_settings_frame(read, max_frame_size).and_then(|(read, frame)| {
        if frame.is_ack() {
            Ok((read, frame))
        } else {
            Err(Error::InvalidFrame("expecting SETTINGS with ack, got without ack".to_owned()))
        }
    }))
}

pub fn recv_settings_frame_set<R : AsyncRead + Send + 'static>(read: R, max_frame_size: u32)
    -> HttpFuture<(R, SettingsFrame)>
{
    Box::new(recv_settings_frame(read, max_frame_size).and_then(|(read, frame)| {
        if !frame.is_ack() {
            Ok((read, frame))
        } else {
            Err(Error::InvalidFrame("expecting SETTINGS without ack, got with ack".to_owned()))
        }
    }))
}

#[allow(dead_code)]
pub fn send_raw_frame<W : AsyncWrite + Send + 'static>(write: W, frame: RawFrame) -> HttpFuture<W> {
    let bytes = frame.serialize();
    Box::new(write_all(write, bytes.clone())
        .map(|(w, _)| w)
        .map_err(|e| e.into()))
}

pub fn send_frame<W : AsyncWrite + Send + 'static, F : FrameIR>(write: W, frame: F) -> HttpFuture<W> {
    let buf = frame.serialize_into_vec();
    debug!("send frame {}", RawFrameRef { raw_content: &buf }.frame_type());
    Box::new(write_all(write, buf)
        .map(|(w, _)| w)
        .map_err(|e| e.into()))
}

static PREFACE: &'static [u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

fn send_settings<W : AsyncWrite + Send + 'static>(conn: W) -> HttpFuture<W> {
    let settings = {
        let mut frame = SettingsFrame::new();
        frame.add_setting(HttpSetting::EnablePush(false));
        frame
    };

    Box::new(send_frame(conn, settings))
}

pub fn client_handshake<I : AsyncWrite + AsyncRead + Send + 'static>(conn: I) -> HttpFuture<I> {
    debug!("send PREFACE");
    let send_preface = write_all(conn, PREFACE)
        .map(|(conn, _)| conn)
        .map_err(|e| e.into());

    let send_settings = send_preface.and_then(send_settings);

    Box::new(send_settings)
}

pub fn server_handshake<I : AsyncRead + AsyncWrite + Send + 'static>(conn: I) -> HttpFuture<I> {
    let mut preface_buf = Vec::with_capacity(PREFACE.len());
    preface_buf.resize(PREFACE.len(), 0);
    let recv_preface = read_exact(conn, preface_buf)
        .map_err(|e| e.into())
        .and_then(|(conn, preface_buf)| {
            done(if preface_buf == PREFACE {
                Ok((conn))
            } else {
                if preface_buf[0] == 0x16 {
                    Err(Error::InvalidFrame(format!("wrong preface, likely TLS: {:?}", BsDebug(&preface_buf))))
                } else {
                    Err(Error::InvalidFrame(format!("wrong preface: {:?}", BsDebug(&preface_buf))))
                }
            })
        });

    let send_settings = recv_preface.and_then(send_settings);

    Box::new(send_settings)
}

pub fn connect_and_handshake(lh: &reactor::Handle, addr: &SocketAddr) -> HttpFuture<TcpStream> {
    let connect = TcpStream::connect(&addr, lh)
        .map_err(|e| e.into());

    let handshake = connect.and_then(client_handshake);

    Box::new(handshake)
}
