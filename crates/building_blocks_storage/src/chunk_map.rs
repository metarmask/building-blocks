//! A sparse lattice map made of up array chunks.
//!
//! # Indexing and Iteration
//!
//! The data can either be addressed by chunk key with the `get_chunk*` methods or by individual points using the `Get*` and
//! `ForEach*` trait impls. The map of chunks uses `Point3i` keys. The key for a chunk is the minimum point in that chunk, which
//! is always a multiple of the chunk shape. Chunk shape dimensions must be powers of 2, which allows for efficiently
//! calculating a chunk key from any point in the chunk.
//!
//! If you require iteration over large, but very sparse regions, you might want an additional `OctreeChunkIndex` to track the
//! set of occupied chunks. Traversing that index can be faster than doing hash map lookups on all of the possible chunks in a
//! region.
//!
//! # Chunk Storage
//!
//! `ChunkMap<N, T, Bldr, Store>` depends on a backing chunk storage `Store`, which can implement some of `ChunkReadStorage` or
//! `ChunkWriteStorage`. A storage can be as simple as a `HashMap`, which provides good performance for both iteration and
//! random access. It could also be something more memory efficient like `FastCompressibleChunkStorage` or
//! `CompressibleChunkStorageReader`, which perform nearly as well but involve some extra management of the cache.
//!
//! # Serialization
//!
//! In order to efficiently serialize a `ChunkMap`, you can first use `SerializableChunks::from_iter` to create a compact
//! serializable representation. It will compress the bincode representation of the chunks.
//!
//! # Example `ChunkHashMap` Usage
//! ```
//! use building_blocks_core::prelude::*;
//! use building_blocks_storage::prelude::*;
//!
//! let chunk_shape = Point3i::fill(16);
//! let ambient_value = 0;
//! let builder = ChunkMapBuilder3x1::new(chunk_shape, ambient_value);
//! let mut map = builder.build_with_hash_map_storage();
//!
//! // Although we only write 3 points, 3 whole dense chunks will be inserted.
//! let write_points = [Point3i::fill(-100), Point3i::ZERO, Point3i::fill(100)];
//! for &p in write_points.iter() {
//!     *map.get_mut(p) = 1;
//! }
//!
//! // Even though the map is sparse, we can get the smallest extent that bounds all of the occupied
//! // chunks.
//! let bounding_extent = map.bounding_extent();
//!
//! // Now we can read back the values.
//! map.for_each(&bounding_extent, |p, value| {
//!     if write_points.iter().position(|pw| p == *pw) != None {
//!         assert_eq!(value, 1);
//!     } else {
//!         // The points that we didn't write explicitly got an ambient value when the chunk was
//!         // inserted. Also any points in `bounding_extent` that don't have a chunk will also take
//!         // the ambient value.
//!         assert_eq!(value, 0);
//!     }
//! });
//!
//! // You can also access individual points like you can with an `Array`. This is
//! // slower than iterating, because it hashes the chunk coordinates for every access.
//! for &p in write_points.iter() {
//!     assert_eq!(map.get(p), 1);
//! }
//! assert_eq!(map.get(Point3i::fill(1)), 0);
//!
//! // Sometimes you need to implement very fast algorithms (like kernel-based methods) that do a
//! // lot of random access. In this case it's most efficient to use `Stride`s, but `ChunkMap`
//! // doesn't support random indexing by `Stride`. Instead, assuming that your query spans multiple
//! // chunks, you should copy the extent into a dense map first. (The copy is fast).
//! let query_extent = Extent3i::from_min_and_shape(Point3i::fill(10), Point3i::fill(32));
//! let mut dense_map = Array3x1::fill(query_extent, ambient_value);
//! copy_extent(&query_extent, &map, &mut dense_map);
//! ```
//!
//! # Example `CompressibleChunkMap` Usage
//!
//! ```
//! # use building_blocks_core::prelude::*;
//! # use building_blocks_storage::prelude::*;
//! #
//! let chunk_shape = Point3i::fill(16);
//! let ambient_value = 0;
//! let builder = ChunkMapBuilder3x1::new(chunk_shape, ambient_value);
//! let mut map = builder.build_with_write_storage(
//!     FastCompressibleChunkStorageNx1::with_bytes_compression(Lz4 { level: 10 })
//! );
//!
//! // You can write voxels the same as any other `ChunkMap`. As chunks are created, they will be placed in an LRU cache.
//! let write_points = [Point3i::fill(-100), Point3i::ZERO, Point3i::fill(100)];
//! for &p in write_points.iter() {
//!     *map.get_mut(p) = 1;
//! }
//!
//! // Save some space by compressing the least recently used chunks. On further access to the compressed chunks, they will get
//! // decompressed and cached.
//! map.storage_mut().compress_lru();
//!
//! // In order to use the read-only access traits, you need to construct a `CompressibleChunkStorageReader`.
//! let local_cache = LocalChunkCache3::new();
//! let reader = map.reader(&local_cache);
//!
//! let bounding_extent = reader.bounding_extent();
//! reader.for_each(&bounding_extent, |p, value| {
//!     if write_points.iter().position(|pw| p == *pw) != None {
//!         assert_eq!(value, 1);
//!     } else {
//!         assert_eq!(value, 0);
//!     }
//! });
//!
//! // For efficient caching, you should flush your local cache back into the main storage when you are done with it.
//! map.storage_mut().flush_local_cache(local_cache);
//! ```

use crate::{
    Array, ArrayCopySrc, ArrayIndexer, Channel, Chunk, ChunkHashMap, ChunkIndexer,
    ChunkReadStorage, ChunkWriteStorage, FillChannels, ForEach, ForEachMut, ForEachMutPtr, Get,
    GetMut, GetRef, IntoMultiMut, IterChunkKeys, MultiMutPtr, MultiRef, ReadExtent,
    SmallKeyHashMap, WriteExtent,
};

use building_blocks_core::{bounding_extent, ExtentN, IntegerPoint, PointN};

use core::hash::Hash;
use either::Either;

/// A lattice map made up of same-shaped `Array` chunks. It takes a value at every possible `PointN`, because accesses made
/// outside of the stored chunks will return some ambient value specified on creation.
///
/// `ChunkMap` is generic over the type used to actually store the `Chunk`s. You can use any storage that implements
/// `ChunkReadStorage` or `ChunkWriteStorage`. Being a lattice map, `ChunkMap` will implement various access traits, depending
/// on the capabilities of the chunk storage.
///
/// If the chunk storage implements `ChunkReadStorage`, then `ChunkMap` will implement:
/// - `Get`
/// - `ForEach`
/// - `ReadExtent`
///
/// If the chunk storage implements `ChunkWriteStorage`, then `ChunkMap` will implement:
/// - `GetMut`
/// - `ForEachMut`
/// - `WriteExtent`
pub struct ChunkMap<N, T, Bldr, Store> {
    /// Translates from lattice coordinates to chunk key space.
    pub indexer: ChunkIndexer<N>,
    storage: Store,
    builder: Bldr,
    ambient_value: T, // Needed for GetRef to return a reference to non-temporary value
}

/// A 2-dimensional `ChunkMap`.
pub type ChunkMap2<T, Bldr, Store> = ChunkMap<[i32; 2], T, Bldr, Store>;
/// A 3-dimensional `ChunkMap`.
pub type ChunkMap3<T, Bldr, Store> = ChunkMap<[i32; 3], T, Bldr, Store>;

/// An N-dimensional, single-channel `ChunkMap`.
pub type ChunkMapNx1<N, T, Store> = ChunkMap<N, T, ChunkMapBuilderNx1<N, T>, Store>;
/// A 2-dimensional, single-channel `ChunkMap`.
pub type ChunkMap2x1<T, Store> = ChunkMapNx1<[i32; 2], T, Store>;
/// A 3-dimensional, single-channel `ChunkMap`.
pub type ChunkMap3x1<T, Store> = ChunkMapNx1<[i32; 3], T, Store>;

/// An object that knows how to construct chunks for a `ChunkMap`.
pub trait ChunkMapBuilder<N, T>: Sized {
    type Chunk: Chunk;

    fn chunk_shape(&self) -> PointN<N>;

    fn ambient_value(&self) -> T;

    /// Construct a new chunk with entirely ambient values.
    fn new_ambient(&self, extent: ExtentN<N>) -> Self::Chunk;

    /// Create a new `ChunkMap` with the given `storage` which must implement both `ChunkReadStorage` and `ChunkWriteStorage`.
    fn build_with_rw_storage<Store>(self, storage: Store) -> ChunkMap<N, T, Self, Store>
    where
        PointN<N>: IntegerPoint<N>,
        Store: ChunkReadStorage<N, Self::Chunk> + ChunkWriteStorage<N, Self::Chunk>,
    {
        ChunkMap::new(self, storage)
    }

    /// Create a new `ChunkMap` with the given `storage` which must implement `ChunkReadStorage`.
    fn build_with_read_storage<Store>(self, storage: Store) -> ChunkMap<N, T, Self, Store>
    where
        PointN<N>: IntegerPoint<N>,
        Store: ChunkReadStorage<N, Self::Chunk>,
    {
        ChunkMap::new(self, storage)
    }

    /// Create a new `ChunkMap` with the given `storage` which must implement `ChunkWriteStorage`.
    fn build_with_write_storage<Store>(self, storage: Store) -> ChunkMap<N, T, Self, Store>
    where
        PointN<N>: IntegerPoint<N>,
        Store: ChunkWriteStorage<N, Self::Chunk>,
    {
        ChunkMap::new(self, storage)
    }

    /// Create a new `ChunkMap` using a `SmallKeyHashMap` as the chunk storage.
    fn build_with_hash_map_storage(self) -> ChunkHashMap<N, T, Self>
    where
        PointN<N>: Hash + IntegerPoint<N>,
    {
        Self::build_with_rw_storage(self, SmallKeyHashMap::default())
    }
}

/// A `ChunkMapBuilder` for `Array` chunks.
#[derive(Clone, Copy)]
pub struct ChunkMapBuilderNxM<N, T, Chan> {
    pub chunk_shape: PointN<N>,
    pub ambient_value: T,
    marker: std::marker::PhantomData<Chan>,
}

impl<N, T, Chan> ChunkMapBuilderNxM<N, T, Chan> {
    pub const fn new(chunk_shape: PointN<N>, ambient_value: T) -> Self {
        Self {
            chunk_shape,
            ambient_value,
            marker: std::marker::PhantomData,
        }
    }
}

macro_rules! builder_type_alias {
    ($name:ident, $dim:ty, $( $chan:ident ),+ ) => {
        pub type $name<$( $chan ),+> = ChunkMapBuilderNxM<$dim, ($($chan),+), ($(Channel<$chan>),+)>;
    };
}

pub mod multichannel_aliases {
    use super::*;

    /// A `ChunkMapBuilder` for `ArrayNx1` chunks.
    pub type ChunkMapBuilderNx1<N, A> = ChunkMapBuilderNxM<N, A, Channel<A>>;

    /// A `ChunkMapBuilder` for `Array2x1` chunks.
    pub type ChunkMapBuilder2x1<A> = ChunkMapBuilderNxM<[i32; 2], A, Channel<A>>;
    builder_type_alias!(ChunkMapBuilder2x2, [i32; 2], A, B);
    builder_type_alias!(ChunkMapBuilder2x3, [i32; 2], A, B, C);
    builder_type_alias!(ChunkMapBuilder2x4, [i32; 2], A, B, C, D);
    builder_type_alias!(ChunkMapBuilder2x5, [i32; 2], A, B, C, D, E);
    builder_type_alias!(ChunkMapBuilder2x6, [i32; 2], A, B, C, D, E, F);

    /// A `ChunkMapBuilder` for `Array3x1` chunks.
    pub type ChunkMapBuilder3x1<A> = ChunkMapBuilderNxM<[i32; 3], A, Channel<A>>;
    builder_type_alias!(ChunkMapBuilder3x2, [i32; 3], A, B);
    builder_type_alias!(ChunkMapBuilder3x3, [i32; 3], A, B, C);
    builder_type_alias!(ChunkMapBuilder3x4, [i32; 3], A, B, C, D);
    builder_type_alias!(ChunkMapBuilder3x5, [i32; 3], A, B, C, D, E);
    builder_type_alias!(ChunkMapBuilder3x6, [i32; 3], A, B, C, D, E, F);
}

pub use multichannel_aliases::*;

impl<N, T, Chan> ChunkMapBuilder<N, T> for ChunkMapBuilderNxM<N, T, Chan>
where
    PointN<N>: IntegerPoint<N>,
    T: Clone,
    Chan: FillChannels<Data = T>,
{
    type Chunk = Array<N, Chan>;

    fn chunk_shape(&self) -> PointN<N> {
        self.chunk_shape
    }

    fn ambient_value(&self) -> T {
        self.ambient_value.clone()
    }

    fn new_ambient(&self, extent: ExtentN<N>) -> Self::Chunk {
        Array::fill(extent, self.ambient_value())
    }
}

impl<N, T, Bldr, Store> ChunkMap<N, T, Bldr, Store>
where
    PointN<N>: IntegerPoint<N>,
    Bldr: ChunkMapBuilder<N, T>,
{
    /// Creates a map using the given `storage`.
    ///
    /// All dimensions of `chunk_shape` must be powers of 2.
    fn new(builder: Bldr, storage: Store) -> Self {
        let indexer = ChunkIndexer::new(builder.chunk_shape());
        let ambient_value = builder.ambient_value();

        Self {
            indexer,
            storage,
            ambient_value,
            builder,
        }
    }
}

impl<N, T, Bldr, Store> ChunkMap<N, T, Bldr, Store> {
    /// Consumes `self` and returns the backing chunk storage.
    #[inline]
    pub fn take_storage(self) -> Store {
        self.storage
    }

    /// Borrows the internal chunk storage.
    #[inline]
    pub fn storage(&self) -> &Store {
        &self.storage
    }

    /// Borrows the internal chunk storage.
    #[inline]
    pub fn storage_mut(&mut self) -> &mut Store {
        &mut self.storage
    }

    #[inline]
    pub fn builder(&self) -> &Bldr {
        &self.builder
    }
}

impl<N, T, Bldr, Store> ChunkMap<N, T, Bldr, Store>
where
    PointN<N>: IntegerPoint<N>,
    Bldr: ChunkMapBuilder<N, T>,
    Store: ChunkReadStorage<N, Bldr::Chunk>,
{
    /// Borrow the chunk at `key`.
    ///
    /// In debug mode only, asserts that `key` is valid.
    #[inline]
    pub fn get_chunk(&self, key: PointN<N>) -> Option<&Bldr::Chunk> {
        debug_assert!(self.indexer.chunk_key_is_valid(key));

        self.storage.get(key)
    }

    /// Call `visitor` on all chunks that overlap `extent`. Vacant chunks will be represented by an `AmbientExtent`.
    #[inline]
    pub fn visit_chunks(
        &self,
        extent: &ExtentN<N>,
        mut visitor: impl FnMut(Either<&Bldr::Chunk, (&ExtentN<N>, AmbientExtent<N, T>)>),
    ) {
        for chunk_key in self.indexer.chunk_keys_for_extent(extent) {
            if let Some(chunk) = self.get_chunk(chunk_key) {
                visitor(Either::Left(chunk))
            } else {
                let chunk_extent = self.indexer.extent_for_chunk_at_key(chunk_key);
                visitor(Either::Right((
                    &chunk_extent,
                    AmbientExtent::new(self.builder.ambient_value()),
                )))
            }
        }
    }

    /// Call `visitor` on all occupied chunks that overlap `extent`.
    #[inline]
    pub fn visit_occupied_chunks(
        &self,
        extent: &ExtentN<N>,
        mut visitor: impl FnMut(&Bldr::Chunk),
    ) {
        for chunk_key in self.indexer.chunk_keys_for_extent(extent) {
            if let Some(chunk) = self.get_chunk(chunk_key) {
                visitor(chunk)
            }
        }
    }
}

impl<N, T, Bldr, Store> ChunkMap<N, T, Bldr, Store>
where
    PointN<N>: IntegerPoint<N>,
    Bldr: ChunkMapBuilder<N, T>,
    Store: ChunkWriteStorage<N, Bldr::Chunk>,
{
    /// Overwrite the `Chunk` at `key` with `chunk`. Drops the previous value.
    ///
    /// In debug mode only, asserts that `key` is valid and `chunk`'s shape is valid.
    #[inline]
    pub fn write_chunk(&mut self, key: PointN<N>, chunk: Bldr::Chunk) {
        debug_assert!(self.indexer.chunk_key_is_valid(key));

        self.storage.write(key, chunk);
    }

    /// Replace the `Chunk` at `key` with `chunk`, returning the old value.
    ///
    /// In debug mode only, asserts that `key` is valid and `chunk`'s shape is valid.
    #[inline]
    pub fn replace_chunk(&mut self, key: PointN<N>, chunk: Bldr::Chunk) -> Option<Bldr::Chunk> {
        debug_assert!(self.indexer.chunk_key_is_valid(key));

        self.storage.replace(key, chunk)
    }

    /// Mutably borrow the chunk at `key`.
    ///
    /// In debug mode only, asserts that `key` is valid.
    #[inline]
    pub fn get_mut_chunk(&mut self, key: PointN<N>) -> Option<&mut Bldr::Chunk> {
        debug_assert!(self.indexer.chunk_key_is_valid(key));

        self.storage.get_mut(key)
    }

    /// Mutably borrow the chunk at `key`. If the chunk doesn't exist, `create_chunk` is called to insert one.
    ///
    /// In debug mode only, asserts that `key` is valid.
    #[inline]
    pub fn get_mut_chunk_or_insert_with(
        &mut self,
        key: PointN<N>,
        create_chunk: impl FnOnce() -> Bldr::Chunk,
    ) -> &mut Bldr::Chunk {
        debug_assert!(self.indexer.chunk_key_is_valid(key));

        self.storage.get_mut_or_insert_with(key, create_chunk)
    }

    /// Mutably borrow the chunk at `key`. If the chunk doesn't exist, a new chunk is created with the ambient value.
    ///
    /// In debug mode only, asserts that `key` is valid.
    #[inline]
    pub fn get_mut_chunk_or_insert_ambient(&mut self, key: PointN<N>) -> &mut Bldr::Chunk {
        debug_assert!(self.indexer.chunk_key_is_valid(key));

        let Self {
            indexer,
            storage,
            builder,
            ..
        } = self;

        storage.get_mut_or_insert_with(key, || {
            builder.new_ambient(indexer.extent_for_chunk_at_key(key))
        })
    }

    /// Call `visitor` on all chunks that overlap `extent`. Vacant chunks will be created first with ambient value.
    #[inline]
    pub fn visit_mut_chunks(
        &mut self,
        extent: &ExtentN<N>,
        mut visitor: impl FnMut(&mut Bldr::Chunk),
    ) {
        for chunk_key in self.indexer.chunk_keys_for_extent(extent) {
            visitor(self.get_mut_chunk_or_insert_ambient(chunk_key));
        }
    }

    /// Call `visitor` on all occupied chunks that overlap `extent`.
    #[inline]
    pub fn visit_occupied_mut_chunks(
        &mut self,
        extent: &ExtentN<N>,
        mut visitor: impl FnMut(&mut Bldr::Chunk),
    ) {
        for chunk_key in self.indexer.chunk_keys_for_extent(extent) {
            if let Some(chunk) = self.get_mut_chunk(chunk_key) {
                visitor(chunk)
            }
        }
    }

    #[inline]
    pub fn delete_chunk(&mut self, key: PointN<N>) {
        debug_assert!(self.indexer.chunk_key_is_valid(key));
        self.storage.delete(key);
    }

    #[inline]
    pub fn pop_chunk(&mut self, key: PointN<N>) -> Option<Bldr::Chunk> {
        debug_assert!(self.indexer.chunk_key_is_valid(key));

        self.storage.pop(key)
    }
}

impl<N, T, Bldr, Store, MutPtr> ChunkMap<N, T, Bldr, Store>
where
    Self: ForEachMutPtr<N, PointN<N>, Item = MutPtr>,
    T: Clone,
    MutPtr: MultiMutPtr<Data = T>,
{
    /// Fill all of `extent` with the same `value`.
    #[inline]
    pub fn fill_extent(&mut self, extent: &ExtentN<N>, value: T) {
        // PERF: write whole chunks using a fast path
        unsafe {
            self.for_each_mut_ptr(extent, |_p, ptr| ptr.write(value.clone()));
        }
    }
}

impl<'a, N, T, Bldr, Store> ChunkMap<N, T, Bldr, Store>
where
    PointN<N>: IntegerPoint<N>,
    Store: IterChunkKeys<'a, N>,
{
    /// The smallest extent that bounds all chunks.
    pub fn bounding_extent(&'a self) -> ExtentN<N> {
        bounding_extent(self.storage.chunk_keys().flat_map(|key| {
            let chunk_extent = self.indexer.extent_for_chunk_at_key(*key);

            vec![chunk_extent.minimum, chunk_extent.max()].into_iter()
        }))
    }
}

/// An extent that takes the same value everywhere.
#[derive(Copy, Clone)]
pub struct AmbientExtent<N, T> {
    pub value: T,
    _n: std::marker::PhantomData<N>,
}

impl<N, T> AmbientExtent<N, T> {
    pub fn new(value: T) -> Self {
        Self {
            value,
            _n: Default::default(),
        }
    }

    pub fn get(&self) -> T
    where
        T: Clone,
    {
        self.value.clone()
    }
}

impl<N, T> ForEach<N, PointN<N>> for AmbientExtent<N, T>
where
    T: Clone,
    PointN<N>: IntegerPoint<N>,
{
    type Item = T;

    fn for_each(&self, extent: &ExtentN<N>, mut f: impl FnMut(PointN<N>, Self::Item)) {
        for p in extent.iter_points() {
            f(p, self.value.clone());
        }
    }
}

//  ██████╗ ███████╗████████╗████████╗███████╗██████╗ ███████╗
// ██╔════╝ ██╔════╝╚══██╔══╝╚══██╔══╝██╔════╝██╔══██╗██╔════╝
// ██║  ███╗█████╗     ██║      ██║   █████╗  ██████╔╝███████╗
// ██║   ██║██╔══╝     ██║      ██║   ██╔══╝  ██╔══██╗╚════██║
// ╚██████╔╝███████╗   ██║      ██║   ███████╗██║  ██║███████║
//  ╚═════╝ ╚══════╝   ╚═╝      ╚═╝   ╚══════╝╚═╝  ╚═╝╚══════╝

impl<'a, N, T, Bldr, Store> Get<PointN<N>> for ChunkMap<N, T, Bldr, Store>
where
    PointN<N>: IntegerPoint<N>,
    T: Clone,
    Bldr: ChunkMapBuilder<N, T>,
    <Bldr::Chunk as Chunk>::Array: Get<PointN<N>, Item = T>,
    Store: ChunkReadStorage<N, Bldr::Chunk>,
{
    type Item = T;

    #[inline]
    fn get(&self, p: PointN<N>) -> Self::Item {
        let key = self.indexer.chunk_key_containing_point(p);

        self.get_chunk(key)
            .map(|chunk| chunk.array().get(p))
            .unwrap_or_else(|| self.ambient_value.clone())
    }
}

impl<'a, N, T, Bldr, Store, Ref> GetRef<'a, PointN<N>> for ChunkMap<N, T, Bldr, Store>
where
    PointN<N>: IntegerPoint<N>,
    Bldr: ChunkMapBuilder<N, T>,
    <Bldr::Chunk as Chunk>::Array: GetRef<'a, PointN<N>, Item = Ref>,
    Store: ChunkReadStorage<N, Bldr::Chunk>,
    Ref: MultiRef<'a, Data = T>,
{
    type Item = Ref;

    #[inline]
    fn get_ref(&'a self, p: PointN<N>) -> Self::Item {
        let key = self.indexer.chunk_key_containing_point(p);

        self.get_chunk(key)
            .map(|chunk| chunk.array().get_ref(p))
            .unwrap_or_else(|| Ref::from_data_ref(&self.ambient_value))
    }
}

impl<'a, N, T, Bldr, Store, Mut> GetMut<'a, PointN<N>> for ChunkMap<N, T, Bldr, Store>
where
    PointN<N>: IntegerPoint<N>,
    Bldr: ChunkMapBuilder<N, T>,
    <Bldr::Chunk as Chunk>::Array: GetMut<'a, PointN<N>, Item = Mut>,
    Store: ChunkWriteStorage<N, Bldr::Chunk>,
{
    type Item = Mut;

    #[inline]
    fn get_mut(&'a mut self, p: PointN<N>) -> Self::Item {
        let key = self.indexer.chunk_key_containing_point(p);
        let chunk = self.get_mut_chunk_or_insert_ambient(key);

        chunk.array_mut().get_mut(p)
    }
}

// ███████╗ ██████╗ ██████╗     ███████╗ █████╗  ██████╗██╗  ██╗
// ██╔════╝██╔═══██╗██╔══██╗    ██╔════╝██╔══██╗██╔════╝██║  ██║
// █████╗  ██║   ██║██████╔╝    █████╗  ███████║██║     ███████║
// ██╔══╝  ██║   ██║██╔══██╗    ██╔══╝  ██╔══██║██║     ██╔══██║
// ██║     ╚██████╔╝██║  ██║    ███████╗██║  ██║╚██████╗██║  ██║
// ╚═╝      ╚═════╝ ╚═╝  ╚═╝    ╚══════╝╚═╝  ╚═╝ ╚═════╝╚═╝  ╚═╝

impl<N, T, Bldr, Store> ForEach<N, PointN<N>> for ChunkMap<N, T, Bldr, Store>
where
    PointN<N>: IntegerPoint<N>,
    Bldr: ChunkMapBuilder<N, T>,
    <Bldr::Chunk as Chunk>::Array: ForEach<N, PointN<N>, Item = T>,
    T: Copy,
    Store: ChunkReadStorage<N, Bldr::Chunk>,
{
    type Item = T;

    #[inline]
    fn for_each(&self, extent: &ExtentN<N>, mut f: impl FnMut(PointN<N>, Self::Item)) {
        self.visit_chunks(extent, |chunk| match chunk {
            Either::Left(chunk) => {
                chunk.array().for_each(extent, |p, value| f(p, value));
            }
            Either::Right((chunk_extent, ambient)) => {
                ambient.for_each(&extent.intersection(&chunk_extent), |p, value| f(p, value))
            }
        });
    }
}

impl<N, T, Bldr, Store, MutPtr> ForEachMutPtr<N, PointN<N>> for ChunkMap<N, T, Bldr, Store>
where
    PointN<N>: IntegerPoint<N>,
    Bldr: ChunkMapBuilder<N, T>,
    <Bldr::Chunk as Chunk>::Array: ForEachMutPtr<N, PointN<N>, Item = MutPtr>,
    Store: ChunkWriteStorage<N, Bldr::Chunk>,
{
    type Item = MutPtr;

    #[inline]
    unsafe fn for_each_mut_ptr(
        &mut self,
        extent: &ExtentN<N>,
        mut f: impl FnMut(PointN<N>, Self::Item),
    ) {
        self.visit_mut_chunks(extent, |chunk| {
            chunk
                .array_mut()
                .for_each_mut_ptr(extent, |p, ptr| f(p, ptr))
        });
    }
}

impl<'a, N, T, Bldr, Store, Mut, MutPtr> ForEachMut<'a, N, PointN<N>>
    for ChunkMap<N, T, Bldr, Store>
where
    Self: ForEachMutPtr<N, PointN<N>, Item = MutPtr>,
    MutPtr: IntoMultiMut<'a, MultiMut = Mut>,
{
    type Item = Mut;

    #[inline]
    fn for_each_mut(&'a mut self, extent: &ExtentN<N>, mut f: impl FnMut(PointN<N>, Self::Item)) {
        unsafe { self.for_each_mut_ptr(extent, |p, ptr| f(p, ptr.into_multi_mut())) }
    }
}

//  ██████╗ ██████╗ ██████╗ ██╗   ██╗
// ██╔════╝██╔═══██╗██╔══██╗╚██╗ ██╔╝
// ██║     ██║   ██║██████╔╝ ╚████╔╝
// ██║     ██║   ██║██╔═══╝   ╚██╔╝
// ╚██████╗╚██████╔╝██║        ██║
//  ╚═════╝ ╚═════╝ ╚═╝        ╚═╝

impl<'a, N, T, Bldr, Store> ReadExtent<'a, N> for ChunkMap<N, T, Bldr, Store>
where
    N: ArrayIndexer<N>,
    PointN<N>: IntegerPoint<N>,
    Bldr: ChunkMapBuilder<N, T>,
    Bldr::Chunk: 'a,
    T: 'a + Copy,
    Store: ChunkReadStorage<N, Bldr::Chunk>,
{
    type Src = ChunkCopySrc<N, T, &'a Bldr::Chunk>;
    type SrcIter = ChunkCopySrcIter<N, T, &'a Bldr::Chunk>;

    fn read_extent(&'a self, extent: &ExtentN<N>) -> Self::SrcIter {
        let chunk_iters = self
            .indexer
            .chunk_keys_for_extent(extent)
            .map(|key| {
                let chunk_extent = self.indexer.extent_for_chunk_at_key(key);
                let intersection = extent.intersection(&chunk_extent);

                (
                    intersection,
                    self.get_chunk(key)
                        .map(|chunk| Either::Left(ArrayCopySrc(chunk)))
                        .unwrap_or_else(|| {
                            Either::Right(AmbientExtent::new(self.builder.ambient_value()))
                        }),
                )
            })
            .collect::<Vec<_>>();

        chunk_iters.into_iter()
    }
}

// If `Array` supports writing from type Src, then so does ChunkMap.
impl<N, T, Bldr, Store, Src> WriteExtent<N, Src> for ChunkMap<N, T, Bldr, Store>
where
    PointN<N>: IntegerPoint<N>,
    Bldr: ChunkMapBuilder<N, T>,
    <Bldr::Chunk as Chunk>::Array: WriteExtent<N, Src>,
    Store: ChunkWriteStorage<N, Bldr::Chunk>,
    Src: Copy,
{
    fn write_extent(&mut self, extent: &ExtentN<N>, src: Src) {
        self.visit_mut_chunks(extent, |chunk| chunk.array_mut().write_extent(extent, src));
    }
}

#[doc(hidden)]
pub type ChunkCopySrc<N, T, Ch> = Either<ArrayCopySrc<Ch>, AmbientExtent<N, T>>;
#[doc(hidden)]
pub type ChunkCopySrcIter<N, T, Ch> = std::vec::IntoIter<(ExtentN<N>, ChunkCopySrc<N, T, Ch>)>;

// ████████╗███████╗███████╗████████╗
// ╚══██╔══╝██╔════╝██╔════╝╚══██╔══╝
//    ██║   █████╗  ███████╗   ██║
//    ██║   ██╔══╝  ╚════██║   ██║
//    ██║   ███████╗███████║   ██║
//    ╚═╝   ╚══════╝╚══════╝   ╚═╝

#[cfg(test)]
mod tests {
    use super::*;

    use crate::{copy_extent, Array3x1, Get};

    use building_blocks_core::prelude::*;

    const CHUNK_SHAPE: Point3i = PointN([16; 3]);
    const BUILDER: ChunkMapBuilder3x1<i32> = ChunkMapBuilder3x1::new(CHUNK_SHAPE, 0);

    #[test]
    fn write_and_read_points() {
        let mut map = BUILDER.build_with_hash_map_storage();

        let points = [
            [0, 0, 0],
            [1, 2, 3],
            [16, 0, 0],
            [0, 16, 0],
            [0, 0, 16],
            [15, 0, 0],
            [-15, 0, 0],
        ];

        for p in points.iter().cloned() {
            assert_eq!(map.get_mut(PointN(p)), &mut 0);
            *map.get_mut(PointN(p)) = 1;
            assert_eq!(map.get_mut(PointN(p)), &mut 1);
        }
    }

    #[test]
    fn write_extent_with_for_each_then_read() {
        let mut map = BUILDER.build_with_hash_map_storage();

        let write_extent = Extent3i::from_min_and_shape(Point3i::fill(10), Point3i::fill(80));
        map.for_each_mut(&write_extent, |_p, value| *value = 1);

        let read_extent = Extent3i::from_min_and_shape(Point3i::ZERO, Point3i::fill(100));
        for p in read_extent.iter_points() {
            if write_extent.contains(p) {
                assert_eq!(map.get(p), 1);
            } else {
                assert_eq!(map.get(p), 0);
            }
        }
    }

    #[test]
    fn copy_extent_from_array_then_read() {
        let extent_to_copy = Extent3i::from_min_and_shape(Point3i::fill(10), Point3i::fill(80));
        let array = Array3x1::fill(extent_to_copy, 1);

        let mut map = BUILDER.build_with_hash_map_storage();

        copy_extent(&extent_to_copy, &array, &mut map);

        let read_extent = Extent3i::from_min_and_shape(Point3i::ZERO, Point3i::fill(100));
        for p in read_extent.iter_points() {
            if extent_to_copy.contains(p) {
                assert_eq!(map.get(p), 1);
            } else {
                assert_eq!(map.get(p), 0);
            }
        }
    }

    #[test]
    fn multichannel_accessors() {
        let builder = ChunkMapBuilder3x2::new(CHUNK_SHAPE, (0, 'a'));
        let mut map = builder.build_with_hash_map_storage();

        assert_eq!(map.get(Point3i::fill(1)), (0, 'a'));
        assert_eq!(map.get_ref(Point3i::fill(1)), (&0, &'a'));
        assert_eq!(map.get_mut(Point3i::fill(1)), (&mut 0, &mut 'a'));

        let extent = Extent3i::from_min_and_shape(Point3i::fill(10), Point3i::fill(80));

        map.for_each_mut(&extent, |_p, (num, letter)| {
            *num = 1;
            *letter = 'b';
        });

        map.for_each(&extent, |_p, (num, letter)| {
            assert_eq!(num, 1);
            assert_eq!(letter, 'b');
        });

        map.fill_extent(&extent, (1, 'b'));
    }

    #[cfg(feature = "lz4")]
    #[test]
    fn multichannel_compressed_accessors() {
        use crate::{FastCompressibleChunkStorageNx2, LocalChunkCache, Lz4};

        let builder = ChunkMapBuilder3x2::new(CHUNK_SHAPE, (0, 'a'));
        let mut map = builder.build_with_write_storage(
            FastCompressibleChunkStorageNx2::with_bytes_compression(Lz4 { level: 10 }),
        );

        assert_eq!(map.get_mut(Point3i::fill(1)), (&mut 0, &mut 'a'));

        let extent = Extent3i::from_min_and_shape(Point3i::fill(10), Point3i::fill(80));

        map.for_each_mut(&extent, |_p, (num, letter)| {
            *num = 1;
            *letter = 'b';
        });

        let local_cache = LocalChunkCache::new();
        let reader = map.reader(&local_cache);
        assert_eq!(reader.get(Point3i::fill(1)), (0, 'a'));
        assert_eq!(reader.get_ref(Point3i::fill(1)), (&0, &'a'));

        reader.for_each(&extent, |_p, (num, letter)| {
            assert_eq!(num, 1);
            assert_eq!(letter, 'b');
        });
    }
}
