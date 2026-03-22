use core::mem;
use core::ptr;

pub type ChunkPtr = *mut u8;

pub const ALIGNMENT: usize = 16;
pub const HEADER_SIZE: usize = 8;
pub const FOOTER_SIZE: usize = 4;
pub const MIN_CHUNK_SIZE: usize = 32;

pub const PREV_IN_USE: u32 = 0x1;
pub const MMAP_CHUNK: u32 = 0x2;
pub const FLAG_MASK: u32 = PREV_IN_USE | MMAP_CHUNK;
pub const SIZE_MASK: u32 = !0xF;

const SIZE_OFFSET: usize = 4;
const FD_OFFSET: usize = HEADER_SIZE;
const BK_OFFSET: usize = HEADER_SIZE + mem::size_of::<ChunkPtr>();

#[inline]
pub unsafe fn prev_size(chunk: ChunkPtr) -> usize {
    unsafe { ptr::read(chunk.cast::<u32>()) as usize }
}

#[inline]
pub unsafe fn set_prev_size(chunk: ChunkPtr, size: usize) {
    debug_assert!(u32::try_from(size).is_ok());
    unsafe {
        ptr::write(chunk.cast::<u32>(), size as u32);
    }
}

#[inline]
pub unsafe fn size_and_flags(chunk: ChunkPtr) -> u32 {
    unsafe { ptr::read(chunk.add(SIZE_OFFSET).cast::<u32>()) }
}

#[inline]
pub unsafe fn size(chunk: ChunkPtr) -> usize {
    (unsafe { size_and_flags(chunk) } & SIZE_MASK) as usize
}

#[inline]
pub unsafe fn flags(chunk: ChunkPtr) -> u32 {
    (unsafe { size_and_flags(chunk) }) & FLAG_MASK
}

#[inline]
pub unsafe fn is_prev_in_use(chunk: ChunkPtr) -> bool {
    (unsafe { flags(chunk) }) & PREV_IN_USE != 0
}

#[inline]
pub unsafe fn is_mmap(chunk: ChunkPtr) -> bool {
    (unsafe { flags(chunk) }) & MMAP_CHUNK != 0
}

#[inline]
pub unsafe fn set_size_flags(chunk: ChunkPtr, chunk_size: usize, new_flags: u32) {
    debug_assert!(u32::try_from(chunk_size).is_ok());
    debug_assert_eq!(chunk_size & (ALIGNMENT - 1), 0);
    unsafe {
        ptr::write(
            chunk.add(SIZE_OFFSET).cast::<u32>(),
            (chunk_size as u32 & SIZE_MASK) | (new_flags & FLAG_MASK),
        );
    }
}

#[inline]
pub unsafe fn set_prev_in_use(chunk: ChunkPtr, in_use: bool) {
    let mut word = unsafe { size_and_flags(chunk) };
    if in_use {
        word |= PREV_IN_USE;
    } else {
        word &= !PREV_IN_USE;
    }
    unsafe {
        ptr::write(chunk.add(SIZE_OFFSET).cast::<u32>(), word);
    }
}

#[inline]
pub unsafe fn set_mmap(chunk: ChunkPtr, mmap: bool) {
    let mut word = unsafe { size_and_flags(chunk) };
    if mmap {
        word |= MMAP_CHUNK;
    } else {
        word &= !MMAP_CHUNK;
    }
    unsafe {
        ptr::write(chunk.add(SIZE_OFFSET).cast::<u32>(), word);
    }
}

#[inline]
pub unsafe fn data_ptr(chunk: ChunkPtr) -> *mut u8 {
    unsafe { chunk.add(HEADER_SIZE) }
}

#[inline]
pub unsafe fn from_data_ptr(data: *mut u8) -> ChunkPtr {
    unsafe { data.sub(HEADER_SIZE) }
}

#[inline]
pub unsafe fn usable_size(chunk: ChunkPtr) -> usize {
    unsafe { size(chunk) }.saturating_sub(HEADER_SIZE)
}

#[inline]
pub unsafe fn next_physical(chunk: ChunkPtr) -> ChunkPtr {
    unsafe { chunk.add(size(chunk)) }
}

#[inline]
pub unsafe fn prev_physical(chunk: ChunkPtr) -> ChunkPtr {
    unsafe { chunk.sub(prev_size(chunk)) }
}

#[inline]
pub unsafe fn write_footer(chunk: ChunkPtr) {
    let footer = unsafe { chunk.add(size(chunk) - FOOTER_SIZE).cast::<u32>() };
    unsafe {
        ptr::write(footer, size_and_flags(chunk));
    }
}

#[inline]
pub unsafe fn footer(chunk: ChunkPtr) -> u32 {
    let footer = unsafe { chunk.add(size(chunk) - FOOTER_SIZE).cast::<u32>() };
    unsafe { ptr::read(footer) }
}

#[inline]
pub unsafe fn fd(chunk: ChunkPtr) -> ChunkPtr {
    unsafe { ptr::read(chunk.add(FD_OFFSET).cast::<ChunkPtr>()) }
}

#[inline]
pub unsafe fn set_fd(chunk: ChunkPtr, next: ChunkPtr) {
    unsafe {
        ptr::write(chunk.add(FD_OFFSET).cast::<ChunkPtr>(), next);
    }
}

#[inline]
pub unsafe fn bk(chunk: ChunkPtr) -> ChunkPtr {
    unsafe { ptr::read(chunk.add(BK_OFFSET).cast::<ChunkPtr>()) }
}

#[inline]
pub unsafe fn set_bk(chunk: ChunkPtr, prev: ChunkPtr) {
    unsafe {
        ptr::write(chunk.add(BK_OFFSET).cast::<ChunkPtr>(), prev);
    }
}

#[inline]
pub unsafe fn clear_links(chunk: ChunkPtr) {
    unsafe {
        set_fd(chunk, ptr::null_mut());
        set_bk(chunk, ptr::null_mut());
    }
}
