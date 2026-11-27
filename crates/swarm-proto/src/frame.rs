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
    match read_frame_body(r).await? {
        Some(body) => decode(&body).map(Some),
        None => Ok(None),
    }
}

/// The body of the next frame, not yet decoded; `None` at the end of the stream.
pub async fn read_frame_body<R>(r: &mut R) -> io::Result<Option<Vec<u8>>>
where
    R: AsyncRead + Unpin,
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
    // Grown as the bytes come, not all at once from the length alone.
    let mut body = Vec::with_capacity(len.min(64 * 1024));
    (&mut *r).take(len as u64).read_to_end(&mut body).await?;
    if body.len() < len {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "frame cut short"));
    }
    Ok(Some(body))
}

pub fn decode<T: DeserializeOwned>(body: &[u8]) -> io::Result<T> {
    postcard::from_bytes(body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// The id of a `ClientMsg::Request` whose request this build can't decode
/// (a newer client's), so it can be answered with an error rather than the
/// whole connection dropped.
pub fn undecodable_request_id(body: &[u8]) -> Option<u64> {
    // postcard: the variant index, then the fields in order.
    let (variant, rest) = postcard::take_from_bytes::<u32>(body).ok()?;
    if variant != 1 {
        return None;
    }
    postcard::take_from_bytes::<u64>(rest).ok().map(|(id, _)| id)
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

    #[test]
    fn an_unknown_request_still_gives_its_id() {
        let mut body = postcard::to_allocvec(&ClientMsg::Request { id: 300, req: Request::List }).unwrap();
        // Variant 1, id 300 as a two-byte varint, then the request's variant.
        assert_eq!(body[..3], [1, 0xac, 0x02]);
        body[3] = 0x7f;
        assert!(decode::<ClientMsg>(&body).is_err());
        assert_eq!(undecodable_request_id(&body), Some(300));
        let input = postcard::to_allocvec(&ClientMsg::Input { stream: 1, data: vec![] }).unwrap();
        assert_eq!(undecodable_request_id(&input), None);
    }

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

    #[tokio::test]
    async fn a_frame_cut_short_is_an_error() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        a.write_all(&(1_000_000u32).to_be_bytes()).await.unwrap();
        a.write_all(b"abc").await.unwrap();
        drop(a);
        let err = read_frame::<_, ClientMsg>(&mut b).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }
}
