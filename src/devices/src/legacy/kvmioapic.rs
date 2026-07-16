// Copyright 2019 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// x86_64 + Linux only. aarch64 uses KvmGicV2/KvmGicV3; macOS uses HVF.

use std::collections::HashMap;
use std::io;
use std::os::unix::io::AsRawFd;
use std::sync::Mutex;

use crate::Error as DeviceError;
use crate::bus::BusDevice;
use crate::legacy::irqchip::IrqChipT;

use kvm_bindings::{
    KVM_IRQ_ROUTING_IRQCHIP, KVM_IRQCHIP_IOAPIC, KVM_IRQCHIP_PIC_MASTER, KVM_IRQCHIP_PIC_SLAVE,
    KVM_PIT_SPEAKER_DUMMY, KvmIrqRouting, kvm_irq_routing_entry,
    kvm_irq_routing_entry__bindgen_ty_1, kvm_irq_routing_irqchip, kvm_irqchip, kvm_msi,
    kvm_pit_config,
};
use kvm_ioctls::{Error, VmFd};
use utils::eventfd::EventFd;

const IOAPIC_NUM_PINS: u32 = 24;

// Linux ioctl number format: direction[31:30] | size[29:16] | type[15:8] | nr[7:0]
const fn iowr(type_: u32, nr: u32, size: usize) -> libc::c_ulong {
    (3 << 30) | ((size as libc::c_ulong) << 16) | ((type_ as libc::c_ulong) << 8) | (nr as libc::c_ulong)
}
const fn iow(type_: u32, nr: u32, size: usize) -> libc::c_ulong {
    (1 << 30) | ((size as libc::c_ulong) << 16) | ((type_ as libc::c_ulong) << 8) | (nr as libc::c_ulong)
}

const KVM_GET_IRQCHIP: libc::c_ulong = iowr(0xAE, 0x62, std::mem::size_of::<kvm_irqchip>());
const KVM_SIGNAL_MSI: libc::c_ulong = iow(0xAE, 0xa5, std::mem::size_of::<kvm_msi>());

pub struct KvmIoapic {
    vm_raw_fd: libc::c_int,
    // MSI params cached per IRQ line after the guest programs the IOAPIC
    // redirection entry. Populated on the first signal for each IRQ line.
    msi_cache: Mutex<HashMap<u32, kvm_msi>>,
}

impl KvmIoapic {
    pub fn new(vm: &VmFd) -> Result<Self, Error> {
        vm.create_irq_chip()?;
        let pit_config = kvm_pit_config {
            // We need to enable the emulation of a dummy speaker port stub so that writing to port
            // 0x61 (i.e. KVM_SPEAKER_BASE_ADDRESS) does not trigger an exit to user space.
            flags: KVM_PIT_SPEAKER_DUMMY,
            ..Default::default()
        };
        vm.create_pit2(pit_config)?;

        Self::setup_irq_routing(vm)?;

        Ok(Self {
            vm_raw_fd: vm.as_raw_fd(),
            msi_cache: Mutex::new(HashMap::new()),
        })
    }

    fn setup_irq_routing(vm: &VmFd) -> Result<(), Error> {
        let mut entries: Vec<kvm_irq_routing_entry> = Vec::new();

        for gsi in 0..IOAPIC_NUM_PINS {
            entries.push(kvm_irq_routing_entry {
                gsi,
                type_: KVM_IRQ_ROUTING_IRQCHIP,
                flags: 0,
                u: kvm_irq_routing_entry__bindgen_ty_1 {
                    irqchip: kvm_irq_routing_irqchip {
                        irqchip: KVM_IRQCHIP_IOAPIC,
                        pin: gsi,
                    },
                },
                ..Default::default()
            });

            if gsi < 8 {
                entries.push(kvm_irq_routing_entry {
                    gsi,
                    type_: KVM_IRQ_ROUTING_IRQCHIP,
                    flags: 0,
                    u: kvm_irq_routing_entry__bindgen_ty_1 {
                        irqchip: kvm_irq_routing_irqchip {
                            irqchip: KVM_IRQCHIP_PIC_MASTER,
                            pin: gsi,
                        },
                    },
                    ..Default::default()
                });
            } else if gsi < 16 {
                entries.push(kvm_irq_routing_entry {
                    gsi,
                    type_: KVM_IRQ_ROUTING_IRQCHIP,
                    flags: 0,
                    u: kvm_irq_routing_entry__bindgen_ty_1 {
                        irqchip: kvm_irq_routing_irqchip {
                            irqchip: KVM_IRQCHIP_PIC_SLAVE,
                            pin: gsi - 8,
                        },
                    },
                    ..Default::default()
                });
            }
        }

        let mut routing =
            KvmIrqRouting::new(entries.len()).map_err(|_| kvm_ioctls::Error::new(libc::ENOMEM))?;
        routing.as_mut_slice().copy_from_slice(&entries);
        vm.set_gsi_routing(&routing)
    }

    /// Read the IOAPIC redirection entry the guest programmed for `irq_line`
    /// and convert it to an MSI (address + data) for direct LAPIC injection.
    /// Called once per IRQ line; result is cached in `msi_cache`.
    fn resolve_msi(&self, irq_line: u32) -> Result<kvm_msi, DeviceError> {
        let mut irqchip = kvm_irqchip {
            chip_id: KVM_IRQCHIP_IOAPIC,
            ..Default::default()
        };

        let ret = unsafe {
            libc::ioctl(self.vm_raw_fd, KVM_GET_IRQCHIP, &mut irqchip)
        };
        if ret < 0 {
            return Err(DeviceError::FailedSignalingUsedQueue(
                io::Error::last_os_error(),
            ));
        }

        // SAFETY: chip_id == KVM_IRQCHIP_IOAPIC so the ioapic union field is valid.
        let entry = unsafe { irqchip.chip.ioapic }.redirtbl[irq_line as usize];
        // SAFETY: reading the `fields` interpretation of the redirtbl union entry.
        let fields = unsafe { entry.fields };

        // Construct an x86 MSI write from the IOAPIC redirection entry.
        // This is x86_64-specific: the LAPIC MSI address is 0xFEE00000 | (dest << 12).
        // KvmIoapic is never instantiated on aarch64 (GIC is used there instead).
        // address_lo: LAPIC base (0xFEE00000) | dest_id in bits [19:12]
        // data: vector in bits [7:0], delivery mode FIXED (0b000) in [10:8]
        let msi = kvm_msi {
            address_lo: 0xFEE0_0000 | ((fields.dest_id as u32) << 12),
            address_hi: 0,
            data: fields.vector as u32,
            flags: 0,
            devid: 0,
            pad: [0; 12],
        };

        Ok(msi)
    }
}

impl IrqChipT for KvmIoapic {
    fn get_mmio_addr(&self) -> u64 {
        0
    }

    fn get_mmio_size(&self) -> u64 {
        0
    }

    fn set_irq(
        &self,
        irq_line: Option<u32>,
        _interrupt_evt: Option<&EventFd>,
    ) -> Result<(), DeviceError> {
        let irq = irq_line.ok_or_else(|| {
            error!("no irq line set for virtio device");
            DeviceError::FailedSignalingUsedQueue(io::Error::new(
                io::ErrorKind::NotFound,
                "no irq line",
            ))
        })?;

        let msi = {
            let mut cache = self.msi_cache.lock().unwrap();
            if let Some(&m) = cache.get(&irq) {
                m
            } else {
                let m = self.resolve_msi(irq)?;
                cache.insert(irq, m);
                m
            }
        };

        // Synchronous MSI injection: bypasses the async irqfd/ioapic path
        // that loses notifications under nested KVM.
        let ret = unsafe { libc::ioctl(self.vm_raw_fd, KVM_SIGNAL_MSI, &msi) };
        if ret < 0 {
            let e = io::Error::last_os_error();
            error!("KVM_SIGNAL_MSI failed: {e:?}");
            return Err(DeviceError::FailedSignalingUsedQueue(e));
        }

        Ok(())
    }
}

impl BusDevice for KvmIoapic {
    fn read(&mut self, _vcpuid: u64, _offset: u64, _data: &mut [u8]) {
        unreachable!("MMIO operations are managed in-kernel");
    }

    fn write(&mut self, _vcpuid: u64, _offset: u64, _data: &[u8]) {
        unreachable!("MMIO operations are managed in-kernel");
    }
}
