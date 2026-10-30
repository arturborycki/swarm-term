//! Length-prefixed postcard frames.

use std::io;

use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Upper bound on one frame; output chunks are far smaller than this.
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

pub fn encode<T: Serialize>(msg: &T) -> io::Result<Vec<u8>> {
    let body = postcard::to_stdvec(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if body.len() > MAX_FRAME_LEN {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

pub async fn write_frame<W, T>(w: &mut W, msg: &T) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    w.write_all(&encode(msg)?).await?;
    w.flush().await
}

/// Reads one frame. Returns `Ok(None)` on a clean end of stream between frames.
pub async fn read_frame<R, T>(r: &mut R) -> io::Result<Option<T>>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME_LEN {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    postcard::from_bytes(&body)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// A reader that owns its stream, for use in `select!` loops.
pub struct FrameReader<R> {
    inner: R,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub fn new(inner: R) -> Self {
        FrameReader { inner }
    }

    pub async fn next<T: DeserializeOwned>(&mut self) -> io::Result<Option<T>> {
        read_frame(&mut self.inner).await
    }

    pub fn into_inner(self) -> R {
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ClientMsg, Request};

    #[tokio::test]
    async fn round_trip() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        let msg = ClientMsg::Request { id: 7, req: Request::List };
        write_frame(&mut a, &msg).await.unwrap();
        write_frame(&mut a, &ClientMsg::Input { stream: 1, data: b"ls\n".to_vec() }).await.unwrap();
        drop(a);
        match read_frame::<_, ClientMsg>(&mut b).await.unwrap() {
            Some(ClientMsg::Request { id: 7, req: Request::List }) => {}
            other => panic!("unexpected {other:?}"),
        }
        match read_frame::<_, ClientMsg>(&mut b).await.unwrap() {
            Some(ClientMsg::Input { stream: 1, data }) => assert_eq!(data, b"ls\n"),
            other => panic!("unexpected {other:?}"),
        }
        assert!(read_frame::<_, ClientMsg>(&mut b).await.unwrap().is_none());
    }
}
