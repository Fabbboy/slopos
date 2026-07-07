# USB / xHCI Stack

Replace PS/2 as the input mechanism and enable USB mass storage, USB
networking, etc. Current input path is `drivers/src/ps2/`. Very high effort
(months of work); post-MVP candidate.

- [ ] **1** Implement xHCI (USB 3.x) host controller driver:
  - Discover xHCI via PCI (class 0x0C, subclass 0x03, progif 0x30)
  - Map MMIO registers (capability, operational, runtime, doorbell)
  - Initialize: reset controller, set up device context base array, configure interrupter
  - Command ring + event ring + transfer ring management
- [ ] **2** Implement USB device enumeration:
  - Address assignment, device descriptor reading
  - Configuration descriptor parsing
  - Interface and endpoint descriptor handling
- [ ] **3** Implement USB HID driver (keyboard + mouse):
  - HID report descriptor parsing (boot protocol as minimum)
  - Interrupt IN endpoint for key events
  - Replace PS/2 keyboard/mouse as primary input
- [ ] **4** Implement USB mass storage driver (optional):
  - Bulk-Only Transport (BOT) protocol
  - SCSI command set (INQUIRY, READ, WRITE)
  - Integrate with VFS as block device

Fit with the driver framework: build on the match-table/binding registry and
devres-managed probe resources from `driver-framework-base.html`; MSI-X per
the existing VirtIO discipline (no legacy line IRQs).
