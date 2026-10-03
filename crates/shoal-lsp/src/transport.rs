use std::io;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
    DuplexStream,
};
use tokio::task::JoinHandle;

pub const MAX_LSP_HEADER_BYTES: usize = 16 * 1024;
pub const MAX_LSP_HEADERS: usize = 64;
/// Four MiB documents can expand by up to six times when JSON escaped. Leave
/// envelope headroom while keeping tower-lsp's private codec behind a hard
/// allocation wall.
pub const MAX_LSP_BODY_BYTES: usize = 32 * 1024 * 1024;
const PUMP_BUFFER_BYTES: usize = 64 * 1024;

pub fn bounded_lsp_input<R>(reader: R) -> (DuplexStream, JoinHandle<io::Result<()>>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let (tower_input, pump_output) = tokio::io::duplex(PUMP_BUFFER_BYTES);
    let task = tokio::spawn(pump_lsp_frames(reader, pump_output));
    (tower_input, task)
}

pub async fn pump_lsp_frames<R, W>(reader: R, mut output: W) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut reader = BufReader::with_capacity(8 * 1024, reader);
    loop {
        let Some((header, body_len)) = read_header(&mut reader).await? else {
            output.shutdown().await?;
            return Ok(());
        };
        output.write_all(&header).await?;
        let mut remaining = body_len;
        let mut buffer = [0_u8; 16 * 1024];
        while remaining != 0 {
            let take = remaining.min(buffer.len());
            let count = reader.read(&mut buffer[..take]).await?;
            if count == 0 {
                return Err(invalid_data("LSP body ended before Content-Length"));
            }
            output.write_all(&buffer[..count]).await?;
            remaining -= count;
        }
        output.flush().await?;
    }
}

async fn read_header<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> io::Result<Option<(Vec<u8>, usize)>> {
    let mut header = Vec::with_capacity(512);
    let mut content_length = None;
    let mut count = 0_usize;
    loop {
        let Some(line) = read_bounded_line(reader, MAX_LSP_HEADER_BYTES - header.len()).await?
        else {
            if header.is_empty() {
                return Ok(None);
            }
            return Err(invalid_data("LSP header ended before blank line"));
        };
        if header.len().saturating_add(line.len()) > MAX_LSP_HEADER_BYTES {
            return Err(invalid_data("LSP header exceeds byte limit"));
        }
        if !line.ends_with(b"\r\n") {
            return Err(invalid_data("LSP headers require CRLF"));
        }
        header.extend_from_slice(&line);
        if line == b"\r\n" {
            break;
        }
        count += 1;
        if count > MAX_LSP_HEADERS {
            return Err(invalid_data("LSP header count exceeds limit"));
        }
        let field = &line[..line.len() - 2];
        let Some(colon) = field.iter().position(|byte| *byte == b':') else {
            return Err(invalid_data("malformed LSP header"));
        };
        let name = &field[..colon];
        let value = trim_ascii_space(&field[colon + 1..]);
        if name.eq_ignore_ascii_case(b"content-length") {
            if content_length.is_some() {
                return Err(invalid_data("duplicate LSP Content-Length"));
            }
            let parsed = parse_content_length(value)?;
            if parsed > MAX_LSP_BODY_BYTES {
                return Err(invalid_data("LSP body exceeds byte limit"));
            }
            content_length = Some(parsed);
        }
    }
    let content_length =
        content_length.ok_or_else(|| invalid_data("missing LSP Content-Length"))?;
    Ok(Some((header, content_length)))
}

async fn read_bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    remaining_header: usize,
) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::with_capacity(128.min(remaining_header));
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(invalid_data("LSP header line ended before newline"))
            };
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if line.len().saturating_add(take) > remaining_header {
            return Err(invalid_data("LSP header exceeds byte limit"));
        }
        let found_newline = available.get(take - 1) == Some(&b'\n');
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if found_newline {
            return Ok(Some(line));
        }
    }
}

fn parse_content_length(value: &[u8]) -> io::Result<usize> {
    if value.is_empty() || value.len() > 20 || !value.iter().all(u8::is_ascii_digit) {
        return Err(invalid_data("invalid LSP Content-Length"));
    }
    value.iter().try_fold(0_usize, |length, digit| {
        length
            .checked_mul(10)
            .and_then(|length| length.checked_add(usize::from(digit - b'0')))
            .ok_or_else(|| invalid_data("invalid LSP Content-Length"))
    })
}

fn trim_ascii_space(mut value: &[u8]) -> &[u8] {
    while matches!(value.first(), Some(b' ' | b'\t')) {
        value = &value[1..];
    }
    while matches!(value.last(), Some(b' ' | b'\t')) {
        value = &value[..value.len() - 1];
    }
    value
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn frame(body: &[u8]) -> Vec<u8> {
        let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        frame.extend_from_slice(body);
        frame
    }

    #[tokio::test]
    async fn forwards_multiple_valid_frames_exactly() {
        let mut input = frame(br#"{"jsonrpc":"2.0","id":1}"#);
        input.extend_from_slice(&frame(br#"{"jsonrpc":"2.0","id":2}"#));
        let expected = input.clone();
        let mut output = Vec::new();
        pump_lsp_frames(input.as_slice(), &mut output)
            .await
            .unwrap();
        assert_eq!(output, expected);
    }

    #[tokio::test]
    async fn rejects_huge_or_malformed_headers_without_waiting_for_body() {
        for input in [
            format!("Content-Length: {}\r\n\r\n", MAX_LSP_BODY_BYTES + 1).into_bytes(),
            format!("X: {}\r\n\r\n", "x".repeat(MAX_LSP_HEADER_BYTES)).into_bytes(),
            b"Content-Length: 12\n\n".to_vec(),
            b"Content-Length: 1\r\nContent-Length: 1\r\n\r\n".to_vec(),
            b"Content-Length: nope\r\n\r\n".to_vec(),
            b"X-Test: ok\r\n\r\n".to_vec(),
        ] {
            let mut output = Vec::new();
            let error = pump_lsp_frames(input.as_slice(), &mut output)
                .await
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(error.to_string().len() < 96);
            assert!(output.is_empty());
        }
    }

    #[tokio::test]
    async fn missing_and_trickled_bodies_fail_or_stream_cleanly() {
        let mut output = Vec::new();
        let error = pump_lsp_frames(b"Content-Length: 4\r\n\r\nxx".as_slice(), &mut output)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        let (mut sender, input) = tokio::io::duplex(64);
        let (mut receiver, pump_output) = tokio::io::duplex(64);
        let pump = tokio::spawn(pump_lsp_frames(input, pump_output));
        sender
            .write_all(b"Content-Length: 4\r\n\r\nx")
            .await
            .unwrap();
        let expected_prefix = b"Content-Length: 4\r\n\r\nx";
        let mut prefix = vec![0; expected_prefix.len()];
        receiver.read_exact(&mut prefix).await.unwrap();
        assert_eq!(&prefix, expected_prefix);
        sender.write_all(b"yz!").await.unwrap();
        sender.shutdown().await.unwrap();
        let mut tail = Vec::new();
        receiver.read_to_end(&mut tail).await.unwrap();
        assert_eq!(tail, b"yz!");
        pump.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn header_count_limit_is_exact() {
        let mut accepted = Vec::new();
        for index in 0..MAX_LSP_HEADERS - 1 {
            accepted.extend_from_slice(format!("X-{index}: ok\r\n").as_bytes());
        }
        accepted.extend_from_slice(b"Content-Length: 0\r\n\r\n");
        let mut output = Vec::new();
        pump_lsp_frames(accepted.as_slice(), &mut output)
            .await
            .unwrap();
        assert_eq!(output, accepted);

        let mut rejected = Vec::new();
        for index in 0..MAX_LSP_HEADERS {
            rejected.extend_from_slice(format!("X-{index}: ok\r\n").as_bytes());
        }
        rejected.extend_from_slice(b"Content-Length: 0\r\n\r\n");
        assert!(
            pump_lsp_frames(rejected.as_slice(), Vec::new())
                .await
                .is_err()
        );
    }
}
