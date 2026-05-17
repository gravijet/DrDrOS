//! drdr-store — DrDrOS persistent storage.
//!
//! The whole OS lives in RAM (a gzipped cpio the kernel unpacks into a
//! tmpfs). That is great for "boot fast, leave no trace" and terrible
//! for "I wrote a document and want it next time". This crate is the
//! bridge: discover the machine's real disks, mount one the user picks,
//! and expose one tiny `save`/`load` API plus a *persisted* idea of
//! where files go — so DrDrEdit / Notes write to spinning rust (or NVMe)
//! instead of vanishing on reboot.
//!
//! Everything here fails *soft*. No disk, not root, an unknown
//! filesystem — none of it panics or blocks; it just falls back to a
//! clearly-labelled RAM directory. A storage layer must never be the
//! reason a desktop won't come up.
//!
//! Layout we manage on a chosen volume:
//!
//! ```text
//!   <volume>/.drdros/        marker — "DrDrOS keeps data here"
//!   <volume>/Documents/      what save()/load()/list_documents() use
//! ```
//!
//! And, valid only for the current boot (RAM):
//!
//! ```text
//!   /run/drdros/datadir      a line of text: the absolute data root
//! ```

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use nix::mount::{MsFlags, mount, umount};

// ─── Block device discovery ──────────────────────────────────────────

/// One row of `/proc/partitions`: a kernel block device and its size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockDev {
    /// Kernel name, e.g. `sda1`, `nvme0n1p2`, `mmcblk0p1`, `vda`.
    pub name: String,
    /// Size in 1 KiB blocks (the unit `/proc/partitions` reports).
    pub blocks: u64,
    /// A partition (something we can mount) vs. a whole disk / loop /
    /// ram device.
    pub partition: bool,
    /// `/sys/block/.../removable` == 1 (a USB stick, an SD card).
    pub removable: bool,
    /// Where it is mounted right now, if anywhere (from `/proc/mounts`).
    pub mountpoint: Option<String>,
}

impl BlockDev {
    pub fn size_mb(&self) -> u64 {
        self.blocks / 1024
    }

    /// `/dev/<name>` — the node `mount(2)` wants.
    pub fn dev_path(&self) -> String {
        format!("/dev/{}", self.name)
    }
}

/// A parsed `/proc/mounts` line (only the fields we use).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountInfo {
    pub source: String,
    pub target: String,
    pub fstype: String,
}

/// Filesystems that are *not* persistent storage — a data dir living on
/// one of these is RAM and will not survive a reboot.
const EPHEMERAL_FS: &[&str] = &[
    "tmpfs", "ramfs", "rootfs", "devtmpfs", "proc", "sysfs", "cgroup",
    "cgroup2", "overlay", "squashfs", "debugfs", "tracefs", "devpts",
];

/// Filesystems we will try, in order, when mounting a user-chosen
/// partition we know nothing about. The kernel rejects the wrong ones
/// quickly (EINVAL) so probing is cheap and safe.
const TRY_FS: &[&str] =
    &["ext4", "ext3", "ext2", "vfat", "exfat", "ntfs3", "ntfs", "iso9660", "btrfs", "xfs"];

/// Heuristic: does this kernel device name look like a *partition*
/// (mountable) rather than a whole disk or a pseudo device?
///
/// - `sda` → disk, `sda1` → partition
/// - `nvme0n1` → disk, `nvme0n1p2` → partition
/// - `mmcblk0` → disk, `mmcblk0p1` → partition
/// - `loop*`, `ram*`, `dm-*`, `sr*`, `fd*`, `zram*` → skip
pub fn looks_like_partition(name: &str) -> bool {
    // Pseudo / non-mountable devices.
    for pre in ["loop", "ram", "zram", "dm-", "fd", "sr", "md"] {
        if name.starts_with(pre) {
            return false;
        }
    }
    // mmc / nvme / loop family: the *disk* is `mmcblk0` / `nvme0n1`; a
    // **partition** appends `p<N>` (`mmcblk0p1`, `nvme0n1p2`). Require a
    // digit before the `p` so a hypothetical bare `…p1` isn't misread.
    if let Some(pos) = name.rfind('p') {
        let num = &name[pos + 1..];
        if !num.is_empty()
            && num.bytes().all(|b| b.is_ascii_digit())
            && name[..pos].bytes().any(|b| b.is_ascii_digit())
        {
            return true;
        }
    }
    // SCSI / virtio / IDE / Xen family: the *disk* is all letters
    // (`sda`, `vdb`, `xvda`); a **partition** is letters then digits
    // (`sda1`, `vdb3`, `xvda12`).
    for pre in ["sd", "vd", "hd", "xvd"] {
        if let Some(rest) = name.strip_prefix(pre) {
            let letters = rest
                .bytes()
                .take_while(|b| b.is_ascii_alphabetic())
                .count();
            let digits = &rest[letters..];
            if letters > 0
                && !digits.is_empty()
                && digits.bytes().all(|b| b.is_ascii_digit())
            {
                return true;
            }
        }
    }
    false
}

/// Pure parser for `/proc/partitions` (split out so it's unit-tested
/// without a procfs).
pub fn parse_partitions(text: &str) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    for line in text.lines().skip(1) {
        let mut it = line.split_whitespace();
        let (_maj, _min, blocks, name) =
            match (it.next(), it.next(), it.next(), it.next()) {
                (Some(a), Some(b), Some(c), Some(d)) => (a, b, c, d),
                _ => continue,
            };
        if let Ok(blocks) = blocks.parse::<u64>() {
            out.push((name.to_string(), blocks));
        }
    }
    out
}

/// Pure parser for `/proc/mounts` / `/proc/self/mountinfo`-style lines
/// (space separated: source target fstype options …).
pub fn parse_mounts(text: &str) -> Vec<MountInfo> {
    text.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some(MountInfo {
                source: it.next()?.to_string(),
                target: unescape_mount(it.next()?),
                fstype: it.next()?.to_string(),
            })
        })
        .collect()
}

/// `/proc/mounts` octal-escapes spaces etc. as `\040`. Decode the few
/// that matter so a mountpoint with a space still compares correctly.
fn unescape_mount(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() {
            if let Ok(code) = u8::from_str_radix(&s[i + 1..i + 4], 8) {
                out.push(code as char);
                i += 4;
                continue;
            }
        }
        out.push(b[i] as char);
        i += 1;
    }
    out
}

fn read_removable(name: &str) -> bool {
    // /sys/block/<disk>/removable. Partitions don't have it; consult the
    // parent disk by trimming the partition suffix.
    let disk = strip_partition_suffix(name);
    fs::read_to_string(format!("/sys/block/{disk}/removable"))
        .map(|s| s.trim() == "1")
        .unwrap_or(false)
}

/// `sda1` → `sda`, `nvme0n1p2` → `nvme0n1`, `mmcblk0p1` → `mmcblk0`.
fn strip_partition_suffix(name: &str) -> String {
    if let Some(pos) = name.rfind('p') {
        if name[pos + 1..].chars().all(|c| c.is_ascii_digit())
            && !name[pos + 1..].is_empty()
            && name[..pos].chars().any(|c| c.is_ascii_digit())
        {
            return name[..pos].to_string();
        }
    }
    name.trim_end_matches(|c: char| c.is_ascii_digit()).to_string()
}

/// Enumerate the machine's block devices, cross-referenced with what is
/// currently mounted. Returns `[]` (never an error) if `/proc` is
/// unreadable — callers just show "no disks".
pub fn list_block_devices() -> Vec<BlockDev> {
    let parts = fs::read_to_string("/proc/partitions")
        .map(|t| parse_partitions(&t))
        .unwrap_or_default();
    let mounts = current_mounts();
    parts
        .into_iter()
        .map(|(name, blocks)| {
            let dev = format!("/dev/{name}");
            let mountpoint = mounts
                .iter()
                .find(|m| m.source == dev)
                .map(|m| m.target.clone());
            BlockDev {
                partition: looks_like_partition(&name),
                removable: read_removable(&name),
                mountpoint,
                name,
                blocks,
            }
        })
        .collect()
}

/// Everything mounted right now.
pub fn current_mounts() -> Vec<MountInfo> {
    fs::read_to_string("/proc/mounts")
        .map(|t| parse_mounts(&t))
        .unwrap_or_default()
}

// ─── Mount / unmount ─────────────────────────────────────────────────

/// Mount `dev` (e.g. `/dev/sda1`) at `target`, probing filesystem types
/// until one sticks. Returns the fstype that worked.
///
/// Read-write is attempted first; if the driver refuses (a dirty NTFS,
/// a read-only medium) we retry read-only so the user can at least see
/// their files. Requires root — under DrDrOS the desktop is spawned by
/// PID 1, so it is root.
pub fn mount_device(dev: &str, target: &str) -> io::Result<String> {
    fs::create_dir_all(target)?;
    let mut last = io::Error::other("no filesystem type matched");
    for fs_ty in TRY_FS {
        for flags in [MsFlags::empty(), MsFlags::MS_RDONLY] {
            match mount(
                Some(dev),
                target,
                Some(*fs_ty),
                flags,
                Option::<&str>::None,
            ) {
                Ok(()) => return Ok((*fs_ty).to_string()),
                Err(e) => last = io::Error::from(e),
            }
        }
    }
    Err(last)
}

/// Unmount whatever is at `target` (best effort).
pub fn unmount(target: &str) -> io::Result<()> {
    umount(target).map_err(io::Error::from)
}

// ─── The data directory (where save/load go) ─────────────────────────

/// RAM fallback — the rootfs is a tmpfs, so this exists and is writable
/// on every boot, but it does NOT survive a reboot. Clearly named so
/// the UI can warn the user.
const RAM_DATA: &str = "/root/DrDrOS-Data";

/// Per-boot pointer (RAM): the absolute path the user chose this session.
const POINTER: &str = "/run/drdros/datadir";

/// The current data root. Resolution order:
///   1. the per-boot pointer the user set via [`set_data_dir`];
///   2. a still-mounted volume that carries our `.drdros` marker
///      (so a disk used last session is auto-adopted);
///   3. the RAM fallback (non-persistent).
///
/// The directory is created if missing; this never fails to *return* a
/// path (worst case the RAM one, which always works).
pub fn data_dir() -> PathBuf {
    if let Ok(p) = fs::read_to_string(POINTER) {
        let p = PathBuf::from(p.trim());
        if !p.as_os_str().is_empty() && fs::create_dir_all(&p).is_ok() {
            return p;
        }
    }
    if let Some(p) = discover_marked_volume() {
        let _ = write_pointer(&p);
        return p;
    }
    let _ = fs::create_dir_all(RAM_DATA);
    PathBuf::from(RAM_DATA)
}

/// Adopt `dir` as the data root for this boot, and drop a `.drdros`
/// marker so a future boot rediscovers it automatically.
pub fn set_data_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let marker = dir.join(".drdros");
    let _ = fs::create_dir_all(&marker);
    let _ = fs::write(marker.join("config"), b"datadir\n");
    write_pointer(dir)
}

fn write_pointer(dir: &Path) -> io::Result<()> {
    fs::create_dir_all("/run/drdros")?;
    fs::write(POINTER, format!("{}\n", dir.display()))
}

/// Scan currently-mounted, non-ephemeral filesystems for one that
/// already has a `.drdros` marker — "this disk was a DrDrOS data disk".
fn discover_marked_volume() -> Option<PathBuf> {
    for m in current_mounts() {
        if EPHEMERAL_FS.contains(&m.fstype.as_str()) {
            continue;
        }
        let cand = Path::new(&m.target).join(".drdros");
        if cand.is_dir() {
            return Some(PathBuf::from(&m.target));
        }
    }
    None
}

/// Is the current data dir on persistent media (true) or RAM (false)?
pub fn data_is_persistent() -> bool {
    let dir = data_dir();
    let dir = dir.to_string_lossy();
    // The mount whose target is the longest prefix of `dir` owns it.
    let mut best: Option<MountInfo> = None;
    for m in current_mounts() {
        if dir.starts_with(&m.target)
            && best
                .as_ref()
                .map(|b| m.target.len() > b.target.len())
                .unwrap_or(true)
        {
            best = Some(m);
        }
    }
    best.map(|m| !EPHEMERAL_FS.contains(&m.fstype.as_str()))
        .unwrap_or(false)
}

// ─── Documents API used by apps ──────────────────────────────────────

/// `<data_dir>/Documents`, created on demand.
pub fn documents_dir() -> PathBuf {
    let d = data_dir().join("Documents");
    let _ = fs::create_dir_all(&d);
    d
}

/// Save `bytes` as `name` under the documents dir. Returns the full
/// path actually written (handy for a "saved to …" status line).
pub fn save(name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    let safe = sanitize(name);
    let path = documents_dir().join(&safe);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, bytes)?;
    Ok(path)
}

/// Read a document previously [`save`]d.
pub fn load(name: &str) -> io::Result<Vec<u8>> {
    fs::read(documents_dir().join(sanitize(name)))
}

/// Every file currently in the documents dir (names only, sorted).
pub fn list_documents() -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(documents_dir())
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().is_file())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// Reduce a user-supplied document name to a safe *basename*: take the
/// last path component, drop leading dots (no `..`, no dotfiles
/// escaping the dir), and strip NULs. Anything that collapses to
/// nothing becomes `untitled.txt`. This makes `save("../../etc/passwd")`
/// land at `<documents>/passwd`, never outside the documents dir.
fn sanitize(name: &str) -> String {
    let base = name
        .rsplit(['/', '\\'])
        .find(|s| !s.is_empty())
        .unwrap_or("");
    let base: String = base
        .trim()
        .trim_start_matches('.')
        .chars()
        .filter(|&c| c != '\0')
        .collect();
    if base.is_empty() {
        "untitled.txt".to_string()
    } else {
        base
    }
}

// ─── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const PARTITIONS: &str = "\
major minor  #blocks  name

   8        0  500107608 sda
   8        1     524288 sda1
   8        2  499582976 sda2
 259        0  976762584 nvme0n1
 259        1    1048576 nvme0n1p1
 259        2  975713984 nvme0n1p2
   7        0     125360 loop0
 179        0   31166976 mmcblk0
 179        1   31162880 mmcblk0p1
";

    #[test]
    fn parses_partitions_table() {
        let p = parse_partitions(PARTITIONS);
        assert_eq!(p.len(), 9);
        assert_eq!(p[0], ("sda".to_string(), 500107608));
        assert_eq!(p[4], ("nvme0n1p1".to_string(), 1048576));
    }

    #[test]
    fn whole_disks_vs_partitions() {
        assert!(!looks_like_partition("sda"));
        assert!(looks_like_partition("sda1"));
        assert!(!looks_like_partition("nvme0n1"));
        assert!(looks_like_partition("nvme0n1p2"));
        assert!(!looks_like_partition("mmcblk0"));
        assert!(looks_like_partition("mmcblk0p1"));
        assert!(!looks_like_partition("loop0"));
        assert!(!looks_like_partition("sr0"));
        assert!(!looks_like_partition("zram0"));
    }

    #[test]
    fn strips_partition_suffix_to_parent_disk() {
        assert_eq!(strip_partition_suffix("sda1"), "sda");
        assert_eq!(strip_partition_suffix("nvme0n1p2"), "nvme0n1");
        assert_eq!(strip_partition_suffix("mmcblk0p1"), "mmcblk0");
        assert_eq!(strip_partition_suffix("vdb3"), "vdb");
    }

    #[test]
    fn parses_mounts_and_unescapes_spaces() {
        let m = parse_mounts(
            "/dev/sda1 /mnt/My\\040Disk ext4 rw 0 0\nproc /proc proc rw 0 0\n",
        );
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].source, "/dev/sda1");
        assert_eq!(m[0].target, "/mnt/My Disk");
        assert_eq!(m[0].fstype, "ext4");
        assert_eq!(m[1].fstype, "proc");
    }

    #[test]
    fn sanitize_blocks_traversal() {
        // Path components are stripped — the write can't escape the
        // documents dir, and `..` never survives.
        assert_eq!(sanitize("../../etc/passwd"), "passwd");
        assert_eq!(sanitize("notes.txt"), "notes.txt");
        assert_eq!(sanitize("   "), "untitled.txt");
        assert_eq!(sanitize("a/b\\c"), "c");
        assert_eq!(sanitize("../"), "untitled.txt");
        assert_eq!(sanitize(".hidden"), "hidden");
    }

    #[test]
    fn ephemeral_classification() {
        assert!(EPHEMERAL_FS.contains(&"tmpfs"));
        assert!(EPHEMERAL_FS.contains(&"rootfs"));
        assert!(!EPHEMERAL_FS.contains(&"ext4"));
    }

    #[test]
    fn block_device_size_and_path() {
        let d = BlockDev {
            name: "sda1".into(),
            blocks: 2 * 1024 * 1024,
            partition: true,
            removable: true,
            mountpoint: None,
        };
        assert_eq!(d.size_mb(), 2048);
        assert_eq!(d.dev_path(), "/dev/sda1");
    }
}
