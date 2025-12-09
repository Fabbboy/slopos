/*
 * Wrapper header for bindgen
 * Include all C headers that Rust needs to call
 */

#include "../../lib/klog.h"
#include "../../boot/kernel_panic.h"
#include "../../mm/kernel_heap.h"
#include "../../video/framebuffer.h"
#include "../../mm/phys_virt.h"
#include "../../boot/limine_protocol.h"
