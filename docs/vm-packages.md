# VM packages: how a VM gets onto the phone

mothershipOS builds no VM. A VM is a repository of its own (e.g.
[pixelOS_16_VM](https://github.com/MothershipOS/pixelOS_16_VM),
[ubuntu_26.04_desktop_VM](https://github.com/MothershipOS/ubuntu_26.04_desktop_VM)); mothershipOS installs it on the
phone from that repository, building it there, or from a package that was built elsewhere:

```
mship-system vm-install <repository | package> [--name <id>]
```

- a **repository**: a git URL, or a directory. mothershipOS clones it and runs its `mothership-build`, which makes
  the package; the clone and everything the build left are removed afterwards. Use this when the VM can be built on
  the phone (Ubuntu: Ubuntu's cloud image plus a seed).
- a **package**, prebuilt: an archive (a URL or a file: `.tar`, `.tar.gz`, `.tar.xz`) or a directory. Use this when
  the VM cannot be built on the phone (PixelOS needs an AOSP tree and an x86 PC): its repository builds the archive
  and publishes it, e.g. as a release asset.

A [VM store](https://github.com/MothershipOS/vmstore) hosts packages for download, one folder per VM
(`<id>/package.tar.xz`, `info.txt`, an icon); its page shows each VM's `vm-install` command. The shell's VM store
(Home › VM store) lists one, by default mothershipOS's own, https://vmstore.buzz-ltd.com (another in the page's
Store row): search, install, and updates (mothershipd checks the store every 6 hours; an installed VM whose version
differs from the store's has one). A store's package may
also be a VM repository's source as an archive (with its `mothership-build`): it is built on the phone like a clone.

Running it again with a newer repository or package updates the installed VM; the user's data stays (see `keep`).

## The package
A directory (or an archive of one) with:

| File | What |
|---|---|
| `vm.json` | the VM's manifest (`crates/mship-core/src/manifest.rs`); it must run on the installing release |
| `mothership-package.json` | how to install it (below) |
| the files the manifest names | its disks, kernel, initrd, ... (`boot.*`: paths inside the package) |

`mothership-package.json`:

```json
{
  "id": "pixelos",
  "version": "16.0.0",
  "keep": ["disk.img"],
  "install": "pixelos-install"
}
```

| Key | What |
|---|---|
| `id` | the VM's directory on the phone, `/var/lib/mothership/vms/<id>` (lowercase letters, digits, `-`; `--name` overrides it) |
| `version` | the package's version: shown on the phone, and compared with a store's to offer updates (the store's `info.txt` should carry the same; the VM repositories' release tasks write it there) |
| `keep` | paths an update leaves as they are when the VM already has them: the user's disk, a used seed. Everything else the package has replaces the installed copy; files the package does not have (logs, runtime state) stay |
| `install` | optional: a program in the package that finishes the install in the VM's directory: `<install> install` the first time, `<install> update` after an update (PixelOS: makes the disk from its images, or writes the new images into an installed disk keeping the user's data) |

## The repository's build
A repository that can be built on the phone has an executable `mothership-build` at its root:
`mothership-build <out dir>` makes the package in `<out dir>`. It runs in the clone, as the session user, with a
small environment: `PATH`, `HOME`, `MSHIP_RELEASE` (the installing release), `MSHIP_VM_SSH_KEY` (the host's public
key for ssh into VMs, to authorize in the guest), `MSHIP_UPDATE=1` when the VM is installed already (the build may
then leave out what its package keeps) and every `MSHIP_*` setting of the caller (e.g. `MSHIP_VM_DISK`).
It can count on a POSIX shell with coreutils, `tar` (gzip, xz), `curl` or `wget`, `git` and `python3`. What it
needs from the network it downloads itself; what needs a PC (a kernel, packages) it takes from its own repository's
releases.

## Rules
- The package's paths stay inside it (no `/`, no `..`): `keep`, `install`, and vm.json's `boot.*` and `log` (a file
  name).
- Only regular files and directories are installed: a package with a symlink is refused, one with a device node,
  FIFO or socket is refused before anything runs, setuid and setgid bits are removed.
- A package gets no root: its build and its install step run as the session user, like the VM itself.
- The VM must be stopped to update it.
- Nothing of the VM's is installed outside its directory.
- A VM runs on the phone through the contracts only (`docs/contracts/`): no phone drivers in it. Guest-side
  drivers for the contracts are mothershipOS's (`guest/drivers`), as source the VM builds in.
