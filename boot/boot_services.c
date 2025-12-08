/*
 * Service-layer boot steps that run after core drivers are ready.
 */

#include "../boot/init.h"
#include "../video/framebuffer.h"
#include "../lib/klog.h"

static int boot_step_mark_kernel_ready(void) {
    boot_mark_initialized();
    klog_info("Kernel core services initialized.");
    return 0;
}

static int boot_step_framebuffer_demo(void) {
    /* Optional validation only; failure should not halt boot. */
    framebuffer_info_t *fb_info = framebuffer_get_info();
    if (!fb_info || !framebuffer_is_initialized()) {
        klog_info("Graphics demo: framebuffer not initialized, skipping");
        return 0;
    }

    if (fb_info->virtual_addr && fb_info->virtual_addr != (void*)fb_info->physical_addr) {
        klog_printf(KLOG_DEBUG,
                    "Graphics: Framebuffer using translated virtual address 0x%lx (translation verified)\n",
                    (uint64_t)fb_info->virtual_addr);
    }

    klog_debug("Graphics demo: framebuffer validation complete");
    return 0;
}

BOOT_INIT_STEP_WITH_FLAGS(services, "mark ready", boot_step_mark_kernel_ready, BOOT_INIT_PRIORITY(60));
BOOT_INIT_OPTIONAL_STEP(optional, "framebuffer demo", boot_step_framebuffer_demo);

