use std::convert::TryFrom;
use std::path::{Path, PathBuf};
use std::{fs, io, mem, sync::Arc};

use crate::control;
use crate::v4l2;
use crate::v4l_sys::*;
use crate::{
    capability::Capabilities, fourcc::FourCC, frameinterval::FrameInterval, framesize::FrameSize,
};

pub use crate::buffer::BufferType as Type;

/// Manage buffers for a device
pub trait Device {
    /// Returns the raw device handle
    fn handle(&self) -> Arc<Handle>;

    /// Type of the device (capture, overlay, output)
    fn typ(&self) -> Type;
}

/// Device handle for low-level access.
///
/// Acquiring a handle facilitates (possibly mutating) interactions with the device.
pub struct Handle {
    fd: std::os::raw::c_int,
}

impl Handle {
    /// Returns the raw file descriptor
    pub fn fd(&self) -> std::os::raw::c_int {
        self.fd
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        v4l2::close(self.fd).unwrap();
    }
}

impl From<std::os::raw::c_int> for Handle {
    fn from(fd: std::os::raw::c_int) -> Self {
        Handle { fd }
    }
}

/// Query device properties such as supported formats and controls
pub trait QueryDevice {
    /// Returns a vector of all frame intervals that the device supports for the given pixel format
    /// and frame size
    fn enum_frameintervals(
        &self,
        fourcc: FourCC,
        width: u32,
        height: u32,
    ) -> io::Result<Vec<FrameInterval>>;

    /// Returns a vector of valid framesizes that the device supports for the given pixel format
    fn enum_framesizes(&self, fourcc: FourCC) -> io::Result<Vec<FrameSize>>;

    /// Returns video4linux framework defined information such as card, driver, etc.
    fn query_caps(&self) -> io::Result<Capabilities>;

    /// Returns the supported controls for a device such as gain, focus, white balance, etc.
    fn query_controls(&self) -> io::Result<Vec<control::Description>>;
}

impl<T: Device> QueryDevice for T {
    fn enum_frameintervals(
        &self,
        fourcc: FourCC,
        width: u32,
        height: u32,
    ) -> io::Result<Vec<FrameInterval>> {
        let mut frameintervals = Vec::new();
        let mut v4l2_struct: v4l2_frmivalenum = unsafe { mem::zeroed() };

        v4l2_struct.index = 0;
        v4l2_struct.pixel_format = fourcc.into();
        v4l2_struct.width = width;
        v4l2_struct.height = height;

        loop {
            let ret = unsafe {
                v4l2::ioctl(
                    self.handle().fd(),
                    v4l2::vidioc::VIDIOC_ENUM_FRAMEINTERVALS,
                    &mut v4l2_struct as *mut _ as *mut std::os::raw::c_void,
                )
            };

            if ret.is_err() {
                if v4l2_struct.index == 0 {
                    return Err(ret.err().unwrap());
                } else {
                    return Ok(frameintervals);
                }
            }

            if let Ok(frame_interval) = FrameInterval::try_from(v4l2_struct) {
                frameintervals.push(frame_interval);
            }

            v4l2_struct.index += 1;
        }
    }

    fn enum_framesizes(&self, fourcc: FourCC) -> io::Result<Vec<FrameSize>> {
        let mut framesizes = Vec::new();
        let mut v4l2_struct: v4l2_frmsizeenum = unsafe { mem::zeroed() };

        v4l2_struct.index = 0;
        v4l2_struct.pixel_format = fourcc.into();

        loop {
            let ret = unsafe {
                v4l2::ioctl(
                    self.handle().fd(),
                    v4l2::vidioc::VIDIOC_ENUM_FRAMESIZES,
                    &mut v4l2_struct as *mut _ as *mut std::os::raw::c_void,
                )
            };

            if ret.is_err() {
                if v4l2_struct.index == 0 {
                    return Err(ret.err().unwrap());
                } else {
                    return Ok(framesizes);
                }
            }

            if let Ok(frame_size) = FrameSize::try_from(v4l2_struct) {
                framesizes.push(frame_size);
            }

            v4l2_struct.index += 1;
        }
    }

    fn query_caps(&self) -> io::Result<Capabilities> {
        unsafe {
            let mut v4l2_caps: v4l2_capability = mem::zeroed();
            v4l2::ioctl(
                self.handle().fd(),
                v4l2::vidioc::VIDIOC_QUERYCAP,
                &mut v4l2_caps as *mut _ as *mut std::os::raw::c_void,
            )?;

            Ok(Capabilities::from(v4l2_caps))
        }
    }

    fn query_controls(&self) -> io::Result<Vec<control::Description>> {
        let mut controls = Vec::new();
        unsafe {
            let mut v4l2_ctrl: v4l2_queryctrl = mem::zeroed();

            loop {
                v4l2_ctrl.id |= V4L2_CTRL_FLAG_NEXT_CTRL;
                v4l2_ctrl.id |= V4L2_CTRL_FLAG_NEXT_COMPOUND;
                match v4l2::ioctl(
                    self.handle().fd(),
                    v4l2::vidioc::VIDIOC_QUERYCTRL,
                    &mut v4l2_ctrl as *mut _ as *mut std::os::raw::c_void,
                ) {
                    Ok(_) => {
                        // get the basic control information
                        let mut control = control::Description::from(v4l2_ctrl);

                        // if this is a menu control, enumerate its items
                        if control.typ == control::Type::Menu
                            || control.typ == control::Type::IntegerMenu
                        {
                            let mut items = Vec::new();

                            let mut v4l2_menu: v4l2_querymenu = mem::zeroed();
                            v4l2_menu.id = v4l2_ctrl.id;

                            for i in (v4l2_ctrl.minimum..=v4l2_ctrl.maximum)
                                .step_by(v4l2_ctrl.step as usize)
                            {
                                v4l2_menu.index = i as u32;
                                let res = v4l2::ioctl(
                                    self.handle().fd(),
                                    v4l2::vidioc::VIDIOC_QUERYMENU,
                                    &mut v4l2_menu as *mut _ as *mut std::os::raw::c_void,
                                );

                                // BEWARE OF DRAGONS!
                                // The API docs [1] state VIDIOC_QUERYMENU should may return EINVAL
                                // for some indices between minimum and maximum when an item is not
                                // supported by a driver.
                                //
                                // I have no idea why it is advertised in the first place then, but
                                // have seen this happen with a Logitech C920 HD Pro webcam.
                                // In case of errors, let's just skip the offending index.
                                //
                                // [1] https://github.com/torvalds/linux/blob/master/Documentation/userspace-api/media/v4l/vidioc-queryctrl.rst#description
                                if res.is_err() {
                                    continue;
                                }

                                let item =
                                    control::MenuItem::try_from((control.typ, v4l2_menu)).unwrap();
                                items.push((v4l2_menu.index, item));
                            }

                            control.items = Some(items);
                        }

                        controls.push(control);
                    }
                    Err(e) => {
                        if controls.is_empty() || e.kind() != io::ErrorKind::InvalidInput {
                            return Err(e);
                        } else {
                            break;
                        }
                    }
                }
            }
        }

        Ok(controls)
    }
}

/// Represents a video4linux device node
pub struct Info {
    /// Device node path
    path: PathBuf,
}

impl Info {
    /// Returns a device node observer by path
    ///
    /// The device is opened in read only mode.
    ///
    /// # Arguments
    ///
    /// * `path` - Node path (usually a character device or sysfs entry)
    ///
    /// # Example
    ///
    /// ```
    /// use v4l::device::Info;
    /// let node = Info::new("/dev/video0");
    /// ```
    pub fn new<P: AsRef<Path>>(path: P) -> Self {
        Info {
            path: PathBuf::from(path.as_ref()),
        }
    }

    /// Returns the absolute path of the device node
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the index of the device node
    pub fn index(&self) -> Option<usize> {
        let file_name = self.path.file_name()?;

        let mut index_str = String::new();
        for c in file_name
            .to_str()?
            .chars()
            .rev()
            .collect::<String>()
            .chars()
        {
            if !c.is_digit(10) {
                break;
            }

            index_str.push(c);
        }

        let index = index_str.parse::<usize>();
        if index.is_err() {
            return None;
        }

        Some(index.unwrap())
    }

    /// Returns name of the device by parsing its sysfs entry
    pub fn name(&self) -> Option<String> {
        let index = self.index()?;
        let path = format!("{}{}{}", "/sys/class/video4linux/video", index, "/name");
        let name = fs::read_to_string(path);
        match name {
            Ok(name) => Some(name.trim().to_string()),
            Err(_) => None,
        }
    }
}

/// Represents an iterable list of valid devices
#[derive(Default)]
pub struct List {
    /// Position in the list
    pos: usize,
    /// All paths representing potential video4linux devices
    paths: Vec<PathBuf>,
}

impl List {
    /// Returns a list of devices currently known to the system
    ///
    /// # Example
    ///
    /// ```
    /// use v4l::device::List;
    /// let list = List::new();
    /// for dev in list {
    ///     print!("{}{}", dev.index().unwrap(), dev.name().unwrap());
    /// }
    /// ```
    pub fn new() -> Self {
        let mut list = List {
            pos: 0,
            paths: Vec::new(),
        };

        let nodes = fs::read_dir("/dev");
        if let Ok(nodes) = nodes {
            for node in nodes {
                if node.is_err() {
                    continue;
                }
                let node = node.unwrap();
                let file_name = node.file_name();
                let file_name = file_name.to_str().unwrap();

                if file_name.starts_with("video") {
                    list.paths.push(node.path());
                }
            }
        }

        list.paths.sort();
        list
    }
}

impl Iterator for List {
    type Item = Info;

    fn next(&mut self) -> Option<Info> {
        let pos = self.pos;
        if pos == self.paths.len() {
            return None;
        }

        self.pos += 1;
        Some(Info::new(&self.paths[pos]))
    }
}
