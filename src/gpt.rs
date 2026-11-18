//! The GPT of one UFS LUN, enough to read and swap partition type GUIDs.
//!
//! gptman parses and serialises the tables. Its GPT::write_into writes the
//! primary copy first and assumes where the backup entries sit, so both copies
//! are written here from their own on-disk headers, backup first, each synced,
//! so a torn write leaves a valid primary for the loaders.

use gptman::{GPTHeader, GPTPartitionEntry};
use std::fs::{File, OpenOptions};
use std::io::{Error, ErrorKind, Result, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub struct Gpt {
    path: PathBuf,
    sector: u64,
    primary: GPTHeader,
    backup: GPTHeader,
    entries: Vec<GPTPartitionEntry>,
}

fn bad(path: &Path, what: impl std::fmt::Display) -> Error {
    Error::new(ErrorKind::InvalidData, format!("{}: {what}", path.display()))
}

/// Turns the on-disk mixed-endian GUID into its usual text form.
pub fn guid_text(b: &[u8; 16]) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{}",
        b[3], b[2], b[1], b[0], b[5], b[4], b[7], b[6], b[8], b[9],
        b[10..16].iter().map(|x| format!("{x:02x}")).collect::<String>()
    )
}

/// Reads one header at `lba` and its entries, both CRC checked by gptman.
fn read_copy(f: &mut File, path: &Path, sector: u64, lba: u64) -> Result<(GPTHeader, Vec<GPTPartitionEntry>)> {
    f.seek(SeekFrom::Start(lba * sector))?;
    let header = GPTHeader::read_from(f).map_err(|e| bad(path, format!("GPT header at LBA {lba}: {e}")))?;
    let entries = header
        .read_partitions(f, sector)
        .map_err(|e| bad(path, format!("GPT entries of LBA {lba}: {e}")))?;
    Ok((header, entries))
}

impl Gpt {
    /// Reads both copies and insists they agree, rather than trusting either one alone.
    pub fn open(path: &Path, sector: u64) -> Result<Gpt> {
        let mut f = File::open(path)?;
        let (primary, entries) = read_copy(&mut f, path, sector, 1)?;
        let (backup, backup_entries) = read_copy(&mut f, path, sector, primary.backup_lba)?;
        if entries != backup_entries {
            return Err(bad(path, "primary and backup GPT entries differ"));
        }
        Ok(Gpt { path: path.to_path_buf(), sector, primary, backup, entries })
    }

    pub fn type_guid(&self, num: u32) -> String {
        guid_text(&self.entries[num as usize - 1].partition_type_guid)
    }

    /// Partition names and numbers, for finding the A/B pairs.
    #[cfg(test)]
    pub fn names(&self) -> Vec<(String, u32)> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.is_used())
            .map(|(i, e)| (e.partition_name.as_str().to_string(), i as u32 + 1))
            .collect()
    }

    pub fn swap(&mut self, a: u32, b: u32) {
        let (a, b) = (a as usize - 1, b as usize - 1);
        let t = self.entries[a].partition_type_guid;
        self.entries[a].partition_type_guid = self.entries[b].partition_type_guid;
        self.entries[b].partition_type_guid = t;
    }

    /// Writes the backup copy, then the primary, so a torn write leaves one valid copy.
    pub fn write(&mut self) -> Result<()> {
        let mut f = OpenOptions::new().read(true).write(true).open(&self.path)?;
        for h in [&mut self.backup, &mut self.primary] {
            h.write_into(&mut f, self.sector, &self.entries)
                .map_err(|e| bad(&self.path, format!("writing the GPT at LBA {}: {e}", h.primary_lba)))?;
            f.sync_all()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::FileExt;

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
            let n = (len - off).min(6 * 4096) as usize;
            let (mut x, mut y) = (vec![0u8; n], vec![0u8; n]);
            orig.read_exact_at(&mut x, off).unwrap();
            back.read_exact_at(&mut y, off).unwrap();
            assert_eq!(x, y, "GPT differs at offset {off}");
        }
        std::fs::remove_file(&work).unwrap();
        eprintln!("swapped {} pairs and back", pairs.len());
    }
}
