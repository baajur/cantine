use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{self, BufRead, BufReader, Cursor, Result, Write},
    marker::PhantomData,
    mem::size_of,
    path::Path,
};

use bincode::{deserialize, serialize};
use byteorder::{NativeEndian, ReadBytesExt, WriteBytesExt};
use serde::{de::DeserializeOwned, Serialize};
use uuid::Uuid;
use zerocopy::{AsBytes, FromBytes, LayoutVerified, U64};

use super::mapped_file::MappedFile;

pub struct BincodeDatabase<T> {
    offsets: StructuredLog<LogEntry>,
    data: MappedFile,

    uuid_index: HashMap<Uuid, usize>,
    id_index: HashMap<u64, usize>,

    _marker: PhantomData<T>,
}

const OFFSETS_FILE: &str = "offsets.bin";
const DATA_FILE: &str = "data.bin";
const DATA_HEADER_SIZE: usize = size_of::<u64>();

pub trait DatabaseRecord {
    fn get_id(&self) -> u64;
    fn get_uuid(&self) -> &Uuid;
}

impl<T> BincodeDatabase<T>
where
    T: Serialize + DeserializeOwned + DatabaseRecord,
{
    pub fn create<P: AsRef<Path>>(base_dir: P, initial_size: u64) -> Result<Self> {
        let offsets_path = base_dir.as_ref().join(OFFSETS_FILE);
        let data_path = base_dir.as_ref().join(DATA_FILE);

        if offsets_path.exists() || data_path.exists() {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "database files already exist",
            ))
        } else {
            File::create(offsets_path)?;

            let mut data = File::create(data_path)?;
            data.set_len(initial_size)?;

            // First u64 is the append offset, in this case
            // we append'll right after the header
            data.write_u64::<NativeEndian>(DATA_HEADER_SIZE as u64)?;

            BincodeDatabase::open(base_dir)
        }
    }

    pub fn open<P: AsRef<Path>>(base_dir: P) -> Result<Self> {
        let offsets = StructuredLog::new(base_dir.as_ref().join(OFFSETS_FILE))?;

        let num_entries = offsets.len()?;
        let mut id_index = HashMap::with_capacity(num_entries);
        let mut uuid_index = HashMap::with_capacity(num_entries);

        let mut max_offset = DATA_HEADER_SIZE;
        offsets.for_each_entry(|entry: &LogEntry| {
            max_offset = entry.offset.get() as usize;
            uuid_index.insert(Uuid::from_bytes(entry.uuid), max_offset);
            id_index.insert(entry.id.get(), max_offset);
        })?;

        let datafile = OpenOptions::new()
            .read(true)
            .append(true)
            .open(base_dir.as_ref().join(DATA_FILE))?;
        let mut data = MappedFile::open(datafile)?;

        if max_offset > data.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("index points at unreachable offset: {}", max_offset),
            ));
        }

        let append_offset = {
            let mut cursor = Cursor::new(&data as &[u8]);
            cursor.read_u64::<NativeEndian>()? as usize
        };

        if append_offset < DATA_HEADER_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Got weird append offset from data file: {}", append_offset),
            ));
        }

        data.set_append_offset(append_offset)?;

        Ok(BincodeDatabase {
            offsets,
            data,
            uuid_index,
            id_index,
            _marker: PhantomData,
        })
    }

    pub fn add(&mut self, obj: &T) -> Result<()> {
        let data = serialize(obj).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Failed to serialize data being added",
            )
        })?;

        let read_offset = self.data.append(data.as_slice())?;

        let uuid = obj.get_uuid();
        let id = obj.get_id();

        let entry = LogEntry::new(uuid, id, read_offset);
        self.offsets.append(&entry)?;

        self.uuid_index.insert(*uuid, read_offset);
        self.id_index.insert(id, read_offset);

        let new_append_offset = U64::<NativeEndian>::new(self.data.offset() as u64);
        self.data[0..DATA_HEADER_SIZE].copy_from_slice(new_append_offset.as_bytes());

        Ok(())
    }

    fn deserialize_at(&self, offset: usize) -> Result<Option<T>> {
        Ok(Some(deserialize(&self.data[offset..]).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Failed to deserialize stored data",
            )
        })?))
    }

    pub fn get_by_id(&self, id: u64) -> Result<Option<T>> {
        match self.id_index.get(&id) {
            Some(&offset) => self.deserialize_at(offset),
            None => Ok(None),
        }
    }

    pub fn get_by_uuid(&self, uuid: &Uuid) -> Result<Option<T>> {
        match self.uuid_index.get(uuid) {
            Some(&offset) => self.deserialize_at(offset),
            None => Ok(None),
        }
    }
}

#[derive(FromBytes, AsBytes)]
#[repr(C)]
struct LogEntry {
    uuid: uuid::Bytes,
    id: U64<NativeEndian>,
    offset: U64<NativeEndian>,
}

impl LogEntry {
    fn new(uuid: &Uuid, id: u64, offset: usize) -> Self {
        Self {
            id: U64::new(id),
            uuid: *uuid.as_bytes(),
            offset: U64::new(offset as u64),
        }
    }
}

struct StructuredLog<T> {
    file: File,
    _header: PhantomData<T>,
}

impl<T> StructuredLog<T>
where
    T: FromBytes + AsBytes,
{
    fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        if !path.as_ref().exists() {
            File::create(&path)?;
        }

        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&path.as_ref())?;

        let entry_len = size_of::<T>();

        let file_size = file.metadata()?.len() as usize;
        if file_size % entry_len != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Expected file to size to be a multiple of {}. Got {}",
                    entry_len, file_size
                ),
            ));
        }

        Ok(Self {
            file,
            _header: PhantomData,
        })
    }

    fn len(&self) -> Result<usize> {
        Ok(self.file.metadata()?.len() as usize)
    }

    fn for_each_entry<F>(&self, mut each_entry: F) -> std::io::Result<()>
    where
        F: FnMut(&T),
    {
        let entry_len = size_of::<T>();
        let mut log_reader = BufReader::with_capacity((8192 / entry_len) * entry_len, &self.file);

        loop {
            let buf = log_reader.fill_buf()?;

            if buf.is_empty() {
                break;
            }

            let mut bytes_consumed = 0;
            if let Some(slice) = LayoutVerified::new_slice(buf) {
                let entries: &[T] = slice.into_slice();
                for entry in entries {
                    (each_entry)(entry);
                    bytes_consumed += entry_len;
                }
            } else {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "Log corrupted!"));
            }

            log_reader.consume(bytes_consumed);
        }

        Ok(())
    }

    fn append(&mut self, item: &T) -> Result<()> {
        self.file.write_all(item.as_bytes())
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use tempfile;

    use serde::Deserialize;

    #[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy)]
    struct Item(u64, Uuid);

    impl Item {
        fn new(id: u64) -> Self {
            Self(id, Uuid::new_v4())
        }
    }

    impl DatabaseRecord for Item {
        fn get_id(&self) -> u64 {
            self.0
        }
        fn get_uuid(&self) -> &Uuid {
            &self.1
        }
    }

    fn open_empty() -> Result<BincodeDatabase<Item>> {
        let tmpdir = tempfile::TempDir::new().unwrap();
        BincodeDatabase::create(tmpdir, 10)
    }

    #[test]
    fn can_open_empty_db() {
        open_empty().unwrap();
    }

    #[test]
    fn get_on_empty_works() -> Result<()> {
        assert_eq!(None, open_empty()?.get_by_uuid(&Uuid::new_v4())?);
        assert_eq!(None, open_empty()?.get_by_id(42)?);
        Ok(())
    }

    #[test]
    fn can_add_and_get() -> Result<()> {
        let mut db = open_empty()?;

        let one = Item::new(1);
        let two = Item::new(2);
        let three = Item::new(3);

        db.add(&one)?;
        db.add(&two)?;
        db.add(&three)?;

        assert_eq!(Some(one), db.get_by_id(1)?);
        assert_eq!(Some(three), db.get_by_id(3)?);
        assert_eq!(Some(two), db.get_by_id(2)?);

        Ok(())
    }

    #[test]
    fn add_updates_both_indices_correctly() -> Result<()> {
        let mut db = open_empty()?;

        let item = Item::new(42);
        db.add(&item)?;

        assert_eq!(
            db.get_by_id(item.get_id())?,
            db.get_by_uuid(item.get_uuid())?
        );
        Ok(())
    }

    #[test]
    fn cannot_overwrite_database() -> Result<()> {
        let tmpdir = tempfile::TempDir::new()?;

        BincodeDatabase::<Item>::create(&tmpdir, 1)?;
        let overwrite_result = BincodeDatabase::<Item>::create(tmpdir, 1);
        assert!(overwrite_result.is_err());

        Ok(())
    }

    #[test]
    fn can_load_existing_database() -> Result<()> {
        let tmpdir = tempfile::TempDir::new()?;

        const DB_SIZE: u64 = 1_000;

        let one = Item::new(1);
        let two = Item::new(2);
        let three = Item::new(3);

        {
            let mut db = BincodeDatabase::create(&tmpdir, DB_SIZE)?;

            db.add(&one)?;
            db.add(&two)?;
        }

        {
            let mut db = BincodeDatabase::open(&tmpdir)?;
            db.add(&three)?;
        }

        let existing_db = BincodeDatabase::open(&tmpdir)?;
        assert_eq!(Some(one), existing_db.get_by_uuid(one.get_uuid())?);
        assert_eq!(Some(two), existing_db.get_by_uuid(two.get_uuid())?);
        assert_eq!(Some(three), existing_db.get_by_uuid(three.get_uuid())?);

        // Shouldn't have grown from DB_SIZE
        let data_file = OpenOptions::new()
            .read(true)
            .open(tmpdir.path().join(DATA_FILE))?;
        assert_eq!(DB_SIZE, data_file.metadata()?.len());

        Ok(())
    }

    struct Db<T> {
        data: Vec<u8>,
        index: HashMap<u64, usize>,
        _marker: PhantomData<T>,
    }

    use std::borrow::Cow;

    trait Config<'a> {
        type Item: 'a;
        fn to_bytes(item: &'a Self::Item) -> Option<Cow<'a, [u8]>>;
        fn from_bytes(src: &'a [u8]) -> Option<Self::Item>;
    }

    struct BincodeConfig<T>(PhantomData<T>);

    impl<T> BincodeConfig<T> {
        fn new() -> Self {
            Self(PhantomData)
        }
    }

    impl<'a, T: 'a> Config<'a> for BincodeConfig<T>
    where
        T: Deserialize<'a> + Serialize + Clone,
    {
        type Item = T;

        fn from_bytes(src: &'a [u8]) -> Option<T> {
            deserialize(src).ok()
        }

        fn to_bytes(item: &'a T) -> Option<Cow<[u8]>> {
            serialize(item).map(Cow::Owned).ok()
        }
    }

    struct ConfigDb<'a, T: 'a, TConfig>
    where
        TConfig: Config<'a, Item = T>,
    {
        data: Vec<u8>,
        index: HashMap<u64, usize>,
        _config: TConfig,
        _marker: PhantomData<&'a T>,
    }

    impl<'a, T: 'a, TConfig> ConfigDb<'a, T, TConfig>
    where
        TConfig: Config<'a, Item = T>,
    {
        fn new(_config: TConfig) -> Self {
            Self {
                data: Vec::new(),
                index: HashMap::new(),
                _marker: PhantomData,
                _config,
            }
        }

        fn add(&mut self, id: u64, item: &'a TConfig::Item) -> Result<()> {
            if let Some(encoded) = TConfig::to_bytes(item) {
                let start_offset = self.data.len();
                self.data.extend(encoded.iter());
                self.index.insert(id, start_offset);
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "to_bytes() fail",
                ))
            }
        }

        fn get(&self, id: u64) -> Result<Option<TConfig::Item>> {
            if let Some(&offset) = self.index.get(&id) {
                let data = self.data[offset..].as_ptr();
                let len = self.data.len() - offset;
                if let Some(decoded) =
                    TConfig::from_bytes(unsafe { std::slice::from_raw_parts(data, len) })
                {
                    Ok(Some(decoded))
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "to_bytes() fail",
                    ))
                }
            } else {
                Ok(None)
            }
        }
    }

    impl<T> Db<T> {
        fn new() -> Self {
            Self {
                data: Vec::new(),
                index: HashMap::new(),
                _marker: PhantomData,
            }
        }

        fn add<'a, TEncoder>(&mut self, id: u64, item: &'a TEncoder::Item) -> Result<()>
        where
            TEncoder: Config<'a>,
        {
            if let Some(encoded) = TEncoder::to_bytes(item) {
                let start_offset = self.data.len();
                self.data.extend(encoded.iter());
                self.index.insert(id, start_offset);
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "to_bytes() fail",
                ))
            }
        }

        fn get<'a, TDecoder>(&self, id: u64) -> Result<Option<TDecoder::Item>>
        where
            TDecoder: Config<'a>,
        {
            if let Some(&offset) = self.index.get(&id) {
                let data = self.data[offset..].as_ptr();
                let len = self.data.len() - offset;
                if let Some(decoded) =
                    TDecoder::from_bytes(unsafe { std::slice::from_raw_parts(data, len) })
                {
                    Ok(Some(decoded))
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "to_bytes() fail",
                    ))
                }
            } else {
                Ok(None)
            }
        }
    }

    #[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
    struct Named<'a>(&'a str, &'a str);

    // TODO Cannot remap now that I want to return references
    //      So create a DatabaseWriter that writes straight to the file
    //      And optionally truncates from a max_size? (maybe store
    //      length in the LogEntry)
    //      And the database only holds the memory map
    #[test]
    fn item_db_functional() -> Result<()> {
        let mut db: Db<BincodeConfig<Named>> = Db::new();

        let first = Named("caio", "romao");
        let second = Named("costa", "nasciment");

        db.add::<BincodeConfig<Named>>(0, &first)?;
        db.add::<BincodeConfig<Named>>(1, &second)?;

        assert_eq!(Some(first), db.get::<BincodeConfig<Named>>(0)?);
        assert_eq!(Some(second), db.get::<BincodeConfig<Named>>(1)?);

        Ok(())
    }

    #[test]
    fn less_awkward_api() -> Result<()> {
        let mut db = ConfigDb::new(BincodeConfig::<Named>::new());

        let first = Named("caio", "romao");
        let second = Named("costa", "nasciment");

        db.add(0, &first)?;
        db.add(1, &second)?;

        // drop(second);

        assert_eq!(first, db.get(0)?.unwrap());
        assert_eq!(second, db.get(1)?.unwrap());

        Ok(())
    }
}
