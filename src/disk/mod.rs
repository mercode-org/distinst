mod mounts;
mod operations;
mod partitions;
mod serial;
mod swaps;

use self::mounts::Mounts;
use self::operations::*;
pub use self::partitions::{FileSystemType, PartitionBuilder, PartitionInfo, PartitionType};
use self::serial::get_serial_no;
pub use self::swaps::Swaps;
use libparted::{Device, DeviceType, Disk as PedDisk, DiskType as PedDiskType};
pub use libparted::PartitionFlag;
use mount::{swapoff, umount};
use std::ffi::OsString;
use std::io;
use std::iter::FromIterator;
use std::path::{Path, PathBuf};
use std::str;

/// Defines a variety of errors that may arise from configuring and committing changes to disks.
#[derive(Debug, Fail)]
pub enum DiskError {
    #[fail(display = "unable to get device: {}", why)] DeviceGet {
        why: io::Error,
    },
    #[fail(display = "unable to probe for devices")] DeviceProbe,
    #[fail(display = "unable to commit changes to disk: {}", why)]
    DiskCommit {
        why: io::Error,
    },
    #[fail(display = "unable to format partition table: {}", why)]
    DiskFresh {
        why: io::Error,
    },
    #[fail(display = "unable to find disk")] DiskGet,
    #[fail(display = "unable to open disk: {}", why)] DiskNew {
        why: io::Error,
    },
    #[fail(display = "unable to sync disk changes with OS: {}", why)]
    DiskSync {
        why: io::Error,
    },
    #[fail(display = "serial model does not match")] InvalidSerial,
    #[fail(display = "failed to create partition geometry: {}", why)]
    GeometryCreate {
        why: io::Error,
    },
    #[fail(display = "failed to duplicate partition geometry")] GeometryDuplicate,
    #[fail(display = "failed to set values on partition geometry")] GeometrySet,
    #[fail(display = "partition layout on disk has changed")] LayoutChanged,
    #[fail(display = "unable to get mount points: {}", why)]
    MountsObtain {
        why: io::Error,
    },
    #[fail(display = "new partition could not be found")] NewPartNotFound,
    #[fail(display = "no file system was found on the partition")] NoFilesystem,
    #[fail(display = "unable to create partition: {}", why)]
    PartitionCreate {
        why: io::Error,
    },
    #[fail(display = "unable to format partition: {}", why)]
    PartitionFormat {
        why: io::Error,
    },
    #[fail(display = "partition {} not be found on disk", partition)]
    PartitionNotFound {
        partition: i32,
    },
    #[fail(display = "partition overlaps other partitions")] PartitionOverlaps,
    #[fail(display = "unable to remove partition {}: {}", partition, why)]
    PartitionRemove {
        partition: i32,
        why:       io::Error,
    },
    #[fail(display = "unable to resize partition")] PartitionResize,
    #[fail(display = "partition table not found on disk")] PartitionTableNotFound,
    #[fail(display = "too many primary partitions in MSDOS partition table")]
    PrimaryPartitionsExceeded,
    #[fail(display = "sector overlaps partition {}", id)] SectorOverlaps {
        id: i32,
    },
    #[fail(display = "unable to get serial model of device: {}", why)]
    SerialGet {
        why: io::Error,
    },
    #[fail(display = "partition exceeds size of disk")] PartitionOOB,
    #[fail(display = "partition resize value is too small")] ResizeTooSmall,
    #[fail(display = "unable to unmount partition(s): {}", why)]
    Unmount {
        why: io::Error,
    },
}

impl From<DiskError> for io::Error {
    fn from(err: DiskError) -> io::Error {
        io::Error::new(io::ErrorKind::Other, format!("{}", err))
    }
}

/// Bootloader type
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Bootloader {
    Bios,
    Efi,
}

impl Bootloader {
    pub fn detect() -> Bootloader {
        if Path::new("/sys/firmware/efi").is_dir() {
            Bootloader::Efi
        } else {
            Bootloader::Bios
        }
    }
}

/// Specifies whether the partition table on the disk is **MSDOS** or **GPT**.
#[derive(Debug, PartialEq, Clone, Copy, Hash)]
pub enum PartitionTable {
    Msdos,
    Gpt,
}

/// Used with the `Disk::get_sector` method for converting a more human-readable unit
/// into the corresponding sector for the given disk.
#[derive(Debug, PartialEq, Clone, Copy, Hash)]
pub enum Sector {
    /// The first sector in the disk where partitions should be created.
    Start,
    /// The last sector in the disk where partitions should be created.
    End,
    /// A raw value that directly corrects to the exact number of sectors that will be used.
    Unit(u64),
    /// Rather than specifying the sector count, the user can specify the actual size in megabytes.
    /// This value will later be used to get the exact sector count based on the sector size.
    Megabyte(u64),
    /// The percent can be represented by specifying a value between 0 and
    /// u16::MAX, where u16::MAX is 100%.
    Percent(u16),
}

/// Gets a `libparted::Device` from the given name.
fn get_device<'a, P: AsRef<Path>>(name: P) -> Result<Device<'a>, DiskError> {
    Device::get(name).map_err(|why| DiskError::DeviceGet { why })
}

/// Gets and opens a `libparted::Device` from the given name.
fn open_device<'a, P: AsRef<Path>>(name: P) -> Result<Device<'a>, DiskError> {
    Device::new(name).map_err(|why| DiskError::DeviceGet { why })
}

/// Opens a `libparted::Disk` from a `libparted::Device`.
fn open_disk<'a>(device: &'a mut Device) -> Result<PedDisk<'a>, DiskError> {
    PedDisk::new(device).map_err(|why| DiskError::DiskNew { why })
}

/// Attempts to commit changes to the disk, return a `DiskError` on failure.
fn commit(disk: &mut PedDisk) -> Result<(), DiskError> {
    disk.commit().map_err(|why| DiskError::DiskCommit { why })
}

/// Flushes the OS cache, return a `DiskError` on failure.
fn sync(device: &mut Device) -> Result<(), DiskError> {
    device.sync().map_err(|why| DiskError::DiskSync { why })
}

/// Contains all of the information relevant to a given device.
///
/// # Note
///
/// The `device_path` field may be used for identification of the device in the system.
#[derive(Debug, Clone, PartialEq)]
pub struct Disk {
    /// The model name of the device, assigned by the manufacturer.
    pub model_name: String,
    /// A unique identifier to this disk.
    pub serial: String,
    /// The location in the file system where the block device is located.
    pub device_path: PathBuf,
    /// The size of the disk in sectors.
    pub size: u64,
    /// The size of sectors on the disk.
    pub sector_size: u64,
    /// The type of the device, such as SCSI.
    pub device_type: String,
    /// The partition table may be either **MSDOS** or **GPT**.
    pub table_type: Option<PartitionTable>,
    /// Whether the device is currently in a read-only state.
    pub read_only: bool,
    /// Defines whether the device should be wiped or not. The `table_type`
    /// field will be used to determine which table to write to the disk.
    pub mklabel: bool,
    /// The partitions that are stored on the device.
    pub partitions: Vec<PartitionInfo>,
}

impl Disk {
    fn new(device: &mut Device) -> Result<Disk, DiskError> {
        let model_name = device.model().into();
        let device_path = device.path().to_owned();
        let serial = get_serial_no(&device_path).map_err(|why| DiskError::SerialGet { why })?;
        let size = device.length();
        let sector_size = device.sector_size();
        let device_type = format!("{:?}", device.type_());
        let read_only = device.read_only();

        // Attempts to open the disk to obtain information regarding the partition table
        // and the partitions stored on the device.
        let disk = open_disk(device)?;

        // Checks whether there is a partition table, and if so, which kind.
        let table_type = disk.get_disk_type_name().and_then(|tn| match tn {
            "gpt" => Some(PartitionTable::Gpt),
            "msdos" => Some(PartitionTable::Msdos),
            _ => None,
        });

        // Determines if the table is of the msdos variable, as msdos partition
        // tables do not support partition names.
        let is_msdos = table_type.map_or(false, |tt| tt == PartitionTable::Msdos);

        Ok(Disk {
            model_name,
            device_path,
            serial,
            size,
            sector_size,
            device_type,
            read_only,
            table_type,
            mklabel: false,
            partitions: if table_type.is_some() {
                let mut partitions = Vec::new();
                for part in disk.parts() {
                    // skip invalid partitions (metadata / free)
                    if part.num() == -1 {
                        continue;
                    }

                    // grab partition info results
                    let part_result = PartitionInfo::new_from_ped(&part, is_msdos)
                        .map_err(|why| DiskError::MountsObtain { why })?;
                    if let Some(part) = part_result {
                        partitions.push(part);
                    }
                }
                partitions
            } else {
                Vec::new()
            },
        })
    }

    /// Obtains the disk that corresponds to a given device path.
    ///
    /// The `name` of the device should be a path, such as `/dev/sda`. If the device could
    /// not be found, then `Err(DiskError::DeviceGet)` will be returned.
    pub fn from_name<P: AsRef<Path>>(name: P) -> Result<Disk, DiskError> {
        get_device(name).and_then(|mut device| Disk::new(&mut device))
    }

    /// Obtains the disk that corresponds to a given serial model.
    ///
    /// First attempts to check if the supplied name has the valid serial number (highly likely),
    /// then performs a full probe of all disks in the system to attempt to find the matching
    /// serial number, in the event that the user swapped hard drive positions.
    ///
    /// If no match is found, then `Err(DiskError::DeviceGet)` is returned.
    fn from_name_with_serial<P: AsRef<Path>>(name: P, serial: &str) -> Result<Disk, DiskError> {
        Disk::from_name(name).and_then(|source| {
            if source.serial == serial {
                Ok(source)
            } else {
                // Attempt to find the serial model on another disk.
                Disks::probe_devices().and_then(|disks| {
                    disks
                        .0
                        .into_iter()
                        .find(|disk| disk.serial == serial)
                        .ok_or(DiskError::InvalidSerial)
                })
            }
        })
    }

    /// Calculates the requested sector from a given `Sector` variant.
    pub fn get_sector(&self, sector: Sector) -> u64 {
        const MIB2: u64 = 2 * 1024 * 1024;
        match sector {
            Sector::Start => MIB2 / self.sector_size,
            Sector::End => self.size - (MIB2 / self.sector_size),
            Sector::Megabyte(size) => (size * 1_000_000) / self.sector_size,
            Sector::Unit(size) => size,
            Sector::Percent(value) => {
                ((self.size * self.sector_size) / ::std::u16::MAX as u64) * value as u64
                    / self.sector_size
            }
        }
    }

    /// Obtain the number of primary and logical partitions, in that order.
    fn get_partition_type_count(&self) -> (usize, usize) {
        self.partitions
            .iter()
            .fold((0, 0), |sum, part| match part.part_type {
                PartitionType::Logical => (sum.0, sum.1 + 1),
                PartitionType::Primary => (sum.0 + 1, sum.1),
            })
    }

    /// Unmounts all partitions on the device
    pub fn unmount_all_partitions(&mut self) -> Result<(), io::Error> {
        info!("unmount all partitions on {}", self.path().display());

        for partition in &mut self.partitions {
            if let Some(ref mount) = partition.mount_point {
                info!(
                    "unmounting {}, which is mounted at {}",
                    partition.path().display(),
                    mount.display()
                );

                umount(mount, false)?;
            }

            partition.mount_point = None;

            if partition.swapped {
                info!("unswapping '{}'", partition.path().display(),);
                swapoff(&partition.path())?;
            }

            partition.swapped = false;
        }

        Ok(())
    }

    /// Drops all partitions in the in-memory disk representation, and marks that a new
    /// partition table should be written to the disk during the disk operations phase.
    pub fn mklabel(&mut self, kind: PartitionTable) -> Result<(), DiskError> {
        self.unmount_all_partitions()
            .map_err(|why| DiskError::Unmount { why })?;

        self.partitions.clear();
        self.mklabel = true;
        self.table_type = Some(kind);
        Ok(())
    }

    /// Adds a partition to the partition scheme.
    ///
    /// An error can occur if the partition will not fit onto the disk.
    pub fn add_partition(&mut self, builder: PartitionBuilder) -> Result<(), DiskError> {
        info!(
            "checking if {}:{} overlaps",
            builder.start_sector, builder.end_sector
        );
        // Ensure that the values aren't already contained within an existing partition.
        if let Some(id) = self.overlaps_region(builder.start_sector, builder.end_sector) {
            return Err(DiskError::SectorOverlaps { id });
        }

        // And that the end can fit onto the disk.
        if self.size < builder.end_sector as u64 {
            return Err(DiskError::PartitionOOB);
        }

        // Perform partition table & MSDOS restriction tests.
        match self.table_type {
            Some(PartitionTable::Gpt) => (),
            Some(PartitionTable::Msdos) => {
                let (primary, logical) = self.get_partition_type_count();
                if builder.part_type == PartitionType::Primary {
                    if primary == 4 || (primary == 3 && logical != 0) {
                        return Err(DiskError::PrimaryPartitionsExceeded);
                    }
                } else if primary == 4 {
                    return Err(DiskError::PrimaryPartitionsExceeded);
                }
            }
            None => return Err(DiskError::PartitionTableNotFound),
        }

        self.partitions.push(builder.build());

        Ok(())
    }

    /// Marks that the partition should be removed.
    ///
    /// Partitions marked as source partitions (pre-existing on disk) will have their `remove`
    /// field set to `true`, whereas all other theoretical partitions will simply be removed
    /// from the partition vector.
    pub fn remove_partition(&mut self, partition: i32) -> Result<(), DiskError> {
        let id = self.partitions
            .iter_mut()
            .enumerate()
            .find(|&(_, ref p)| p.number == partition)
            .ok_or(DiskError::PartitionNotFound { partition })
            .map(|(id, p)| {
                if p.is_source {
                    p.remove = true;
                    0
                } else {
                    id
                }
            })?;

        if id != 0 {
            self.partitions.remove(id);
        }

        Ok(())
    }

    /// Obtains a mutable reference to a partition within the partition scheme.
    pub fn get_partition_mut(&mut self, partition: i32) -> Option<&mut PartitionInfo> {
        self.partitions
            .iter_mut()
            .find(|part| part.number == partition)
    }

    /// Designates that the provided partition number should be resized to a specified length,
    /// and calculates whether it will be possible to do that.
    pub fn resize_partition(&mut self, partition: i32, length: u64) -> Result<(), DiskError> {
        let sector_size = self.sector_size;
        if length <= ((10 * 1024 * 1024) / sector_size) {
            return Err(DiskError::ResizeTooSmall);
        }

        let (backup, num, start, end);
        {
            let partition = self.get_partition_mut(partition)
                .ok_or(DiskError::PartitionNotFound { partition })?;

            backup = partition.end_sector;
            num = partition.number;
            start = partition.start_sector;
            end = start + length;
            partition.end_sector = end;
        }

        // Ensure that the new dimensions are not overlapping.
        if let Some(id) = self.overlaps_region_excluding(start, end, num) {
            let partition = self.get_partition_mut(partition).unwrap();
            partition.end_sector = backup;
            return Err(DiskError::SectorOverlaps { id });
        }

        Ok(())
    }

    /// Designates that the provided partition number should be moved to a specified sector,
    /// and calculates whether it will be possible to do that.
    pub fn move_partition(&mut self, partition: i32, start: u64) -> Result<(), DiskError> {
        let end = {
            let partition = self.get_partition_mut(partition)
                .ok_or(DiskError::PartitionNotFound { partition })?;

            if start == partition.start_sector {
                return Ok(());
            }

            if start > partition.start_sector {
                partition.end_sector + (start - partition.start_sector)
            } else {
                partition.end_sector - (partition.start_sector - start)
            }
        };

        if let Some(id) = self.overlaps_region_excluding(start, end, partition) {
            return Err(DiskError::SectorOverlaps { id });
        }

        let partition = self.get_partition_mut(partition).unwrap();

        partition.start_sector = start;
        partition.end_sector = end;
        Ok(())
    }

    /// Designates that the specified partition ID should be formatted with the given file
    /// system.
    pub fn format_partition(
        &mut self,
        partition: i32,
        fs: FileSystemType,
    ) -> Result<(), DiskError> {
        self.get_partition_mut(partition)
            .ok_or(DiskError::PartitionNotFound { partition })
            .map(|partition| {
                partition.format = true;
                partition.filesystem = Some(fs);
                ()
            })
    }

    /// Returns a partition ID if the given sector is within that partition.
    fn get_partition_at(&self, sector: u64) -> Option<i32> {
        self.partitions.iter()
            // Only consider partitions which are not set to be removed.
            .filter(|part| !part.remove)
            // Return upon the first partition where the sector is within the partition.
            .find(|part| sector >= part.start_sector && sector <= part.end_sector)
            // If found, return the partition number.
            .map(|part| part.number)
    }

    /// If a given start and end range overlaps a pre-existing partition, that
    /// partition's number will be returned to indicate a potential conflict.
    fn overlaps_region(&self, start: u64, end: u64) -> Option<i32> {
        self.partitions.iter()
            // Only consider partitions which are not set to be removed.
            .filter(|part| !part.remove)
            // Return upon the first partition where the sector is within the partition.
            .find(|part|
                !(
                    (start < part.start_sector && end < part.start_sector)
                    || (start > part.end_sector && end > part.end_sector)
                )
            )
            // If found, return the partition number.
            .map(|part| part.number)
    }

    /// If a given start and end range overlaps a pre-existing partition, that
    /// partition's number will be returned to indicate a potential conflict.
    ///
    /// Allows for a partition to be excluded from the search.
    fn overlaps_region_excluding(&self, start: u64, end: u64, exclude: i32) -> Option<i32> {
        self.partitions.iter()
            // Only consider partitions which are not set to be removed,
            // and are not to be excluded.
            .filter(|part| !part.remove && part.number != exclude)
            // Return upon the first partition where the sector is within the partition.
            .find(|part|
                !(
                    (start < part.start_sector && end < part.start_sector)
                    || (start > part.end_sector && end > part.end_sector)
                )
            )
            // If found, return the partition number.
            .map(|part| part.number)
    }

    /// Returns an error if the new disk does not contain the same source partitions.
    fn validate_layout(&self, new: &Disk) -> Result<(), DiskError> {
        if !new.mklabel {
            let mut new_parts = new.partitions.iter();
            for source in &self.partitions {
                match new_parts.next() {
                    Some(new) => if !source.is_same_partition_as(new) {
                        return Err(DiskError::LayoutChanged);
                    },
                    None => return Err(DiskError::LayoutChanged),
                }
            }
        }

        Ok(())
    }

    /// Compares the source disk's partition scheme to a possible new partition scheme.
    ///
    /// An error can occur if the layout of the new disk conflicts with the source.
    fn diff<'a>(&'a self, new: &Disk) -> Result<DiskOps<'a>, DiskError> {
        self.validate_layout(new)?;

        let mut remove_partitions = Vec::new();
        let mut change_partitions = Vec::new();
        let mut create_partitions = Vec::new();

        let mut new_parts = new.partitions.iter();
        let mut new_part = None;

        fn flags_diff<I: Iterator<Item = PartitionFlag>>(
            source: &[PartitionFlag],
            flags: I,
        ) -> Vec<PartitionFlag> {
            flags.filter(|f| !source.contains(f)).collect()
        }

        let mklabel = if new.mklabel {
            self.table_type
        } else {
            'outer: for source in &self.partitions {
                loop {
                    let mut next_part = new_part.take().or_else(|| new_parts.next());
                    if let Some(new) = next_part {
                        // Source partitions may be removed or changed.
                        if new.is_source {
                            if source.number != new.number {
                                unreachable!("layout validation: wrong number");
                            }

                            if new.remove {
                                remove_partitions.push(new.number);
                                continue 'outer;
                            }

                            if source.requires_changes(new) {
                                change_partitions.push(PartitionChange {
                                    num:    source.number,
                                    start:  new.start_sector,
                                    end:    new.end_sector,
                                    format: if new.format {
                                        new.filesystem
                                    } else {
                                        None
                                    },
                                    flags:  flags_diff(
                                        &source.flags,
                                        new.flags.clone().into_iter(),
                                    ),
                                });
                            }

                            continue 'outer;
                        } else {
                            // Non-source partitions should not be discovered at this stage.
                            unreachable!("layout validation: less sources");
                        }
                    }
                }
            }

            None
        };

        // Handle all of the non-source partitions, which are to be added to the disk.
        for partition in new_parts {
            if partition.is_source {
                unreachable!("layout validation: extra sources")
            }

            create_partitions.push(PartitionCreate {
                start_sector: partition.start_sector,
                end_sector:   partition.end_sector,
                file_system:  partition.filesystem.unwrap(),
                kind:         partition.part_type,
                flags:        partition.flags.clone(),
            });
        }

        Ok(DiskOps {
            mklabel,
            device_path: &self.device_path,
            remove_partitions,
            change_partitions,
            create_partitions,
        })
    }

    /// Attempts to commit all changes that have been made to the disk.
    pub fn commit(&mut self) -> Result<(), DiskError> {
        Disk::from_name_with_serial(&self.device_path, &self.serial).and_then(|source| {
            source.diff(self).and_then(|ops| {
                ops.remove()
                    .and_then(|ops| ops.change())
                    .and_then(|ops| ops.create())
            })
        })?;

        self.reload()
    }

    /// Reloads the disk information from the disk into our in-memory representation.
    pub fn reload(&mut self) -> Result<(), DiskError> {
        let mounts: Vec<(u64, PathBuf)> = self.partitions
            .iter()
            .filter_map(|p| match p.target {
                Some(ref path) => Some((p.start_sector, path.to_path_buf())),
                None => None,
            })
            .collect();

        *self = Disk::from_name_with_serial(&self.device_path, &self.serial)?;

        for (sector, mount) in mounts {
            let mut part = self.get_partition_at(sector)
                .and_then(|num| self.get_partition_mut(num))
                .expect("partition sectors are off");
            part.target = Some(mount);
        }

        Ok(())
    }

    pub fn path(&self) -> &Path { &self.device_path }
}

pub struct Disks(pub Vec<Disk>);

impl AsRef<[Disk]> for Disks {
    fn as_ref(&self) -> &[Disk] { &self.0 }
}

impl Disks {
    /// Probes for and returns disk information for every disk in the system.
    pub fn probe_devices() -> Result<Disks, DiskError> {
        let mut output: Vec<Disk> = Vec::new();
        for mut device in Device::devices(true) {
            match device.type_() {
                DeviceType::PED_DEVICE_UNKNOWN
                | DeviceType::PED_DEVICE_LOOP
                | DeviceType::PED_DEVICE_FILE => continue,
                _ => output.push(Disk::new(&mut device)?),
            }
        }

        Ok(Disks(output))
    }

    /// Finds the partition block path and associated partition information that is associated with
    /// the given target mount point.
    pub fn find_partition<'a>(&'a self, target: &Path) -> Option<(&'a Path, &'a PartitionInfo)> {
        for disk in self.as_ref() {
            for partition in &disk.partitions {
                if let Some(ref ptarget) = partition.target {
                    if ptarget == target {
                        return Some((&disk.device_path, partition));
                    }
                }
            }
        }

        None
    }

    /// Obtains the paths to the device and partition block paths where the root and EFI
    /// partitions are installed. The paths for the EFI partition will not be collected if
    /// the provided boot loader was of the EFI variety.
    pub fn get_base_partitions(
        &self,
        bootloader: Bootloader,
    ) -> ((&Path, &PartitionInfo), Option<(&Path, &PartitionInfo)>) {
        match bootloader {
            Bootloader::Bios => {
                let root = self.find_partition(Path::new("/")).expect(
                    "verify_partitions() should have ensured that a root partition was created",
                );

                (root, None)
            }
            Bootloader::Efi => {
                let efi = self.find_partition(Path::new("/boot/efi")).expect(
                    "verify_partitions() should have ensured that an EFI partition was created",
                );

                let root = self.find_partition(Path::new("/")).expect(
                    "verify_partitions() should have ensured that a root partition was created",
                );

                (root, Some(efi))
            }
        }
    }

    /// Ensures that EFI installs contain a `/boot/efi` and `/` partition, whereas MBR installs
    /// contain a `/` partition. Additionally, the EFI partition must have the ESP flag set.
    pub fn verify_partitions(&self, bootloader: Bootloader) -> io::Result<()> {
        let _root = self.find_partition(Path::new("/")).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "root partition was not defined",
            )
        })?;

        if bootloader == Bootloader::Efi {
            let efi = self.find_partition(Path::new("/boot/efi")).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "EFI partition was not defined")
            })?;

            if !efi.1.flags.contains(&PartitionFlag::PED_PARTITION_ESP) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "EFI partition did not have ESP flag set",
                ));
            }
        }

        Ok(())
    }

    /// Generates fstab entries in memory
    pub fn generate_fstab(&self) -> OsString {
        let mut fstab = OsString::with_capacity(1024);

        let fs_entries = self.as_ref()
            .iter()
            .flat_map(|disk| disk.partitions.iter())
            .filter_map(|part| part.get_block_info());

        // <file system>  <mount point>  <type>  <options>  <dump>  <pass>
        for entry in fs_entries {
            fstab.reserve_exact(entry.len() + 16);
            fstab.push("UUID=");
            fstab.push(&entry.uuid);
            fstab.push("  ");
            fstab.push(&entry.mount);
            fstab.push("  ");
            fstab.push(&entry.fs);
            fstab.push("  ");
            fstab.push(&entry.options);
            fstab.push("  ");
            fstab.push(if entry.dump { "1" } else { "0" });
            fstab.push("  ");
            fstab.push(if entry.pass { "1" } else { "0" });
            fstab.push("\n");
        }

        fstab.shrink_to_fit();
        fstab
    }
}

impl IntoIterator for Disks {
    type Item = Disk;
    type IntoIter = ::std::vec::IntoIter<Disk>;

    fn into_iter(self) -> Self::IntoIter { self.0.into_iter() }
}

impl FromIterator<Disk> for Disks {
    fn from_iter<I: IntoIterator<Item = Disk>>(iter: I) -> Self {
        Disks(iter.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get_default() -> Disks {
        Disks(vec![
            Disk {
                model_name:  "Test Disk".into(),
                serial:      "Test Disk 123".into(),
                device_path: "/dev/sdz".into(),
                size:        1953525168,
                sector_size: 512,
                device_type: "TEST".into(),
                table_type:  Some(PartitionTable::Gpt),
                read_only:   false,
                partitions:  vec![
                    PartitionInfo {
                        active:       true,
                        busy:         true,
                        is_source:    true,
                        remove:       false,
                        format:       false,
                        device_path:  Path::new("/dev/sdz1").to_path_buf(),
                        flags:        vec![],
                        mount_point:  Some(Path::new("/boot/efi").to_path_buf()),
                        target:       Some(Path::new("/boot/efi").to_path_buf()),
                        start_sector: 2048,
                        end_sector:   1026047,
                        filesystem:   Some(FileSystemType::Fat16),
                        name:         None,
                        number:       1,
                        part_type:    PartitionType::Primary,
                    },
                    PartitionInfo {
                        active:       true,
                        busy:         true,
                        is_source:    true,
                        remove:       false,
                        format:       false,
                        device_path:  Path::new("/dev/sdz2").to_path_buf(),
                        flags:        vec![],
                        mount_point:  Some(Path::new("/").to_path_buf()),
                        target:       Some(Path::new("/").to_path_buf()),
                        start_sector: 1026048,
                        end_sector:   420456447,
                        filesystem:   Some(FileSystemType::Btrfs),
                        name:         Some("Pop!_OS".into()),
                        number:       2,
                        part_type:    PartitionType::Primary,
                    },
                    PartitionInfo {
                        active:       false,
                        busy:         false,
                        is_source:    true,
                        remove:       false,
                        format:       false,
                        device_path:  Path::new("/dev/sdz3").to_path_buf(),
                        flags:        vec![],
                        mount_point:  None,
                        target:       None,
                        start_sector: 420456448,
                        end_sector:   1936738303,
                        filesystem:   Some(FileSystemType::Ext4),
                        name:         Some("Solus OS".into()),
                        number:       3,
                        part_type:    PartitionType::Primary,
                    },
                    PartitionInfo {
                        active:       true,
                        busy:         false,
                        is_source:    true,
                        remove:       false,
                        format:       false,
                        device_path:  Path::new("/dev/sdz4").to_path_buf(),
                        flags:        vec![],
                        mount_point:  None,
                        target:       None,
                        start_sector: 1936738304,
                        end_sector:   1953523711,
                        filesystem:   Some(FileSystemType::Swap),
                        name:         None,
                        number:       4,
                        part_type:    PartitionType::Primary,
                    },
                ],
            },
        ])
    }

    fn get_empty() -> Disks {
        Disks(vec![
            Disk {
                model_name:  "Test Disk".into(),
                serial:      "Test Disk 123".into(),
                device_path: "/dev/sdz".into(),
                size:        1953525168,
                sector_size: 512,
                device_type: "TEST".into(),
                table_type:  Some(PartitionTable::Gpt),
                read_only:   false,
                partitions:  Vec::new(),
            },
        ])
    }

    const GIB20: u64 = 41943040;

    // 500 MiB Fat16 partition.
    fn boot_part(start: u64) -> PartitionBuilder {
        PartitionBuilder::new(start, 1024_000 + start, FileSystemType::Fat16)
    }

    // 20 GiB Ext4 partition.
    fn root_part(start: u64) -> PartitionBuilder {
        PartitionBuilder::new(start, GIB20 + start, FileSystemType::Ext4)
    }

    #[test]
    fn layout_diff() {
        let source = get_default().0.into_iter().next().unwrap();
        let mut new = source.clone();
        new.remove_partition(1).unwrap();
        new.remove_partition(2).unwrap();
        new.format_partition(3, FileSystemType::Xfs).unwrap();
        new.resize_partition(3, GIB20).unwrap();
        new.remove_partition(4).unwrap();
        new.add_partition(boot_part(2048)).unwrap();
        new.add_partition(root_part(1026_048)).unwrap();
        assert_eq!(
            source.diff(&new).unwrap(),
            DiskOps {
                device_path:       Path::new("/dev/sdz"),
                remove_partitions: vec![1, 2, 4],
                change_partitions: vec![
                    PartitionChange {
                        num:    3,
                        start:  420456448,
                        end:    420456448 + GIB20,
                        format: Some(FileSystemType::Xfs),
                        flags:  vec![],
                    },
                ],
                create_partitions: vec![
                    PartitionCreate {
                        start_sector: 2048,
                        end_sector:   1024_000 + 2047,
                        file_system:  FileSystemType::Fat16,
                        kind:         PartitionType::Primary,
                        flags:        vec![],
                    },
                    PartitionCreate {
                        start_sector: 1026_048,
                        end_sector:   GIB20 + 1026_047,
                        file_system:  FileSystemType::Ext4,
                        kind:         PartitionType::Primary,
                        flags:        vec![],
                    },
                ],
            }
        )
    }

    #[test]
    fn partition_add() {
        // The default sample is maxed out, so any partition added should fail.
        let mut source = get_default().0.into_iter().next().unwrap();
        assert!(
            source
                .add_partition(PartitionBuilder::new(2048, 2_000_000, FileSystemType::Ext4))
                .is_err()
        );

        // Failures should also occur if the end sector exceeds the size of
        assert!(
            source
                .add_partition(PartitionBuilder::new(
                    2048,
                    1953525169,
                    FileSystemType::Ext4
                ))
                .is_err()
        );

        // An empty disk should succeed, on the other hand.
        let mut source = get_empty().0.into_iter().next().unwrap();

        // Create 500MiB Fat16 partition w/ 512 byte sectors.
        source.add_partition(boot_part(2048)).unwrap();

        // This should fail with an off by one error, due to the start
        // sector being located within the previous partition.
        assert!(source.add_partition(root_part(1026_047)).is_err());

        // Create 20GiB Ext4 partition after that.
        source.add_partition(root_part(1026_048)).unwrap();
    }

    #[test]
    fn layout_validity() {
        // This test ensures that invalid layouts will raise a flag. An invalid layout is
        // a layout which is missing some of the original source partitions.
        let source = get_default().0.into_iter().next().unwrap();
        let mut duplicate = source.clone();
        assert!(source.validate_layout(&duplicate).is_ok());

        // This should fail, because a critical source partition was removed.
        duplicate.partitions.remove(0);
        assert!(source.validate_layout(&duplicate).is_err());

        // An empty partition should always succeed.
        let source = get_empty().0.into_iter().next().unwrap();
        let mut duplicate = source.clone();
        assert!(source.validate_layout(&duplicate).is_ok());
        duplicate
            .add_partition(PartitionBuilder::new(
                2048,
                1024_00 + 2048,
                FileSystemType::Fat16,
            ))
            .unwrap();
        assert!(source.validate_layout(&duplicate).is_ok());
    }
}
