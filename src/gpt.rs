//! Just enough GPT to read and swap partition type GUIDs on one UFS LUN.

use std::fs::{File, OpenOptions};
use std::io::{Error, ErrorKind, Result};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const O_SYNC: i32 = 0o4010000;

struct Header {
    raw: Vec<u8>,
    lba: u64,
    entries_lba: u64,
}

pub struct Gpt {
    path: PathBuf,
    sector: u64,
    entries: Vec<u8>,
    entry_size: usize,
    primary: Header,
    backup: Header,
}

fn bad(path: &Path, what: &str) -> Error {
    Error::new(ErrorKind::InvalidData, format!("{}: {}", path.display(), what))
}

fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}

fn header_crc(raw: &[u8]) -> u32 {
    let size = u32_at(raw, 12) as usize;
    let mut h = raw[..size].to_vec();
    h[16..20].fill(0);
    crc32fast::hash(&h)
}

/// Turns the on-disk mixed-endian GUID into its usual text form.
pub fn guid_text(b: &[u8]) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{}",
        b[3], b[2], b[1], b[0], b[5], b[4], b[7], b[6], b[8], b[9],
        b[10..16].iter().map(|x| format!("{x:02x}")).collect::<String>()
    )
}

impl Gpt {
    pub fn open(path: &Path, sector: u64) -> Result<Gpt> {
        let f = File::open(path)?;
        let (primary, entries, entry_size) = Self::read(&f, path, sector, 1)?;
        let backup_lba = u64_at(&primary.raw, 32);
        let (backup, backup_entries, _) = Self::read(&f, path, sector, backup_lba)?;
        if entries != backup_entries {
            return Err(bad(path, "primary and backup GPT entries differ"));
        }
        Ok(Gpt { path: path.to_path_buf(), sector, entries, entry_size, primary, backup })
    }

    fn read(f: &File, path: &Path, sector: u64, lba: u64) -> Result<(Header, Vec<u8>, usize)> {
        let mut raw = vec![0u8; sector as usize];
        f.read_exact_at(&mut raw, lba * sector)?;
        if &raw[..8] != b"EFI PART" {
            return Err(bad(path, &format!("no GPT header at LBA {lba}")));
        }
        if header_crc(&raw) != u32_at(&raw, 16) {
            return Err(bad(path, &format!("bad GPT header CRC at LBA {lba}")));
        }
        let entries_lba = u64_at(&raw, 72);
        let count = u32_at(&raw, 80) as usize;
        let entry_size = u32_at(&raw, 84) as usize;
        let mut entries = vec![0u8; count * entry_size];
        f.read_exact_at(&mut entries, entries_lba * sector)?;
        if crc32fast::hash(&entries) != u32_at(&raw, 88) {
            return Err(bad(path, &format!("bad GPT entries CRC at LBA {lba}")));
        }
        Ok((Header { raw, lba, entries_lba }, entries, entry_size))
    }

    fn entry(&self, num: u32) -> &[u8] {
        let off = (num as usize - 1) * self.entry_size;
        &self.entries[off..off + self.entry_size]
    }

    pub fn type_guid(&self, num: u32) -> String {
        guid_text(&self.entry(num)[..16])
    }

    /// Partition names and numbers, for finding the A/B pairs.
    #[cfg(test)]
    pub fn names(&self) -> Vec<(String, u32)> {
        let mut out = Vec::new();
        for (i, e) in self.entries.chunks(self.entry_size).enumerate() {
            if e[..16].iter().all(|&b| b == 0) {
                continue;
            }
            let utf16: Vec<u16> = e[56..128]
                .chunks(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .take_while(|&c| c != 0)
                .collect();
            out.push((String::from_utf16_lossy(&utf16), i as u32 + 1));
        }
        out
    }

    pub fn swap(&mut self, a: u32, b: u32) {
        let (oa, ob) = ((a as usize - 1) * self.entry_size, (b as usize - 1) * self.entry_size);
        for i in 0..16 {
            self.entries.swap(oa + i, ob + i);
        }
    }

    /// Writes the backup GPT, then the primary, so a torn write leaves one valid copy.
    pub fn write(&mut self) -> Result<()> {
        let f = OpenOptions::new().read(true).write(true).custom_flags(O_SYNC).open(&self.path)?;
        let crc = crc32fast::hash(&self.entries);
        for h in [&mut self.backup, &mut self.primary] {
            h.raw[88..92].copy_from_slice(&crc.to_le_bytes());
            let hcrc = header_crc(&h.raw);
            h.raw[16..20].copy_from_slice(&hcrc.to_le_bytes());
            f.write_all_at(&self.entries, h.entries_lba * self.sector)?;
            f.write_all_at(&h.raw, h.lba * self.sector)?;
            f.sync_all()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Swaps every A/B pair of a GPT image and back, set IMDT_BOOT_GPT_IMAGE to run.
    #[test]
    fn swap_round_trip() {
        let Ok(img) = std::env::var("IMDT_BOOT_GPT_IMAGE") else { return };
        let work = format!("{img}.test");
        std::fs::copy(&img, &work).unwrap();
        let path = Path::new(&work);
        let mut g = Gpt::open(path, 4096).unwrap();
        let names: std::collections::HashMap<_, _> = g.names().into_iter().collect();
        let pairs: Vec<(u32, u32)> = names
            .iter()
            .filter_map(|(n, &a)| n.strip_suffix("_a").and_then(|b| names.get(&format!("{b}_b"))).map(|&b| (a, b)))
            .filter(|&(a, b)| g.type_guid(a) != g.type_guid(b))
            .collect();
        let before: Vec<_> = pairs.iter().map(|&(a, b)| (g.type_guid(a), g.type_guid(b))).collect();
        for &(a, b) in &pairs {
            g.swap(a, b);
        }
        g.write().unwrap();
        let mut g = Gpt::open(path, 4096).unwrap();
        for (&(a, b), (ga, gb)) in pairs.iter().zip(&before) {
            assert_eq!((&g.type_guid(a), &g.type_guid(b)), (gb, ga));
            g.swap(a, b);
        }
        g.write().unwrap();
        let len = std::fs::metadata(&img).unwrap().len();
        let (orig, back) = (File::open(&img).unwrap(), File::open(&work).unwrap());
        for off in [0, len - 5 * 4096] {
            let (mut x, mut y) = (vec![0u8; 6 * 4096], vec![0u8; 6 * 4096]);
            let n = (len - off).min(6 * 4096) as usize;
            orig.read_exact_at(&mut x[..n], off).unwrap();
            back.read_exact_at(&mut y[..n], off).unwrap();
            assert_eq!(x, y, "GPT differs at offset {off}");
        }
        std::fs::remove_file(&work).unwrap();
        eprintln!("swapped {} pairs and back", pairs.len());
    }
}
