// Modified for Shellmate (per-channel flow control, port of russh#730) - see ../../SHELLMATE-PATCH.md
use std::borrow::BorrowMut;
use std::io;
use std::pin::Pin;
use std::task::{ready, Context, Poll};

use tokio::io::AsyncRead;

use super::{ChannelMsg, ChannelReadHalf};

#[derive(Debug)]
pub struct ChannelRx<R> {
    channel: R,
    buffer: Option<(ChannelMsg, usize)>,
    eof: bool,

    ext: Option<u32>,
}

impl<R> ChannelRx<R> {
    pub fn new(channel: R, ext: Option<u32>) -> Self {
        Self {
            channel,
            buffer: None,
            eof: false,
            ext,
        }
    }
}

impl<R> AsyncRead for ChannelRx<R>
where
    R: BorrowMut<ChannelReadHalf> + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.eof {
            return Poll::Ready(Ok(()));
        }
        let (msg, mut idx) = match self.buffer.take() {
            Some(msg) => msg,
            None => {
                let read_half = self.channel.borrow_mut();
                match ready!(read_half.receiver.poll_recv(cx)) {
                    Some(msg) => {
                        // Room has been made in the queue: let the session use it.
                        read_half.notify_drained();
                        (msg, 0)
                    }
                    None => return Poll::Ready(Ok(())),
                }
            }
        };

        match (&msg, self.ext) {
            (ChannelMsg::Data { data }, None) => {
                let readable = buf.remaining().min(data.len() - idx);

                // Clamped to maximum `buf.remaining()` and `data.len() - idx` with `.min`
                #[allow(clippy::indexing_slicing)]
                buf.put_slice(&data[idx..idx + readable]);
                idx += readable;

                if idx != data.len() {
                    self.buffer = Some((msg, idx));
                }

                Poll::Ready(Ok(()))
            }
            (ChannelMsg::ExtendedData { data, ext }, Some(target)) if *ext == target => {
                let readable = buf.remaining().min(data.len() - idx);

                // Clamped to maximum `buf.remaining()` and `data.len() - idx` with `.min`
                #[allow(clippy::indexing_slicing)]
                buf.put_slice(&data[idx..idx + readable]);
                idx += readable;

                if idx != data.len() {
                    self.buffer = Some((msg, idx));
                }

                Poll::Ready(Ok(()))
            }
            (ChannelMsg::Eof, _) => {
                // Remember the end of the data instead of closing the receiver, so that what the
                // session still has queued behind it (exit status, close) can be read afterwards.
                self.eof = true;

                Poll::Ready(Ok(()))
            }
            _ => {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }
}
