use std::ffi::CString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

const EPHMFS_MOUNTPOINT: &str = "/mnt/ephmfs";

#[allow(dead_code)]
pub struct DaxDevice {
    // No region field because the region is assumed to be 0
    // The device ID to identify the device within a region
    device_id: u32,
    // The path to the device file, e.g. /dev/dax0.0
    dev_path: String,
    // The size of the device in bytes
    size: u64,
}

impl DaxDevice {
    pub fn init() -> std::io::Result<()> {
        if !ephmfs_mounted(EPHMFS_MOUNTPOINT)? {
            mount_ephmfs(EPHMFS_MOUNTPOINT)?;
        }

        // We don't need to create a DCD region if it exists
        if Self::dcd_region0_exists()? {
            return Ok(());
        }

        // Create the DCD region for the devices to be added to.
        // As of now, hardcode the arguments. That's fine for a research project
        // where we control how the VMs are setup.
        let output = Command::new("cxl")
            .args([
                "create-region",
                "-t",
                "dynamic_ram_a",
                "-d",
                "decoder0.0",
                "-m",
                "mem0",
            ])
            .output()?;
        if !output.status.success() {
            return Err(std::io::Error::other(format!(
                "Failed to create DCD region: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        Ok(())
    }

    pub fn new(size_from_orch: u64) -> std::io::Result<Self> {
        // Create the DCD device for ephemeral memory in region 0.
        // uuid of 0 means that no tag was used (I think, lol)
        let output = Command::new("daxctl")
            .args(["create-device", "-r", "0", "--uuid", "0"])
            .output()?;
        if !output.status.success() {
            return Err(std::io::Error::other(format!(
                "Failed to create DCD device: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        let json_str = String::from_utf8_lossy(&output.stdout);
        let json: serde_json::Value = serde_json::from_str(&json_str)?;
        let (chardev, size) = Self::parse_device_info(&json)?;

        let device_id = Self::parse_device_id(&chardev)?;

        // The create-device command should return devices in the order they
        // were created by the orchestrator, and the orchestrator should send
        // EphMemResponse commands in the same order. If there's a size mismatch,
        // just bail out.
        if size != size_from_orch {
            // TODO return this device to the orchestrator.
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Size mismatch between orchestrator ({size_from_orch}) and DCD ({size})"),
            ));
        }

        let dev_path = format!("/dev/{chardev}");

        Self::add_device_to_ephmfs(&chardev, &dev_path)?;

        Ok(Self {
            device_id,
            dev_path,
            size,
        })
    }

    fn dcd_region0_exists() -> std::io::Result<bool> {
        let output = Command::new("cxl").args(["list", "-r", "0"]).output()?;
        if !output.status.success() {
            return Err(std::io::Error::other(format!(
                "Failed to list DCD region 0: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        let json_str = String::from_utf8_lossy(&output.stdout);
        let json: serde_json::Value = serde_json::from_str(&json_str)?;
        // It would be better to check from the presence of region0 specifically
        // instead of just checking if any region exists at all. However, in our
        // setup, there should't be any other regions.
        Ok(!json.as_array().unwrap_or(&vec![]).is_empty())
    }

    /// Extracts the device ID from a chardev name of the form
    /// "dax<Region>.<Device>". The region must be 0, as assumed by DaxDevice.
    fn parse_device_id(chardev: &str) -> std::io::Result<u32> {
        let invalid = || {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Unexpected chardev name '{chardev}'"),
            )
        };
        let (region, device) = chardev
            .strip_prefix("dax")
            .and_then(|s| s.split_once('.'))
            .ok_or_else(invalid)?;
        if region != "0" {
            return Err(invalid());
        }
        device.parse().map_err(|_| invalid())
    }

    fn parse_device_info(json: &serde_json::Value) -> std::io::Result<(String, u64)> {
        // The format of the output is an array of length 1 containing the
        // following JSON objects:
        // {
        //     "chardev": "dax<Region>.<Device>",
        //     "size": <size in bytes>,
        //     "target_node": <NUMA node ID>,
        //     "align": <alignment in bytes>,
        //     "mode": "devdax",
        // }
        let json = json.get(0).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "Missing device info")
        })?;
        let chardev = json
            .get("chardev")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Missing or invalid 'chardev' field",
                )
            })?
            .to_string();

        let size = json.get("size").and_then(|v| v.as_u64()).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Missing or invalid 'size' field",
            )
        })?;

        Ok((chardev, size))
    }

    fn add_device_to_ephmfs(chardev: &str, dev_path: &str) -> std::io::Result<()> {
        // First, we need to ensure the device is bound to a EphMFS compatible
        // driver.
        fs::write("/sys/bus/dax/drivers/device_dax/unbind", chardev)?;
        fs::write("/sys/bus/dax/drivers/fsdev_dax/new_id", chardev)?;

        // Finally, we can add the device to the EphMFS filesystem.
        fs::write("/sys/fs/ephmfs/devs", dev_path)
    }
}

/// Returns true if an EphMFS filesystem is mounted on the system
fn ephmfs_mounted(mountpoint: &str) -> std::io::Result<bool> {
    // If the mountpoint doesn't exist, nothing can be mounted there.
    let target = match fs::canonicalize(mountpoint) {
        Ok(path) => path,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };

    let mounts = fs::read_to_string("/proc/self/mounts")?;
    Ok(mounts.lines().any(|line| {
        // Each line is: source mountpoint fstype options dump pass
        let mut fields = line.split_whitespace();
        let (_source, mp, fstype) = (fields.next(), fields.next(), fields.next());
        // Canonicalize so differences like trailing slashes or symlinks
        // don't cause a false negative.
        fstype == Some("EphMFS")
            && mp.is_some_and(|mp| fs::canonicalize(mp).is_ok_and(|mp| mp == target))
    }))
}

fn mount_ephmfs(mountpoint: &str) -> std::io::Result<()> {
    let target = CString::new(mountpoint)?;
    let fstype = CString::new("EphMFS")?;

    fs::create_dir_all(mountpoint)?;

    // SAFETY: We are using libc::mount with valid C strings and appropriate arguments.
    let ret = unsafe {
        libc::mount(
            fstype.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            0,
            std::ptr::null(),
        )
    };

    if ret != 0 {
        // Keep the errno's ErrorKind so callers can distinguish failures
        // (e.g., PermissionDenied when not root) while still adding context.
        let err = std::io::Error::last_os_error();
        return Err(std::io::Error::new(
            err.kind(),
            format!("Failed to mount EphMFS at {}: {}", mountpoint, err),
        ));
    }

    // Make sure users can access the mounted EphMFS filesystem.
    fs::set_permissions(mountpoint, fs::Permissions::from_mode(0o1777))?;

    Ok(())
}
