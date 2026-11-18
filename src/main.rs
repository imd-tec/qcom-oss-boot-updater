//! Check and update the boot firmware against an OSTree deployment's manifest.
//!
//! The boot firmware (xbl, tz, hyp, uefi, NON-HLOS, ...) lives on A/B raw
//! partitions outside the OSTree rootfs, so an OSTree update leaves it alone.
//! Each deployment ships /usr/lib/imdt-boot/manifest and the matching images.
//!
//! A/B selection follows the Qualcomm UFS scheme: xbl and xbl_config sit on
//! boot LUNs 1 (slot a) and 2 (slot b) and bBootLunEn picks one, while every
//! other pair shares a LUN and the loaders pick the partition with the real
//! type GUID, the other one carrying the "inactive" GUID.

mod gpt;

use gpt::Gpt;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{exit, Command};

const BOOT_DIR: &str = "usr/lib/imdt-boot";
const UFS_BSG: &str = "/dev/bsg/ufs-bsg0";
const CHUNK: usize = 4 << 20;
const INACTIVE_GUID: &str = "77036cd4-03d5-42bb-8ed1-37e5a88baa34";
const BOOTED_SLOT: &str = "/run/imdt-boot/booted-slot";

const USAGE: &str = "usage: imdt-boot <command> [options]

  status [--pending | --root DIR]   slot in use and which partitions match
  check  [--pending | --root DIR] [--warn-only]
                                    warn when the booted slot differs
  update [--pending | --root DIR] [--images BUNDLE] --yes
                                    write the other slot and switch to it
  switch --yes                      boot the other slot next time, or
                                    cancel a pending switch
  record                            save the booted slot, run at boot

--pending uses the deployment ostree boots next, after ostree-imdt-update.
--images takes the firmware bundle, imdt-boot-<machine>.tar.zst, when the
image does not carry the firmware images, or a directory of <image>.zst.";

type Res<T> = Result<T, String>;

fn ctx<T, E: std::fmt::Display>(r: Result<T, E>, what: impl std::fmt::Display) -> Res<T> {
    r.map_err(|e| format!("{what}: {e}"))
}

// --- manifest -------------------------------------------------------------

#[derive(PartialEq)]
struct Entry {
    base: String,
    image: String,
    size: u64,
    sha: String,
}

struct Manifest {
    dir: PathBuf,
    version: String,
    entries: Vec<Entry>,
}

impl Manifest {
    fn load(root: &Path) -> Res<Manifest> {
        let dir = root.join(BOOT_DIR);
        let path = dir.join("manifest");
        let text = ctx(fs::read_to_string(&path), path.display())?;
        Manifest::parse(dir, &text, &path.display().to_string())
    }

    fn parse(dir: PathBuf, text: &str, what: &str) -> Res<Manifest> {
        let mut m = Manifest { dir, version: "unknown".into(), entries: Vec::new() };
        for line in text.lines().map(str::trim) {
            if let Some(v) = line.strip_prefix("# version ") {
                m.version = v.to_string();
            }
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let f: Vec<&str> = line.split_whitespace().collect();
            let [base, image, size, sha] = f[..] else {
                return Err(format!("{what}: bad line '{line}'"));
            };
            let size = ctx(size.parse(), format!("{what}: size"))?;
            m.entries.push(Entry { base: base.into(), image: image.into(), size, sha: sha.into() });
        }
        Ok(m)
    }
}

fn deployment_root(opts: &Opts) -> Res<PathBuf> {
    if let Some(r) = &opts.root {
        return Ok(r.clone());
    }
    if !opts.pending {
        return Ok(PathBuf::from("/"));
    }
    let out = ctx(Command::new("ostree").args(["admin", "status"]).output(), "ostree admin status")?;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() >= 3 && (f[2] == "(pending)" || f[2] == "(staged)") {
            return Ok(PathBuf::from(format!("/ostree/deploy/{}/deploy/{}", f[0], f[1])));
        }
    }
    Err("no pending deployment, run ostree-imdt-update first".into())
}

// --- partitions -----------------------------------------------------------

#[derive(Clone)]
struct Part {
    dev: PathBuf,
    disk: String,
    num: u32,
    size: u64,
}

fn read_sys(path: impl AsRef<Path>) -> Res<String> {
    let p = path.as_ref();
    Ok(ctx(fs::read_to_string(p), p.display())?.trim().to_string())
}

fn part(label: &str) -> Res<Option<Part>> {
    let link = Path::new("/dev/disk/by-partlabel").join(label);
    let Ok(dev) = fs::canonicalize(&link) else { return Ok(None) };
    let name = dev.file_name().unwrap().to_string_lossy().to_string();
    let sysfs = ctx(fs::canonicalize(format!("/sys/class/block/{name}")), &name)?;
    let disk = sysfs.parent().unwrap().file_name().unwrap().to_string_lossy().to_string();
    let num = ctx(read_sys(sysfs.join("partition"))?.parse(), format!("{name} partition"))?;
    let sectors: u64 = ctx(read_sys(sysfs.join("size"))?.parse(), format!("{name} size"))?;
    Ok(Some(Part { dev, disk, num, size: sectors * 512 }))
}

fn open_gpt(disk: &str) -> Res<Gpt> {
    let sector = ctx(read_sys(format!("/sys/block/{disk}/queue/logical_block_size"))?.parse(), disk)?;
    ctx(Gpt::open(Path::new(&format!("/dev/{disk}")), sector), disk)
}

/// Every X_a/X_b pair on the device.
fn ab_pairs() -> Res<BTreeMap<String, (Part, Part)>> {
    let mut pairs = BTreeMap::new();
    for e in ctx(fs::read_dir("/dev/disk/by-partlabel"), "/dev/disk/by-partlabel")? {
        let label = e.map_err(|e| e.to_string())?.file_name().to_string_lossy().to_string();
        if let Some(base) = label.strip_suffix("_a") {
            if let (Some(a), Some(b)) = (part(&label)?, part(&format!("{base}_b"))?) {
                pairs.insert(base.to_string(), (a, b));
            }
        }
    }
    Ok(pairs)
}

// --- slots ----------------------------------------------------------------

fn boot_lun_path() -> Res<PathBuf> {
    for e in ctx(fs::read_dir("/sys/bus/platform/devices"), "/sys/bus/platform/devices")?.flatten() {
        let p = e.path().join("attributes/boot_lun_enabled");
        if e.file_name().to_string_lossy().contains("ufshc") && p.exists() {
            return Ok(p);
        }
    }
    Err("no UFS boot LUN attribute, A/B update needs UFS".into())
}

fn boot_lun() -> Res<u32> {
    let v = read_sys(boot_lun_path()?)?;
    ctx(u32::from_str_radix(v.trim_start_matches("0x"), 16), "boot_lun_enabled")
}

/// The slot the next boot uses.
fn next_slot() -> Res<char> {
    match boot_lun()? {
        1 => Ok('a'),
        2 => Ok('b'),
        n => Err(format!("unexpected bBootLunEn {n}")),
    }
}

/// The slot this boot came from, saved by `record` before anything switched.
fn booted_slot() -> Res<char> {
    match fs::read_to_string(BOOTED_SLOT) {
        Ok(s) if s.trim() == "a" || s.trim() == "b" => Ok(s.trim().chars().next().unwrap()),
        _ => next_slot(),
    }
}

/// Saves the booted slot once per boot, so later switches cannot hide it.
fn record_booted() -> Res<char> {
    if Path::new(BOOTED_SLOT).exists() {
        return booted_slot();
    }
    let slot = next_slot()?;
    let dir = Path::new(BOOTED_SLOT).parent().unwrap();
    ctx(fs::create_dir_all(dir), dir.display())?;
    ctx(fs::write(BOOTED_SLOT, format!("{slot}\n")), BOOTED_SLOT)?;
    Ok(slot)
}

fn other(slot: char) -> char {
    if slot == 'a' { 'b' } else { 'a' }
}

/// The GUIDs must agree with bBootLunEn, or a switch would mix the slots.
fn check_slot_state(slot: char) -> Res<()> {
    let mut gpts: BTreeMap<String, Gpt> = BTreeMap::new();
    let mut bad = Vec::new();
    for (base, (a, b)) in ab_pairs()? {
        if a.disk != b.disk {
            continue;
        }
        if !gpts.contains_key(&a.disk) {
            gpts.insert(a.disk.clone(), open_gpt(&a.disk)?);
        }
        let g = &gpts[&a.disk];
        let booted = if slot == 'a' { a.num } else { b.num };
        if g.type_guid(a.num) != g.type_guid(b.num) && g.type_guid(booted) == INACTIVE_GUID {
            bad.push(base);
        }
    }
    if bad.is_empty() {
        Ok(())
    } else {
        Err(format!("slot {slot} is booted but these are not marked active: {}", bad.join(" ")))
    }
}

fn switch_slots(to: char) -> Res<()> {
    let mut gpts: BTreeMap<String, Gpt> = BTreeMap::new();
    for (_, (a, b)) in ab_pairs()? {
        if a.disk != b.disk {
            continue;
        }
        if !gpts.contains_key(&a.disk) {
            gpts.insert(a.disk.clone(), open_gpt(&a.disk)?);
        }
        let g = gpts.get_mut(&a.disk).unwrap();
        if g.type_guid(a.num) != g.type_guid(b.num) {
            g.swap(a.num, b.num);
        }
    }
    for (disk, g) in gpts.iter_mut() {
        ctx(g.write(), format!("writing the GPT of {disk}"))?;
    }
    let lun = if to == 'a' { 1 } else { 2 };
    let st = ctx(
        Command::new("ufs-utils").args(["attr", "-t", "0", "-w", &format!("{lun:#x}"), "-p", UFS_BSG]).status(),
        "ufs-utils",
    )?;
    if !st.success() || boot_lun()? != lun {
        return Err(format!("could not set bBootLunEn to {lun}, the GPTs already point at slot {to}"));
    }
    Ok(())
}

// --- hashing and writing --------------------------------------------------

fn sha256_dev(dev: &Path, size: u64) -> Res<String> {
    let mut f = ctx(File::open(dev), dev.display())?.take(size);
    let mut h = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let mut done = 0;
    loop {
        let n = ctx(f.read(&mut buf), dev.display())?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        done += n as u64;
    }
    if done != size {
        return Ok(String::new());
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Writes everything `src` yields to `dev`, which must be `size` bytes.
fn write_from(src: &mut dyn Read, what: &str, dev: &Path, size: u64) -> Res<()> {
    let mut out = ctx(OpenOptions::new().write(true).open(dev), dev.display())?;
    let mut buf = vec![0u8; CHUNK];
    let mut done = 0u64;
    loop {
        let n = ctx(src.read(&mut buf), what)?;
        if n == 0 {
            break;
        }
        ctx(out.write_all(&buf[..n]), dev.display())?;
        done += n as u64;
    }
    ctx(out.sync_all(), dev.display())?;
    if done != size {
        return Err(format!("{what}: {done} bytes, the manifest says {size}"));
    }
    Ok(())
}

fn zstd_reader(path: &Path) -> Res<impl Read> {
    let f = ctx(File::open(path), path.display())?;
    ctx(ruzstd::decoding::StreamingDecoder::new(f), path.display())
}

/// The next member of a ustar stream, as its name and size, or None at the end.
fn tar_next(r: &mut dyn Read, what: &str) -> Res<Option<(String, u64)>> {
    let mut h = [0u8; 512];
    ctx(r.read_exact(&mut h), what)?;
    if h.iter().all(|&b| b == 0) {
        return Ok(None);
    }
    let field = |a: usize, b: usize| String::from_utf8_lossy(&h[a..b]).trim_end_matches('\0').to_string();
    let size = ctx(u64::from_str_radix(field(124, 136).trim(), 8), format!("{what}: tar size"))?;
    let name = field(0, 100);
    let name = name.strip_prefix("./").unwrap_or(&name).to_string();
    if h[156] != b'0' && h[156] != 0 {
        return Err(format!("{what}: {name} is not a regular file"));
    }
    Ok(Some((name, size)))
}

/// Skips the rest of a member and its padding to the next 512-byte block.
fn tar_skip(r: &mut dyn Read, left: u64, size: u64, what: &str) -> Res<()> {
    let pad = (512 - size % 512) % 512;
    ctx(std::io::copy(&mut r.take(left + pad), &mut std::io::sink()), what)?;
    Ok(())
}

fn copy_part(src: &Path, dst: &Path) -> Res<()> {
    let mut i = ctx(File::open(src), src.display())?;
    let mut o = ctx(OpenOptions::new().write(true).open(dst), dst.display())?;
    ctx(std::io::copy(&mut i, &mut o), dst.display())?;
    ctx(o.sync_all(), dst.display())
}

/// Each manifest entry's state on one slot: ok, differs, missing or too big.
fn compare(m: &Manifest, slot: char) -> Res<Vec<&'static str>> {
    m.entries
        .iter()
        .map(|e| {
            Ok(match part(&format!("{}_{slot}", e.base))? {
                None => "missing",
                Some(p) if e.size > p.size => "too big",
                Some(p) if sha256_dev(&p.dev, e.size)? == e.sha => "ok",
                Some(_) => "differs",
            })
        })
        .collect()
}

// --- commands -------------------------------------------------------------

#[derive(Default)]
struct Opts {
    pending: bool,
    root: Option<PathBuf>,
    yes: bool,
    warn_only: bool,
    images: Option<PathBuf>,
}

fn status(o: &Opts) -> Res<i32> {
    let m = Manifest::load(&deployment_root(o)?)?;
    let slot = booted_slot()?;
    let (cur, oth) = (compare(&m, slot)?, compare(&m, other(slot))?);
    println!("manifest version: {}", m.version);
    println!("booted slot: {slot}, next boot: {}", next_slot()?);
    println!("{:16} {:10} {:10}", "partition", format!("slot {slot}"), format!("slot {}", other(slot)));
    for (i, e) in m.entries.iter().enumerate() {
        println!("{:16} {:10} {:10}", e.base, cur[i], oth[i]);
    }
    Ok(0)
}

fn check(o: &Opts) -> Res<i32> {
    let m = Manifest::load(&deployment_root(o)?)?;
    let slot = booted_slot()?;
    let drift: Vec<_> = m.entries.iter().zip(compare(&m, slot)?).filter(|(_, s)| *s != "ok").collect();
    if drift.is_empty() {
        println!("imdt-boot: boot firmware matches {} (slot {slot})", m.version);
        return Ok(0);
    }
    let which = if o.pending { "the pending deployment" } else { "this deployment" };
    println!("imdt-boot: WARNING: boot firmware on slot {slot} does not match {which} ({})", m.version);
    for (e, s) in &drift {
        println!("imdt-boot: WARNING:   {}_{slot}: {s}", e.base);
    }
    let flag = if o.pending { " --pending" } else { "" };
    println!("imdt-boot: WARNING: run 'imdt-boot update{flag} --yes' or flash the release bundle with QDL");
    Ok(if o.warn_only { 0 } else { 1 })
}

/// Streams a manifest-first tar.zst bundle straight onto the partitions in `todo`.
fn write_bundle(
    m: &Manifest,
    path: &Path,
    todo: &[(&Entry, String, Part)],
    verify: &dyn Fn(&Entry, &str, &Part) -> Res<()>,
) -> Res<()> {
    let what = path.display().to_string();
    let mut r = zstd_reader(path)?;
    let text = match tar_next(&mut r, &what)? {
        Some((name, size)) if name == "manifest" => {
            let mut t = String::new();
            ctx((&mut r).take(size).read_to_string(&mut t), &what)?;
            tar_skip(&mut r, 0, size, &what)?;
            t
        }
        _ => return Err(format!("{what}: does not start with a manifest")),
    };
    let theirs = Manifest::parse(PathBuf::new(), &text, &what)?;
    if theirs.entries != m.entries {
        return Err(format!("{what} holds {}, this deployment needs {}", theirs.version, m.version));
    }
    let mut done = 0;
    while let Some((name, size)) = tar_next(&mut r, &what)? {
        match todo.iter().find(|(e, _, _)| e.image == name) {
            Some((e, label, p)) => {
                println!("  {label}: writing {name}");
                write_from(&mut (&mut r).take(size), &format!("{what}: {name}"), &p.dev, e.size)?;
                tar_skip(&mut r, 0, size, &what)?;
                verify(e, label, p)?;
                done += 1;
            }
            None => tar_skip(&mut r, size, size, &what)?,
        }
    }
    if done != todo.len() {
        return Err(format!("{what}: {} of {} images missing", todo.len() - done, todo.len()));
    }
    Ok(())
}

fn update(o: &Opts) -> Res<i32> {
    let m = Manifest::load(&deployment_root(o)?)?;
    let slot = record_booted()?;
    if next_slot()? != slot {
        return Err(format!("slot {} already boots next, reboot or run 'imdt-boot switch --yes' first", other(slot)));
    }
    check_slot_state(slot)?;
    let target = other(slot);
    println!("imdt-boot: booted slot {slot}, writing {} to slot {target}", m.version);
    if !o.yes {
        return Err("this rewrites the boot firmware, pass --yes to go ahead".into());
    }
    let mut todo = Vec::new();
    for e in &m.entries {
        let label = format!("{}_{target}", e.base);
        let p = match part(&label)? {
            Some(p) if p.size >= e.size => p,
            _ => return Err(format!("{label}: missing or smaller than {}", e.image)),
        };
        if sha256_dev(&p.dev, e.size)? == e.sha {
            println!("  {label}: already {}", e.image);
        } else {
            todo.push((e, label, p));
        }
    }
    let verify = |e: &Entry, label: &str, p: &Part| -> Res<()> {
        if sha256_dev(&p.dev, e.size)? != e.sha {
            return Err(format!("{label}: verify failed, slot {slot} is still the boot slot"));
        }
        Ok(())
    };
    match &o.images {
        Some(b) if b.is_file() => write_bundle(&m, b, &todo, &verify)?,
        dir => {
            let dir = dir.clone().unwrap_or_else(|| m.dir.join("images"));
            for (e, label, p) in &todo {
                let src = dir.join(format!("{}.zst", e.image));
                if !src.exists() {
                    return Err(format!("{} is missing, pass the firmware bundle with --images", src.display()));
                }
                println!("  {label}: writing {}", e.image);
                write_from(&mut zstd_reader(&src)?, &src.display().to_string(), &p.dev, e.size)?;
                verify(e, label, p)?;
            }
        }
    }
    // Partitions without an image (licences, DRM data) follow the booted slot.
    for (base, (a, b)) in ab_pairs()? {
        if m.entries.iter().any(|e| e.base == base) {
            continue;
        }
        let (src, dst) = if slot == 'a' { (a, b) } else { (b, a) };
        if src.size != dst.size {
            println!("  {base}: the slots differ in size, left alone");
            continue;
        }
        println!("  {base}_{target}: copying from {base}_{slot}");
        copy_part(&src.dev, &dst.dev)?;
    }
    switch_slots(target)?;
    println!("imdt-boot: slot {target} boots next, reboot to apply. Once it is up, 'imdt-boot switch --yes' goes back to slot {slot}.");
    Ok(0)
}

fn switch(o: &Opts) -> Res<i32> {
    let booted = record_booted()?;
    let next = next_slot()?;
    check_slot_state(next)?;
    let to = other(next);
    if !o.yes {
        return Err(format!("this makes slot {to} the boot slot, pass --yes to go ahead"));
    }
    switch_slots(to)?;
    if to == booted {
        println!("imdt-boot: switch cancelled, slot {to} boots next as before");
    } else {
        println!("imdt-boot: slot {to} boots next, reboot to apply");
    }
    Ok(0)
}

fn record(_: &Opts) -> Res<i32> {
    println!("imdt-boot: booted from slot {}", record_booted()?);
    Ok(0)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else {
        eprintln!("{USAGE}");
        exit(2);
    };
    let mut o = Opts::default();
    let mut it = args[1..].iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--pending" => o.pending = true,
            "--root" => o.root = it.next().map(PathBuf::from),
            "--images" => o.images = it.next().map(PathBuf::from),
            "--yes" => o.yes = true,
            "--warn-only" => o.warn_only = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                exit(0);
            }
            _ => {
                eprintln!("imdt-boot: unknown option {a}\n{USAGE}");
                exit(2);
            }
        }
    }
    let run = match cmd.as_str() {
        "status" => status,
        "check" => check,
        "update" => update,
        "switch" => switch,
        "record" => record,
        "-h" | "--help" => {
            println!("{USAGE}");
            exit(0);
        }
        _ => {
            eprintln!("imdt-boot: unknown command {cmd}\n{USAGE}");
            exit(2);
        }
    };
    match run(&o) {
        Ok(code) => exit(code),
        Err(e) => {
            eprintln!("imdt-boot: {e}");
            exit(2);
        }
    }
}
