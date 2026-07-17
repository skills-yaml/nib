use serde_json::Value;
use std::fmt;
#[cfg(test)]
use std::io::BufRead;
use std::io::{self, Write};
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

pub(crate) const MAX_MCP_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
pub(crate) enum FrameError {
    Io(io::Error),
    Json(serde_json::Error),
    TooLarge { max_bytes: usize },
    Unterminated,
}

impl fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::Json(error) => write!(formatter, "JSON error: {error}"),
            Self::TooLarge { max_bytes } => {
                write!(formatter, "frame exceeds the {max_bytes}-byte limit")
            }
            Self::Unterminated => formatter.write_str("frame is not newline terminated"),
        }
    }
}

impl From<io::Error> for FrameError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub(crate) fn encode_json(value: &Value, max_bytes: usize) -> Result<Vec<u8>, FrameError> {
    let mut bytes = Vec::new();
    let (result, exceeded) = {
        let mut writer = BoundedWriter {
            bytes: &mut bytes,
            max_bytes,
            exceeded: false,
        };
        let result = serde_json::to_writer(&mut writer, value);
        (result, writer.exceeded)
    };
    if exceeded {
        return Err(FrameError::TooLarge { max_bytes });
    }
    result.map_err(FrameError::Json)?;
    Ok(bytes)
}

pub(crate) fn encode_json_line(value: &Value) -> Result<Vec<u8>, FrameError> {
    let mut bytes = encode_json(value, MAX_MCP_FRAME_BYTES - 1)?;
    bytes.push(b'\n');
    Ok(bytes)
}

#[cfg(test)]
pub(crate) fn read_sync_frame<R: BufRead>(reader: &mut R) -> Result<Option<Vec<u8>>, FrameError> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if frame.is_empty() {
                Ok(None)
            } else {
                Err(FrameError::Unterminated)
            };
        }

        let consumed = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        if frame.len().saturating_add(consumed) > MAX_MCP_FRAME_BYTES {
            return Err(FrameError::TooLarge {
                max_bytes: MAX_MCP_FRAME_BYTES,
            });
        }
        frame.extend_from_slice(&available[..consumed]);
        let terminated = frame.last() == Some(&b'\n');
        reader.consume(consumed);
        if terminated {
            frame.pop();
            return Ok(Some(frame));
        }
    }
}

pub(crate) async fn read_async_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<Vec<u8>>, FrameError> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if frame.is_empty() {
                Ok(None)
            } else {
                Err(FrameError::Unterminated)
            };
        }

        let consumed = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        if frame.len().saturating_add(consumed) > MAX_MCP_FRAME_BYTES {
            return Err(FrameError::TooLarge {
                max_bytes: MAX_MCP_FRAME_BYTES,
            });
        }
        frame.extend_from_slice(&available[..consumed]);
        let terminated = frame.last() == Some(&b'\n');
        reader.consume(consumed);
        if terminated {
            frame.pop();
            return Ok(Some(frame));
        }
    }
}

struct BoundedWriter<'a> {
    bytes: &'a mut Vec<u8>,
    max_bytes: usize,
    exceeded: bool,
}

impl Write for BoundedWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.bytes.len().saturating_add(buffer.len()) > self.max_bytes {
            self.exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bounded MCP JSON payload exceeded",
            ));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor};

    #[test]
    fn hostile_oversized_sync_frame_is_rejected_without_reading_to_eof() {
        let input = vec![b'x'; MAX_MCP_FRAME_BYTES + 128];
        let mut reader = BufReader::with_capacity(64, Cursor::new(input));

        let error = read_sync_frame(&mut reader).expect_err("oversized frame must fail");

        assert!(matches!(error, FrameError::TooLarge { .. }));
    }

    #[tokio::test]
    async fn hostile_oversized_async_frame_is_rejected_without_reading_to_eof() {
        let input = vec![b'x'; MAX_MCP_FRAME_BYTES + 128];
        let mut reader = tokio::io::BufReader::with_capacity(64, input.as_slice());

        let error = read_async_frame(&mut reader)
            .await
            .expect_err("oversized frame must fail");

        assert!(matches!(error, FrameError::TooLarge { .. }));
    }

    #[test]
    fn outbound_frame_encoder_never_emits_an_oversized_line() {
        let value = serde_json::json!({"payload": "x".repeat(MAX_MCP_FRAME_BYTES)});

        let error = encode_json_line(&value).expect_err("oversized frame must fail");

        assert!(matches!(error, FrameError::TooLarge { .. }));
    }
}
