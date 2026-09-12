//! Multi-page PDFs from scanned PNM pages. Each page is encoded on its own: colour and gray as
//! JPEG, Lineart as lossless 1-bit. Its size follows its own resolution, and only one page's
//! pixels are in memory at a time.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use jpeg_encoder::{ColorType, Encoder};
use pdf_writer::{Content, Filter, Finish, Name, Pdf, Rect, Ref};

#[derive(Clone, Copy, Debug, PartialEq)]
enum Pixels {
    Bits,
    Gray,
    Rgb,
}

/// Header of an 8-bit binary PNM (P4, P5 or P6) as scanimage writes it: (width, height, kind, data).
fn read_pnm(bytes: &[u8]) -> Result<(u32, u32, Pixels, &[u8])> {
    let kind = match bytes.get(..2) {
        Some(b"P4") => Pixels::Bits,
        Some(b"P5") => Pixels::Gray,
        Some(b"P6") => Pixels::Rgb,
        _ => bail!("not a binary PNM file"),
    };
    let mut pos = 2;
    let mut number = || -> Result<u32> {
        loop {
            match bytes.get(pos) {
                Some(b'#') => {
                    pos += bytes[pos..]
                        .iter()
                        .position(|&b| b == b'\n')
                        .context("truncated PNM header")?
                }
                Some(b) if b.is_ascii_whitespace() => pos += 1,
                _ => break,
            }
        }
        let digits = bytes[pos..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count();
        let text = std::str::from_utf8(&bytes[pos..pos + digits])?;
        pos += digits;
        text.parse().context("bad PNM header")
    };
    let (width, height) = (number()?, number()?);
    if kind != Pixels::Bits {
        ensure!(number()? == 255, "only 8-bit scans can go into a PDF");
    }
    let start = pos + 1; // one whitespace byte ends the header
    let (w, h) = (width as usize, height as usize);
    let len = match kind {
        Pixels::Bits => w.div_ceil(8) * h,
        Pixels::Gray => w * h,
        Pixels::Rgb => 3 * w * h,
    };
    let data = bytes.get(start..start + len).context("truncated scan")?;
    Ok((width, height, kind, data))
}

/// Write the pages, each `(PNM file, its resolution in dpi)`, to `output`. The file is written
/// next to it first, so an interrupted save never destroys an existing file.
pub fn write(pages: &[(PathBuf, u32)], output: &Path) -> Result<()> {
    let mut pdf = Pdf::new();
    let (catalog, tree) = (Ref::new(1), Ref::new(2));
    let mut next = Ref::new(3);
    let mut kids = vec![];
    for (path, dpi) in pages {
        let bytes = fs::read(path).with_context(|| format!("can't read {}", path.display()))?;
        let (width, height, kind, data) = read_pnm(&bytes)?;
        let (page, image, content) = (next.bump(), next.bump(), next.bump());
        kids.push(page);
        let points = |pixels: u32| pixels as f32 * 72.0 / *dpi as f32;
        let (w, h) = (points(width), points(height));
        let name = Name(b"Scan");

        let mut page_writer = pdf.page(page);
        page_writer
            .parent(tree)
            .media_box(Rect::new(0.0, 0.0, w, h))
            .contents(content);
        page_writer.resources().x_objects().pair(name, image);
        page_writer.finish();

        // JPEG can't store more than 65535 px a side: such pages are compressed losslessly.
        let jpeg = kind != Pixels::Bits && width <= u16::MAX as u32 && height <= u16::MAX as u32;
        let encoded = if jpeg {
            let mut out = vec![];
            let color = if kind == Pixels::Gray {
                ColorType::Luma
            } else {
                ColorType::Rgb
            };
            Encoder::new(&mut out, 90).encode(data, width as u16, height as u16, color)?;
            out
        } else if kind == Pixels::Bits {
            // PNM uses 1 for black, PDF's DeviceGray 0.
            miniz_oxide::deflate::compress_to_vec_zlib(
                &data.iter().map(|b| !b).collect::<Vec<_>>(),
                6,
            )
        } else {
            miniz_oxide::deflate::compress_to_vec_zlib(data, 6)
        };
        let mut xobject = pdf.image_xobject(image, &encoded);
        xobject.width(width as i32).height(height as i32);
        xobject.bits_per_component(if kind == Pixels::Bits { 1 } else { 8 });
        match kind {
            Pixels::Rgb => xobject.color_space().device_rgb(),
            Pixels::Bits | Pixels::Gray => xobject.color_space().device_gray(),
        }
        xobject.filter(if jpeg {
            Filter::DctDecode
        } else {
            Filter::FlateDecode
        });
        xobject.finish();

        let mut ops = Content::new();
        ops.save_state()
            .transform([w, 0.0, 0.0, h, 0.0, 0.0])
            .x_object(name)
            .restore_state();
        pdf.stream(content, &ops.finish());
    }
    let count = kids.len() as i32;
    pdf.pages(tree).kids(kids).count(count);
    pdf.catalog(catalog).pages(tree);

    let part = crate::scan::part_path(output);
    fs::write(&part, pdf.finish()).with_context(|| format!("can't write {}", part.display()))?;
    fs::rename(&part, output)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::TempDir;

    fn pnm(magic: &str, width: u32, height: u32, bytes_per_row: u32, extra: &str) -> Vec<u8> {
        let mut file =
            format!("{magic}\n# SANE data follows\n{width} {height}\n{extra}").into_bytes();
        file.extend(vec![0x5a; (bytes_per_row * height) as usize]);
        file
    }

    #[test]
    fn reads_pnm() {
        let rgb = pnm("P6", 3, 2, 9, "255\n");
        let (w, h, kind, data) = read_pnm(&rgb).unwrap();
        assert_eq!((w, h, kind, data.len()), (3, 2, Pixels::Rgb, 18));
        assert_eq!(read_pnm(&pnm("P4", 10, 3, 2, "")).unwrap().3.len(), 6);
        assert!(read_pnm(&pnm("P5", 3, 2, 6, "65535\n")).is_err());
        assert!(read_pnm(&pnm("P5", 3, 2, 1, "255\n")).is_err()); // truncated
    }

    #[test]
    fn writes_pages_sized_by_resolution() {
        let dir = TempDir::new().unwrap();
        let pages: Vec<(PathBuf, u32)> =
            [("P6", 900, "255\n"), ("P5", 300, "255\n"), ("P4", 38, "")]
                .iter()
                .enumerate()
                .map(|(n, (magic, row, extra))| {
                    let path = dir.path().join(format!("{n}.pnm"));
                    fs::write(&path, pnm(magic, 300, 150, *row, extra)).unwrap();
                    (path, if n == 2 { 300 } else { 150 })
                })
                .collect();
        let out = dir.path().join("out.pdf");
        write(&pages, &out).unwrap();
        let text = String::from_utf8_lossy(&fs::read(&out).unwrap()).into_owned();
        assert_eq!(
            text.matches("/Type /Page\n").count() + text.matches("/Type /Page ").count(),
            3
        );
        assert_eq!(text.matches("/MediaBox [0 0 144 72]").count(), 2); // 300x150 px at 150 dpi = 2x1 in
        assert_eq!(text.matches("/MediaBox [0 0 72 36]").count(), 1); // the same at 300 dpi
        assert_eq!(text.matches("/DCTDecode").count(), 2);
        assert_eq!(text.matches("/FlateDecode").count(), 1);
        assert!(text.contains("/BitsPerComponent 1"));
        assert!(!dir.path().join("out.pdf.part").exists());
    }
}
