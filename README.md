# qcom-oss-boot-updater

`imdt-boot` keeps the A/B boot firmware of the IMDT Qualcomm SBCs in step
with the [meta-imdt-qcom-oss](https://github.com/imd-tec/meta-imdt-qcom-oss)
OSTree image it runs.

The boot firmware (xbl, tz, hyp, uefi, NON-HLOS, ...) lives on raw
partitions outside the OSTree rootfs, so an OSTree update does not change it.
Each image ships a manifest of the firmware it was built with under
`/usr/lib/imdt-boot/`, and usually the images themselves too. `imdt-boot`
compares the board against that manifest and can write the firmware to the
slot that is not running, then switch to it.

## Commands

```
imdt-boot status [--pending | --root DIR]   booted slot and which partitions match
imdt-boot check  [--pending | --root DIR] [--warn-only]
                                            warn when the booted slot differs
imdt-boot update [--pending | --root DIR] [--images BUNDLE] --yes
                                            write the other slot and switch to it
imdt-boot switch --yes                      boot the other slot next time, or
                                            cancel a pending switch
imdt-boot record                            save the booted slot, run at boot
```

`--pending` uses the deployment OSTree boots next, so after
`ostree-imdt-update` you can check, and update, before rebooting:

```
ostree-imdt-update
imdt-boot check --pending
imdt-boot update --pending --yes
reboot
```

An image built without the firmware images carries only the manifest. Then
pass the firmware bundle the build deploys, `imdt-boot-<machine>.tar.zst`:

```
imdt-boot update --pending --images imdt-boot-imdt-8550-sbc.tar.zst --yes
```

The bundle is streamed straight onto the partitions, so the board needs no
room to unpack it. It must hold the same firmware as the deployment's
manifest, or nothing is written. `--images` also takes a directory of
`<image>.zst` files.

Once the board is up on the new slot, `imdt-boot switch --yes` goes back to
the old firmware, which stays on the other slot. The first boot after a
switch can reset once more before it comes up.

## How the slots work

This follows the Qualcomm UFS A/B scheme:

- `xbl`, `xbl_config` and the `multiimg` partitions sit on boot LUN 1
  (slot a) and LUN 2 (slot b). The UFS `bBootLunEn` attribute picks one.
- Every other pair sits on one LUN. The loaders find partitions by type
  GUID, and the inactive slot carries the "inactive" GUID
  `77036cd4-03d5-42bb-8ed1-37e5a88baa34`.

A switch swaps the type GUIDs of each pair, in both the backup and the
primary GPT, then sets `bBootLunEn` with `ufs-utils`. The tables are parsed
and serialised by [gptman](https://crates.io/crates/gptman). Both copies must
pass their CRC checks and agree before anything is written, and they are
written backup first, each synced, so a torn write leaves a valid primary.

An update writes each image to the other slot and reads it back to check
its sha256. Pairs without an image, such as `qweslicstore`, are copied from
the booted slot. Only then does it switch. If anything fails before the
switch, the booted slot is untouched.

Only UFS boards are supported. If the new slot does not boot at all, the
board needs a QDL flash.

## Manifest

`scripts/imdt-boot-mkmanifest` builds the manifest and the zstd-compressed
images from the `rawprogram*.xml` files of a qcomflash bundle. With
`--bundle` it also writes the firmware bundle, a ustar archive of the
manifest followed by the raw images, compressed as one zstd stream.

```
# imdt-boot manifest
# version v1.7.0 BOOT.MXF 2.1.1
# partition image size sha256
tz tz.mbn 4075952 ef5123b6...
```

## Building

```
cargo build --release
cargo test    # IMDT_BOOT_GPT_IMAGE=<disk image> also runs the GPT round trip
```
