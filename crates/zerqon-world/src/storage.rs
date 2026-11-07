use crate::{
    Chunk, GenerationPolicy, SECTION_COUNT, SECTION_VOLUME, Section, WorldError, WorldMetadata,
};
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

const MAGIC: &[u8; 8] = b"ZERQON\0\x01";
const FORMAT_VERSION: u16 = 1;
const HEADER_LENGTH: usize = 26;
const MAX_FILE_BYTES: usize = 1024 * 1024;

pub struct WorldStore {
    root: PathBuf,
    _lock: File,
    stripes: [RwLock<()>; 64],
    metadata: Option<WorldMetadata>,
}

impl WorldStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, WorldError> {
        Self::open_inner(root.as_ref(), false)
    }
    pub(crate) fn open_import(root: &Path) -> Result<Self, WorldError> {
        Self::open_inner(root, true)
    }
    fn open_inner(root: &Path, importing: bool) -> Result<Self, WorldError> {
        crate::metadata::reject_links(root)?;
        for marker in [
            "level.dat",
            "level.dat_old",
            "region",
            "dimensions/minecraft/overworld/region",
        ] {
            match std::fs::symlink_metadata(root.join(marker)) {
                Ok(_) => return Err(WorldError::ForeignWorld),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        let incomplete = root.join("IMPORT_INCOMPLETE").try_exists()?;
        if incomplete && !importing {
            return Err(WorldError::Corrupt(
                "world import is incomplete; import again into a new destination",
            ));
        }
        if importing && !incomplete {
            return Err(WorldError::Corrupt(
                "import store requires an incomplete marker",
            ));
        }
        std::fs::create_dir_all(root)?;
        crate::metadata::reject_links(&root.join("world.lock"))?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("world.lock"))?;
        lock.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => WorldError::Locked,
            TryLockError::Error(error) => WorldError::Io(error),
        })?;
        let metadata = if importing {
            None
        } else {
            WorldMetadata::load(root)?
        };
        if !importing
            && metadata.is_none()
            && (root.join("import-report.json").try_exists()?
                || root.join("source-data").try_exists()?)
        {
            return Err(WorldError::Metadata(
                "imported world metadata is missing; reimport the source into a new directory"
                    .into(),
            ));
        }
        if metadata
            .as_ref()
            .is_some_and(|metadata| metadata.retained_source.is_none())
        {
            return Err(WorldError::Metadata(
                "imported world has no retained source snapshot".into(),
            ));
        }
        Ok(Self {
            root: root.to_owned(),
            _lock: lock,
            stripes: std::array::from_fn(|_| RwLock::new(())),
            metadata,
        })
    }

    pub fn load(&self, x: i32, z: i32) -> Result<Option<Chunk>, WorldError> {
        let _guard = self.stripes[stripe(x, z)]
            .read()
            .map_err(|_| std::io::Error::other("world storage lock poisoned"))?;
        self.load_unlocked(x, z)
    }

    fn load_unlocked(&self, x: i32, z: i32) -> Result<Option<Chunk>, WorldError> {
        let file = match File::open(self.chunk_path(x, z)) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return if self.generation_policy() == GenerationPolicy::ExistingOnly {
                    Err(WorldError::MissingImportedChunk { x, z })
                } else {
                    Ok(None)
                };
            }
            Err(error) => return Err(error.into()),
        };
        if file.metadata()?.len() > MAX_FILE_BYTES as u64 {
            return Err(WorldError::Corrupt("chunk exceeds the size limit"));
        }
        let mut data = Vec::new();
        file.take(MAX_FILE_BYTES as u64 + 1)
            .read_to_end(&mut data)?;
        let chunk = Chunk::from_bytes(&data)?;
        if chunk.x != x || chunk.z != z {
            return Err(WorldError::Corrupt(
                "chunk coordinates differ from filename",
            ));
        }
        Ok(Some(chunk))
    }

    pub fn save(&self, chunk: &Chunk) -> Result<(), WorldError> {
        let _guard = self.stripes[stripe(chunk.x, chunk.z)]
            .write()
            .map_err(|_| std::io::Error::other("world storage lock poisoned"))?;
        self.load_unlocked(chunk.x, chunk.z)?;
        let mut temporary = tempfile::NamedTempFile::new_in(&self.root)?;
        temporary.write_all(&chunk.to_bytes())?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(self.chunk_path(chunk.x, chunk.z))
            .map_err(|error| WorldError::Io(error.error))?;
        #[cfg(unix)]
        File::open(&self.root)?.sync_all()?;
        Ok(())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn metadata(&self) -> Option<&WorldMetadata> {
        self.metadata.as_ref()
    }
    pub fn generation_policy(&self) -> GenerationPolicy {
        self.metadata
            .as_ref()
            .map_or(GenerationPolicy::NativeFlat, |metadata| {
                metadata.generation_policy
            })
    }
    fn chunk_path(&self, x: i32, z: i32) -> PathBuf {
        self.root.join(format!("c.{x}.{z}.zqc"))
    }
}

fn stripe(x: i32, z: i32) -> usize {
    ((x as u32).wrapping_mul(31) ^ z as u32) as usize % 64
}

impl Chunk {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut payload = Vec::new();
        for section in &self.sections {
            let (palette, bits, words) = section.palette_data();
            payload.extend_from_slice(&(palette.len() as u16).to_le_bytes());
            for state in palette {
                payload.extend_from_slice(&state.to_le_bytes());
            }
            payload.push(bits);
            payload.extend_from_slice(&(words.len() as u16).to_le_bytes());
            for word in words {
                payload.extend_from_slice(&word.to_le_bytes());
            }
        }
        let mut encoded = Vec::with_capacity(HEADER_LENGTH + payload.len());
        encoded.extend_from_slice(MAGIC);
        encoded.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        encoded.extend_from_slice(&self.x.to_le_bytes());
        encoded.extend_from_slice(&self.z.to_le_bytes());
        encoded.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        let mut checksum = crc32fast::Hasher::new();
        checksum.update(&encoded);
        checksum.update(&payload);
        encoded.extend_from_slice(&checksum.finalize().to_le_bytes());
        encoded.extend_from_slice(&payload);
        encoded
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self, WorldError> {
        if !(HEADER_LENGTH..=MAX_FILE_BYTES).contains(&data.len()) {
            return Err(WorldError::Corrupt("invalid chunk length"));
        }
        let mut header = Reader { data, position: 0 };
        if &header.array::<8>()? != MAGIC {
            return Err(WorldError::Corrupt("invalid chunk signature"));
        }
        if u16::from_le_bytes(header.array()?) != FORMAT_VERSION {
            return Err(WorldError::Corrupt("unsupported chunk version"));
        }
        let x = i32::from_le_bytes(header.array()?);
        let z = i32::from_le_bytes(header.array()?);
        let payload_len = u32::from_le_bytes(header.array()?) as usize;
        let stored_crc = u32::from_le_bytes(header.array()?);
        if payload_len != data.len() - HEADER_LENGTH {
            return Err(WorldError::Corrupt("payload length mismatch"));
        }
        let mut checksum = crc32fast::Hasher::new();
        checksum.update(&data[..HEADER_LENGTH - 4]);
        checksum.update(&data[HEADER_LENGTH..]);
        if stored_crc != checksum.finalize() {
            return Err(WorldError::Corrupt("checksum mismatch"));
        }
        let mut sections = std::array::from_fn(|_| Section::uniform(0));
        for section in &mut sections {
            let count = u16::from_le_bytes(header.array()?) as usize;
            if !(1..=SECTION_VOLUME).contains(&count) {
                return Err(WorldError::Corrupt("invalid palette length"));
            }
            let mut palette = Vec::with_capacity(count);
            for _ in 0..count {
                palette.push(u32::from_le_bytes(header.array()?));
            }
            let bits = header.array::<1>()?[0];
            let count = u16::from_le_bytes(header.array()?) as usize;
            if count > 820 {
                return Err(WorldError::Corrupt("too many palette words"));
            }
            let mut words = Vec::with_capacity(count);
            for _ in 0..count {
                words.push(u64::from_le_bytes(header.array()?));
            }
            *section = Section::from_palette(palette, bits, words)?;
        }
        if header.position != data.len() {
            return Err(WorldError::Corrupt("trailing chunk data"));
        }
        let sections: [Section; SECTION_COUNT] = sections;
        Ok(Self { x, z, sections })
    }
}

struct Reader<'a> {
    data: &'a [u8],
    position: usize,
}

impl Reader<'_> {
    fn array<const N: usize>(&mut self) -> Result<[u8; N], WorldError> {
        let end = self
            .position
            .checked_add(N)
            .ok_or(WorldError::Corrupt("length overflow"))?;
        let source = self
            .data
            .get(self.position..end)
            .ok_or(WorldError::Corrupt("truncated chunk"))?;
        let mut result = [0; N];
        result.copy_from_slice(source);
        self.position = end;
        Ok(result)
    }
}
