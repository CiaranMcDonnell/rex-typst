//! Kitty graphics protocol: upload images once, then place parts of them.
//!
//! Images travel inside the escape sequence (t=d) as zlib-compressed RGBA
//! (f=32, o=z), so this works when the Rex server is on another machine.
//! q=2 keeps the terminal from replying into our input.

use std::io::Write;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use flate2::Compression;
use flate2::write::ZlibEncoder;

const CHUNK: usize = 4096;

/// Upload RGBA pixels as image `id`, replacing any image with that id.
pub fn upload(out: &mut impl Write, id: u32, width: u32, height: u32, rgba: &[u8]) -> std::io::Result<()> {
    let mut z = ZlibEncoder::new(Vec::with_capacity(rgba.len() / 8), Compression::fast());
    z.write_all(rgba)?;
    let data = STANDARD.encode(z.finish()?);
    let bytes = data.as_bytes();
    let mut first = true;
    let mut i = 0;
    while i < bytes.len() || first {
        let end = (i + CHUNK).min(bytes.len());
        let more = u8::from(end < bytes.len());
        let chunk = std::str::from_utf8(&bytes[i..end]).unwrap_or("");
        if first {
            write!(out, "\x1b_Ga=t,f=32,o=z,s={width},v={height},i={id},q=2,m={more};{chunk}\x1b\\")?;
            first = false;
        } else {
            write!(out, "\x1b_Gm={more};{chunk}\x1b\\")?;
        }
        i = end;
    }
    Ok(())
}

/// A part of an uploaded image to show on screen.
pub struct Placement {
    pub id: u32,
    /// Screen position, 0-based cells.
    pub col: u16,
    pub row: u16,
    /// Source rectangle in image pixels.
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    /// Size on screen in cells.
    pub cols: u16,
    pub rows: u16,
}

/// Show a placement. Each image has one placement (p=1), so placing it again
/// moves it. The cursor is saved and restored around it.
pub fn place(out: &mut impl Write, p: &Placement) -> std::io::Result<()> {
    write!(
        out,
        "\x1b7\x1b[{};{}H\x1b_Ga=p,i={},p=1,x={},y={},w={},h={},c={},r={},C=1,q=2\x1b\\\x1b8",
        p.row + 1,
        p.col + 1,
        p.id,
        p.x,
        p.y,
        p.w,
        p.h,
        p.cols,
        p.rows
    )
}

/// Remove an image's placement but keep its pixels for later.
pub fn hide(out: &mut impl Write, id: u32) -> std::io::Result<()> {
    write!(out, "\x1b_Ga=d,d=i,i={id},q=2\x1b\\")
}

/// Remove an image and free its pixels.
pub fn delete(out: &mut impl Write, id: u32) -> std::io::Result<()> {
    write!(out, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\")
}

/// Remove every image this program placed and free them.
pub fn delete_all(out: &mut impl Write) -> std::io::Result<()> {
    write!(out, "\x1b_Ga=d,d=A,q=2\x1b\\")
}
