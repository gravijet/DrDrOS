//! DrDrCodec — from-scratch decoders for the binary file formats a
//! desktop must open.
//!
//! One algorithm — DEFLATE ([`inflate`]) — is the foundation: PNG image
//! data, every ZIP/DOCX entry, and most PDF content streams are DEFLATE.
//! On top of it sit a PNG decoder, a ZIP reader (and the DOCX text
//! extractor that rides on it), and a PDF text extractor. GIF (LZW) and
//! baseline JPEG (Huffman + IDCT) bring their own algorithms, and a media
//! probe reads MP4 / Matroska container metadata without decoding frames.
//! No external crates: same rule as the rest of DrDrOS.

pub mod gif;
pub mod inflate;
pub mod jpeg;
pub mod media;
pub mod pdf;
pub mod png;
pub mod zip;

pub use gif::decode_gif;
pub use inflate::{inflate as deflate_inflate, zlib_decompress};
pub use jpeg::decode_jpeg;
pub use media::{probe_media, MediaInfo};
pub use png::{decode_png, Image};
pub use zip::{list_zip, read_zip_entry, ZipEntry};
