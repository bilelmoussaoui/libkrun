use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

use super::layout::RSDP_ADDR;

const XSDT_OFFSET: u64 = 0x100;
const FADT_OFFSET: u64 = 0x200;
const MADT_OFFSET: u64 = 0x320;
const DSDT_OFFSET: u64 = 0x400;

const FADT_LEN: usize = 276;
const FADT_FLAGS_OFFSET: usize = 112;
const FADT_X_DSDT_OFFSET: usize = 140;
const HW_REDUCED_ACPI: u32 = 1 << 20;

const LAPIC_BASE: u32 = 0xFEE0_0000;
const IOAPIC_BASE: u32 = 0xFEC0_0000;

const MADT_LAPIC_ENTRY: u8 = 0;
const MADT_LAPIC_ENTRY_LEN: u8 = 8;
const MADT_IOAPIC_ENTRY: u8 = 1;
const MADT_IOAPIC_ENTRY_LEN: u8 = 12;
const MADT_FLAGS_PCAT_COMPAT: u32 = 1;

#[derive(Debug, Eq, PartialEq)]
#[allow(clippy::enum_variant_names)]
pub enum Error {
    WriteRsdp,
    WriteXsdt,
    WriteFadt,
    WriteMadt,
    WriteDsdt,
}

pub type Result<T> = std::result::Result<T, Error>;

fn acpi_checksum(data: &[u8]) -> u8 {
    let sum: u8 = data.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
    (!sum).wrapping_add(1)
}

fn acpi_header(signature: &[u8; 4], length: u32, revision: u8, table_id: &[u8; 8]) -> [u8; 36] {
    let mut hdr = [0u8; 36];
    hdr[0..4].copy_from_slice(signature);
    hdr[4..8].copy_from_slice(&length.to_le_bytes());
    hdr[8] = revision;
    hdr[10..16].copy_from_slice(b"LIBKRN");
    hdr[16..24].copy_from_slice(table_id);
    hdr[24..28].copy_from_slice(&1u32.to_le_bytes());
    hdr[28..32].copy_from_slice(b"KRUN");
    hdr[32..36].copy_from_slice(&1u32.to_le_bytes());
    hdr
}

fn aml_pkg_length(content_len: usize) -> Vec<u8> {
    if content_len < 0x3F {
        vec![(content_len + 1) as u8]
    } else if content_len + 1 < 0xFFF {
        let total = content_len + 2;
        vec![0x40 | (total & 0x0F) as u8, ((total >> 4) & 0xFF) as u8]
    } else if content_len + 3 <= 0x0F_FFFF {
        let total = content_len + 3;
        vec![
            0x80 | (total & 0x0F) as u8,
            ((total >> 4) & 0xFF) as u8,
            ((total >> 12) & 0xFF) as u8,
        ]
    } else {
        let total = content_len + 4;
        vec![
            0xC0 | (total & 0x0F) as u8,
            ((total >> 4) & 0xFF) as u8,
            ((total >> 12) & 0xFF) as u8,
            ((total >> 20) & 0xFF) as u8,
        ]
    }
}

fn aml_name(name: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(1 + 4 + data.len());
    v.push(0x08);
    v.extend_from_slice(name);
    v.extend_from_slice(data);
    v
}

fn aml_string(s: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(2 + s.len());
    v.push(0x0D);
    v.extend_from_slice(s);
    v.push(0x00);
    v
}

fn aml_resource_template(base: u32, len: u32, irq: u32) -> Vec<u8> {
    let mut resources = Vec::with_capacity(23);

    // Memory32Fixed(ReadWrite, base, len)
    resources.push(0x86);
    resources.extend_from_slice(&9u16.to_le_bytes());
    resources.push(0x01);
    resources.extend_from_slice(&base.to_le_bytes());
    resources.extend_from_slice(&len.to_le_bytes());

    // Interrupt(ResourceConsumer, Level, ActiveHigh, Exclusive) { irq }
    resources.push(0x89);
    resources.extend_from_slice(&6u16.to_le_bytes());
    resources.push(0x01);
    resources.push(0x01);
    resources.extend_from_slice(&irq.to_le_bytes());

    // End tag
    resources.push(0x79);
    resources.push(0x00);

    let buf_size = [0x0A, resources.len() as u8];
    let inner_len = buf_size.len() + resources.len();
    let pkg_len = aml_pkg_length(inner_len);

    let mut v = Vec::with_capacity(1 + pkg_len.len() + buf_size.len() + resources.len());
    v.push(0x11);
    v.extend_from_slice(&pkg_len);
    v.extend_from_slice(&buf_size);
    v.extend(resources);
    v
}

fn aml_device(name: &[u8; 4], contents: &[u8]) -> Vec<u8> {
    let inner_len = 4 + contents.len();
    let pkg_len = aml_pkg_length(inner_len);

    let mut v = Vec::with_capacity(2 + pkg_len.len() + 4 + contents.len());
    v.push(0x5B);
    v.push(0x82);
    v.extend_from_slice(&pkg_len);
    v.extend_from_slice(name);
    v.extend_from_slice(contents);
    v
}

fn build_dsdt(devices: &[(u64, u32, u64)]) -> Vec<u8> {
    let mut device_bytes = Vec::new();

    for (i, &(addr, irq, len)) in devices.iter().enumerate() {
        let name = [b'V', b'R', b'0' + (i / 10) as u8, b'0' + (i % 10) as u8];

        let mut dev_content = Vec::new();
        let hid = aml_string(b"LNRO0005");
        dev_content.extend(aml_name(b"_HID", &hid));
        let uid = [0x0A, i as u8];
        dev_content.extend(aml_name(b"_UID", &uid));
        let crs = aml_resource_template(addr as u32, len as u32, irq);
        dev_content.extend(aml_name(b"_CRS", &crs));

        device_bytes.extend(aml_device(&name, &dev_content));
    }

    // Scope(\_SB) { devices }
    let scope_name: &[u8] = &[0x5C, b'_', b'S', b'B', b'_'];
    let inner_len = scope_name.len() + device_bytes.len();
    let pkg_len = aml_pkg_length(inner_len);

    let mut body = Vec::with_capacity(1 + pkg_len.len() + scope_name.len() + device_bytes.len());
    body.push(0x10);
    body.extend_from_slice(&pkg_len);
    body.extend_from_slice(scope_name);
    body.extend(device_bytes);

    let total_len = 36 + body.len();
    let hdr = acpi_header(b"DSDT", total_len as u32, 2, b"KRUNDSDT");

    let mut dsdt = Vec::with_capacity(total_len);
    dsdt.extend_from_slice(&hdr);
    dsdt.extend(body);
    dsdt[9] = acpi_checksum(&dsdt);
    dsdt
}

fn build_madt(num_cpus: u8) -> Vec<u8> {
    let ioapic_id = num_cpus + 1;
    let entries_len =
        MADT_LAPIC_ENTRY_LEN as usize * num_cpus as usize + MADT_IOAPIC_ENTRY_LEN as usize;
    let total_len = 36 + 4 + 4 + entries_len; // header + LAPIC addr + flags + entries
    let hdr = acpi_header(b"APIC", total_len as u32, 5, b"KRUNMADT");

    let mut madt = Vec::with_capacity(total_len);
    madt.extend_from_slice(&hdr);
    madt.extend_from_slice(&LAPIC_BASE.to_le_bytes());
    madt.extend_from_slice(&MADT_FLAGS_PCAT_COMPAT.to_le_bytes());

    for cpu_id in 0..num_cpus {
        madt.push(MADT_LAPIC_ENTRY);
        madt.push(MADT_LAPIC_ENTRY_LEN);
        madt.push(cpu_id); // ACPI Processor UID
        madt.push(cpu_id); // APIC ID
        madt.extend_from_slice(&1u32.to_le_bytes()); // Flags: Enabled
    }

    madt.push(MADT_IOAPIC_ENTRY);
    madt.push(MADT_IOAPIC_ENTRY_LEN);
    madt.push(ioapic_id); // I/O APIC ID
    madt.push(0); // Reserved
    madt.extend_from_slice(&IOAPIC_BASE.to_le_bytes());
    madt.extend_from_slice(&0u32.to_le_bytes()); // GSI base

    madt[9] = acpi_checksum(&madt);
    madt
}

pub fn setup_acpi_tables(
    mem: &GuestMemoryMmap,
    num_cpus: u8,
    devices: &[(u64, u32, u64)],
) -> Result<()> {
    let xsdt_addr = RSDP_ADDR + XSDT_OFFSET;
    let fadt_addr = RSDP_ADDR + FADT_OFFSET;
    let madt_addr = RSDP_ADDR + MADT_OFFSET;
    let dsdt_addr = RSDP_ADDR + DSDT_OFFSET;

    // RSDP (36 bytes, ACPI 2.0)
    let mut rsdp = [0u8; 36];
    rsdp[0..8].copy_from_slice(b"RSD PTR ");
    rsdp[9..15].copy_from_slice(b"LIBKRN");
    rsdp[15] = 2;
    rsdp[20..24].copy_from_slice(&36u32.to_le_bytes());
    rsdp[24..32].copy_from_slice(&xsdt_addr.to_le_bytes());
    rsdp[8] = acpi_checksum(&rsdp[0..20]);
    rsdp[32] = acpi_checksum(&rsdp);
    mem.write_slice(&rsdp, GuestAddress(RSDP_ADDR))
        .map_err(|_| Error::WriteRsdp)?;

    // XSDT (header + two 64-bit entries: FADT and MADT)
    let xsdt_len: u32 = 36 + 16;
    let hdr = acpi_header(b"XSDT", xsdt_len, 1, b"KRUNXSDT");
    let mut xsdt = Vec::with_capacity(xsdt_len as usize);
    xsdt.extend_from_slice(&hdr);
    xsdt.extend_from_slice(&fadt_addr.to_le_bytes());
    xsdt.extend_from_slice(&madt_addr.to_le_bytes());
    xsdt[9] = acpi_checksum(&xsdt);
    mem.write_slice(&xsdt, GuestAddress(xsdt_addr))
        .map_err(|_| Error::WriteXsdt)?;

    // FADT (276 bytes, revision 6, HW_REDUCED_ACPI)
    let mut fadt = vec![0u8; FADT_LEN];
    let hdr = acpi_header(b"FACP", FADT_LEN as u32, 6, b"KRUNFACP");
    fadt[0..36].copy_from_slice(&hdr);
    fadt[FADT_FLAGS_OFFSET..FADT_FLAGS_OFFSET + 4].copy_from_slice(&HW_REDUCED_ACPI.to_le_bytes());
    fadt[FADT_X_DSDT_OFFSET..FADT_X_DSDT_OFFSET + 8].copy_from_slice(&dsdt_addr.to_le_bytes());
    fadt[9] = acpi_checksum(&fadt);
    mem.write_slice(&fadt, GuestAddress(fadt_addr))
        .map_err(|_| Error::WriteFadt)?;

    // MADT with LAPIC + IOAPIC entries
    let madt = build_madt(num_cpus);
    mem.write_slice(&madt, GuestAddress(madt_addr))
        .map_err(|_| Error::WriteMadt)?;

    // DSDT with LNRO0005 device entries
    let dsdt = build_dsdt(devices);
    mem.write_slice(&dsdt, GuestAddress(dsdt_addr))
        .map_err(|_| Error::WriteDsdt)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use vm_memory::GuestMemoryMmap;

    #[test]
    fn test_acpi_checksum() {
        let data = [0x01, 0x02, 0x03];
        let cksum = acpi_checksum(&data);
        assert_eq!(
            data[0]
                .wrapping_add(data[1])
                .wrapping_add(data[2])
                .wrapping_add(cksum),
            0
        );
    }

    #[test]
    fn test_pkg_length_one_byte() {
        let pkg = aml_pkg_length(10);
        assert_eq!(pkg.len(), 1);
        assert_eq!(pkg[0], 11);
    }

    #[test]
    fn test_pkg_length_two_bytes() {
        let pkg = aml_pkg_length(100);
        assert_eq!(pkg.len(), 2);
        let total = 102;
        assert_eq!(pkg[0], 0x40 | (total & 0x0F) as u8);
        assert_eq!(pkg[1], ((total >> 4) & 0xFF) as u8);
    }

    #[test]
    fn test_build_dsdt_single_device() {
        let devices = vec![(0xd000_0000u64, 5u32, 0x1000u64)];
        let dsdt = build_dsdt(&devices);

        assert_eq!(&dsdt[0..4], b"DSDT");
        let len = u32::from_le_bytes(dsdt[4..8].try_into().unwrap());
        assert_eq!(len as usize, dsdt.len());

        let sum: u8 = dsdt.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);
    }

    #[test]
    fn test_build_madt() {
        let madt = build_madt(4);

        assert_eq!(&madt[0..4], b"APIC");
        let len = u32::from_le_bytes(madt[4..8].try_into().unwrap());
        assert_eq!(len as usize, madt.len());
        // header(36) + lapic_addr(4) + flags(4) + 4*lapic(32) + ioapic(12) = 88
        assert_eq!(madt.len(), 88);

        let sum: u8 = madt.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);
    }

    #[test]
    fn test_setup_acpi_tables() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10_0000)]).unwrap();
        let devices = vec![
            (0xd000_0000u64, 5u32, 0x1000u64),
            (0xd000_1000u64, 6u32, 0x1000u64),
        ];
        setup_acpi_tables(&mem, 4, &devices).unwrap();

        let mut rsdp = [0u8; 36];
        mem.read_slice(&mut rsdp, GuestAddress(RSDP_ADDR)).unwrap();
        assert_eq!(&rsdp[0..8], b"RSD PTR ");
        let sum: u8 = rsdp[0..20].iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);
        let sum: u8 = rsdp.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);

        let xsdt_addr = RSDP_ADDR + XSDT_OFFSET;
        let mut xsdt = [0u8; 52];
        mem.read_slice(&mut xsdt, GuestAddress(xsdt_addr)).unwrap();
        assert_eq!(&xsdt[0..4], b"XSDT");
        let sum: u8 = xsdt.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);

        let fadt_addr = RSDP_ADDR + FADT_OFFSET;
        let mut fadt = [0u8; FADT_LEN];
        mem.read_slice(&mut fadt, GuestAddress(fadt_addr)).unwrap();
        assert_eq!(&fadt[0..4], b"FACP");
        let sum: u8 = fadt.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);

        let madt_addr = RSDP_ADDR + MADT_OFFSET;
        let mut madt = [0u8; 88];
        mem.read_slice(&mut madt, GuestAddress(madt_addr)).unwrap();
        assert_eq!(&madt[0..4], b"APIC");
        let sum: u8 = madt.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);
    }
}
