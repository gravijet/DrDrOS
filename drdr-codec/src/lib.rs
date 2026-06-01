//! DrDrCodec — from-scratch decoders for the binary file formats a
//! desktop must open.
//!
//! One algorithm — DEFLATE ([`inflate`]) — is the foundation: PNG image
//! data, every ZIP/DOCX entry, and most PDF content streams are DEFLATE.
//! On top of it sit a PNG decoder, a ZIP reader (and the DOCX text
//! extractor that rides on it), and a PDF text extractor. No external
//! crates: same rule as the rest of DrDrOS.

pub mod inflate;
pub mod pdf;
pub mod png;
pub mod zip;

pub use inflate::{inflate as deflate_inflate, zlib_decompress};
pub use png::{decode_png, Image};
pub use zip::{list_zip, read_zip_entry, ZipEntry};
