use std::{
    collections::HashMap,
    io::{self, Cursor, Seek, SeekFrom, Write},
    num::NonZeroUsize,
    ops::Range,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use log::{error, info, warn};
use lru::LruCache;
use tantivy::{
    directory::{
        error::{DeleteError, OpenReadError, OpenWriteError},
        WatchHandle, WritePtr,
    },
    Directory,
};
use tantivy_common::{file_slice::FileHandle, AntiCallToken, HasLen, OwnedBytes, TerminatingWrite};

thread_local! {
    static BLOCKING_HTTP_CLIENT: reqwest::blocking::Client = reqwest::blocking::Client::new();
    static HTTP_CLIENT: reqwest::Client = reqwest::Client::new();
}

const CHUNK_SIZE: usize = 1024 * 32;

static LRU_CACHE: OnceLock<Mutex<LruCache<CacheKey, Vec<u8>>>> = OnceLock::new();
static LENGTHS: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct CacheKey {
    base_url: String,
    path: String,
    chunk: usize,
}

#[derive(Debug, Clone)]
pub struct HttpFileHandle {
    url: String,
}

#[async_trait::async_trait]
impl FileHandle for HttpFileHandle {
    fn read_bytes(&self, range: Range<usize>) -> std::io::Result<OwnedBytes> {
        let chunk_start = range.start / CHUNK_SIZE;
        let chunk_end = range.end / CHUNK_SIZE;
        let cache = LRU_CACHE
            .get_or_init(|| Mutex::new(LruCache::new(NonZeroUsize::new(128 * 1024).unwrap())));

        let mut have_all_chunks = true;
        for chunk in chunk_start..=chunk_end {
            let key = CacheKey {
                base_url: self.url.clone(),
                path: self.url.clone(),
                chunk,
            };
            let cache = cache.lock().unwrap();
            if !cache.contains(&key) {
                have_all_chunks = false;
                break;
            }
        }
        let mut accumulated_chunks = Vec::new();
        if have_all_chunks {
            info!("Reading bytes from cache: {:?}", range);
            let mut cache = cache.lock().unwrap();
            for chunk in chunk_start..=chunk_end {
                let key = CacheKey {
                    base_url: self.url.clone(),
                    path: self.url.clone(),
                    chunk,
                };
                accumulated_chunks.extend(cache.get(&key).unwrap());
            }
            let chunk_start_offset = range.start % CHUNK_SIZE;
            let chunk_end_offset = (chunk_end - chunk_start) * CHUNK_SIZE + range.end % CHUNK_SIZE;
            return Ok(OwnedBytes::new(
                accumulated_chunks[chunk_start_offset..chunk_end_offset].to_vec(),
            ));
        }

        info!(
            "Reading bytes: {:?} in chunks from {} to {}",
            range, chunk_start, chunk_end
        );
        let start_time = std::time::Instant::now();
        let response = BLOCKING_HTTP_CLIENT.with(|client| {
            client
                .get(&self.url)
                .timeout(Duration::from_millis(
                    500 + (range.end - range.start) as u64 / 1024,
                ))
                .header(
                    "Range",
                    dbg!(format!(
                        "bytes={}-{}",
                        chunk_start * CHUNK_SIZE,
                        (chunk_end + 1) * CHUNK_SIZE
                    )),
                )
                .send()
        });
        let response = if let Err(e) = response {
            error!("Error: {:?}", e);
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Error fetching chunk",
            ));
        } else {
            response.unwrap()
        };
        if response.status() != 206 {
            error!("Response: {:?}", response);
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Error fetching chunk: non-200 status",
            ));
        } else {
            let data = response.bytes().unwrap();
            let data = data.to_vec();
            {
                let mut cache = cache.lock().unwrap();
                for chunk in 0..=(chunk_end - chunk_start) {
                    let key = CacheKey {
                        base_url: self.url.clone(),
                        path: self.url.clone(),
                        chunk: chunk_start + chunk,
                    };
                    let start = chunk * CHUNK_SIZE;
                    let end = (chunk + 1) * CHUNK_SIZE;
                    let data = data[start..end.min(data.len())].to_vec();
                    cache.put(key, data);
                }
            }
            accumulated_chunks.extend(data);
        }
        info!(
            "Fetched {} bytes in: {:?}",
            accumulated_chunks.len(),
            start_time.elapsed()
        );
        let chunk_start_offset = range.start % CHUNK_SIZE;
        let chunk_end_offset = (chunk_end - chunk_start) * CHUNK_SIZE + range.end % CHUNK_SIZE;
        Ok(OwnedBytes::new(
            accumulated_chunks[chunk_start_offset..chunk_end_offset.min(accumulated_chunks.len())]
                .to_vec(),
        ))
    }
}

impl HasLen for HttpFileHandle {
    fn len(&self) -> usize {
        let lengths = LENGTHS.get_or_init(|| Mutex::new(HashMap::new()));
        {
            let lengths = lengths.lock().unwrap();
            if let Some(length) = lengths.get(&PathBuf::from(&self.url)) {
                return *length;
            }
        }

        let url = format!("{}", self.url);
        info!("Fetching length from: {}", url);
        let response = BLOCKING_HTTP_CLIENT
            .with(|client| client.head(&url).timeout(Duration::from_millis(500)).send());
        if let Err(e) = response {
            error!("Error fetching length: {:?}", e);
            panic!();
        }
        let response = response.unwrap();
        if response.status() != 200 {
            error!("Response: {:?}", response);
            panic!();
        } else {
            let length = response
                .headers()
                .get("Content-Length")
                .unwrap()
                .to_str()
                .unwrap()
                .parse()
                .unwrap();
            info!("Length: {}", length);
            let mut lengths = lengths.lock().unwrap();
            lengths.insert(PathBuf::from(&self.url), length);
            length
        }
    }
}

// impl Deref for HttpFileHandle {
//     type Target = [u8];

//     fn deref(&self) -> &Self::Target {
//         warn!("Dereferencing an HttpFileHandle is not performant.");

//     }
// }

#[derive(Debug, Clone)]
pub struct HttpDirectory {
    base_url: String,
}

impl HttpDirectory {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.to_string(),
        }
    }

    pub fn format_url(&self, path: &Path) -> String {
        if self.base_url.ends_with('/') {
            format!("{}{}", self.base_url, path.display())
        } else {
            format!("{}/{}", self.base_url, path.display())
        }
    }
}

impl Directory for HttpDirectory {
    fn get_file_handle(&self, path: &Path) -> Result<Arc<dyn FileHandle>, OpenReadError> {
        Ok(Arc::new(HttpFileHandle {
            url: self.format_url(path),
        }))
    }

    fn delete(&self, path: &Path) -> Result<(), DeleteError> {
        if path == Path::new(".tantivy-meta.lock") {
            return Ok(());
        }

        Err(DeleteError::IoError {
            io_error: Arc::new(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Delete not supported",
            )),
            filepath: path.to_path_buf(),
        })
    }

    fn exists(&self, path: &Path) -> Result<bool, OpenReadError> {
        if path == Path::new(".tantivy-meta.lock") {
            return Ok(true);
        }
        let handle = HttpFileHandle {
            url: self.format_url(path),
        };
        Ok(handle.len() > 0)
    }

    fn open_write(&self, path: &Path) -> Result<WritePtr, OpenWriteError> {
        if path == Path::new(".tantivy-meta.lock") {
            return Ok(WritePtr::new(Box::new(VecWriter::new(path.to_path_buf()))));
        }
        dbg!(path);
        Err(OpenWriteError::IoError {
            io_error: Arc::new(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Write not supported",
            )),
            filepath: path.to_path_buf(),
        })
    }

    fn atomic_read(&self, path: &Path) -> Result<Vec<u8>, OpenReadError> {
        let handle = HttpFileHandle {
            url: self.format_url(path),
        };
        Ok(handle
            .read_bytes(0..handle.len())
            .map_err(|_| OpenReadError::IoError {
                io_error: Arc::new(std::io::Error::new(std::io::ErrorKind::Other, "Read error")),
                filepath: path.to_path_buf(),
            })?
            .to_vec())
    }

    fn atomic_write(&self, _path: &Path, _data: &[u8]) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "Write not supported",
        ))
    }

    fn sync_directory(&self) -> std::io::Result<()> {
        Ok(())
    }

    fn watch(
        &self,
        _watch_callback: tantivy::directory::WatchCallback,
    ) -> tantivy::Result<tantivy::directory::WatchHandle> {
        Ok(WatchHandle::empty())
    }
}

struct VecWriter {
    path: PathBuf,
    data: Cursor<Vec<u8>>,
    is_flushed: bool,
}

impl VecWriter {
    fn new(path_buf: PathBuf) -> VecWriter {
        VecWriter {
            path: path_buf,
            data: Cursor::new(Vec::new()),
            is_flushed: true,
        }
    }
}

impl Drop for VecWriter {
    fn drop(&mut self) {
        if !self.is_flushed {
            warn!(
                "You forgot to flush {:?} before its writer got Drop. Do not rely on drop. This \
                 also occurs when the indexer crashed, so you may want to check the logs for the \
                 root cause.",
                self.path
            )
        }
    }
}

impl Seek for VecWriter {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.data.seek(pos)
    }
}

impl Write for VecWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.is_flushed = false;
        self.data.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.is_flushed = true;
        Ok(())
    }
}

impl TerminatingWrite for VecWriter {
    fn terminate_ref(&mut self, _: AntiCallToken) -> io::Result<()> {
        self.flush()
    }
}
