//! Sending files to a shared PC: one stream per file, inside the session
//! (see [`FileOffer`] for the exchange). The host saves to Downloads, or to
//! the folder the file was dropped on when it can tell.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::net::{Conn, RecvStream, SendStream};
use crate::proto::{read_msg, write_msg, FileOffer, FileReply};

/// A host that hasn't answered an offer by then can't receive files (an
/// older Test Bed never reads the stream).
pub const OFFER_TIMEOUT: Duration = Duration::from_secs(10);
const CHUNK: usize = 256 * 1024;
/// Longest file name kept, in characters.
const MAX_NAME: usize = 200;

/// This user's Downloads folder.
pub fn downloads_dir() -> PathBuf {
    directories::UserDirs::new()
        .map(|d| d.download_dir().map(Path::to_path_buf).unwrap_or_else(|| d.home_dir().join("Downloads")))
        .unwrap_or_else(|| PathBuf::from("Downloads"))
}

/// A file name that is safe on Linux and Windows: the last path component
/// only, without characters or names Windows reserves.
pub fn safe_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String =
        base.chars().map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { '_' } else { c }).take(MAX_NAME).collect();
    let cleaned = cleaned.trim().trim_end_matches(['.', ' ']).to_string();
    if cleaned.is_empty() {
        return "file".into();
    }
    let stem = cleaned.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit());
    if reserved { format!("_{cleaned}") } else { cleaned }
}

/// `dir/name`, or `dir/stem (2).ext` and so on if that is taken.
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let p = Path::new(name);
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| name.into());
    let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    (2..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .expect("some name is free")
}

/// Sends `path`; `progress(sent, total)` is called as bytes go out. Returns
/// where the host saved it.
pub async fn send(conn: &Conn, path: &Path, drop_at: Option<(f32, f32)>, mut progress: impl FnMut(u64, u64)) -> Result<String> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
    let mut file = tokio::fs::File::open(path).await.with_context(|| format!("opening {}", path.display()))?;
    let meta = file.metadata().await?;
    if !meta.is_file() {
        bail!("{name} is a folder: only files can be sent");
    }
    let size = meta.len();
    let (mut send, mut recv) = conn.open_bi().await?;
    write_msg(&mut send, &FileOffer { name: name.clone(), size, drop_at }).await?;
    match tokio::time::timeout(OFFER_TIMEOUT, read_msg::<FileReply>(&mut recv)).await {
        Err(_) => bail!("the other PC didn't answer: update Test Bed there to receive files"),
        Ok(reply) => match reply? {
            Some(FileReply::Ready { .. }) => {}
            Some(FileReply::Refused { text }) => bail!(text),
            other => bail!("unexpected answer {other:?}"),
        },
    }
    let mut buf = vec![0u8; CHUNK];
    let mut sent = 0u64;
    progress(0, size);
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        send.write_all(&buf[..n]).await?;
        sent += n as u64;
        progress(sent, size);
    }
    if sent != size {
        bail!("{name} changed while it was being sent");
    }
    send.finish();
    match read_msg::<FileReply>(&mut recv).await? {
        Some(FileReply::Saved { path }) => Ok(path),
        Some(FileReply::Refused { text }) => bail!(text),
        other => bail!("unexpected answer {other:?}"),
    }
}

/// The host's side of one file stream. `folder` picks the folder from the
/// drop point; it runs on a blocking thread. Returns the saved file.
pub async fn receive(
    mut send: SendStream,
    mut recv: RecvStream,
    folder: impl FnOnce(Option<(f32, f32)>) -> PathBuf + Send + 'static,
) -> Result<PathBuf> {
    let Some(offer) = read_msg::<FileOffer>(&mut recv).await? else { bail!("empty file stream") };
    let dir = tokio::task::spawn_blocking(move || folder(offer.drop_at)).await?;
    let name = safe_name(&offer.name);
    let opened = async {
        tokio::fs::create_dir_all(&dir).await?;
        let part = unique_path(&dir, &format!("{name}.part"));
        let f = tokio::fs::File::create(&part).await?;
        anyhow::Ok((part, f))
    }
    .await;
    let (part, mut f) = match opened {
        Ok(o) => o,
        Err(e) => {
            let text = format!("can't save in {}: {e:#}", dir.display());
            let _ = write_msg(&mut send, &FileReply::Refused { text: text.clone() }).await;
            bail!(text);
        }
    };
    write_msg(&mut send, &FileReply::Ready { folder: dir.display().to_string() }).await?;

    let copied = async {
        let mut buf = vec![0u8; CHUNK];
        let mut got = 0u64;
        while let Some(n) = recv.read(&mut buf).await? {
            got += n as u64;
            if got > offer.size {
                bail!("more data than offered");
            }
            f.write_all(&buf[..n]).await?;
        }
        if got != offer.size {
            bail!("the transfer stopped after {got} of {} bytes", offer.size);
        }
        f.flush().await?;
        anyhow::Ok(())
    }
    .await;
    drop(f);
    let saved = match copied {
        Ok(()) => {
            let dest = unique_path(&dir, &name);
            tokio::fs::rename(&part, &dest).await.map(|()| dest).map_err(anyhow::Error::from)
        }
        Err(e) => Err(e),
    };
    match saved {
        Ok(dest) => {
            write_msg(&mut send, &FileReply::Saved { path: dest.display().to_string() }).await?;
            send.finish();
            let _ = tokio::time::timeout(Duration::from_secs(2), send.stopped()).await;
            Ok(dest)
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(&part).await;
            let _ = write_msg(&mut send, &FileReply::Refused { text: format!("{e:#}") }).await;
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_made_safe() {
        assert_eq!(safe_name("report.pdf"), "report.pdf");
        assert_eq!(safe_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_name(r"C:\Users\me\a.txt"), "a.txt");
        assert_eq!(safe_name("a<b>c?.txt"), "a_b_c_.txt");
        assert_eq!(safe_name(".."), "file");
        assert_eq!(safe_name(""), "file");
        assert_eq!(safe_name("CON.txt"), "_CON.txt");
        assert_eq!(safe_name("com1"), "_com1");
        assert_eq!(safe_name("notes. "), "notes");
    }

    #[test]
    fn taken_names_get_a_number() {
        let dir = std::env::temp_dir().join(format!("tb-files-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(unique_path(&dir, "a.txt"), dir.join("a.txt"));
        std::fs::write(dir.join("a.txt"), b"").unwrap();
        assert_eq!(unique_path(&dir, "a.txt"), dir.join("a (2).txt"));
        std::fs::write(dir.join("a (2).txt"), b"").unwrap();
        assert_eq!(unique_path(&dir, "a.txt"), dir.join("a (3).txt"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
