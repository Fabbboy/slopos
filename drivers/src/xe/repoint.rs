//! Inherit-and-repoint orchestrator for the active display.
//!
//! Inherits the firmware's live modeset and re-points the active primary plane at
//! a kernel-owned linear framebuffer, changing only the plane group — never the
//! pipe, PLL, transcoder, or eDP link. The sequence: claim the scanout, find the
//! active pipe, decode the live plane, snapshot the plane group, allocate and
//! GGTT-map a linear surface strictly above the firmware framebuffer, seed it
//! from the current image, program the linear repoint, confirm the pipe is still
//! scanning with the watchdog, then commit or roll back.
//!
//! Safety: the scanout is claimed before any hardware touch and released on every
//! bail; the plane group is snapshotted before the first write and restored
//! (`PLANE_SURF` last) on any failure, so a bad repoint never leaves a black
//! screen; our GGTT PTEs land strictly above the firmware surface; and no lock is
//! held across the watchdog's timer delays.
//!
//! After the repoint commits, the hardware cursor and the tear-free present path
//! layer on top: the cursor carves its DBUF allocation off the primary
//! ([`super::ddb`]) and binds the separate cursor plane ([`super::cursor`]); the
//! present path flips the plane between two scan buffers, falling back to a
//! single-buffer flush if the second buffer cannot be allocated. Neither writes
//! any pipe/transcoder register, and a failure in either never disturbs the
//! primary scanout.

use core::ffi::c_int;

use slopos_abi::damage::DamageRect;
use slopos_abi::{DisplayInfo, FramebufferData, PixelFormat};
use slopos_kernel_services::syscall_services::scanout::{
    self, ClaimOutcome, GpuControlFns, InstallCtx, ScanoutId, ScanoutProvider,
};
use slopos_mm::mmio::MmioRegion;
use slopos_mm::page_alloc::free_page_frame;
use slopos_mm::paging_defs::PAGE_SIZE_4KB;
use slopos_ostd::arch::x86_64::mem_fence::sfence;
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::util::ptr_buf;
use slopos_ostd::{align_up_u64, klog_info, klog_warn};

use super::{cursor, ddb, fb_mem, ggtt, mmio_map, pipe, plane, present, snapshot, watchdog};
use crate::pci::{PciProbeError, ProbeOutcome};
use crate::xe_logic::cmdline::XeConfig;
use crate::xe_logic::ggtt_pte;
use crate::xe_logic::platform::XePlatform;
use crate::xe_logic::regs;

/// Minimum watchdog window. A repoint must see the pipe advance within at least
/// this long before it may commit, even if the cmdline asked for less.
const MIN_WDOG_MS: u32 = 20;

/// A generous firmware-surface reservation our GGTT placement is kept clear of,
/// so the firmware framebuffer's PTEs are never overwritten even if its true
/// extent exceeds what the plane geometry implies.
const FW_RESERVE_BYTES: u64 = 32 * 1024 * 1024;

/// GGTT placement alignment for our linear scanout surfaces (256 KiB). The Intel
/// display engine requires a linear primary-plane surface base aligned to 256 KiB.
const GGTT_ALIGN: u64 = 0x4_0000;

/// State retained once a repoint commits: the mapped register window, the full
/// plane-group program (pipe + geometry + format), and the GGTT byte address of
/// our linear surface. Consulted by [`flush`] to re-issue a full plane-group flip
/// of the primary.
struct XeState {
    mmio: MmioRegion,
    program: plane::PlaneProgram,
    surf_ggtt: u32,
}

/// Installed when a repoint commits; consulted by [`flush`]. `None` until xe owns
/// the scanout.
static XE_STATE: SpinLock<Option<XeState>> = SpinLock::new(None, LOCK_LEVEL_RESOURCE);

/// Run the inherit-and-repoint sequence for the active display.
///
/// Returns [`ProbeOutcome::Bound`] on a watchdog-confirmed commit. Every other
/// outcome — a lost arbitration, a missing/disabled plane, an allocation or
/// mapping failure, or a watchdog timeout — restores the firmware framebuffer
/// (when anything was written), releases the scanout claim, and returns
/// [`ProbeOutcome::Declined`] so the firmware scanout keeps the panel.
pub fn run(
    mmio: &MmioRegion,
    cfg: XeConfig,
    platform: XePlatform,
) -> Result<ProbeOutcome, PciProbeError> {
    // Reserve the scanout before any hardware touch. A higher-priority provider
    // already owning it means we stay passive and touch nothing.
    match scanout::SCANOUT.claim(scanout::PRIO_INTEL_XE) {
        ClaimOutcome::Won => {}
        ClaimOutcome::Lost | ClaimOutcome::LostTie => {
            klog_info!("XE: lost scanout arbitration; staying passive");
            return Ok(ProbeOutcome::Declined);
        }
    }

    // The GGTT page-table bank and the firmware's active pipe. Either missing
    // means we cannot drive the display; release the claim and decline.
    let Some(bank) = mmio_map::ggtt_bank(mmio) else {
        klog_info!("XE: GGTT bank unavailable; declining");
        scanout::SCANOUT.abort_claim();
        return Ok(ProbeOutcome::Declined);
    };
    // `xe.pipe` forces the pipe; otherwise auto-detect the one the firmware is
    // scanning. No detectable pipe means we cannot drive the display.
    let pipe = match cfg.pipe {
        Some(forced) => forced,
        None => {
            let Some(found) = pipe::find_active(mmio) else {
                klog_info!("XE: no active pipe; declining");
                scanout::SCANOUT.abort_claim();
                return Ok(ProbeOutcome::Declined);
            };
            found
        }
    };

    // Decode the live primary plane; a disabled plane has nothing to repoint.
    let live = plane::read_live(mmio, pipe);
    if !live.enable {
        klog_info!("XE: active plane disabled; declining");
        scanout::SCANOUT.abort_claim();
        return Ok(ProbeOutcome::Declined);
    }

    // Geometry for our linear surface: a 64-byte-aligned pitch and the page
    // count that covers it. All width math is done in `u64` so an implausible
    // register read cannot overflow before the sanity check rejects it.
    let width = live.width;
    let height = live.height;
    let pitch = align_up_u64((width as u64) * 4, 64) as u32;
    let bytes = pitch as u64 * height as u64;
    let pages = (align_up_u64(bytes, PAGE_SIZE_4KB) / PAGE_SIZE_4KB) as u32;
    if width == 0
        || height == 0
        || width > DisplayInfo::MAX_DIMENSION
        || height > DisplayInfo::MAX_DIMENSION
        || pages == 0
    {
        klog_warn!("XE: implausible geometry {}x{}; declining", width, height);
        scanout::SCANOUT.abort_claim();
        return Ok(ProbeOutcome::Declined);
    }

    // Snapshot every plane-group register before the first write so the firmware
    // framebuffer can be put back verbatim.
    let snap = snapshot::capture(mmio, pipe);

    // Allocate the kernel-owned linear backing as a Write-Combining surface, so
    // the display engine's direct GGTT read never sees stale CPU-cached pixels.
    let Some((phys, fb_virt)) = fb_mem::alloc_wc_scanout(pages) else {
        klog_warn!("XE: WC backing alloc of {} pages failed; declining", pages);
        scanout::SCANOUT.abort_claim();
        return Ok(ProbeOutcome::Declined);
    };

    // Seed the new surface from the live firmware framebuffer so the repoint is
    // visually seamless. Best-effort: a missing seed just leaves zeroed pages.
    if let Some(seed) = scanout::current_framebuffer() {
        let copy_len = core::cmp::min(seed.info.buffer_size() as u64, bytes) as usize;
        ptr_buf::copy_bytes(fb_virt as *mut u8, seed.address as *const u8, copy_len);
    }

    // Place our GGTT mapping strictly above the firmware framebuffer extent so no
    // firmware PTE is ever rewritten.
    let ggtt_total = (bank.size() as u64 / regs::GGTT_PTE_BYTES as u64) * PAGE_SIZE_4KB;
    let fw_len = core::cmp::max(bytes, FW_RESERVE_BYTES);
    let Some(our_ggtt) =
        ggtt_pte::alloc_above(live.surf_ggtt as u64, fw_len, GGTT_ALIGN, pages, ggtt_total)
    else {
        free_page_frame(phys);
        scanout::SCANOUT.abort_claim();
        klog_warn!("XE: no GGTT room above firmware surface; declining");
        return Ok(ProbeOutcome::Declined);
    };
    if !ggtt::map_pages(&bank, our_ggtt, phys, pages) {
        free_page_frame(phys);
        scanout::SCANOUT.abort_claim();
        klog_warn!("XE: GGTT mapping failed; declining");
        return Ok(ProbeOutcome::Declined);
    }
    // Flush the display engine's GGTT TLB so it sees our freshly written PTEs.
    ggtt::invalidate_tlb(mmio);

    // The geometry plane-group program for our linear surface. Every flip re-issues
    // this full group — `PLANE_STRIDE` / `PLANE_CTL` (linear) and the rest, then
    // `PLANE_SURF` last as the arming write — and leaves the inherited firmware
    // watermark/DDB/colour/key state untouched.
    let program = plane::PlaneProgram {
        pipe,
        width,
        height,
        pitch_bytes: pitch,
        format: live.format,
        color_order: live.color_order,
    };

    // The destructive step: program the linear repoint, arming with a trailing
    // `PLANE_SURF`.
    program.flip(mmio, our_ggtt as u32);

    // Confirm the panel kept scanning before committing.
    let alive = watchdog::confirm_scanning(mmio, pipe, cfg.wdog_ms.max(MIN_WDOG_MS));

    // A watchdog timeout rolls back: restore the firmware framebuffer, release the
    // claim, and decline, so a bad repoint never leaves a black screen.
    if !alive {
        snapshot::restore(mmio, &snap);
        scanout::SCANOUT.abort_claim();
        free_page_frame(phys);
        klog_warn!("XE: watchdog timeout; rolled back to firmware framebuffer");
        return Ok(ProbeOutcome::Declined);
    }

    // Confirmed-alive panel: commit. Retain the state so [`flush`] can re-arm the
    // primary plane, then layer the cursor and present front-ends on top.
    *XE_STATE.lock() = Some(XeState {
        mmio: mmio.clone(),
        program,
        surf_ggtt: our_ggtt as u32,
    });

    // Reserve the cursor's DBUF allocation before standing up the hardware cursor.
    // The cursor is a real plane in the DBUF/watermark model: enabling it with a
    // zero DDB allocation starves the pipe's fetch and makes the primary plane
    // decode its linear surface at the X-tile (512-byte) stride, replicating it 8x
    // vertically. [`ddb::program_cursor_ddb`] carves the cursor's blocks off the
    // tail of the primary's allocation and re-arms the primary so the shrink
    // latches while the plane still scans the clean primary surface, before any
    // present scan flip. Skipped when the hardware cursor is disabled; on a carve
    // failure the cursor stays disabled (a software cursor) so it can never corrupt
    // the scanout.
    let cursor_ddb_ok =
        !cfg.nocursor && ddb::program_cursor_ddb(mmio, pipe, &program, our_ggtt as u32);

    // Scanout front-end. The scanout is always tear-free double-buffered: a second
    // GGTT surface, seeded from the primary, flipped to once at commit and per-frame
    // by [`present::present`], vblank-latched. The single-buffer `flush` (the
    // compositor draws into the one permanently-scanned surface and we re-arm that
    // same `PLANE_SURF` address) is the automatic fallback when the second scan
    // buffer cannot be allocated.
    let present_flush: Option<fn(*const DamageRect, u32) -> c_int> = match setup_present(
        mmio,
        &bank,
        program,
        fb_virt as *const u8,
        our_ggtt as u32,
        pages,
        bytes,
        ggtt_total,
    ) {
        Some(scan_ggtt) => {
            // Wait for the primary flip above to latch at vblank before the scan
            // flip; a lone flip latches cleanly.
            pipe::wait_for_vblank(mmio, pipe);
            // The scan buffer holds an identical copy of the primary, so this flip
            // is seamless. Full synchronous plane-group flip, `PLANE_SURF` last,
            // latching at the next vblank.
            program.flip(mmio, scan_ggtt);
            klog_info!(
                "XE-PRESENT: tear-free double-buffered scanout (pipe {:?})",
                pipe
            );
            Some(present::present)
        }
        None => {
            klog_warn!(
                "XE-PRESENT: second scan buffer unavailable; falling back to single-buffer (pipe {:?})",
                pipe
            );
            None
        }
    };

    // Bind the separate hardware cursor plane and expose its control entry points.
    // The cursor DDB carve above must have succeeded; `init` allocates the WC cursor
    // surface. The cursor plane never touches the primary plane, so a cursor failure
    // cannot disturb the scanout — it just falls back to a software cursor.
    let gpu_control = if cfg.nocursor {
        klog_warn!("XE-CURSOR: hardware cursor disabled (xe.nocursor); software cursor in use");
        None
    } else if !cursor_ddb_ok {
        klog_warn!(
            "XE-CURSOR: cursor DBUF carve failed; software cursor (HW cursor would corrupt scanout)"
        );
        None
    } else if cursor::init(mmio, pipe, platform.display_ip_version()) {
        klog_info!("XE-CURSOR: hardware cursor plane bound (pipe {:?})", pipe);
        Some(GpuControlFns {
            available: cursor::available,
            set_image: cursor::set_image,
            move_cursor: cursor::move_cursor,
            set_mode: xe_set_mode,
        })
    } else {
        klog_warn!("XE-CURSOR: cursor surface alloc failed; software cursor in use");
        None
    };

    // Take scanout ownership (evicting any displaced provider through its own
    // hook), then install the front-end through the video-registered installer.
    scanout::SCANOUT.commit_install(
        ScanoutProvider {
            id: ScanoutId::IntelXe,
            priority: scanout::PRIO_INTEL_XE,
            evict: xe_evict,
        },
        scanout::PRIO_INTEL_XE,
        |displaced| {
            if let Some(p) = displaced {
                (p.evict)();
            }
        },
    );

    // The installed framebuffer is always the primary (draw) surface: the compositor
    // draws there whether the plane scans it directly or the present path copies
    // damaged rows from it into the scan buffer. Single-buffer `flush` re-arms the
    // plane; otherwise `present::present` does.
    let ctx = InstallCtx {
        fb: FramebufferData {
            address: fb_virt as *mut u8,
            info: DisplayInfo::new(width, height, pitch, PixelFormat::Xrgb8888),
        },
        flush: present_flush.or(Some(flush)),
        gpu_control,
    };
    if scanout::run_scanout_install(&ctx) {
        klog_info!("XE: repoint committed; xe drives scanout (pipe {:?})", pipe);
    } else {
        klog_warn!("XE: repoint committed but scanout install reported failure");
    }

    Ok(ProbeOutcome::Bound)
}

/// Stand up the double-buffered present path on top of a committed repoint.
///
/// Allocates TWO linear scanout surfaces of the primary's geometry, GGTT-maps them
/// strictly above the firmware framebuffer, the primary surface, and each other
/// (reserving each region's full page extent, so all three are provably disjoint),
/// seeds both from the primary buffer, and records the present state via
/// [`present::install`]. Two buffers are required for tear-free present: the plane
/// always scans one while the compositor's damage is copied into the other (see
/// `present`). Returns the FIRST scan buffer's GGTT byte address (which the caller
/// arms at commit, making it `front`) on success, or `None` — freeing any partial
/// allocation and leaving the single-buffer path untouched — on any allocation,
/// addressing, placement, or mapping failure. Writes no display register: the
/// caller arms `PLANE_SURF` at commit.
fn setup_present(
    mmio: &MmioRegion,
    bank: &MmioRegion,
    program: plane::PlaneProgram,
    primary_virt: *const u8,
    primary_ggtt: u32,
    pages: u32,
    bytes: u64,
    ggtt_total: u64,
) -> Option<u32> {
    // Reserve each surface's whole page extent above the previous one so no PTEs
    // ever collide: scan0 above the primary, scan1 above scan0.
    let surface_len = pages as u64 * PAGE_SIZE_4KB;

    // Scan buffer 0, strictly above the primary surface.
    let (phys0, virt0) = fb_mem::alloc_wc_scanout(pages)?;
    let Some(scan0_ggtt) = ggtt_pte::alloc_above(
        primary_ggtt as u64,
        surface_len,
        GGTT_ALIGN,
        pages,
        ggtt_total,
    ) else {
        free_page_frame(phys0);
        return None;
    };
    if !ggtt::map_pages(bank, scan0_ggtt, phys0, pages) {
        free_page_frame(phys0);
        return None;
    }

    // Scan buffer 1, strictly above scan buffer 0.
    let Some((phys1, virt1)) = fb_mem::alloc_wc_scanout(pages) else {
        free_page_frame(phys0);
        return None;
    };
    let Some(scan1_ggtt) =
        ggtt_pte::alloc_above(scan0_ggtt, surface_len, GGTT_ALIGN, pages, ggtt_total)
    else {
        free_page_frame(phys0);
        free_page_frame(phys1);
        return None;
    };
    if !ggtt::map_pages(bank, scan1_ggtt, phys1, pages) {
        free_page_frame(phys0);
        free_page_frame(phys1);
        return None;
    }

    // Flush the display engine's GGTT TLB so it sees both scan buffers' PTEs.
    ggtt::invalidate_tlb(mmio);

    // Seed both scan buffers from the primary so the commit flip and the first
    // present are seamless. The copy is bounded by the primary's content extent,
    // which fits each scan buffer's identical page count.
    ptr_buf::copy_bytes(virt0 as *mut u8, primary_virt, bytes as usize);
    ptr_buf::copy_bytes(virt1 as *mut u8, primary_virt, bytes as usize);

    // Drain the write-combining seed stores before the caller arms the plane.
    sfence();

    present::install(
        mmio,
        program,
        primary_virt,
        virt0 as *mut u8,
        virt1 as *mut u8,
        scan0_ggtt as u32,
        scan1_ggtt as u32,
    );
    Some(scan0_ggtt as u32)
}

/// Runtime mode-set entry point exposed through [`GpuControlFns`]. The firmware
/// modeset is inherited and fixed, so a runtime mode change is out of scope:
/// every request declines, leaving the current mode in place.
fn xe_set_mode(_width: u32, _height: u32) -> Option<FramebufferData> {
    None
}

/// Present hook for the single-buffer fallback path (when the second scan buffer
/// could not be allocated). The compositor draws directly into the scanned
/// surface, so a flush re-issues a full plane-group flip of the primary —
/// `PLANE_STRIDE` / `PLANE_CTL` before the trailing `PLANE_SURF`, so the group
/// latches atomically.
///
/// The committed state is snapshotted under the lock and the lock is dropped
/// before any MMIO, so no lock is ever held across the hardware access. Returns
/// `0` on success, or `-1` when xe owns no scanout.
pub fn flush(_damage: *const DamageRect, _count: u32) -> c_int {
    let held = {
        let guard = XE_STATE.lock();
        guard
            .as_ref()
            .map(|state| (state.mmio.clone(), state.program, state.surf_ggtt))
    };
    let Some((mmio, program, surf)) = held else {
        return -1;
    };
    // Lock dropped above; a full plane-group flip re-arms the plane atomically.
    program.flip(&mmio, surf);
    0
}

/// Eviction hook. xe sits at the top of the scanout priority ladder, so it is
/// never displaced; the hook is the required no-op.
fn xe_evict() {}
