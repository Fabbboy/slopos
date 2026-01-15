# VFS Layer Implementation Plan

> **Created**: January 2026
> **Status**: ✅ COMPLETE
> **Priority**: Stage 1 - Critical Path (blocks exec(), ramfs, devfs)
> **Completed**: January 2026 - All 5 phases implemented and integrated

---

## Executive Summary

SlopOS currently has a complete Ext2 filesystem implementation but lacks a **Virtual File System (VFS) abstraction layer** that would allow:
- Multiple filesystem types (ext2, ramfs, devfs, procfs)
- Mount points at arbitrary paths (`/tmp`, `/dev`, `/proc`)
- Unified file operations across different backends
- Device special files (`/dev/null`, `/dev/zero`, `/dev/random`)

This plan outlines a phased approach to implement a VFS layer on top of the existing infrastructure.

---

## Current State Analysis

### What We Have (Solid Foundation)

| Component | Location | Status |
|-----------|----------|--------|
| Ext2 filesystem | `fs/src/ext2.rs` | Complete |
| Block device trait | `fs/src/blockdev.rs` | Complete |
| File descriptor tables | `fs/src/fileio.rs` | Complete (32 fds/process) |
| File syscalls | `core/src/syscall/fs.rs` | Complete |
| Path resolution | `fs/src/ext2.rs` | Ext2-only |
| Boot mounting | `boot/src/boot_services.rs` | Hardcoded to ext2 |

### What's Missing (The Gap)

1. **Generic Filesystem Trait** - No `trait FileSystem` to unify ext2/ramfs/devfs
2. **Mount Table** - No tracking of what's mounted where
3. **VFS Path Resolution** - Can't traverse mount boundaries
4. **Inode Abstraction** - `Ext2Inode` is concrete, not trait-based
5. **Special Files** - No device nodes, no `/dev/*`

---

## Architecture Design

### Layer Diagram

```
┌─────────────────────────────────────────────────────────────────┐
│                         Userland                                 │
│              sys_fs_open(), sys_fs_read(), etc.                 │
├─────────────────────────────────────────────────────────────────┤
│                      VFS Layer (NEW)                            │
│  ┌─────────────┬─────────────┬─────────────┬─────────────────┐  │
│  │ Mount Table │ Path Resolve│ File Handle │ Inode Cache     │  │
│  └─────────────┴─────────────┴─────────────┴─────────────────┘  │
├─────────────────────────────────────────────────────────────────┤
│                   FileSystem Trait Impls                        │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────────┐    │
│  │  Ext2Fs  │  │  RamFs   │  │  DevFs   │  │  (Future)    │    │
│  └──────────┘  └──────────┘  └──────────┘  └──────────────┘    │
├─────────────────────────────────────────────────────────────────┤
│                   Block Device Layer                            │
│  ┌──────────────────┐  ┌──────────────────┐                    │
│  │ MemoryBlockDevice│  │ VirtioBlockDevice│                    │
│  └──────────────────┘  └──────────────────┘                    │
└─────────────────────────────────────────────────────────────────┘
```

### Core Traits

```rust
// fs/src/vfs/traits.rs

/// Unique identifier for an inode within a filesystem
pub type InodeId = u64;

/// File type enumeration
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    Regular,
    Directory,
    CharDevice,
    BlockDevice,
    Symlink,
    Pipe,
    Socket,
}

/// Metadata about a file/directory
#[derive(Debug, Clone)]
pub struct FileStat {
    pub inode: InodeId,
    pub file_type: FileType,
    pub size: u64,
    pub mode: u16,       // Unix permissions
    pub nlink: u32,      // Hard link count
    pub uid: u32,
    pub gid: u32,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
    pub dev_major: u32,  // For device files
    pub dev_minor: u32,
}

/// Result type for VFS operations
pub type VfsResult<T> = Result<T, VfsError>;

/// Errors that can occur in VFS operations
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VfsError {
    NotFound,
    NotDirectory,
    NotFile,
    IsDirectory,
    PermissionDenied,
    ReadOnly,
    NoSpace,
    IoError,
    InvalidPath,
    AlreadyExists,
    NotEmpty,
    CrossDevice,
    NotSupported,
    TooManyLinks,
    NameTooLong,
}

/// A filesystem implementation
pub trait FileSystem: Send + Sync {
    /// Get the name of this filesystem type (e.g., "ext2", "ramfs")
    fn name(&self) -> &'static str;
    
    /// Get the root inode of this filesystem
    fn root_inode(&self) -> InodeId;
    
    /// Look up a child entry in a directory
    fn lookup(&self, parent: InodeId, name: &[u8]) -> VfsResult<InodeId>;
    
    /// Get metadata for an inode
    fn stat(&self, inode: InodeId) -> VfsResult<FileStat>;
    
    /// Read from a file
    fn read(&self, inode: InodeId, offset: u64, buf: &mut [u8]) -> VfsResult<usize>;
    
    /// Write to a file
    fn write(&self, inode: InodeId, offset: u64, buf: &[u8]) -> VfsResult<usize>;
    
    /// Create a file in a directory
    fn create(&self, parent: InodeId, name: &[u8], file_type: FileType) -> VfsResult<InodeId>;
    
    /// Remove an entry from a directory
    fn unlink(&self, parent: InodeId, name: &[u8]) -> VfsResult<()>;
    
    /// Read directory entries
    fn readdir(&self, inode: InodeId, offset: usize, callback: &mut dyn FnMut(&[u8], InodeId, FileType) -> bool) -> VfsResult<usize>;
    
    /// Sync filesystem to backing store
    fn sync(&self) -> VfsResult<()>;
}
```

### Mount Table Design

```rust
// fs/src/vfs/mount.rs

use alloc::vec::Vec;
use alloc::sync::Arc;
use slopos_lib::IrqMutex;

/// A mounted filesystem
pub struct MountPoint {
    /// Path where this filesystem is mounted (e.g., "/dev", "/tmp")
    pub path: [u8; 256],
    pub path_len: usize,
    /// The filesystem implementation
    pub fs: Arc<dyn FileSystem>,
    /// Flags (read-only, noexec, etc.)
    pub flags: u32,
}

/// Global mount table
pub struct MountTable {
    mounts: Vec<MountPoint>,
}

impl MountTable {
    pub const fn new() -> Self {
        Self { mounts: Vec::new() }
    }
    
    /// Mount a filesystem at the given path
    pub fn mount(&mut self, path: &[u8], fs: Arc<dyn FileSystem>, flags: u32) -> VfsResult<()>;
    
    /// Unmount the filesystem at the given path
    pub fn unmount(&mut self, path: &[u8]) -> VfsResult<()>;
    
    /// Find the filesystem and relative path for a given absolute path
    /// Returns (filesystem, remaining_path)
    pub fn resolve(&self, path: &[u8]) -> VfsResult<(&Arc<dyn FileSystem>, &[u8])>;
}

// Global mount table instance
static MOUNT_TABLE: IrqMutex<MountTable> = IrqMutex::new(MountTable::new());
```

### VFS Path Resolution

```rust
// fs/src/vfs/path.rs

/// Resolve an absolute path to a (filesystem, inode) pair
/// Handles mount point traversal
pub fn resolve_path(path: &[u8]) -> VfsResult<(Arc<dyn FileSystem>, InodeId)> {
    if path.is_empty() || path[0] != b'/' {
        return Err(VfsError::InvalidPath);
    }
    
    let mount_table = MOUNT_TABLE.lock();
    
    // Find the deepest mount point that matches this path
    let (fs, relative_path) = mount_table.resolve(path)?;
    
    // Start from the filesystem's root inode
    let mut current_inode = fs.root_inode();
    
    // Walk each path component
    for component in path_components(relative_path) {
        // Skip "." (current directory)
        if component == b"." {
            continue;
        }
        
        // Handle ".." (parent directory) - may need to cross mount boundaries
        if component == b".." {
            // TODO: Handle mount point traversal for ..
            current_inode = fs.lookup(current_inode, b"..")?;
            continue;
        }
        
        // Look up the component in the current directory
        current_inode = fs.lookup(current_inode, component)?;
        
        // Check if this is a mount point
        // TODO: Check mount table for nested mounts
    }
    
    Ok((Arc::clone(fs), current_inode))
}
```

---

## Implementation Phases

### Phase 1: Core VFS Traits (Week 1) ✅ COMPLETE

**Goal**: Define the abstraction layer without breaking existing functionality

| Task | File | Description | Status |
|------|------|-------------|--------|
| 1.1 | `fs/src/vfs/mod.rs` | Create VFS module structure | ✅ |
| 1.2 | `fs/src/vfs/traits.rs` | Define `FileSystem`, `FileStat`, `VfsError` | ✅ |
| 1.3 | `fs/src/vfs/mount.rs` | Implement `MountTable` with basic mount/unmount | ✅ |
| 1.4 | `fs/src/vfs/path.rs` | Path resolution with mount awareness | ✅ |

**Deliverable**: VFS traits compile, no runtime changes yet

### Phase 2: Ext2 Adapter (Week 1-2) ✅ COMPLETE

**Goal**: Wrap existing Ext2 implementation in VFS trait

| Task | File | Description | Status |
|------|------|-------------|--------|
| 2.1 | `fs/src/ext2_vfs.rs` | Create `Ext2VfsAdapter` implementing `FileSystem` | ✅ |
| 2.2 | `fs/src/ext2_vfs.rs` | Translate `Ext2Error` to `VfsError` | ✅ |
| 2.3 | `fs/src/vfs/ops.rs` | Create VFS-level file operations | ✅ |
| 2.4 | `boot/src/boot_services.rs` | Mount ext2 via VFS at boot | ✅ |

**Deliverable**: Existing functionality works through VFS layer

### Phase 3: RamFS Implementation (Week 2) ✅ COMPLETE

**Goal**: First non-ext2 filesystem for `/tmp`

| Task | File | Description | Status |
|------|------|-------------|--------|
| 3.1 | `fs/src/ramfs/mod.rs` | In-memory filesystem implementation | ✅ |
| 3.2 | `fs/src/ramfs/mod.rs` | RamFS inode with data storage (combined) | ✅ |
| 3.3 | `fs/src/ramfs/mod.rs` | Directory entry management (combined) | ✅ |
| 3.4 | `fs/src/vfs/init.rs` | Mount ramfs at `/tmp` | ✅ |

**Implementation Notes:**
- Fixed-size design: 64 inodes × 4KB data each, 32 dir entries per directory
- Uses `new_const()` for static initialization (no heap at mount time)
- Supports create/read/write/unlink/readdir operations

**RamFS Design:**
```rust
struct RamFsInode {
    id: InodeId,
    file_type: FileType,
    data: Vec<u8>,           // File contents (for regular files)
    children: Vec<DirEntry>, // Directory entries (for directories)
    stat: FileStat,
}

struct RamFs {
    inodes: IrqMutex<Vec<RamFsInode>>,
    next_inode: AtomicU64,
}
```

**Deliverable**: `/tmp` works as a separate mount

### Phase 4: DevFS Implementation (Week 3) ✅ COMPLETE

**Goal**: Device special files at `/dev`

| Task | File | Description | Status |
|------|------|-------------|--------|
| 4.1 | `fs/src/devfs/mod.rs` | DevFS skeleton | ✅ |
| 4.2 | `fs/src/devfs/mod.rs` | `/dev/null` - reads return 0, writes succeed | ✅ |
| 4.3 | `fs/src/devfs/mod.rs` | `/dev/zero` - reads return zeros | ✅ |
| 4.4 | `fs/src/devfs/mod.rs` | `/dev/random` - reads return random bytes | ✅ |
| 4.5 | `fs/src/devfs/mod.rs` | `/dev/console` - maps to serial/framebuffer | ✅ |
| 4.6 | `fs/src/vfs/init.rs` | Mount devfs at `/dev` | ✅ |

**Implementation Notes:**
- All devices implemented in single `mod.rs` file for simplicity
- XorShift64 PRNG for `/dev/random`
- Read-only directory structure with character device nodes

**DevFS Design:**
```rust
trait DeviceOps: Send + Sync {
    fn read(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize>;
    fn write(&self, offset: u64, buf: &[u8]) -> VfsResult<usize>;
}

struct NullDevice;
impl DeviceOps for NullDevice {
    fn read(&self, _: u64, _: &mut [u8]) -> VfsResult<usize> { Ok(0) }
    fn write(&self, _: u64, buf: &[u8]) -> VfsResult<usize> { Ok(buf.len()) }
}

struct ZeroDevice;
impl DeviceOps for ZeroDevice {
    fn read(&self, _: u64, buf: &mut [u8]) -> VfsResult<usize> {
        buf.fill(0);
        Ok(buf.len())
    }
    fn write(&self, _: u64, buf: &[u8]) -> VfsResult<usize> { Ok(buf.len()) }
}
```

**Deliverable**: `/dev/null`, `/dev/zero`, `/dev/random` work

### Phase 5: Syscall Integration (Week 3-4) ✅ COMPLETE

**Goal**: Route syscalls through VFS layer

| Task | File | Description | Status |
|------|------|-------------|--------|
| 5.1 | `fs/src/vfs_fileio.rs` | VFS-level file handle with position tracking | ✅ |
| 5.2 | `fs/src/vfs_fileio.rs` | New VFS-based file descriptor table | ✅ |
| 5.3 | `core/src/syscall/fs.rs` | Update syscall handlers to use VFS | ✅ |
| 5.4 | `make test` | Verify all file operations work | ✅ |

**Implementation Notes:**
- Created new `vfs_fileio.rs` parallel to legacy `fileio.rs`
- Syscalls switched from `file_*` to `vfs_file_*` functions
- File descriptors store both inode and filesystem pointer for cross-FS support
- Legacy `fileio.rs` and `ext2_state.rs` kept for backward compatibility (can be removed later)

**Deliverable**: Full VFS integration, backwards compatible

---

## File Structure

```
fs/src/
├── lib.rs              # Module exports (update)
├── blockdev.rs         # Block device trait (keep)
├── ext2.rs             # Ext2 implementation (keep)
├── ext2_state.rs       # Ext2 global state (keep)
├── ext2_vfs.rs         # NEW: Ext2 VFS adapter
├── fileio.rs           # File descriptor tables (update)
├── vfs/
│   ├── mod.rs          # VFS module root
│   ├── traits.rs       # FileSystem trait, VfsError
│   ├── mount.rs        # Mount table management
│   ├── path.rs         # Path resolution
│   ├── file.rs         # VFS file handle
│   └── ops.rs          # High-level VFS operations
├── ramfs/
│   ├── mod.rs          # RamFS filesystem
│   ├── inode.rs        # In-memory inode
│   └── dir.rs          # Directory operations
└── devfs/
    ├── mod.rs          # DevFS filesystem
    ├── null.rs         # /dev/null
    ├── zero.rs         # /dev/zero
    ├── random.rs       # /dev/random
    └── console.rs      # /dev/console
```

---

## Migration Strategy

### Breaking Changes Expected

SlopOS is **pre-alpha**. No backwards compatibility is maintained. The VFS implementation will be a **big breaking change** that requires updates across the entire codebase.

#### What Will Change

| Component | Current | After VFS |
|-----------|---------|-----------|
| `fs/src/fileio.rs` | Direct ext2 calls | Replaced with VFS ops |
| `fs/src/ext2_state.rs` | Global ext2 state | Removed (ext2 becomes a plugin) |
| `core/src/syscall/fs.rs` | Calls fileio directly | Calls VFS layer |
| `userland/src/syscall.rs` | May need signature updates | Update as needed |
| `userland/src/shell.rs` | Uses old file APIs | Update to new APIs |
| `boot/src/boot_services.rs` | Hardcoded ext2 init | Use VFS mount system |

#### Migration Approach

1. **Implement VFS layer first** (Phases 1-4)
2. **Rip out old fileio.rs** - Replace entirely with VFS-based implementation
3. **Update all syscall handlers** - Point to VFS
4. **Update all userland code** - Shell, compositor, file manager
5. **Remove ext2_state.rs** - Ext2 becomes just another `FileSystem` impl

#### Files to Delete/Replace

```
DELETE:
  fs/src/ext2_state.rs      → Merged into VFS mount system

MODIFY:
  fs/src/fileio.rs          → Use VFS instead of direct ext2
  core/src/syscall/fs.rs    → Import changes only (VFS has same signatures)
  boot/src/boot_services.rs → VFS mount() instead of ext2_init()

LIKELY UNCHANGED:
  userland/src/syscall.rs   → Syscall numbers/signatures stay same
  userland/src/shell.rs     → Uses syscalls, not internal APIs
```

Each phase can be a single commit. No feature branch needed - changes are localized.

### Testing Checkpoints

| Checkpoint | Test |
|------------|------|
| After Phase 2 | Shell file operations (cat, ls, edit) still work |
| After Phase 3 | Can create/read/delete files in /tmp |
| After Phase 4 | `cat /dev/null`, `dd if=/dev/zero` work |
| After Phase 5 | Full regression test, all apps work |

---

## Dependencies

### Required Before Starting
- None (can start immediately)

### Enables After Completion
- `exec()` syscall - Can load ELF from any mounted filesystem
- `/proc` filesystem - For process information
- `/sys` filesystem - For device/driver information
- Pipes and FIFOs - Special file types
- Unix domain sockets - Local IPC

---

## Risk Assessment

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Breaking existing file ops | **Certain** | Accepted | Update all callers in same commit |
| Performance regression | Low | Medium | Benchmark before/after |
| Lock contention in mount table | Low | Medium | Use fine-grained locking |
| Memory leaks in ramfs | Medium | Low | Use RAII patterns |

---

## Success Criteria

1. **Functional**: All file operations work through VFS (old code removed)
2. **Mount Points**: Can mount ext2 at `/`, ramfs at `/tmp`, devfs at `/dev`
3. **Device Files**: `/dev/null`, `/dev/zero`, `/dev/random` functional
4. **Clean Codebase**: No legacy fileio.rs or ext2_state.rs remnants
5. **Updated Userland**: Shell, compositor work with new VFS APIs
6. **Performance**: No measurable slowdown in file operations
7. **Code Quality**: Clean trait boundaries, documented interfaces

---

## Next Steps

1. Create `fs/src/vfs/` directory structure
2. Define `FileSystem` trait in `fs/src/vfs/traits.rs`
3. Implement `MountTable` with basic mount/resolve
4. Create `Ext2VfsAdapter` to wrap existing implementation
5. Test that existing functionality still works
6. Proceed to RamFS and DevFS implementations

---

## References

- Linux VFS Documentation: https://www.kernel.org/doc/html/latest/filesystems/vfs.html
- Redox OS Scheme Design: https://doc.redox-os.org/book/ch04-06-schemes.html
- xv6 File System: https://pdos.csail.mit.edu/6.828/2023/xv6/book-riscv-rev3.pdf (Chapter 8)
