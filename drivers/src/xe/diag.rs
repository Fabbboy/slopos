//! Read-only display-engine diagnostics: never writes a display register, so
//! the firmware scanout keeps driving the panel. Every value is logged on its
//! own short line to keep each function's stack frame small.

use slopos_mm::mmio::MmioRegion;
use slopos_ostd::klog_info;

use crate::pci_defs::PciDeviceInfo;
use crate::xe_logic::ggtt_pte;
use crate::xe_logic::plane_config::PlaneConfig;
use crate::xe_logic::platform::XePlatform;
use crate::xe_logic::regs::{self, Pipe};

pub fn dump(mmio: &MmioRegion, info: &PciDeviceInfo, platform: XePlatform) {
    log_identity(info, platform);

    let Some(active) = scan_pipes(mmio) else {
        klog_info!("XE-DIAG: no pipe enabled; firmware scanout idle");
        return;
    };

    log_pipe_geometry(mmio, active);
    log_scanline(mmio, active);
    let plane = log_plane(mmio, active);
    log_diag_registers(mmio, active);
    log_ggtt_extent(mmio, plane.surf_ggtt);
}

fn log_identity(info: &PciDeviceInfo, platform: XePlatform) {
    klog_info!("XE-DIAG: platform {}", platform.name);
    klog_info!("XE-DIAG:   vendor id 0x{:04x}", info.vendor_id);
    klog_info!("XE-DIAG:   device id 0x{:04x}", info.device_id);
    klog_info!(
        "XE-DIAG:   display ip version {}",
        platform.display_ip_version()
    );

    let bar0 = info.bars[0];
    klog_info!("XE-DIAG:   bar0 base 0x{:016x}", bar0.base);
    klog_info!("XE-DIAG:   bar0 size 0x{:x}", bar0.size);
}

/// Returns the first enabled pipe — the one driving live output.
fn scan_pipes(mmio: &MmioRegion) -> Option<Pipe> {
    let mut active = None;
    for pipe in Pipe::ALL {
        let conf = mmio.read::<u32>(regs::pipe_conf(pipe));
        let enabled = conf & regs::PIPECONF_ENABLE != 0;
        let pipe_active = conf & regs::PIPECONF_STATE_ACTIVE != 0;
        klog_info!("XE-DIAG: pipe {:?} PIPECONF 0x{:08x}", pipe, conf);
        klog_info!("XE-DIAG:   enabled {} active {}", enabled, pipe_active);
        if enabled && active.is_none() {
            active = Some(pipe);
        }
    }
    active
}

fn log_pipe_geometry(mmio: &MmioRegion, pipe: Pipe) {
    let src = mmio.read::<u32>(regs::pipe_src(pipe));
    let width = (src >> 16) + 1;
    let height = (src & 0xffff) + 1;
    klog_info!("XE-DIAG: active pipe {:?}", pipe);
    klog_info!("XE-DIAG:   PIPESRC 0x{:08x}", src);
    klog_info!("XE-DIAG:   source {} x {}", width, height);
}

/// Sample PIPEDSL twice, one millisecond apart, to prove the pipe is scanning.
fn log_scanline(mmio: &MmioRegion, pipe: Pipe) {
    let dsl = regs::pipe_dsl(pipe);
    let first = mmio.read::<u32>(dsl);
    crate::hpet::delay_ms(1);
    let second = mmio.read::<u32>(dsl);
    let advancing = first != second;
    klog_info!("XE-DIAG:   PIPEDSL {} then {}", first, second);
    klog_info!("XE-DIAG:   scanline advancing {}", advancing);
}

fn log_plane(mmio: &MmioRegion, pipe: Pipe) -> PlaneConfig {
    let ctl = mmio.read::<u32>(regs::plane_ctl(pipe));
    let size = mmio.read::<u32>(regs::plane_size(pipe));
    let pos = mmio.read::<u32>(regs::plane_pos(pipe));
    let stride = mmio.read::<u32>(regs::plane_stride(pipe));
    let surf = mmio.read::<u32>(regs::plane_surf(pipe));
    let plane = PlaneConfig::from_registers(ctl, size, pos, stride, surf);

    klog_info!("XE-DIAG: plane PLANE_CTL 0x{:08x}", ctl);
    klog_info!("XE-DIAG:   enable {}", plane.enable);
    klog_info!("XE-DIAG:   format {:?}", plane.format);
    klog_info!("XE-DIAG:   tiling {:?}", plane.tiling);
    klog_info!("XE-DIAG:   color order {:?}", plane.color_order);
    klog_info!(
        "XE-DIAG:   render decompressed {}",
        plane.render_decompressed
    );
    klog_info!("XE-DIAG:   stride reg {}", plane.stride_reg);
    klog_info!("XE-DIAG:   size {} x {}", plane.width, plane.height);
    klog_info!("XE-DIAG:   surf ggtt 0x{:08x}", plane.surf_ggtt);
    plane
}

fn log_diag_registers(mmio: &MmioRegion, pipe: Pipe) {
    let ddi_func = mmio.read::<u32>(regs::trans_ddi_func_ctl(pipe));
    let ddi_buf = mmio.read::<u32>(regs::DDI_BUF_CTL_A);
    let pp_status = mmio.read::<u32>(regs::PCH_PP_STATUS);
    let pp_control = mmio.read::<u32>(regs::PCH_PP_CONTROL);
    let pwr_well = mmio.read::<u32>(regs::PWR_WELL_CTL2);

    klog_info!("XE-DIAG: TRANS_DDI_FUNC_CTL 0x{:08x}", ddi_func);
    klog_info!("XE-DIAG: DDI_BUF_CTL_A 0x{:08x}", ddi_buf);
    klog_info!("XE-DIAG: PCH_PP_STATUS 0x{:08x}", pp_status);
    klog_info!("XE-DIAG: PCH_PP_CONTROL 0x{:08x}", pp_control);
    klog_info!("XE-DIAG: PWR_WELL_CTL2 0x{:08x}", pwr_well);
}

fn log_ggtt_extent(mmio: &MmioRegion, surf_ggtt: u32) {
    let index = ggtt_pte::entry_index(surf_ggtt as u64);
    let pte_offset = regs::GTTMMADR_GGTT_OFFSET + (index as usize) * regs::GGTT_PTE_BYTES;

    klog_info!("XE-DIAG: fb surf ggtt 0x{:08x}", surf_ggtt);
    klog_info!("XE-DIAG:   ggtt entry index {}", index);
    match mmio.try_read::<u64>(pte_offset) {
        Ok(pte) => klog_info!("XE-DIAG:   ggtt pte 0x{:016x}", pte),
        Err(_) => klog_info!("XE-DIAG:   ggtt pte offset out of range"),
    }
}
