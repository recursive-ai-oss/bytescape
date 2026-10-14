# Mount table fixtures

Inputs for the mount table tests in `bs-platform`. None of them is a
byte-for-byte recording of a machine, and the tests rely only on the shape of
the lines, not on any one machine's contents.

| File | Origin |
| --- | --- |
| `ubuntu-desktop.txt` | Modelled line by line on a recorded Ubuntu desktop table: sysfs, proc, devtmpfs, snap squashfs loops, tmpfs, three ext4 disks, the EFI partition, a desktop portal, snapd namespaces and container overlays. Host names, user names and container ids are changed or shortened. **Two lines are synthetic**: the bind mount of a folder of `/home` at `/srv/projects` (mount id 2600) and the USB stick at `/media/alice/USB STICK` (mount id 2700). |
| `btrfs-subvolumes.txt` | **Synthetic.** Written from the documented mountinfo format and the usual Ubuntu `@`, `@home`, `@log`, `@snapshots` layout. Each subvolume mount has its own anonymous device number, as btrfs reports them, and the top-level mount (subvolume id 5) is added at `/data`. Replace with a recording from a btrfs machine when one is available. |
| `fedora.txt` | **Synthetic.** Written from the documented format and Fedora's usual layout (btrfs `root` and `home` subvolumes on an encrypted device, ext4 `/boot`, desktop FUSE mounts, a USB disk, NFS and SMB shares). |
| `wsl.txt` | **Synthetic.** Written from the documented format and what WSL 2 is known to mount (an ext4 root, 9p drive mounts, system mounts under `/usr/lib/wsl`). |
| `escapes.txt` | **Synthetic.** Mount points that need escaping: space, tab, newline, backslash and UTF-8 text. |

Real recordings are better test data than these. `cat /proc/self/mountinfo`
on the machine, change names that identify people, and add the file here with
a row in this table that says it was recorded.
