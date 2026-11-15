//! Running the external programs that decode and encode media: ImageMagick
//! for pictures, FFmpeg for videos, ExifTool (optional) for photo metadata.

use anyhow::{Context, Result, bail};
use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Checks that `program` runs; otherwise explains how to install it.
pub fn need(program: &str) -> Result<()> {
    if available(program) {
        return Ok(());
    }
    let how = match program {
        "magick" => {
            "ImageMagick 7: macOS `brew install imagemagick`, Windows `winget install ImageMagick.ImageMagick`, Linux your package manager (version 7, the `magick` command)"
        }
        "ffmpeg" | "ffprobe" => {
            "FFmpeg: macOS `brew install ffmpeg`, Windows `winget install Gyan.FFmpeg`, Linux `sudo apt install ffmpeg`"
        }
        _ => "it",
    };
    bail!("`{program}` was not found. Install {how}, then run this again.")
}

pub fn available(program: &str) -> bool {
    Command::new(program)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn run<I, S>(program: &str, args: I, input: Option<&[u8]>) -> Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().with_context(|| format!("cannot start `{program}`"))?;
    if let Some(data) = input {
        // Write on another thread so a full stdout pipe cannot block us.
        let mut stdin = child.stdin.take().expect("piped stdin");
        let data = data.to_vec();
        let writer = std::thread::spawn(move || stdin.write_all(&data));
        let out = child.wait_with_output()?;
        writer.join().map_err(|_| anyhow::anyhow!("writer thread panicked"))??;
        return Ok(out);
    }
    Ok(child.wait_with_output()?)
}

fn tail(b: &[u8]) -> String {
    let s = String::from_utf8_lossy(b);
    let s = s.trim();
    s[s.len().saturating_sub(400)..].to_string()
}

/// Runs ImageMagick and returns what it printed. A damaged picture makes
/// ImageMagick warn and exit with an error while still decoding what it
/// can, so output is accepted whenever there is some.
pub fn magick<I, S>(args: I) -> Result<Vec<u8>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let out = run("magick", args, None)?;
    if !out.status.success() && out.stdout.is_empty() {
        bail!("ImageMagick: {}", tail(&out.stderr));
    }
    Ok(out.stdout)
}

/// Runs ImageMagick with `input` on its standard input; it must succeed.
pub fn magick_in<I, S>(args: I, input: &[u8]) -> Result<Vec<u8>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let out = run("magick", args, Some(input))?;
    if !out.status.success() {
        bail!("ImageMagick: {}", tail(&out.stderr));
    }
    Ok(out.stdout)
}

/// Runs a program that must succeed and returns its standard output as text.
pub fn text<I, S>(program: &str, args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let out = run(program, args, None)?;
    if !out.status.success() {
        bail!("{program}: {}", tail(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Runs a program and returns (success, stderr).
pub fn status<I, S>(program: &str, args: I) -> Result<(bool, String)>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let out = run(program, args, None)?;
    Ok((out.status.success(), String::from_utf8_lossy(&out.stderr).into_owned()))
}

/// `path[0]`: the first picture in a file, as ImageMagick wants it.
pub fn first_frame(path: &Path) -> std::ffi::OsString {
    let mut s = path.as_os_str().to_os_string();
    s.push("[0]");
    s
}

/// A scratch folder that is removed when dropped.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new() -> Result<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!("apfs-rescue-repair-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&p)?;
        Ok(Self(p))
    }
    pub fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
