//! Streaming writers for the strip export: PNG, TIFF (8/16 bit, BigTIFF for huge files) and JPEG
//! are written strip by strip, so the whole picture never has to be in memory.
//!
//! Use [`write_file`]: it opens the file, hands a [`RowSink`] to a producer (the strip render) and
//! finishes the file when the producer is done. The caller is responsible for the temporary file
//! and the rename (see `write_atomically` in `app/export.rs`), so a failed or cancelled export
//! leaves no half-written picture.
//!
//! * Strips must arrive in order from the top, without gaps.
//! * TIFF: every strip except the last must have exactly `rows_per_strip` rows.
//! * JPEG: baseline only (no progressive scans, no optimised Huffman tables – both would need the
//!   whole picture in memory), at most 65535 pixels per side. JPEG has no transparency; pixels
//!   outside the panorama are black.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Mutex;
use std::sync::mpsc;

use crate::stitch::{to_rgba16, to_rgba8};

/// Largest side of a JPEG in pixels.
pub const JPEG_MAX_SIDE: usize = 65_535;
/// Estimated file size above which TIFF is written as BigTIFF (64 bit offsets): 3.5 GB.
pub const BIGTIFF_ABOVE_BYTES: u64 = 3_500_000_000;
/// Strips waiting for the JPEG encoder; bounds the extra memory of the pipeline.
pub const JPEG_QUEUE: usize = 2;
/// Buffer size of the file writers.
const FILE_BUFFER: usize = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Jpeg { quality: u8 },
    Png,
    Tiff { sixteen_bit: bool },
}

/// Settings of [`write_file`]. `WriteOptions::new(format)` gives the documented defaults.
#[derive(Clone, Copy, Debug)]
pub struct WriteOptions {
    pub format: Format,
    /// TIFF: rows per strip of the file (must equal the height of the strips the producer sends)
    pub rows_per_strip: usize,
    /// TIFF: use BigTIFF when the uncompressed data is larger than this many bytes
    pub bigtiff_above: u64,
    /// JPEG: strips queued for the encoder
    pub jpeg_queue: usize,
}

impl WriteOptions {
    pub fn new(format: Format, rows_per_strip: usize) -> Self {
        WriteOptions { format, rows_per_strip: rows_per_strip.max(1), bigtiff_above: BIGTIFF_ABOVE_BYTES, jpeg_queue: JPEG_QUEUE }
    }
}

/// Takes the finished strips of the panorama in order from the top.
pub trait RowSink {
    /// `col` / `cov`: colour (linear) and coverage of the rows starting at row `y0`.
    fn push(&mut self, y0: usize, col: &[[f32; 3]], cov: &[f32]) -> Result<(), String>;
}

/// Size in bytes of the uncompressed picture data.
pub fn raw_bytes(w: usize, h: usize, format: Format) -> u64 {
    let per_px = match format {
        Format::Jpeg { .. } => 3,
        Format::Png | Format::Tiff { sixteen_bit: false } => 4,
        Format::Tiff { sixteen_bit: true } => 8,
    };
    w as u64 * h as u64 * per_px
}

/// Size in bytes the file can reach at most, for the free-space check: exact for TIFF, a safe
/// upper bound for PNG (photos compress to about 70 %), and for JPEG a generous bound by quality.
pub fn estimated_file_bytes(w: usize, h: usize, format: Format) -> u64 {
    let px = w as u64 * h as u64;
    match format {
        Format::Tiff { .. } => raw_bytes(w, h, format),
        Format::Png => raw_bytes(w, h, format) * 8 / 10,
        // bytes per pixel of a photo at quality 100 stay below 3; lower qualities are much smaller
        Format::Jpeg { quality } => px * (quality as u64 * 3 / 100).max(1),
    }
}

/// Writes a picture of `w` x `h` pixels to `path`. `produce` gets the sink and must send all rows;
/// an error from `produce` is returned unchanged (the file is then incomplete – delete it).
pub fn write_file(path: &Path, w: usize, h: usize, o: &WriteOptions, produce: impl FnOnce(&mut dyn RowSink) -> Result<(), String>) -> Result<(), String> {
    if w == 0 || h == 0 {
        return Err("the picture has no pixels".into());
    }
    if w > u32::MAX as usize || h > u32::MAX as usize {
        return Err("the picture is too large for this file format".into());
    }
    if let Format::Jpeg { .. } = o.format
        && (w > JPEG_MAX_SIDE || h > JPEG_MAX_SIDE) {
            return Err(format!("JPEG files can be at most {JPEG_MAX_SIDE} pixels wide or high (this picture is {w} x {h}) – choose TIFF or a smaller size"));
        }
    let file = File::create(path).map_err(|e| format!("could not create {}: {e}", path.display()))?;
    let out = BufWriter::with_capacity(FILE_BUFFER, file);
    match o.format {
        Format::Png => write_png(out, w, h, produce),
        Format::Tiff { sixteen_bit } => {
            if raw_bytes(w, h, o.format) > o.bigtiff_above {
                let enc = tiff::encoder::TiffEncoder::new_big(out).map_err(|e| e.to_string())?;
                write_tiff(enc, w, h, sixteen_bit, o.rows_per_strip, produce)
            } else {
                let enc = tiff::encoder::TiffEncoder::new(out).map_err(|e| e.to_string())?;
                write_tiff(enc, w, h, sixteen_bit, o.rows_per_strip, produce)
            }
        }
        Format::Jpeg { quality } => write_jpeg(out, w, h, quality, o.jpeg_queue, produce),
    }
}

/// Checks that strips come in order and have the expected size; returns the number of rows.
fn check_strip(next_y: usize, y0: usize, w: usize, h: usize, col: &[[f32; 3]], cov: &[f32]) -> Result<usize, String> {
    if col.len() != cov.len() || w == 0 || !col.len().is_multiple_of(w) {
        return Err("a strip of the picture has the wrong size".into());
    }
    let rows = col.len() / w;
    if y0 != next_y {
        return Err(format!("strips must arrive in order: expected row {next_y}, got row {y0}"));
    }
    if y0 + rows > h {
        return Err("more rows than the picture has".into());
    }
    Ok(rows)
}

// ---------- PNG ----------

struct PngSink<W: Write + 'static> {
    out: png::StreamWriter<'static, W>,
    w: usize,
    h: usize,
    next_y: usize,
}

impl<W: Write + 'static> RowSink for PngSink<W> {
    fn push(&mut self, y0: usize, col: &[[f32; 3]], cov: &[f32]) -> Result<(), String> {
        let rows = check_strip(self.next_y, y0, self.w, self.h, col, cov)?;
        self.out.write_all(&to_rgba8(col, cov)).map_err(|e| e.to_string())?;
        self.next_y += rows;
        Ok(())
    }
}

fn write_png<W: Write + 'static>(out: W, w: usize, h: usize, produce: impl FnOnce(&mut dyn RowSink) -> Result<(), String>) -> Result<(), String> {
    let mut enc = png::Encoder::new(out, w as u32, h as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let writer = enc.write_header().map_err(|e| e.to_string())?;
    let mut sink = PngSink { out: writer.into_stream_writer_with_size(FILE_BUFFER).map_err(|e| e.to_string())?, w, h, next_y: 0 };
    produce(&mut sink)?;
    if sink.next_y != h {
        return Err(format!("the picture is incomplete: {} of {h} rows", sink.next_y));
    }
    sink.out.finish().map_err(|e| e.to_string())
}

// ---------- TIFF ----------

struct TiffSink<'a, W: Write + std::io::Seek, C: tiff::encoder::colortype::ColorType, K: tiff::encoder::TiffKind> {
    img: tiff::encoder::ImageEncoder<'a, W, C, K>,
    convert: fn(&[[f32; 3]], &[f32]) -> Vec<C::Inner>,
    w: usize,
    h: usize,
    rows_per_strip: usize,
    next_y: usize,
}

impl<W: Write + std::io::Seek, C: tiff::encoder::colortype::ColorType, K: tiff::encoder::TiffKind> RowSink for TiffSink<'_, W, C, K>
where
    [C::Inner]: tiff::encoder::TiffValue,
{
    fn push(&mut self, y0: usize, col: &[[f32; 3]], cov: &[f32]) -> Result<(), String> {
        let rows = check_strip(self.next_y, y0, self.w, self.h, col, cov)?;
        let last = y0 + rows == self.h;
        if rows != self.rows_per_strip && !last {
            return Err(format!("TIFF strips must have {} rows, got {rows}", self.rows_per_strip));
        }
        self.img.write_strip(&(self.convert)(col, cov)).map_err(|e| e.to_string())?;
        self.next_y += rows;
        Ok(())
    }
}

fn tiff_pass<W: Write + std::io::Seek, C: tiff::encoder::colortype::ColorType, K: tiff::encoder::TiffKind>(
    enc: &mut tiff::encoder::TiffEncoder<W, K>,
    convert: fn(&[[f32; 3]], &[f32]) -> Vec<C::Inner>,
    w: usize,
    h: usize,
    rows_per_strip: usize,
    produce: impl FnOnce(&mut dyn RowSink) -> Result<(), String>,
) -> Result<(), String>
where
    [C::Inner]: tiff::encoder::TiffValue,
{
    let mut img = enc.new_image::<C>(w as u32, h as u32).map_err(|e| e.to_string())?;
    img.rows_per_strip(rows_per_strip.min(h) as u32).map_err(|e| e.to_string())?;
    let mut sink = TiffSink { img, convert, w, h, rows_per_strip: rows_per_strip.min(h), next_y: 0 };
    produce(&mut sink)?;
    if sink.next_y != h {
        return Err(format!("the picture is incomplete: {} of {h} rows", sink.next_y));
    }
    sink.img.finish().map_err(|e| e.to_string())
}

fn write_tiff<W: Write + std::io::Seek, K: tiff::encoder::TiffKind>(
    mut enc: tiff::encoder::TiffEncoder<W, K>,
    w: usize,
    h: usize,
    sixteen_bit: bool,
    rows_per_strip: usize,
    produce: impl FnOnce(&mut dyn RowSink) -> Result<(), String>,
) -> Result<(), String> {
    use tiff::encoder::colortype::{RGBA8, RGBA16};
    if sixteen_bit {
        tiff_pass::<W, RGBA16, K>(&mut enc, to_rgba16, w, h, rows_per_strip, produce)
    } else {
        tiff_pass::<W, RGBA8, K>(&mut enc, to_rgba8, w, h, rows_per_strip, produce)
    }
}

// ---------- JPEG ----------

/// Gives the JPEG encoder its rows from the strips that arrive over a channel. The encoder asks
/// for rows from top to bottom (the last row again for padding).
struct PullImage {
    w: u16,
    h: u16,
    state: Mutex<PullState>,
}

struct PullState {
    rx: mpsc::Receiver<(usize, Vec<u8>)>,
    y0: usize,
    rows: usize,
    rgb: Vec<u8>,
}

/// Marker payload of the panic that stops the encoder when the producer gives up.
struct ProducerStopped;

impl jpeg_encoder::ImageBuffer for PullImage {
    fn get_jpeg_color_type(&self) -> jpeg_encoder::JpegColorType {
        jpeg_encoder::JpegColorType::Ycbcr
    }
    fn width(&self) -> u16 {
        self.w
    }
    fn height(&self) -> u16 {
        self.h
    }
    fn fill_buffers(&self, y: u16, buffers: &mut [Vec<u8>; 4]) {
        let mut s = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let y = y as usize;
        while y >= s.y0 + s.rows {
            match s.rx.recv() {
                Ok((y0, rgb)) => {
                    s.y0 = y0;
                    s.rows = rgb.len() / (3 * self.w as usize);
                    s.rgb = rgb;
                }
                // the producer stopped before sending all rows: end the encoder quickly
                Err(_) => std::panic::resume_unwind(Box::new(ProducerStopped)),
            }
        }
        let row = &s.rgb[(y - s.y0) * 3 * self.w as usize..][..3 * self.w as usize];
        for p in row.as_chunks::<3>().0 {
            let (yy, cb, cr) = jpeg_encoder::rgb_to_ycbcr(p[0], p[1], p[2]);
            buffers[0].push(yy);
            buffers[1].push(cb);
            buffers[2].push(cr);
        }
    }
}

struct JpegSink {
    tx: Option<mpsc::SyncSender<(usize, Vec<u8>)>>,
    w: usize,
    h: usize,
    next_y: usize,
}

impl RowSink for JpegSink {
    fn push(&mut self, y0: usize, col: &[[f32; 3]], cov: &[f32]) -> Result<(), String> {
        let rows = check_strip(self.next_y, y0, self.w, self.h, col, cov)?;
        let rgba = to_rgba8(col, cov);
        let rgb: Vec<u8> = rgba.as_chunks::<4>().0.iter().flat_map(|p| [p[0], p[1], p[2]]).collect();
        drop(rgba);
        self.tx.as_ref().ok_or("the JPEG writer has stopped")?.send((y0, rgb)).map_err(|_| "the JPEG encoder stopped unexpectedly".to_string())?;
        self.next_y += rows;
        Ok(())
    }
}

fn write_jpeg<W: Write + Send>(out: W, w: usize, h: usize, quality: u8, queue: usize, produce: impl FnOnce(&mut dyn RowSink) -> Result<(), String>) -> Result<(), String> {
    let (tx, rx) = mpsc::sync_channel(queue.max(1));
    let image = PullImage { w: w as u16, h: h as u16, state: Mutex::new(PullState { rx, y0: 0, rows: 0, rgb: Vec::new() }) };
    let mut enc = jpeg_encoder::Encoder::new(out, quality.clamp(1, 100));
    enc.set_sampling_factor(jpeg_encoder::SamplingFactor::F_1_1);
    // progressive scans and optimised Huffman tables are off by default (they need the whole picture)
    std::thread::scope(|scope| {
        let worker = scope.spawn(move || enc.encode_image(image));
        let mut sink = JpegSink { tx: Some(tx), w, h, next_y: 0 };
        let res = produce(&mut sink).and_then(|_| if sink.next_y == h { Ok(()) } else { Err(format!("the picture is incomplete: {} of {h} rows", sink.next_y)) });
        // closing the channel ends the encoder: normally after the last row, early on an error
        sink.tx = None;
        let encoded = worker.join();
        res?;
        match encoded {
            Ok(r) => r.map_err(|e| e.to_string()),
            Err(_) => Err("the JPEG encoder stopped unexpectedly".into()),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: usize = 37;
    const H: usize = 50;

    /// A picture with a gradient, a hole in the middle and coverage 0 in the top-left corner.
    fn picture() -> (Vec<[f32; 3]>, Vec<f32>) {
        let mut col = vec![[0.0f32; 3]; W * H];
        let mut cov = vec![0.0f32; W * H];
        for y in 0..H {
            for x in 0..W {
                let i = y * W + x;
                if x < 5 && y < 5 {
                    continue;
                }
                col[i] = [x as f32 / W as f32, y as f32 / H as f32, 0.25 + 0.5 * ((x + y) % 7) as f32 / 7.0];
                cov[i] = 1.0;
            }
        }
        (col, cov)
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join("panoweave_strip_write_tests");
        std::fs::create_dir_all(&d).unwrap();
        d.join(name)
    }

    fn send(rows: usize, sink: &mut dyn RowSink) -> Result<(), String> {
        let (col, cov) = picture();
        let mut y = 0;
        while y < H {
            let r = rows.min(H - y);
            sink.push(y, &col[y * W..(y + r) * W], &cov[y * W..(y + r) * W])?;
            y += r;
        }
        Ok(())
    }

    #[test]
    fn png_and_tiff8_are_identical_to_the_whole_picture() {
        let (col, cov) = picture();
        let expected = to_rgba8(&col, &cov);
        for (name, f, bigtiff) in [("a.png", Format::Png, false), ("b.tif", Format::Tiff { sixteen_bit: false }, false), ("c.tif", Format::Tiff { sixteen_bit: false }, true)] {
            let p = tmp(name);
            let mut o = WriteOptions::new(f, 8);
            if bigtiff {
                o.bigtiff_above = 0;
            }
            write_file(&p, W, H, &o, |s| send(8, s)).unwrap();
            let got = image::open(&p).unwrap().to_rgba8();
            assert_eq!((got.width() as usize, got.height() as usize), (W, H), "{name}");
            assert_eq!(got.as_raw(), &expected, "{name}");
            let _ = std::fs::remove_file(&p);
        }
    }

    #[test]
    fn bigtiff_has_the_bigtiff_header() {
        let p = tmp("big.tif");
        let mut o = WriteOptions::new(Format::Tiff { sixteen_bit: false }, 8);
        o.bigtiff_above = 0;
        write_file(&p, W, H, &o, |s| send(8, s)).unwrap();
        let head = std::fs::read(&p).unwrap();
        assert_eq!(&head[..4], b"II\x2b\x00", "BigTIFF magic number is 43");
        let p2 = tmp("small.tif");
        write_file(&p2, W, H, &WriteOptions::new(Format::Tiff { sixteen_bit: false }, 8), |s| send(8, s)).unwrap();
        assert_eq!(&std::fs::read(&p2).unwrap()[..4], b"II\x2a\x00");
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(&p2);
    }

    #[test]
    fn tiff16_matches_the_whole_picture_and_strip_height_does_not_matter() {
        let (col, cov) = picture();
        let expected = to_rgba16(&col, &cov);
        for rows in [1, 7, 16, 50] {
            let p = tmp("d.tif");
            write_file(&p, W, H, &WriteOptions::new(Format::Tiff { sixteen_bit: true }, rows), |s| send(rows, s)).unwrap();
            let got = image::open(&p).unwrap().to_rgba16();
            assert_eq!(got.as_raw(), &expected, "rows {rows}");
            let _ = std::fs::remove_file(&p);
        }
    }

    #[test]
    fn jpeg_is_close_to_the_picture_for_any_strip_height() {
        let (col, cov) = picture();
        let rgba = to_rgba8(&col, &cov);
        let mut sizes = Vec::new();
        for rows in [1, 8, 13, 50] {
            let p = tmp("e.jpg");
            write_file(&p, W, H, &WriteOptions::new(Format::Jpeg { quality: 95 }, rows), |s| send(rows, s)).unwrap();
            let got = image::open(&p).unwrap().to_rgb8();
            assert_eq!((got.width() as usize, got.height() as usize), (W, H));
            let err: f64 = got.as_raw().iter().enumerate().map(|(i, &v)| (v as f64 - rgba[i / 3 * 4 + i % 3] as f64).abs()).sum::<f64>() / got.as_raw().len() as f64;
            assert!(err < 6.0, "rows {rows}: mean error {err}");
            sizes.push(std::fs::read(&p).unwrap());
            let _ = std::fs::remove_file(&p);
        }
        // the strip height does not change the file at all
        assert!(sizes.windows(2).all(|w| w[0] == w[1]));
    }

    #[test]
    fn wrong_order_wrong_size_and_incomplete_pictures_are_errors() {
        for f in [Format::Png, Format::Tiff { sixteen_bit: false }, Format::Jpeg { quality: 90 }] {
            let p = tmp("f.out");
            let (col, cov) = picture();
            let o = WriteOptions::new(f, 8);
            let e = write_file(&p, W, H, &o, |s| s.push(8, &col[8 * W..16 * W], &cov[8 * W..16 * W])).unwrap_err();
            assert!(e.contains("in order"), "{f:?}: {e}");
            let e = write_file(&p, W, H, &o, |s| s.push(0, &col[..8 * W], &cov[..8 * W])).unwrap_err();
            assert!(e.contains("incomplete"), "{f:?}: {e}");
            let e = write_file(&p, W, H, &o, |s| s.push(0, &col[..W + 1], &cov[..W + 1])).unwrap_err();
            assert!(e.contains("wrong size"), "{f:?}: {e}");
            let _ = std::fs::remove_file(&p);
        }
        let e = write_file(&tmp("g.tif"), W, H, &WriteOptions::new(Format::Tiff { sixteen_bit: false }, 8), |s| send(5, s)).unwrap_err();
        assert!(e.contains("must have 8 rows"), "{e}");
    }

    #[test]
    fn a_stopping_producer_ends_the_jpeg_writer_with_its_own_error() {
        let p = tmp("h.jpg");
        let e = write_file(&p, W, H, &WriteOptions::new(Format::Jpeg { quality: 90 }, 8), |s| {
            let (col, cov) = picture();
            s.push(0, &col[..8 * W], &cov[..8 * W])?;
            Err(stitch_cancelled())
        })
        .unwrap_err();
        assert_eq!(e, "Cancelled");
        let _ = std::fs::remove_file(&p);
    }

    fn stitch_cancelled() -> String {
        crate::stitch::CANCELLED.to_string()
    }

    #[test]
    fn jpeg_over_65535_pixels_is_refused_with_a_hint() {
        let e = write_file(&tmp("i.jpg"), 70_000, 100, &WriteOptions::new(Format::Jpeg { quality: 90 }, 8), |_| Ok(())).unwrap_err();
        assert!(e.contains("65535") && e.contains("TIFF"), "{e}");
        assert!(!tmp("i.jpg").exists());
    }

    #[test]
    fn the_file_size_estimate_is_an_upper_bound_that_follows_the_format() {
        let t8 = estimated_file_bytes(100, 100, Format::Tiff { sixteen_bit: false });
        let t16 = estimated_file_bytes(100, 100, Format::Tiff { sixteen_bit: true });
        assert_eq!((t8, t16), (40_000, 80_000));
        assert!(estimated_file_bytes(100, 100, Format::Png) <= t8);
        assert!(estimated_file_bytes(100, 100, Format::Jpeg { quality: 50 }) < estimated_file_bytes(100, 100, Format::Jpeg { quality: 100 }));
        // the real files of the tests stay below the estimate
        let p = tmp("est.png");
        write_file(&p, W, H, &WriteOptions::new(Format::Png, 8), |s| send(8, s)).unwrap();
        assert!(std::fs::metadata(&p).unwrap().len() <= estimated_file_bytes(W, H, Format::Png));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn raw_size_follows_the_format() {
        assert_eq!(raw_bytes(10, 10, Format::Png), 400);
        assert_eq!(raw_bytes(10, 10, Format::Tiff { sixteen_bit: true }), 800);
        assert_eq!(raw_bytes(10, 10, Format::Jpeg { quality: 90 }), 300);
    }
}
