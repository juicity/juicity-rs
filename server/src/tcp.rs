use std::io;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::watch;
use tokio::time::Instant;

/// Keep the reverse direction alive after EOF, and expire only when neither
/// direction makes progress. Both copies belong to this future so cancellation
/// also releases the sockets and streams.
pub(super) async fn relay<R, W, S>(
    reader: R,
    writer: W,
    remote: S,
    idle_timeout: Duration,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (remote_reader, remote_writer) = tokio::io::split(remote);
    let (activity, mut last_activity) = watch::channel(Instant::now());
    let copy = async {
        tokio::try_join!(
            copy_direction(reader, remote_writer, &activity),
            copy_direction(remote_reader, writer, &activity),
        )?;
        Ok(())
    };
    let idle = async {
        loop {
            let deadline = *last_activity.borrow_and_update() + idle_timeout;
            tokio::time::sleep_until(deadline).await;
            if !last_activity.has_changed().unwrap_or(false) {
                break;
            }
        }
    };

    tokio::select! {
        result = copy => result,
        _ = idle => Err(io::Error::new(io::ErrorKind::TimedOut, "TCP relay idle timeout")),
    }
}

async fn copy_direction<R, W>(
    mut reader: R,
    mut writer: W,
    activity: &watch::Sender<Instant>,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0; 16 * 1024];
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            return writer.shutdown().await;
        }
        activity.send_replace(Instant::now());
        let mut remaining = &buf[..n];
        while !remaining.is_empty() {
            let written = writer.write(remaining).await?;
            if written == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            activity.send_replace(Instant::now());
            remaining = &remaining[written..];
        }
    }
}

#[cfg(test)]
mod tests;
