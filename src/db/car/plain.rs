// Copyright 2019-2025 ChainSafe Systems
// SPDX-License-Identifier: Apache-2.0, MIT

//! # Varint frames
//!
//! CARs are made of concatenations of _varint frames_. Each varint frame is a concatenation of the
//! _body length_ as an
//! [varint](https://docs.rs/integer-encoding/4.0.0/integer_encoding/trait.VarInt.html), and the
//! _frame body_ itself. [`unsigned_varint::codec::UviBytes`] can be used to read frames
//! piecewise into memory.
//!
//! ```text
//!        varint frame
//! │◄───────────────────────►│
//! │                         │
//! ├───────────┬─────────────┤
//! │varint:    │             │
//! │body length│frame body   │
//! └───────────┼─────────────┤
//!             │             │
//! frame body ►│◄───────────►│
//!     offset     =body length
//! ```
//!
//! # CARv1 layout and seeking
//!
//! The first varint frame is a _header frame_, where the frame body is a [`CarHeader`] encoded
//! using [`ipld_dagcbor`](serde_ipld_dagcbor).
//!
//! Subsequent varint frames are _block frames_, where the frame body is a concatenation of a
//! [`Cid`] and the _block data_ addressed by that CID.
//!
//! ```text
//! block frame ►│
//! body offset  │
//!              │  =body length
//!              │◄────────────►│
//!  ┌───────────┼───┬──────────┤
//!  │body length│cid│block data│
//!  └───────────┴───┼──────────┤
//!                  │◄────────►│
//!                  │  =block data length
//!      block data  │
//!          offset ►│
//! ```
//!
//! ## Block ordering
//! > _... a filecoin-deterministic car-file is currently implementation-defined as containing all
//! > DAG-forming blocks in first-seen order, as a result of a depth-first DAG traversal starting
//! > from a single root._
//! - [CAR documentation](https://ipld.io/specs/transport/car/carv1/#determinism)
//!
//! # Future work
//! - [`fadvise`](https://linux.die.net/man/2/posix_fadvise)-based APIs to pre-fetch parts of the
//!   file, to improve random access performance.
//! - Use an inner [`Blockstore`] for writes.
//! - Use safe arithmetic for all operations - a malicious frame shouldn't cause a crash.
//! - Theoretically, file-backed blockstores should be clonable (or even [`Sync`]) with very low
//!   overhead, so that multiple threads could perform operations concurrently.
//! - CARv2 support
//! - A wrapper that abstracts over car formats for reading.

use crate::cid_collections::{CidHashMap, hash_map::Entry as CidHashMapEntry};
use crate::db::PersistentStore;
use crate::utils::db::car_stream::{CarV1Header, CarV2Header};
use crate::{
    blocks::{Tipset, TipsetKey},
    utils::encoding::from_slice_with_fallback,
};
use CidHashMapEntry::{Occupied, Vacant};
use cid::Cid;
use fvm_ipld_blockstore::Blockstore;
use integer_encoding::{FixedIntReader, VarIntReader};
use nunny::Vec as NonEmpty;
use parking_lot::RwLock;
use positioned_io::ReadAt;
use std::ops::DerefMut;
use std::{
    any::Any,
    io::{
        self, BufReader,
        ErrorKind::{InvalidData, Unsupported},
        Read, Seek, SeekFrom,
    },
    iter,
};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tracing::{debug, trace};

/// **Note that all operations on this store are blocking**.
///
/// It can often be time, memory, or disk prohibitive to read large snapshots into a database like
/// [`ParityDb`](crate::db::parity_db::ParityDb).
///
/// This is an implementer of [`Blockstore`] that simply wraps an uncompressed [CARv1
/// file](https://ipld.io/specs/transport/car/carv1).
///
/// On creation, [`PlainCar`] builds an in-memory index of the [`Cid`]s in the file,
/// and their offsets into that file.
/// Note that it prepares its own buffer for doing so.
///
/// When a block is requested, [`PlainCar`] scrolls to that offset, and reads the block, on-demand.
///
/// Writes for new blocks (which don't exist in the CAR already) are currently cached in-memory.
///
/// Random-access performance is expected to be poor, as the OS will have to load separate parts of
/// the file from disk, and flush it for each read. However, (near) linear access should be pretty
/// good, as file chunks will be pre-fetched.
///
/// See [module documentation](mod@self) for more.
pub struct PlainCar<ReaderT> {
    reader: ReaderT,
    write_cache: RwLock<CidHashMap<Vec<u8>>>,
    index: RwLock<CidHashMap<UncompressedBlockDataLocation>>,
    version: u64,
    header_v1: CarV1Header,
    header_v2: Option<CarV2Header>,
}

impl<ReaderT: super::RandomAccessFileReader> PlainCar<ReaderT> {
    /// To be correct:
    /// - `reader` must read immutable data. e.g if it is a file, it should be
    ///   [`flock`](https://linux.die.net/man/2/flock)ed.
    ///   [`Blockstore`] API calls may panic if this is not upheld.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn new(reader: ReaderT) -> io::Result<Self> {
        let mut cursor = positioned_io::Cursor::new(&reader);
        let position = cursor.position();
        let header_v2 = read_v2_header(&mut cursor)?;
        let (limit_position, version) = if let Some(header_v2) = &header_v2 {
            cursor.set_position(position.saturating_add(header_v2.data_offset as u64));
            (
                Some(
                    cursor
                        .stream_position()?
                        .saturating_add(header_v2.data_size as u64),
                ),
                2,
            )
        } else {
            cursor.set_position(position);
            (None, 1)
        };

        let header_v1 = read_v1_header(&mut cursor)?;
        // When indexing, we perform small reads of the length and CID before seeking
        // Buffering these gives us a ~50% speedup (n=10): https://github.com/ChainSafe/forest/pull/3085#discussion_r1246897333
        let mut buf_reader = BufReader::with_capacity(1024, cursor);

        // now create the index
        let index = iter::from_fn(|| {
            read_block_data_location_and_skip(&mut buf_reader, limit_position).transpose()
        })
        .collect::<Result<CidHashMap<_>, _>>()?;

        match index.len() {
            0 => Err(io::Error::new(
                InvalidData,
                "CARv1 files must contain at least one block",
            )),
            num_blocks => {
                debug!(num_blocks, "indexed CAR");
                Ok(Self {
                    reader,
                    write_cache: RwLock::new(CidHashMap::new()),
                    index: RwLock::new(index),
                    version,
                    header_v1,
                    header_v2,
                })
            }
        }
    }

    pub fn roots(&self) -> &NonEmpty<Cid> {
        &self.header_v1.roots
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn heaviest_tipset_key(&self) -> TipsetKey {
        TipsetKey::from(self.roots().clone())
    }

    pub fn heaviest_tipset(&self) -> anyhow::Result<Tipset> {
        Tipset::load_required(self, &self.heaviest_tipset_key())
    }

    /// In an arbitrary order
    #[cfg(test)]
    pub fn cids(&self) -> Vec<Cid> {
        self.index.read().keys().collect()
    }

    pub fn into_dyn(self) -> PlainCar<Box<dyn super::RandomAccessFileReader>> {
        PlainCar {
            reader: Box::new(self.reader),
            write_cache: self.write_cache,
            index: self.index,
            version: self.version,
            header_v1: self.header_v1,
            header_v2: self.header_v2,
        }
    }
}

impl TryFrom<&'static [u8]> for PlainCar<&'static [u8]> {
    type Error = io::Error;
    fn try_from(bytes: &'static [u8]) -> io::Result<Self> {
        PlainCar::new(bytes)
    }
}

/// If you seek to `offset` (from the start of the file), and read `length` bytes,
/// you should get data that corresponds to a [`Cid`] (but NOT the [`Cid`] itself).
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct UncompressedBlockDataLocation {
    offset: u64,
    length: u32,
}

impl<ReaderT> Blockstore for PlainCar<ReaderT>
where
    ReaderT: ReadAt,
{
    #[tracing::instrument(level = "trace", skip(self))]
    fn get(&self, k: &Cid) -> anyhow::Result<Option<Vec<u8>>> {
        match (self.index.read().get(k), self.write_cache.read().get(k)) {
            (Some(_location), Some(_cached)) => {
                trace!("evicting from write cache");
                Ok(self.write_cache.write().remove(k))
            }
            (Some(UncompressedBlockDataLocation { offset, length }), None) => {
                trace!("fetching from disk");
                let mut data = vec![0; usize::try_from(*length).unwrap()];
                self.reader.read_exact_at(*offset, &mut data)?;
                Ok(Some(data))
            }
            (None, Some(cached)) => {
                trace!("getting from write cache");
                Ok(Some(cached.clone()))
            }
            (None, None) => {
                trace!("not found");
                Ok(None)
            }
        }
    }

    /// # Panics
    /// - If the write cache already contains different data with this CID
    /// - See also [`Self::new`].
    ///
    /// Note: Locks have to be acquired in exactly the same order as in `get`, otherwise a
    /// deadlock is imminent in a multi-threaded context.
    #[tracing::instrument(level = "trace", skip(self, block))]
    fn put_keyed(&self, k: &Cid, block: &[u8]) -> anyhow::Result<()> {
        let mut index = self.index.write();
        let mut cache = self.write_cache.write();
        handle_write_cache(cache.deref_mut(), index.deref_mut(), k, block)
    }
}

impl<ReaderT> PersistentStore for PlainCar<ReaderT>
where
    ReaderT: ReadAt,
{
    fn put_keyed_persistent(&self, k: &Cid, block: &[u8]) -> anyhow::Result<()> {
        self.put_keyed(k, block)
    }
}

pub async fn write_skip_frame_header_async(
    mut writer: impl AsyncWrite + Unpin,
    data_len: u32,
) -> std::io::Result<()> {
    writer
        .write_all(&super::forest::ZSTD_SKIPPABLE_FRAME_MAGIC_HEADER)
        .await?;
    writer.write_all(&data_len.to_le_bytes()).await?;
    Ok(())
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct CompressedBlockDataLocation {
    pub zstd_frame_offset: u64,
    pub location_in_frame: UncompressedBlockDataLocation,
}

/// # Panics
/// - If the write cache already contains different data with this CID
///
/// Note: This could potentially be enhanced with fine-grained read/write
/// locking, however the performance is acceptable for now.
fn handle_write_cache(
    write_cache: &mut CidHashMap<Vec<u8>>,
    index: &mut CidHashMap<impl Any>,
    k: &Cid,
    block: &[u8],
) -> anyhow::Result<()> {
    match (index.get(k), write_cache.entry(*k)) {
        (None, Occupied(already)) => match already.get() == block {
            true => {
                trace!("already in cache");
                Ok(())
            }
            false => panic!("mismatched content on second write for CID {k}"),
        },
        (None, Vacant(vacant)) => {
            trace!(bytes = block.len(), "insert into cache");
            vacant.insert(block.to_owned());
            Ok(())
        }
        (Some(_), Vacant(_)) => {
            trace!("already on disk");
            Ok(())
        }
        (Some(_), Occupied(_)) => {
            unreachable!("we don't insert a CID in the write cache if it exists on disk")
        }
    }
}

fn cid_error_to_io_error(cid_error: cid::Error) -> io::Error {
    match cid_error {
        cid::Error::Io(io_error) => io_error,
        other => io::Error::new(InvalidData, other),
    }
}

/// <https://ipld.io/specs/transport/car/carv2/#header>
/// ```text
/// start ►│    reader end ►│
///        ├──────┬─────────┤
///        │pragma│v2 header│
///        └──────┴─────────┘
/// ```
pub fn read_v2_header(mut reader: impl Read) -> io::Result<Option<CarV2Header>> {
    /// <https://ipld.io/specs/transport/car/carv2/#pragma>
    const CAR_V2_PRAGMA: [u8; 10] = [0xa1, 0x67, 0x76, 0x65, 0x72, 0x73, 0x69, 0x6f, 0x6e, 0x02];

    let len = reader.read_fixedint::<u8>()? as usize;
    if len == CAR_V2_PRAGMA.len() {
        let mut buffer = vec![0; len];
        reader.read_exact(&mut buffer)?;
        if buffer[..] == CAR_V2_PRAGMA {
            let mut characteristics = [0; 16];
            reader.read_exact(&mut characteristics)?;
            let data_offset: i64 = reader.read_fixedint()?;
            let data_size: i64 = reader.read_fixedint()?;
            let index_offset: i64 = reader.read_fixedint()?;
            return Ok(Some(CarV2Header {
                characteristics,
                data_offset,
                data_size,
                index_offset,
            }));
        }
    }
    Ok(None)
}

/// ```text
/// start ►│         reader end ►│
///        ├───────────┬─────────┤
///        │body length│v1 header│
///        └───────────┴─────────┘
/// ```
#[tracing::instrument(level = "trace", skip_all, ret)]
fn read_v1_header(mut reader: impl Read) -> io::Result<CarV1Header> {
    let header_len = reader.read_varint()?;
    let mut buffer = vec![0; header_len];
    reader.read_exact(&mut buffer)?;
    let header: CarV1Header =
        from_slice_with_fallback(&buffer).map_err(|e| io::Error::new(InvalidData, e))?;
    if header.version == 1 {
        Ok(header)
    } else {
        Err(io::Error::new(
            Unsupported,
            format!("unsupported CAR version {}", header.version),
        ))
    }
}

/// Returns ([`Cid`], the `block data offset` and `block data length`)
/// ```text
/// start ►│              reader end ►│
///        ├───────────┬───┬──────────┤
///        │body length│cid│block data│
///        └───────────┴───┼──────────┤
///                        │◄────────►│
///                        │  =block data length
///            block data  │
///                offset ►│
/// ```
/// Importantly, we seek `block data length`, rather than read any in.
/// This allows us to keep indexing fast.
///
/// [`Ok(None)`] on EOF
#[tracing::instrument(level = "trace", skip_all, ret)]
fn read_block_data_location_and_skip(
    mut reader: (impl Read + Seek),
    limit_position: Option<u64>,
) -> io::Result<Option<(Cid, UncompressedBlockDataLocation)>> {
    if let Some(limit_position) = limit_position {
        if reader.stream_position()? >= limit_position {
            return Ok(None);
        }
    }
    let Some(body_length) = read_varint_body_length_or_eof(&mut reader)? else {
        return Ok(None);
    };
    let frame_body_offset = reader.stream_position()?;
    let mut reader = CountRead::new(&mut reader);
    let cid = Cid::read_bytes(&mut reader).map_err(cid_error_to_io_error)?;

    // counting the read bytes saves us a syscall for finding block data offset
    let cid_length = reader.bytes_read();
    let block_data_offset = frame_body_offset + u64::try_from(cid_length).unwrap();
    let next_frame_offset = frame_body_offset + u64::from(body_length);
    let block_data_length = u32::try_from(next_frame_offset - block_data_offset).unwrap();
    reader
        .into_inner()
        .seek(SeekFrom::Start(next_frame_offset))?;
    Ok(Some((
        cid,
        UncompressedBlockDataLocation {
            offset: block_data_offset,
            length: block_data_length,
        },
    )))
}

/// Reads `body length`, leaving the reader at the start of a varint frame,
/// or returns [`Ok(None)`] if we've reached EOF
/// ```text
/// start ►│
///        ├───────────┬─────────────┐
///        │varint:    │             │
///        │body length│frame body   │
///        └───────────┼─────────────┘
///        reader end ►│
/// ```
fn read_varint_body_length_or_eof(mut reader: impl Read) -> io::Result<Option<u32>> {
    let mut byte = [0u8; 1]; // detect EOF
    match reader.read(&mut byte)? {
        0 => Ok(None),
        1 => (byte.chain(reader)).read_varint().map(Some),
        _ => unreachable!(),
    }
}

/// A reader that keeps track of how many bytes it has read.
///
/// This is useful for calculating the _block data length_ when the (_varint frame_) _body length_ is known.
struct CountRead<ReadT> {
    inner: ReadT,
    count: usize,
}

impl<ReadT> CountRead<ReadT> {
    pub fn new(inner: ReadT) -> Self {
        Self { inner, count: 0 }
    }
    pub fn bytes_read(&self) -> usize {
        self.count
    }
    pub fn into_inner(self) -> ReadT {
        self.inner
    }
}

impl<ReadT> Read for CountRead<ReadT>
where
    ReadT: Read,
{
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.count += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::PlainCar;
    use crate::utils::db::{
        car_stream::{CarStream, CarV1Header},
        car_util::load_car,
    };
    use futures::{TryStreamExt as _, executor::block_on};
    use fvm_ipld_blockstore::{Blockstore, MemoryBlockstore};
    use std::io::Cursor;
    use std::sync::LazyLock;
    use tokio::io::{AsyncBufRead, AsyncSeek, BufReader};

    #[test]
    fn test_uncompressed_v1() {
        let car = chain4_car();
        let car_backed = PlainCar::new(car).unwrap();

        assert_eq!(car_backed.version(), 1);
        assert_eq!(car_backed.roots().len(), 1);
        assert_eq!(car_backed.cids().len(), 1222);

        let reference_car = reference(Cursor::new(car));
        let reference_car_zst = reference(Cursor::new(chain4_car_zst()));
        let reference_car_zst_unsafe = reference_unsafe(chain4_car_zst());
        for cid in car_backed.cids() {
            let expected = reference_car.get(&cid).unwrap().unwrap();
            let expected2 = reference_car_zst.get(&cid).unwrap().unwrap();
            let expected3 = reference_car_zst_unsafe.get(&cid).unwrap().unwrap();
            let actual = car_backed.get(&cid).unwrap().unwrap();
            assert_eq!(expected, actual);
            assert_eq!(expected2, actual);
            assert_eq!(expected3, actual);
        }
    }

    #[test]
    fn test_uncompressed_v2() {
        let car = carv2_car();
        let car_backed = PlainCar::new(car).unwrap();

        assert_eq!(car_backed.version(), 2);
        assert_eq!(car_backed.roots().len(), 1);
        assert_eq!(car_backed.cids().len(), 7153);

        let reference_car = reference(Cursor::new(car));
        let reference_car_zst = reference(Cursor::new(carv2_car_zst()));
        let reference_car_zst_unsafe = reference_unsafe(carv2_car_zst());
        for cid in car_backed.cids() {
            let expected = reference_car.get(&cid).unwrap().unwrap();
            let expected2 = reference_car_zst.get(&cid).unwrap().unwrap();
            let expected3 = reference_car_zst_unsafe.get(&cid).unwrap().unwrap();
            let actual = car_backed.get(&cid).unwrap().unwrap();
            assert_eq!(expected, actual);
            assert_eq!(expected2, actual);
            assert_eq!(expected3, actual);
        }
    }

    fn reference(reader: impl AsyncBufRead + AsyncSeek + Unpin) -> MemoryBlockstore {
        let blockstore = MemoryBlockstore::new();
        block_on(load_car(&blockstore, reader)).unwrap();
        blockstore
    }

    fn reference_unsafe(reader: impl AsyncBufRead + Unpin) -> MemoryBlockstore {
        let blockstore = MemoryBlockstore::new();
        block_on(load_car_unsafe(&blockstore, reader)).unwrap();
        blockstore
    }

    pub async fn load_car_unsafe<R>(db: &impl Blockstore, reader: R) -> anyhow::Result<CarV1Header>
    where
        R: AsyncBufRead + Unpin,
    {
        let mut stream = CarStream::new_unsafe(BufReader::new(reader)).await?;
        while let Some(block) = stream.try_next().await? {
            db.put_keyed(&block.cid, &block.data)?;
        }
        Ok(stream.header_v1)
    }

    fn chain4_car_zst() -> &'static [u8] {
        include_bytes!("../../../test-snapshots/chain4.car.zst")
    }

    fn chain4_car() -> &'static [u8] {
        static CAR: LazyLock<Vec<u8>> =
            LazyLock::new(|| zstd::decode_all(chain4_car_zst()).unwrap());
        CAR.as_slice()
    }

    fn carv2_car_zst() -> &'static [u8] {
        include_bytes!("../../../test-snapshots/carv2.car.zst")
    }

    fn carv2_car() -> &'static [u8] {
        static CAR: LazyLock<Vec<u8>> =
            LazyLock::new(|| zstd::decode_all(carv2_car_zst()).unwrap());
        CAR.as_slice()
    }
}
