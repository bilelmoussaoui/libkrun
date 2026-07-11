// Copyright 2019 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::io;

use crate::Error as DeviceError;
use crate::bus::BusDevice;
use crate::legacy::irqchip::IrqChipT;

use kvm_bindings::{
    KVM_IRQ_ROUTING_IRQCHIP, KVM_IRQCHIP_IOAPIC, KVM_IRQCHIP_PIC_MASTER, KVM_IRQCHIP_PIC_SLAVE,
    KVM_PIT_SPEAKER_DUMMY, KvmIrqRouting, kvm_irq_routing_entry,
    kvm_irq_routing_entry__bindgen_ty_1, kvm_irq_routing_irqchip, kvm_pit_config,
};
use kvm_ioctls::{Error, VmFd};
use utils::eventfd::EventFd;

const IOAPIC_NUM_PINS: u32 = 24;

pub struct KvmIoapic {}

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

        Ok(Self {})
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
        _irq_line: Option<u32>,
        interrupt_evt: Option<&EventFd>,
    ) -> Result<(), DeviceError> {
        if let Some(interrupt_evt) = interrupt_evt {
            if let Err(e) = interrupt_evt.write(1) {
                error!("Failed to signal used queue: {e:?}");
                return Err(DeviceError::FailedSignalingUsedQueue(e));
            }
        } else {
            error!("EventFd not set up for irq line");
            return Err(DeviceError::FailedSignalingUsedQueue(io::Error::new(
                io::ErrorKind::NotFound,
                "EventFd not set up for irq line",
            )));
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
