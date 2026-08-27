use std::io;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt};

pub enum Line {
    Value(Vec<u8>),
    Oversized,
    End,
}

pub async fn read_line(reader: &mut (impl AsyncBufRead + Unpin), limit: usize) -> io::Result<Line> {
    let mut retained = Vec::with_capacity(limit.min(1024));
    let mut oversized = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if retained.is_empty() {
                Ok(Line::End)
            } else if oversized {
                Ok(Line::Oversized)
            } else {
                Ok(Line::Value(retained))
            };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |index| index + 1);
        let content = newline.map_or(available, |index| &available[..index]);
        let remaining = limit.saturating_sub(retained.len());
        retained.extend_from_slice(&content[..content.len().min(remaining)]);
        oversized |= content.len() > remaining;
        reader.consume(consumed);
        if newline.is_some() {
            if retained.last() == Some(&b'\r') {
                retained.pop();
            }
            return if oversized {
                Ok(Line::Oversized)
            } else {
                Ok(Line::Value(retained))
            };
        }
    }
}

pub async fn drain(mut reader: impl AsyncRead + Unpin, limit: usize) -> io::Result<Vec<u8>> {
    let mut retained = Vec::with_capacity(limit.min(8 * 1024));
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(retained.len());
        retained.extend_from_slice(&buffer[..read.min(remaining)]);
    }
    Ok(retained)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncWriteExt, BufReader};

    #[tokio::test]
    async fn oversized_line_resynchronizes_and_drain_stays_bounded() {
        let (mut writer, reader) = tokio::io::duplex(1024);
        let write = tokio::spawn(async move {
            writer.write_all(b"12345\nok\n").await.unwrap();
        });
        let mut reader = BufReader::new(reader);
        assert!(matches!(
            read_line(&mut reader, 4).await.unwrap(),
            Line::Oversized
        ));
        assert!(matches!(
            read_line(&mut reader, 4).await.unwrap(),
            Line::Value(value) if value == b"ok"
        ));
        write.await.unwrap();

        let (mut writer, reader) = tokio::io::duplex(1024);
        let write = tokio::spawn(async move {
            writer.write_all(&vec![b'x'; 16 * 1024]).await.unwrap();
        });
        assert_eq!(drain(reader, 512).await.unwrap().len(), 512);
        write.await.unwrap();
    }
}
