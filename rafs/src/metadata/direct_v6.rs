// Copyright (C) 2021 Alibaba Cloud. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

/// A bootstrap driver to directly use on disk bootstrap as runtime in-memory bootstrap.
///
/// To reduce memory footprint and speed up filesystem initialization, the V5 on disk bootstrap
/// layout has been designed to support directly mapping as runtime bootstrap. So we don't need to
/// define another set of runtime data structures to cache on-disk bootstrap in memory.
///
/// To support modification to the runtime bootstrap, several technologies have been adopted:
/// * - arc-swap is used to support RCU-like update instead of Mutex/RwLock.
/// * - `offset` instead of `pointer` is used to record data structure position.
/// * - reference count to the referenced resources/objects.
///
/// # Security
/// The bootstrap file may be provided by untrusted parties, so we must ensure strong validations
/// before making use of any bootstrap, especially we are using them in memory-mapped mode. The
/// rule is to call validate() after creating any data structure from the on-disk bootstrap.
use std::any::Any;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io::{Result, SeekFrom};
use std::mem::size_of;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::io::AsRawFd;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arc_swap::{ArcSwap, Guard};
use nydus_utils::filemap::{clone_file, FileMapState};
use nydus_utils::{digest::RafsDigest, div_round_up, round_up};
use storage::device::{
    v5::BlobV5ChunkInfo, BlobChunkFlags, BlobChunkInfo, BlobDevice, BlobInfo, BlobIoDesc, BlobIoVec,
};
use storage::utils::readahead;

use crate::metadata::layout::v5::RafsV5ChunkInfo;
use crate::metadata::layout::v6::{
    recover_namespace, RafsV6BlobTable, RafsV6Dirent, RafsV6InodeChunkAddr, RafsV6InodeCompact,
    RafsV6InodeExtended, RafsV6OndiskInode, RafsV6XattrEntry, RafsV6XattrIbodyHeader,
    EROFS_BLOCK_SIZE, EROFS_INODE_CHUNK_BASED, EROFS_INODE_FLAT_INLINE, EROFS_INODE_FLAT_PLAIN,
    EROFS_INODE_SLOT_SIZE, EROFS_I_DATALAYOUT_BITS, EROFS_I_VERSION_BIT, EROFS_I_VERSION_BITS,
};
use crate::metadata::layout::{bytes_to_os_str, MetaRange, XattrName, XattrValue};
use crate::metadata::{
    Attr, Entry, Inode, RafsInode, RafsInodeWalkAction, RafsInodeWalkHandler, RafsSuperBlock,
    RafsSuperInodes, RafsSuperMeta, RAFS_ATTR_BLOCK_SIZE, RAFS_MAX_NAME,
};
use crate::{MetaType, RafsError, RafsInodeExt, RafsIoReader, RafsResult};

fn err_invalidate_data(rafs_err: RafsError) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, rafs_err)
}

/// The underlying struct to maintain memory mapped bootstrap for a file system.
///
/// Only the DirectMappingState may store raw pointers.
/// Other data structures should not store raw pointers, instead they should hold a reference to
/// the DirectMappingState object and store an offset, so a `pointer` could be reconstruct by
/// `DirectMappingState.base + offset`.
struct DirectMappingState {
    meta: Arc<RafsSuperMeta>,
    blob_table: RafsV6BlobTable,
    map: FileMapState,
}

impl DirectMappingState {
    fn new(meta: &RafsSuperMeta) -> Self {
        DirectMappingState {
            meta: Arc::new(*meta),
            blob_table: RafsV6BlobTable::default(),
            map: FileMapState::default(),
        }
    }
}

struct DirectCachedInfo {
    meta_offset: usize,
    root_ino: Inode,
    chunk_size: u32,
    chunk_map: Mutex<Option<HashMap<RafsV6InodeChunkAddr, usize>>>,
    attr_timeout: Duration,
    entry_timeout: Duration,
}

/// Direct-mapped Rafs v6 super block.
#[derive(Clone)]
pub struct DirectSuperBlockV6 {
    info: Arc<DirectCachedInfo>,
    state: Arc<ArcSwap<DirectMappingState>>,
}

impl DirectSuperBlockV6 {
    /// Create a new instance of `DirectSuperBlockV6`.
    pub fn new(meta: &RafsSuperMeta) -> Self {
        let state = DirectMappingState::new(meta);
        let meta_offset = meta.meta_blkaddr as usize * EROFS_BLOCK_SIZE as usize;
        let info = DirectCachedInfo {
            meta_offset,
            root_ino: meta.root_nid as Inode,
            chunk_size: meta.chunk_size,
            chunk_map: Mutex::new(None),
            attr_timeout: meta.attr_timeout,
            entry_timeout: meta.entry_timeout,
        };

        Self {
            info: Arc::new(info),
            state: Arc::new(ArcSwap::new(Arc::new(state))),
        }
    }

    fn disk_inode(
        state: &Guard<Arc<DirectMappingState>>,
        offset: usize,
    ) -> Result<&dyn RafsV6OndiskInode> {
        let i: &RafsV6InodeCompact = state.map.get_ref(offset)?;
        if i.format() & EROFS_I_VERSION_BITS == 0 {
            Ok(i)
        } else {
            let i = state.map.get_ref::<RafsV6InodeExtended>(offset)?;
            Ok(i)
        }
    }

    fn inode_wrapper(
        &self,
        state: &Guard<Arc<DirectMappingState>>,
        nid: u64,
    ) -> Result<OndiskInodeWrapper> {
        let offset = self.info.meta_offset + nid as usize * EROFS_INODE_SLOT_SIZE;
        OndiskInodeWrapper::new(state, self.clone(), offset)
    }

    // For RafsV6, we can't get the parent info of a non-dir file with its on-disk inode,
    // so we need to pass corresponding parent info when constructing the child inode.
    fn inode_wrapper_with_info(
        &self,
        state: &Guard<Arc<DirectMappingState>>,
        nid: u64,
        parent_inode: Inode,
        name: OsString,
    ) -> Result<OndiskInodeWrapper> {
        self.inode_wrapper(state, nid).map(|inode| {
            let mut inode = inode;
            // # Safety
            // inode always valid
            inode.parent_inode = Some(parent_inode);
            inode.name = Some(name);
            inode
        })
    }

    fn update_state(&self, r: &mut RafsIoReader) -> Result<()> {
        // Validate file size
        let file = clone_file(r.as_raw_fd())?;
        let md = file.metadata()?;
        let len = md.len();
        let md_range =
            MetaRange::new(EROFS_BLOCK_SIZE as u64, len - EROFS_BLOCK_SIZE as u64, true)?;

        // Validate blob table layout as blob_table_start and blob_table_offset is read from bootstrap.
        let old_state = self.state.load();
        let blob_table_size = old_state.meta.blob_table_size as u64;
        let blob_table_start = old_state.meta.blob_table_offset;
        let blob_table_range = MetaRange::new(blob_table_start, blob_table_size, false)?;
        if !blob_table_range.is_subrange_of(&md_range) {
            return Err(ebadf!("invalid blob table"));
        }

        // Prefetch the bootstrap file
        readahead(file.as_raw_fd(), 0, len);

        // Load extended blob table if the bootstrap including extended blob table.
        let mut blob_table = RafsV6BlobTable::new();
        let meta = &old_state.meta;
        r.seek(SeekFrom::Start(meta.blob_table_offset))?;
        blob_table.load(r, meta.blob_table_size, meta.chunk_size, meta.flags)?;

        let file_map = FileMapState::new(file, 0, len as usize, false)?;
        let state = DirectMappingState {
            meta: old_state.meta.clone(),
            blob_table,
            map: file_map,
        };

        // Swap new and old DirectMappingState object,
        // the old object will be destroyed when the reference count reaches zero.
        self.state.store(Arc::new(state));

        Ok(())
    }

    // For RafsV6, inode doesn't store detailed chunk info, only a simple RafsV6InodeChunkAddr
    // so we need to use the chunk table at the end of the bootstrap to restore the chunk info of an inode
    fn load_chunk_map(&self) -> Result<HashMap<RafsV6InodeChunkAddr, usize>> {
        let mut chunk_map = HashMap::default();
        let state = self.state.load();
        let size = state.meta.chunk_table_size as usize;
        if size == 0 {
            return Ok(chunk_map);
        }

        let unit_size = size_of::<RafsV5ChunkInfo>();
        if size % unit_size != 0 {
            return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
        }

        for idx in 0..(size / unit_size) {
            let chunk = DirectChunkInfoV6::new(&state, self.clone(), idx)?;
            let mut v6_chunk = RafsV6InodeChunkAddr::new();
            v6_chunk.set_blob_index(chunk.blob_index());
            v6_chunk.set_blob_ci_index(chunk.id());
            v6_chunk.set_block_addr((chunk.uncompressed_offset() / EROFS_BLOCK_SIZE) as u32);
            chunk_map.insert(v6_chunk, idx);
        }

        Ok(chunk_map)
    }
}

impl RafsSuperInodes for DirectSuperBlockV6 {
    fn get_max_ino(&self) -> Inode {
        // Library fuse-rs has limit of underlying file system's maximum inode number.
        // FIXME: So we rafs v6 should record it when building.
        0xff_ffff_ffff_ffff - 1
    }

    /// Find inode offset by ino from inode table and mmap to OndiskInode.
    fn get_inode(&self, ino: Inode, _validate_digest: bool) -> Result<Arc<dyn RafsInode>> {
        let state = self.state.load();
        Ok(Arc::new(self.inode_wrapper(&state, ino)?))
    }

    fn get_extended_inode(
        &self,
        ino: Inode,
        _validate_digest: bool,
    ) -> Result<Arc<dyn RafsInodeExt>> {
        let state = self.state.load();
        if ino == state.meta.root_nid as u64 {
            let inode = self.inode_wrapper_with_info(&state, ino, ino, OsString::from("/"))?;
            return Ok(Arc::new(inode));
        }
        let mut inode = self.inode_wrapper(&state, ino)?;
        if inode.is_dir() {
            inode.get_parent()?;
            inode.get_name(&state)?;
            return Ok(Arc::new(inode));
        }
        Err(enoent!(format!(
            "can't get extended inode for {}, root nid {} {:?}",
            ino, state.meta.root_nid, inode.name
        )))
    }
}

impl RafsSuperBlock for DirectSuperBlockV6 {
    fn load(&mut self, r: &mut RafsIoReader) -> Result<()> {
        self.update_state(r)
    }

    fn update(&self, r: &mut RafsIoReader) -> RafsResult<()> {
        self.update_state(r).map_err(RafsError::SwapBackend)
    }

    fn destroy(&mut self) {
        let state = DirectMappingState::new(&RafsSuperMeta::default());
        self.state.store(Arc::new(state));
    }

    fn get_blob_infos(&self) -> Vec<Arc<BlobInfo>> {
        self.state.load().blob_table.get_all()
    }

    fn root_ino(&self) -> u64 {
        self.info.root_ino
    }

    fn get_chunk_info(&self, idx: usize) -> Result<Arc<dyn BlobChunkInfo>> {
        let state = self.state.load();
        let chunk = DirectChunkInfoV6::new(&state, self.clone(), idx)?;
        Ok(Arc::new(chunk))
    }
}

/// Direct-mapped RAFS v6 inode object.
pub struct OndiskInodeWrapper {
    pub mapping: DirectSuperBlockV6,
    pub offset: usize,
    pub blocks_count: u64,
    parent_inode: Option<Inode>,
    name: Option<OsString>,
}

impl OndiskInodeWrapper {
    fn new(
        state: &Guard<Arc<DirectMappingState>>,
        mapping: DirectSuperBlockV6,
        offset: usize,
    ) -> Result<Self> {
        let inode = DirectSuperBlockV6::disk_inode(state, offset)?;
        let blocks_count = div_round_up(inode.size(), EROFS_BLOCK_SIZE);

        Ok(OndiskInodeWrapper {
            mapping,
            offset,
            blocks_count,
            parent_inode: None,
            name: None,
        })
    }

    fn state(&self) -> Guard<Arc<DirectMappingState>> {
        self.mapping.state.load()
    }

    fn blocks_count(&self) -> u64 {
        self.blocks_count
    }

    fn disk_inode<'a>(
        &self,
        state: &'a Guard<Arc<DirectMappingState>>,
    ) -> &'a dyn RafsV6OndiskInode {
        // Safe to unwrap() because `self.offset` has been validated in new().
        DirectSuperBlockV6::disk_inode(state, self.offset).unwrap()
    }

    fn get_entry<'a>(
        &self,
        state: &'a Guard<Arc<DirectMappingState>>,
        inode: &dyn RafsV6OndiskInode,
        block_index: usize,
        index: usize,
    ) -> RafsResult<&'a RafsV6Dirent> {
        let offset = self.data_block_offset(inode, block_index)?;
        let offset = offset + size_of::<RafsV6Dirent>() * index;
        state
            .map
            .get_ref(offset)
            .map_err(|_e| RafsError::InvalidImageData)
    }

    // `max_entries` indicates the quantity of entries residing in a single block including tail packing.
    // Both `block_index` and `index` start from 0.
    fn entry_name<'a>(
        &self,
        state: &'a Guard<Arc<DirectMappingState>>,
        inode: &dyn RafsV6OndiskInode,
        block_index: usize,
        index: usize,
        max_entries: usize,
    ) -> RafsResult<&'a OsStr> {
        let offset = self.data_block_offset(inode, block_index)?;
        let de = self.get_entry(state, inode, block_index, index)?;
        let buf: &[u8] = if index < max_entries - 1 {
            let next_de = self.get_entry(state, inode, block_index, index + 1)?;
            let (next_de_name_off, de_name_off) = (next_de.e_nameoff, de.e_nameoff);
            let len = next_de.e_nameoff.checked_sub(de.e_nameoff).ok_or_else(|| {
                error!(
                        "nid {} entry index {} block index {} next dir entry {:?} current dir entry {:?}",
                        self.ino(), index, block_index, next_de, de
                    );
                RafsError::IllegalMetaStruct(
                    MetaType::Dir,
                    format!("cur {} next {}", next_de_name_off, de_name_off),
                )
            })?;

            state
                .map
                .get_slice(offset + de.e_nameoff as usize, len as usize)
                .map_err(|_e| RafsError::InvalidImageData)?
        } else {
            let head_de = self.get_entry(state, inode, block_index, 0)?;
            let s = (de.e_nameoff - head_de.e_nameoff) as u64
                + (size_of::<RafsV6Dirent>() * max_entries) as u64;

            // The possible maximum len of the last dirent's file name should be calculated
            // differently depends on whether the dirent is at the last block of the dir file.
            // Because the other blocks should be fully used, while the last may not.
            let len = if div_round_up(self.size(), EROFS_BLOCK_SIZE) as usize == block_index + 1 {
                (self.size() % EROFS_BLOCK_SIZE - s) as usize
            } else {
                (EROFS_BLOCK_SIZE - s) as usize
            };

            let buf: &[u8] = state
                .map
                .get_slice(offset + de.e_nameoff as usize, len)
                .map_err(|_e| RafsError::InvalidImageData)?;
            // Use this trick to temporarily decide entry name's length. Improve this?
            let mut l: usize = 0;
            for i in buf {
                if *i != 0 {
                    l += 1;
                    if len == l {
                        break;
                    }
                } else {
                    break;
                }
            }
            &buf[..l]
        };

        Ok(bytes_to_os_str(buf))
    }

    // COPIED from kernel code:
    // erofs inode data layout (i_format in on-disk inode):
    // 0 - inode plain without inline data A: inode, [xattrs], ... | ... | no-holed data
    // 1 - inode VLE compression B (legacy): inode, [xattrs], extents ... | ...
    // 2 - inode plain with inline data C: inode, [xattrs], last_inline_data, ... | ... | no-holed data
    // 3 - inode compression D: inode, [xattrs], map_header, extents ... | ...
    // 4 - inode chunk-based E: inode, [xattrs], chunk indexes ... | ...
    // 5~7 - reserved
    fn data_block_offset(&self, inode: &dyn RafsV6OndiskInode, index: usize) -> RafsResult<usize> {
        if (inode.format() & (!(((1 << EROFS_I_DATALAYOUT_BITS) - 1) << 1 | EROFS_I_VERSION_BITS)))
            != 0
        {
            return Err(RafsError::Incompatible(inode.format()));
        }

        let layout = inode.format() >> EROFS_I_VERSION_BITS;
        let r = match layout {
            EROFS_INODE_FLAT_PLAIN => {
                // `i_u` points to the Nth block
                (inode.union() as u64 * EROFS_BLOCK_SIZE) as usize
                    + index * EROFS_BLOCK_SIZE as usize
            }
            EROFS_INODE_FLAT_INLINE => {
                if index as u64 != self.blocks_count() - 1 {
                    // `i_u` points to the Nth block
                    (inode.union() as u64 * EROFS_BLOCK_SIZE) as usize
                        + index * EROFS_BLOCK_SIZE as usize
                } else {
                    self.offset as usize + Self::inode_xattr_size(inode) as usize
                }
            }
            _ => return Err(RafsError::InvalidImageData),
        };

        Ok(r)
    }

    fn mode_format_bits(&self) -> u32 {
        let state = self.state();
        let i = self.disk_inode(&state);
        i.mode() as u32 & libc::S_IFMT as u32
    }

    fn make_chunk_io(
        &self,
        state: &Guard<Arc<DirectMappingState>>,
        device: &BlobDevice,
        chunk_addr: &RafsV6InodeChunkAddr,
        content_offset: u32,
        content_len: u32,
        user_io: bool,
    ) -> Option<BlobIoDesc> {
        let blob_index = chunk_addr.blob_index();
        let chunk_index = chunk_addr.blob_ci_index();

        match state.blob_table.get(blob_index) {
            Err(e) => {
                warn!(
                    "failed to get blob with index {} for chunk address {:?}, {}",
                    blob_index, chunk_addr, e
                );
                None
            }
            Ok(blob) => device
                .create_io_chunk(blob.blob_index(), chunk_index)
                .map(|v| BlobIoDesc::new(blob, v, content_offset, content_len, user_io)),
        }
    }

    fn chunk_size(&self) -> u32 {
        self.mapping.info.chunk_size
    }

    fn inode_size(inode: &dyn RafsV6OndiskInode) -> usize {
        if (inode.format() & 1 << EROFS_I_VERSION_BIT) != 0 {
            size_of::<RafsV6InodeExtended>()
        } else {
            size_of::<RafsV6InodeCompact>()
        }
    }

    fn xattr_size(inode: &dyn RafsV6OndiskInode) -> usize {
        // Rafs v6 only supports EROFS inline xattr.
        if inode.xattr_inline_count() > 0 {
            (inode.xattr_inline_count() as usize - 1) * size_of::<RafsV6XattrEntry>()
                + size_of::<RafsV6XattrIbodyHeader>()
        } else {
            0
        }
    }

    fn inode_xattr_size(inode: &dyn RafsV6OndiskInode) -> usize {
        let sz = Self::inode_size(inode) as u64 + Self::xattr_size(inode) as u64;
        round_up(sz, size_of::<RafsV6InodeChunkAddr>() as u64) as usize
    }

    fn chunk_addresses<'a>(
        &self,
        state: &'a Guard<Arc<DirectMappingState>>,
        head_chunk_index: u32,
    ) -> RafsResult<&'a [RafsV6InodeChunkAddr]> {
        let inode = self.disk_inode(state);
        assert_eq!(
            inode.format() >> EROFS_I_VERSION_BITS,
            EROFS_INODE_CHUNK_BASED
        );

        let total_chunk_addresses = div_round_up(self.size(), self.chunk_size() as u64) as u32;
        let offset = self.offset as usize
            + Self::inode_xattr_size(inode)
            + head_chunk_index as usize * size_of::<RafsV6InodeChunkAddr>();
        state
            .map
            .get_slice(offset, (total_chunk_addresses - head_chunk_index) as usize)
            .map_err(|_e| RafsError::InvalidImageData)
    }

    fn find_target_block(
        &self,
        state: &Guard<Arc<DirectMappingState>>,
        name: &OsStr,
    ) -> Result<usize> {
        let inode = self.disk_inode(state);
        if inode.size() == 0 {
            return Err(enoent!());
        }

        let blocks_count = div_round_up(inode.size(), EROFS_BLOCK_SIZE);
        let mut first = 0usize;
        let mut last = (blocks_count - 1) as usize;
        let mut target_block = 0usize;
        while first <= last {
            let pivot = first + ((last - first) >> 1);
            let head_entry = self
                .get_entry(state, inode, pivot, 0)
                .map_err(err_invalidate_data)?;
            let head_name_offset = head_entry.e_nameoff as usize;
            let entries_count = head_name_offset / size_of::<RafsV6Dirent>();
            let h_name = self
                .entry_name(state, inode, pivot, 0, entries_count)
                .map_err(err_invalidate_data)?;
            let t_name = self
                .entry_name(state, inode, pivot, entries_count - 1, entries_count)
                .map_err(err_invalidate_data)?;
            if h_name <= name && t_name >= name {
                target_block = pivot;
                break;
            } else if h_name > name {
                last = pivot - 1;
            } else {
                first = pivot + 1;
            }
        }

        Ok(target_block)
    }

    fn get_parent(&mut self) -> Result<()> {
        assert!(self.is_dir());
        let parent = self.get_child_by_name(OsStr::new(".."))?;
        self.parent_inode = Some(parent.ino());
        Ok(())
    }

    fn get_name(&mut self, state: &Guard<Arc<DirectMappingState>>) -> Result<()> {
        assert!(self.is_dir());
        let cur_ino = self.ino();
        if cur_ino == self.mapping.info.root_ino {
            self.name = Some(OsString::from(""));
        } else {
            let parent = self.mapping.inode_wrapper(state, self.parent())?;
            parent.walk_children_inodes(
                0,
                &mut |_inode: Option<Arc<dyn RafsInode>>, name: OsString, ino, _offset| {
                    if cur_ino == ino {
                        self.name = Some(name);
                        return Ok(RafsInodeWalkAction::Break);
                    }
                    Ok(RafsInodeWalkAction::Continue)
                },
            )?;
            assert!(self.name.is_some());
        }

        Ok(())
    }
}

impl RafsInode for OndiskInodeWrapper {
    fn validate(&self, _inode_count: u64, _chunk_size: u64) -> Result<()> {
        let state = self.state();
        let inode = self.disk_inode(&state);
        let max_inode = self.mapping.get_max_ino();

        if self.ino() > max_inode
            || inode.nlink() == 0
            || self.get_name_size() as usize > (RAFS_MAX_NAME + 1)
        {
            return Err(ebadf!(format!(
                "inode validation failure, inode {:?}",
                inode
            )));
        }

        if self.is_reg() {
            if state.meta.is_chunk_dict() {
                // chunk-dict doesn't support chunk_count check
                return Err(std::io::Error::from_raw_os_error(libc::EOPNOTSUPP));
            }
            let chunks = div_round_up(self.size(), self.chunk_size() as u64) as usize;
            let size = OndiskInodeWrapper::inode_xattr_size(inode)
                + chunks * size_of::<RafsV6InodeChunkAddr>();
            state.map.validate_range(self.offset, size)?;
        } else if self.is_dir() {
            if self.get_child_count() as u64 >= max_inode {
                return Err(einval!("invalid directory"));
            }
            let xattr_size = Self::xattr_size(inode) as usize;
            let size = Self::inode_size(inode) + xattr_size;
            state.map.validate_range(self.offset, size)?;
        } else if self.is_symlink() && self.size() == 0 {
            return Err(einval!("invalid symlink target"));
        }
        Ok(())
    }

    fn alloc_bio_vecs(
        &self,
        device: &BlobDevice,
        offset: u64,
        size: usize,
        user_io: bool,
    ) -> Result<Vec<BlobIoVec>> {
        let state = self.state();
        let chunk_size = self.chunk_size();
        let head_chunk_index = offset / chunk_size as u64;
        let mut vec: Vec<BlobIoVec> = Vec::new();
        let chunks = self
            .chunk_addresses(&state, head_chunk_index as u32)
            .map_err(err_invalidate_data)?;
        if chunks.is_empty() {
            return Ok(vec);
        }

        let content_offset = (offset % chunk_size as u64) as u32;
        let mut left = std::cmp::min(self.size() - offset, size as u64) as u32;
        let mut content_len = std::cmp::min(chunk_size - content_offset, left);
        let desc = self
            .make_chunk_io(
                &state,
                device,
                &chunks[0],
                content_offset,
                content_len,
                user_io,
            )
            .ok_or_else(|| einval!("failed to get chunk information"))?;

        let mut descs = BlobIoVec::new(desc.blob.clone());
        descs.push(desc);
        left -= content_len;
        if left != 0 {
            // Handle the rest of chunks since they shares the same content length = 0.
            for c in chunks.iter().skip(1) {
                content_len = std::cmp::min(chunk_size, left);
                let desc = self
                    .make_chunk_io(&state, device, c, 0, content_len, user_io)
                    .ok_or_else(|| einval!("failed to get chunk information"))?;
                if desc.blob.blob_index() != descs.blob_index() {
                    vec.push(descs);
                    descs = BlobIoVec::new(desc.blob.clone());
                }
                descs.push(desc);
                left -= content_len;
                if left == 0 {
                    break;
                }
            }
        }
        if !descs.is_empty() {
            vec.push(descs)
        }
        assert_eq!(left, 0);

        Ok(vec)
    }

    fn collect_descendants_inodes(
        &self,
        descendants: &mut Vec<Arc<dyn RafsInode>>,
    ) -> Result<usize> {
        if !self.is_dir() {
            return Err(enotdir!());
        }

        let mut child_dirs: Vec<Arc<dyn RafsInode>> = Vec::new();
        let callback = &mut |inode: Option<Arc<dyn RafsInode>>, name: OsString, _ino, _offset| {
            if let Some(child_inode) = inode {
                if child_inode.is_dir() {
                    // EROFS packs dot and dotdot, so skip them two.
                    if name != "." && name != ".." {
                        child_dirs.push(child_inode);
                    }
                } else if !child_inode.is_empty_size() && child_inode.is_reg() {
                    descendants.push(child_inode);
                }
                Ok(RafsInodeWalkAction::Continue)
            } else {
                Ok(RafsInodeWalkAction::Continue)
            }
        };

        self.walk_children_inodes(0, callback)?;
        for d in child_dirs {
            d.collect_descendants_inodes(descendants)?;
        }

        Ok(0)
    }

    fn get_entry(&self) -> Entry {
        Entry {
            attr: self.get_attr().into(),
            inode: self.ino(),
            generation: 0,
            attr_timeout: self.mapping.info.attr_timeout,
            entry_timeout: self.mapping.info.entry_timeout,
            ..Default::default()
        }
    }

    fn get_attr(&self) -> Attr {
        let state = self.state();
        let inode = self.disk_inode(&state);

        Attr {
            ino: self.ino(),
            size: inode.size(),
            mode: inode.mode() as u32,
            nlink: inode.nlink(),
            blocks: div_round_up(inode.size(), 512),
            uid: inode.ugid().0,
            gid: inode.ugid().1,
            mtime: inode.mtime_s_ns().0,
            mtimensec: inode.mtime_s_ns().1,
            blksize: RAFS_ATTR_BLOCK_SIZE,
            rdev: inode.rdev(),
            ..Default::default()
        }
    }

    fn ino(&self) -> u64 {
        (self.offset - self.mapping.info.meta_offset) as u64 / EROFS_INODE_SLOT_SIZE as u64
    }

    /// Get real device number of the inode.
    fn rdev(&self) -> u32 {
        let state = self.state();
        self.disk_inode(&state).union()
    }

    /// Get project id associated with the inode.
    fn projid(&self) -> u32 {
        0
    }

    fn is_dir(&self) -> bool {
        self.mode_format_bits() == libc::S_IFDIR as u32
    }

    /// Check whether the inode is a symlink.
    fn is_symlink(&self) -> bool {
        self.mode_format_bits() == libc::S_IFLNK as u32
    }

    /// Check whether the inode is a regular file.
    fn is_reg(&self) -> bool {
        self.mode_format_bits() == libc::S_IFREG as u32
    }

    /// Check whether the inode is a hardlink.
    fn is_hardlink(&self) -> bool {
        let state = self.state();
        let inode = self.disk_inode(&state);
        inode.nlink() > 1 && self.is_reg()
    }

    /// Check whether the inode has extended attributes.
    fn has_xattr(&self) -> bool {
        let state = self.state();
        self.disk_inode(&state).xattr_inline_count() > 0
    }

    fn get_xattr(&self, name: &OsStr) -> Result<Option<XattrValue>> {
        let state = self.state();
        let inode = self.disk_inode(&state);
        let total = inode.xattr_inline_count();
        if total == 0 {
            return Ok(None);
        }

        let mut offset =
            self.offset + Self::inode_size(inode) + size_of::<RafsV6XattrIbodyHeader>();
        let mut remaining = (total - 1) as usize * size_of::<RafsV6XattrEntry>();
        while remaining > 0 {
            let e: &RafsV6XattrEntry = state.map.get_ref(offset)?;
            let mut xa_name = recover_namespace(e.name_index())?;
            let suffix: &[u8] = state.map.get_slice(
                offset + size_of::<RafsV6XattrEntry>(),
                e.name_len() as usize,
            )?;
            xa_name.push(OsStr::from_bytes(suffix));
            if xa_name == name {
                let data: &[u8] = state.map.get_slice(
                    offset + size_of::<RafsV6XattrEntry>() + e.name_len() as usize,
                    e.value_size() as usize,
                )?;
                return Ok(Some(data.to_vec()));
            }

            let mut s = e.name_len() + e.value_size() + size_of::<RafsV6XattrEntry>() as u32;
            s = round_up(s as u64, size_of::<RafsV6XattrEntry>() as u64) as u32;
            remaining -= s as usize;
            offset += s as usize;
        }

        Ok(None)
    }

    fn get_xattrs(&self) -> Result<Vec<XattrName>> {
        let state = self.state();
        let inode = self.disk_inode(&state);
        let mut xattrs = Vec::new();
        let total = inode.xattr_inline_count();
        if total == 0 {
            return Ok(xattrs);
        }

        let mut offset =
            self.offset + Self::inode_size(inode) + size_of::<RafsV6XattrIbodyHeader>();
        let mut remaining = (total - 1) as usize * size_of::<RafsV6XattrEntry>();
        while remaining > 0 {
            let e: &RafsV6XattrEntry = state.map.get_ref(offset)?;
            let name: &[u8] = state.map.get_slice(
                offset + size_of::<RafsV6XattrEntry>(),
                e.name_len() as usize,
            )?;
            let ns = recover_namespace(e.name_index())?;
            let mut xa = ns.into_vec();
            xa.extend_from_slice(name);
            xattrs.push(xa);

            let mut s = e.name_len() + e.value_size() + size_of::<RafsV6XattrEntry>() as u32;
            s = round_up(s as u64, size_of::<RafsV6XattrEntry>() as u64) as u32;
            offset += s as usize;
            remaining -= s as usize;
        }

        Ok(xattrs)
    }

    /// Get symlink target of the inode.
    ///
    /// # Safety
    /// It depends on Self::validate() to ensure valid memory layout.
    fn get_symlink(&self) -> Result<OsString> {
        let state = self.state();
        let inode = self.disk_inode(&state);
        let offset = self
            .data_block_offset(inode, 0)
            .map_err(err_invalidate_data)?;
        let buf: &[u8] = state.map.get_slice(offset, inode.size() as usize)?;
        Ok(bytes_to_os_str(buf).to_os_string())
    }

    fn get_symlink_size(&self) -> u16 {
        let state = self.state();
        let inode = self.disk_inode(&state);
        inode.size() as u16
    }

    fn walk_children_inodes(&self, entry_offset: u64, handler: RafsInodeWalkHandler) -> Result<()> {
        let state = self.state();
        let inode = self.disk_inode(&state);
        if inode.size() == 0 {
            return Err(enoent!());
        }

        let blocks_count = div_round_up(inode.size(), EROFS_BLOCK_SIZE);
        let mut cur_offset = entry_offset;
        let mut skipped = entry_offset;
        trace!(
            "Total blocks count {} skipped {} current offset {} nid {} inode {:?}",
            blocks_count,
            skipped,
            cur_offset,
            self.ino(),
            inode,
        );

        for i in 0..blocks_count as usize {
            let head_entry = self
                .get_entry(&state, inode, i, 0)
                .map_err(err_invalidate_data)?;
            let name_offset = head_entry.e_nameoff;
            let entries_count = name_offset as usize / size_of::<RafsV6Dirent>();

            for j in 0..entries_count {
                let de = self
                    .get_entry(&state, inode, i, j)
                    .map_err(err_invalidate_data)?;
                let name = self
                    .entry_name(&state, inode, i, j, entries_count)
                    .map_err(err_invalidate_data)?;

                // Skip specified offset
                if skipped != 0 {
                    skipped -= 1;
                    continue;
                }

                let nid = de.e_nid;
                let inode = Arc::new(self.mapping.inode_wrapper_with_info(
                    &state,
                    nid,
                    self.ino(),
                    OsString::from(name),
                )?) as Arc<dyn RafsInode>;
                cur_offset += 1;
                match handler(Some(inode), name.to_os_string(), nid, cur_offset) {
                    // Break returned by handler indicates that there is not enough buffer of readdir for entries inreaddir,
                    // such that it has to return. because this is a nested loop,
                    // using break can only jump out of the internal loop, there is no way to jump out of the whole loop.
                    Ok(RafsInodeWalkAction::Break) => return Ok(()),
                    Ok(RafsInodeWalkAction::Continue) => continue,
                    Err(e) => return Err(e),
                };
            }
        }

        Ok(())
    }

    /// Get the child with the specified name.
    ///
    /// # Safety
    /// It depends on Self::validate() to ensure valid memory layout.
    fn get_child_by_name(&self, name: &OsStr) -> Result<Arc<dyn RafsInodeExt>> {
        let state = self.state();
        let inode = self.disk_inode(&state);
        if let Ok(target_block) = self.find_target_block(&state, name) {
            let head_entry = self
                .get_entry(&state, inode, target_block, 0)
                .map_err(err_invalidate_data)?;
            let head_name_offset = head_entry.e_nameoff as usize;
            let entries_count = head_name_offset / size_of::<RafsV6Dirent>();

            let mut first = 0;
            let mut last = entries_count - 1;
            while first <= last {
                let pivot = first + ((last - first) >> 1);
                let de = self
                    .get_entry(&state, inode, target_block, pivot)
                    .map_err(err_invalidate_data)?;
                let d_name = self
                    .entry_name(&state, inode, target_block, pivot, entries_count)
                    .map_err(err_invalidate_data)?;
                match d_name.cmp(name) {
                    Ordering::Equal => {
                        let inode = self.mapping.inode_wrapper_with_info(
                            &state,
                            de.e_nid,
                            self.ino(),
                            OsString::from(name),
                        )?;
                        return Ok(Arc::new(inode));
                    }
                    Ordering::Less => first = pivot + 1,
                    Ordering::Greater => last = pivot - 1,
                }
            }
        }
        Err(enoent!())
    }

    /// Get the child with the specified index.
    ///
    /// # Safety
    /// It depends on Self::validate() to ensure valid memory layout.
    /// `idx` is the number of child files in line. So we can keep the term `idx`
    /// in super crate and keep it consistent with layout v5.
    fn get_child_by_index(&self, idx: u32) -> Result<Arc<dyn RafsInodeExt>> {
        let state = self.state();
        let inode = self.disk_inode(&state);
        if !self.is_dir() {
            return Err(einval!("inode is not a directory"));
        }

        let blocks_count = div_round_up(inode.size(), EROFS_BLOCK_SIZE);
        let mut cur_idx = 0u32;
        for i in 0..blocks_count as usize {
            let head_entry = self
                .get_entry(&state, inode, i, 0)
                .map_err(err_invalidate_data)
                .unwrap();
            let name_offset = head_entry.e_nameoff;
            let entries_count = name_offset as usize / size_of::<RafsV6Dirent>();

            for j in 0..entries_count {
                let de = self
                    .get_entry(&state, inode, i, j)
                    .map_err(err_invalidate_data)?;
                let name = self
                    .entry_name(&state, inode, i, j, entries_count)
                    .map_err(err_invalidate_data)?;
                if name == "." || name == ".." {
                    continue;
                }
                if cur_idx == idx {
                    let inode = self.mapping.inode_wrapper_with_info(
                        &state,
                        de.e_nid,
                        self.ino(),
                        OsString::from(name),
                    )?;
                    return Ok(Arc::new(inode));
                }
                cur_idx += 1;
            }
        }

        Err(enoent!("invalid child index"))
    }

    #[inline]
    fn get_child_count(&self) -> u32 {
        // For regular file, return chunk info count.
        if !self.is_dir() {
            return div_round_up(self.size(), self.chunk_size() as u64) as u32;
        }

        let mut child_cnt = 0;
        let state = self.state();
        let inode = self.disk_inode(&state);
        let blocks_count = div_round_up(self.size(), EROFS_BLOCK_SIZE);
        for i in 0..blocks_count as usize {
            let head_entry = self
                .get_entry(&state, inode, i, 0)
                .map_err(err_invalidate_data)
                .unwrap();
            let name_offset = head_entry.e_nameoff;
            let entries_count = name_offset / size_of::<RafsV6Dirent>() as u16;

            child_cnt += entries_count as u32;
        }
        // Skip DOT and DOTDOT
        child_cnt - 2
    }

    fn get_child_index(&self) -> Result<u32> {
        Ok(0)
    }

    /// Get data size of the inode.
    fn size(&self) -> u64 {
        let state = self.state();
        let i = self.disk_inode(&state);
        i.size()
    }

    #[inline]
    fn get_chunk_count(&self) -> u32 {
        self.get_child_count()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl RafsInodeExt for OndiskInodeWrapper {
    fn as_inode(&self) -> &dyn RafsInode {
        self
    }

    /// Get inode number of the parent directory.
    fn parent(&self) -> u64 {
        assert!(self.parent_inode.is_some());
        self.parent_inode.unwrap()
    }

    /// Get name of the inode.
    ///
    /// # Safety
    /// It depends on Self::validate() to ensure valid memory layout.
    fn name(&self) -> OsString {
        assert!(self.name.is_some());
        self.name.clone().unwrap_or_else(OsString::new)
    }

    /// Get file name size of the inode.
    fn get_name_size(&self) -> u16 {
        self.name().len() as u16
    }

    // RafsV5 flags, not used by v6, return 0
    fn flags(&self) -> u64 {
        0
    }

    fn get_digest(&self) -> RafsDigest {
        RafsDigest::default()
    }

    /// Get chunk information with index `idx`
    ///
    /// # Safety
    /// It depends on Self::validate() to ensure valid memory layout.
    fn get_chunk_info(&self, idx: u32) -> Result<Arc<dyn BlobChunkInfo>> {
        let state = self.state();
        let inode = self.disk_inode(&state);
        if !self.is_reg() || idx >= self.get_chunk_count() {
            return Err(enoent!("invalid chunk info"));
        }

        let offset = self.offset as usize
            + OndiskInodeWrapper::inode_xattr_size(inode)
            + (idx as usize * size_of::<RafsV6InodeChunkAddr>());
        let chunk_addr = state.map.get_ref::<RafsV6InodeChunkAddr>(offset)?;
        let mut chunk_map = self.mapping.info.chunk_map.lock().unwrap();
        if chunk_map.is_none() {
            *chunk_map = Some(self.mapping.load_chunk_map()?);
        }
        match chunk_map.as_ref().unwrap().get(chunk_addr) {
            None => Err(enoent!("failed to get chunk info")),
            Some(idx) => DirectChunkInfoV6::new(&state, self.mapping.clone(), *idx)
                .map(|v| Arc::new(v) as Arc<dyn BlobChunkInfo>),
        }
    }
}

/// Impl get accessor for chunkinfo object.
macro_rules! impl_chunkinfo_getter {
    ($G: ident, $U: ty) => {
        #[inline]
        fn $G(&self) -> $U {
            let state = self.state();

            self.v5_chunk(&state).$G
        }
    };
}

/// RAFS v6 chunk information object.
pub struct DirectChunkInfoV6 {
    mapping: DirectSuperBlockV6,
    offset: usize,
    digest: RafsDigest,
}

// This is *direct* metadata mode in-memory chunk info object.
impl DirectChunkInfoV6 {
    fn new(state: &DirectMappingState, mapping: DirectSuperBlockV6, idx: usize) -> Result<Self> {
        let unit_size = size_of::<RafsV5ChunkInfo>();
        let offset = state.meta.chunk_table_offset as usize + idx * unit_size;
        let chunk_tbl_end = state.meta.chunk_table_offset + state.meta.chunk_table_size;
        if (offset as u64) < state.meta.chunk_table_offset
            || (offset + unit_size) as u64 > chunk_tbl_end
        {
            return Err(einval!(format!(
                "invalid chunk offset {} chunk table {} {}",
                offset, state.meta.chunk_table_offset, state.meta.chunk_table_size
            )));
        }
        let chunk = state.map.get_ref::<RafsV5ChunkInfo>(offset)?;
        Ok(Self {
            mapping,
            offset,
            digest: chunk.block_id,
        })
    }

    #[inline]
    fn state(&self) -> Guard<Arc<DirectMappingState>> {
        self.mapping.state.load()
    }

    /// Dereference the underlying OndiskChunkInfo object.
    ///
    /// # Safety
    /// The OndiskChunkInfoWrapper could only be constructed from a valid OndiskChunkInfo pointer,
    /// so it's safe to dereference the underlying OndiskChunkInfo object.
    fn v5_chunk<'a>(&self, state: &'a DirectMappingState) -> &'a RafsV5ChunkInfo {
        // Safe to unwrap() because we have validated the offset in DirectChunkInfoV6::new().
        state.map.get_ref::<RafsV5ChunkInfo>(self.offset).unwrap()
    }
}

impl BlobChunkInfo for DirectChunkInfoV6 {
    fn chunk_id(&self) -> &RafsDigest {
        &self.digest
    }

    fn id(&self) -> u32 {
        self.index()
    }

    fn is_compressed(&self) -> bool {
        let state = self.state();
        self.v5_chunk(&state)
            .flags
            .contains(BlobChunkFlags::COMPRESSED)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    impl_chunkinfo_getter!(blob_index, u32);
    impl_chunkinfo_getter!(compressed_offset, u64);
    impl_chunkinfo_getter!(compressed_size, u32);
    impl_chunkinfo_getter!(uncompressed_offset, u64);
    impl_chunkinfo_getter!(uncompressed_size, u32);
}

impl BlobV5ChunkInfo for DirectChunkInfoV6 {
    fn as_base(&self) -> &dyn BlobChunkInfo {
        self
    }

    impl_chunkinfo_getter!(index, u32);
    impl_chunkinfo_getter!(file_offset, u64);
    impl_chunkinfo_getter!(flags, BlobChunkFlags);
}
