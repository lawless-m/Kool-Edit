//! `.kepz` portable project archive.
//!
//! Doc 01 §"Project file format" (final paragraph): a `.kepz` is a zip
//! containing `project.json` plus a `sources/` directory with every base
//! file the project references. The point is sharing — open a `.kepz` on a
//! different machine and you get the project plus all its audio.
//!
//! Layout in the zip:
//!
//! ```text
//! project.json
//! sources/<src_id>/base.f32
//! ...
//! ```
//!
//! Sample bytes are raw little-endian f32, matching the on-disk format used
//! by [`crate::storage::NativeStorage`].

use std::io::{Cursor, Read, Write};

use zip::{
    CompressionMethod, ZipArchive, ZipWriter,
    write::SimpleFileOptions,
};

use crate::project::Project;

const PROJECT_JSON: &str = "project.json";

#[derive(Debug)]
pub enum KepzError {
    Zip(zip::result::ZipError),
    Io(std::io::Error),
    MissingProjectJson,
    Project(crate::project::ProjectLoadError),
    BadSampleByteCount {
        path: String,
        bytes: usize,
    },
}

impl std::fmt::Display for KepzError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Zip(e) => write!(f, "zip error: {e}"),
            Self::Io(e) => write!(f, "io error: {e}"),
            Self::MissingProjectJson => write!(f, "kepz archive missing project.json"),
            Self::Project(e) => write!(f, "project parse error: {e}"),
            Self::BadSampleByteCount { path, bytes } => write!(
                f,
                "{path}: sample blob has {bytes} bytes (must be a multiple of 4)"
            ),
        }
    }
}

impl std::error::Error for KepzError {}

impl From<zip::result::ZipError> for KepzError {
    fn from(e: zip::result::ZipError) -> Self {
        Self::Zip(e)
    }
}

impl From<std::io::Error> for KepzError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<crate::project::ProjectLoadError> for KepzError {
    fn from(e: crate::project::ProjectLoadError) -> Self {
        Self::Project(e)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct KepzSource {
    pub path: String,
    pub samples: Vec<f32>,
}

#[derive(Debug, Clone)]
pub struct KepzArchive {
    pub project: Project,
    pub sources: Vec<KepzSource>,
}

/// Build a `.kepz` zip from a project plus the sample blobs every source
/// references. The caller is responsible for reading the samples out of
/// whatever storage backend they live in (e.g. MemoryStorage).
///
/// Samples are streamed into the zip's deflate writer in 16 KB chunks
/// instead of materialised as one big `Vec<u8>` per source — multi-source
/// projects on the 4 GB-capped WASM heap were OOM'ing inside the zip
/// writer because each source was effectively held twice (once as f32s,
/// once as the f32→le_bytes copy).
pub fn write_archive<I>(project: &Project, sources: I) -> Result<Vec<u8>, KepzError>
where
    I: IntoIterator<Item = Result<KepzSource, KepzError>>,
{
    let mut buf = Cursor::new(Vec::<u8>::new());
    {
        let mut zip = ZipWriter::new(&mut buf);
        let opts = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .unix_permissions(0o644);

        zip.start_file(PROJECT_JSON, opts)?;
        let json = project
            .to_json()
            .map_err(|e| KepzError::Io(std::io::Error::other(e)))?;
        zip.write_all(json.as_bytes())?;

        // Reused across sources so we don't pay an allocation per file.
        // 4096 f32 = 16 KB on the stack pays for a deflate input batch
        // without ballooning the WASM heap.
        const CHUNK_FRAMES: usize = 4096;
        let mut chunk_bytes = [0u8; CHUNK_FRAMES * 4];
        for source in sources {
            let source = source?;
            zip.start_file(&source.path, opts)?;
            for chunk in source.samples.chunks(CHUNK_FRAMES) {
                for (i, s) in chunk.iter().enumerate() {
                    chunk_bytes[i * 4..i * 4 + 4].copy_from_slice(&s.to_le_bytes());
                }
                zip.write_all(&chunk_bytes[..chunk.len() * 4])?;
            }
            // `source` (and its samples Vec) drops here — the next
            // iteration reads the next source fresh from storage, so
            // peak WASM heap stays at ~one source rather than scaling
            // with the project's total sample count.
        }

        zip.finish()?;
    }
    Ok(buf.into_inner())
}

/// Read a `.kepz` zip into the project + its sources. The caller is
/// responsible for placing the source data into whatever storage backend
/// the engine is using.
pub fn read_archive(bytes: &[u8]) -> Result<KepzArchive, KepzError> {
    let mut zip = ZipArchive::new(Cursor::new(bytes))?;

    let project_json = {
        let mut entry = zip
            .by_name(PROJECT_JSON)
            .map_err(|_| KepzError::MissingProjectJson)?;
        let mut s = String::new();
        entry.read_to_string(&mut s)?;
        s
    };
    let project = Project::from_json(&project_json)?;

    let mut sources = Vec::new();
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let path = entry.name().to_string();
        if path == PROJECT_JSON {
            continue;
        }
        if entry.is_dir() {
            continue;
        }
        let mut bytes = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut bytes)?;
        if !bytes.len().is_multiple_of(4) {
            return Err(KepzError::BadSampleByteCount {
                path: path.clone(),
                bytes: bytes.len(),
            });
        }
        let samples: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        sources.push(KepzSource { path, samples });
    }

    Ok(KepzArchive { project, sources })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::Project;

    #[test]
    fn empty_project_round_trips() {
        let p = Project::new(96_000);
        let bytes = write_archive(&p, std::iter::empty()).unwrap();
        let arc = read_archive(&bytes).unwrap();
        assert_eq!(arc.project, p);
        assert!(arc.sources.is_empty());
    }

    #[test]
    fn source_blob_round_trips_through_archive() {
        let p = Project::new(96_000);
        let samples = vec![0.0_f32, 0.5, -0.5, 1.0, -1.0];
        let sources = vec![KepzSource {
            path: "sources/src_a/base.f32".into(),
            samples: samples.clone(),
        }];
        let bytes = write_archive(&p, sources.into_iter().map(Ok)).unwrap();
        let arc = read_archive(&bytes).unwrap();
        assert_eq!(arc.sources.len(), 1);
        assert_eq!(arc.sources[0].path, "sources/src_a/base.f32");
        assert_eq!(arc.sources[0].samples, samples);
    }

    #[test]
    fn read_rejects_missing_project_json() {
        // A zip with a single non-project entry.
        let mut buf = Cursor::new(Vec::<u8>::new());
        {
            let mut zip = ZipWriter::new(&mut buf);
            zip.start_file("other.txt", SimpleFileOptions::default()).unwrap();
            zip.write_all(b"hello").unwrap();
            zip.finish().unwrap();
        }
        let err = read_archive(&buf.into_inner()).unwrap_err();
        assert!(matches!(err, KepzError::MissingProjectJson));
    }

    #[test]
    fn read_rejects_bad_sample_byte_count() {
        let p = Project::new(96_000);
        let mut buf = Cursor::new(Vec::<u8>::new());
        {
            let mut zip = ZipWriter::new(&mut buf);
            let opts = SimpleFileOptions::default();
            zip.start_file(PROJECT_JSON, opts).unwrap();
            zip.write_all(p.to_json().unwrap().as_bytes()).unwrap();
            zip.start_file("sources/src_a/base.f32", opts).unwrap();
            zip.write_all(&[1, 2, 3]).unwrap(); // not a multiple of 4
            zip.finish().unwrap();
        }
        let err = read_archive(&buf.into_inner()).unwrap_err();
        assert!(matches!(err, KepzError::BadSampleByteCount { .. }));
    }
}
