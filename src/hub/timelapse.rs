//! Bounded disk-backed media relay. Never decode or buffer a complete movie.
use axum::{body::Body, http::StatusCode};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt, AsyncWriteExt};

pub(super) struct Timelapse {
    // Private, random temporary file, deleted on success, error, or cancellation.
    file: tempfile::NamedTempFile,
    pub length: u64,
    pub digest: String,
}

impl Timelapse {
    pub async fn receive(body: Body, length: u64) -> Result<Self, StatusCode> {
        let file = tokio::task::spawn_blocking(tempfile::NamedTempFile::new)
            .await
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        let mut output = tokio::fs::OpenOptions::new()
            .write(true)
            .read(true)
            .open(file.path())
            .await
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        let mut stream = body.into_data_stream();
        let mut received = 0_u64;
        let mut digest = Sha256::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| StatusCode::BAD_REQUEST)?;
            received = received
                .checked_add(chunk.len() as u64)
                .filter(|total| *total <= length)
                .ok_or(StatusCode::PAYLOAD_TOO_LARGE)?;
            digest.update(&chunk);
            output
                .write_all(&chunk)
                .await
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        }
        if received != length {
            return Err(StatusCode::UNPROCESSABLE_ENTITY);
        }
        output
            .flush()
            .await
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        if !valid_mp4(&mut output, length)
            .await
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
        {
            return Err(StatusCode::UNPROCESSABLE_ENTITY);
        }
        Ok(Self {
            file,
            length,
            digest: STANDARD.encode(digest.finalize()),
        })
    }

    pub async fn body(&self) -> std::io::Result<reqwest::Body> {
        // Each route has an independent cursor; retries reopen from zero.
        let input = tokio::fs::File::open(self.file.path()).await?;
        Ok(reqwest::Body::wrap_stream(
            tokio_util::io::ReaderStream::with_capacity(input, 64 * 1024),
        ))
    }
}

/// Bounded ISO-BMFF box scan; payloads are skipped, never decoded. Limit both
/// header count and ftyp metadata so hostile files cannot cause linear tiny I/O.
pub(super) async fn valid_mp4<R: AsyncRead + AsyncSeek + Unpin>(
    input: &mut R,
    length: u64,
) -> std::io::Result<bool> {
    let mut offset = 0_u64;
    let (mut ftyp, mut moov, mut mdat) = (false, false, false);
    for _ in 0..4096 {
        if offset == length {
            return Ok(ftyp && moov && mdat);
        }
        if length.saturating_sub(offset) < 8 {
            return Ok(false);
        }
        input.seek(std::io::SeekFrom::Start(offset)).await?;
        let mut header = [0_u8; 8];
        input.read_exact(&mut header).await?;
        let mut size = u32::from_be_bytes(header[..4].try_into().expect("four bytes")) as u64;
        let kind = &header[4..8];
        let header_len = if size == 1 {
            if length - offset < 16 {
                return Ok(false);
            }
            size = input.read_u64().await?;
            16
        } else {
            8
        };
        if size == 0 {
            size = length - offset;
        }
        let Some(end) = offset.checked_add(size).filter(|end| *end <= length) else {
            return Ok(false);
        };
        if size < header_len || (offset == 0 && kind != b"ftyp") {
            return Ok(false);
        }
        match kind {
            b"ftyp" => {
                if ftyp || size < header_len + 8 || size > 1024 {
                    return Ok(false);
                }
                let mut payload = vec![0; (size - header_len) as usize];
                input.read_exact(&mut payload).await?;
                if !payload.chunks_exact(4).enumerate().any(|(index, brand)| {
                    index != 1
                        && matches!(
                            brand,
                            b"isom"
                                | b"iso2"
                                | b"iso4"
                                | b"iso5"
                                | b"iso6"
                                | b"mp41"
                                | b"mp42"
                                | b"avc1"
                        )
                }) {
                    return Ok(false);
                }
                ftyp = true;
            }
            b"moov" => moov |= size > header_len,
            b"mdat" => mdat |= size > header_len,
            _ => {}
        }
        offset = end;
    }
    Ok(offset == length && ftyp && moov && mdat)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn movie() -> Vec<u8> {
        b"\0\0\0\x18ftypmp42\0\0\0\0isommp42\0\0\0\x09moovx\0\0\0\x09mdatx".to_vec()
    }

    #[tokio::test]
    async fn spool_hashes_exact_bytes_reopens_independently_and_removes_storage() {
        let bytes = movie();
        let spool = Timelapse::receive(Body::from(bytes.clone()), bytes.len() as u64)
            .await
            .unwrap();
        assert_eq!(spool.digest, STANDARD.encode(Sha256::digest(&bytes)));
        let path = spool.file.path().to_owned();
        let (first, second) = tokio::join!(spool.body(), spool.body());
        async fn read(body: reqwest::Body) -> Vec<u8> {
            axum::body::to_bytes(Body::new(body), 1024)
                .await
                .unwrap()
                .to_vec()
        }
        let (a, b) = tokio::join!(read(first.unwrap()), read(second.unwrap()));
        assert_eq!(a, bytes);
        assert_eq!(b, bytes);
        drop(spool);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn spool_rejects_short_overlong_and_broken_bodies() {
        let bytes = movie();
        for (body, length, expected) in [
            (
                Body::from(bytes.clone()),
                bytes.len() as u64 - 1,
                StatusCode::PAYLOAD_TOO_LARGE,
            ),
            (
                Body::from(bytes.clone()),
                bytes.len() as u64 + 1,
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                Body::from(vec![0; bytes.len()]),
                bytes.len() as u64,
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
        ] {
            assert!(
                matches!(Timelapse::receive(body, length).await, Err(status) if status == expected)
            );
        }
    }
}
