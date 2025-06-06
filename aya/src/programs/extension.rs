//! Extension programs.

use std::os::fd::{AsFd as _, BorrowedFd};

use aya_obj::{
    btf::BtfKind,
    generated::{bpf_attach_type::BPF_CGROUP_INET_INGRESS, bpf_prog_type::BPF_PROG_TYPE_EXT},
};
use object::Endianness;
use thiserror::Error;

use crate::{
    Btf,
    programs::{
        FdLink, FdLinkId, ProgramData, ProgramError, ProgramFd, ProgramType, define_link_wrapper,
        load_program,
    },
    sys::{self, BpfLinkCreateArgs, LinkTarget, SyscallError, bpf_link_create},
};

/// The type returned when loading or attaching an [`Extension`] fails.
#[derive(Debug, Error)]
pub enum ExtensionError {
    /// Target BPF program does not have BTF loaded to the kernel.
    #[error("target BPF program does not have BTF loaded to the kernel")]
    NoBTF,
}

/// A program used to extend existing BPF programs.
///
/// [`Extension`] programs can be loaded to replace a global
/// function in a program that has already been loaded.
///
/// # Minimum kernel version
///
/// The minimum kernel version required to use this feature is 5.9
///
/// # Examples
///
/// ```no_run
/// use aya::{EbpfLoader, programs::{Xdp, XdpFlags, Extension}};
///
/// let mut bpf = EbpfLoader::new().extension("extension").load_file("app.o")?;
/// let prog: &mut Xdp = bpf.program_mut("main").unwrap().try_into()?;
/// prog.load()?;
/// prog.attach("eth0", XdpFlags::default())?;
///
/// let prog_fd = prog.fd().unwrap();
/// let prog_fd = prog_fd.try_clone().unwrap();
/// let ext: &mut Extension = bpf.program_mut("extension").unwrap().try_into()?;
/// ext.load(prog_fd, "function_to_replace")?;
/// ext.attach()?;
/// Ok::<(), aya::EbpfError>(())
/// ```
#[derive(Debug)]
#[doc(alias = "BPF_PROG_TYPE_EXT")]
pub struct Extension {
    pub(crate) data: ProgramData<ExtensionLink>,
}

impl Extension {
    /// The type of the program according to the kernel.
    pub const PROGRAM_TYPE: ProgramType = ProgramType::Extension;

    /// Loads the extension inside the kernel.
    ///
    /// Prepares the code included in the extension to replace the code of the function
    /// `func_name` within the eBPF program represented by the `program` file descriptor.
    /// This requires that both the [`Extension`] and `program` have had their BTF
    /// loaded into the kernel.
    ///
    /// The BPF verifier requires that we specify the target program and function name
    /// at load time, so it can identify that the program and target are BTF compatible
    /// and to enforce this constraint when programs are attached.
    ///
    /// The extension code will be loaded but inactive until it's attached.
    /// There are no restrictions on what functions may be replaced, so you could replace
    /// the main entry point of your program with an extension.
    pub fn load(&mut self, program: ProgramFd, func_name: &str) -> Result<(), ProgramError> {
        let (btf_fd, btf_id) = get_btf_info(program.as_fd(), func_name)?;

        self.data.attach_btf_obj_fd = Some(btf_fd);
        self.data.attach_prog_fd = Some(program);
        self.data.attach_btf_id = Some(btf_id);
        load_program(BPF_PROG_TYPE_EXT, &mut self.data)
    }

    /// Attaches the extension.
    ///
    /// Attaches the extension to the program and function name specified at load time,
    /// effectively replacing the original target function.
    ///
    /// The returned value can be used to detach the extension and restore the
    /// original function, see [Extension::detach].
    pub fn attach(&mut self) -> Result<ExtensionLinkId, ProgramError> {
        let prog_fd = self.fd()?;
        let prog_fd = prog_fd.as_fd();
        let target_fd = self
            .data
            .attach_prog_fd
            .as_ref()
            .ok_or(ProgramError::NotLoaded)?;
        let target_fd = target_fd.as_fd();
        let btf_id = self.data.attach_btf_id.ok_or(ProgramError::NotLoaded)?;
        // the attach type must be set as 0, which is bpf_attach_type::BPF_CGROUP_INET_INGRESS
        let link_fd = bpf_link_create(
            prog_fd,
            LinkTarget::Fd(target_fd),
            BPF_CGROUP_INET_INGRESS,
            0,
            Some(BpfLinkCreateArgs::TargetBtfId(btf_id)),
        )
        .map_err(|io_error| SyscallError {
            call: "bpf_link_create",
            io_error,
        })?;
        self.data
            .links
            .insert(ExtensionLink::new(FdLink::new(link_fd)))
    }

    /// Attaches the extension to another program.
    ///
    /// Attaches the extension to a program and/or function other than the one provided
    /// at load time. You may only attach to another program/function if the BTF
    /// type signature is identical to that which was verified on load. Attempting to
    /// attach to an invalid program/function will result in an error.
    ///
    /// Once attached, the extension effectively replaces the original target function.
    ///
    /// The returned value can be used to detach the extension and restore the
    /// original function, see [Extension::detach].
    pub fn attach_to_program(
        &mut self,
        program: &ProgramFd,
        func_name: &str,
    ) -> Result<ExtensionLinkId, ProgramError> {
        let target_fd = program.as_fd();
        let (_, btf_id) = get_btf_info(target_fd, func_name)?;
        let prog_fd = self.fd()?;
        let prog_fd = prog_fd.as_fd();
        // the attach type must be set as 0, which is bpf_attach_type::BPF_CGROUP_INET_INGRESS
        let link_fd = bpf_link_create(
            prog_fd,
            LinkTarget::Fd(target_fd),
            BPF_CGROUP_INET_INGRESS,
            0,
            Some(BpfLinkCreateArgs::TargetBtfId(btf_id)),
        )
        .map_err(|io_error| SyscallError {
            call: "bpf_link_create",
            io_error,
        })?;
        self.data
            .links
            .insert(ExtensionLink::new(FdLink::new(link_fd)))
    }
}

/// Retrieves the FD of the BTF object for the provided `prog_fd` and the BTF ID of the function
/// with the name `func_name` within that BTF object.
fn get_btf_info(
    prog_fd: BorrowedFd<'_>,
    func_name: &str,
) -> Result<(crate::MockableFd, u32), ProgramError> {
    // retrieve program information
    let info = sys::bpf_prog_get_info_by_fd(prog_fd, &mut [])?;

    // btf_id refers to the ID of the program btf that was loaded with bpf(BPF_BTF_LOAD)
    if info.btf_id == 0 {
        return Err(ProgramError::ExtensionError(ExtensionError::NoBTF));
    }

    // the bpf fd of the BTF object
    let btf_fd = sys::bpf_btf_get_fd_by_id(info.btf_id)?;

    // we need to read the btf bytes into a buffer but we don't know the size ahead of time.
    // assume 4kb. if this is too small we can resize based on the size obtained in the response.
    let mut buf = vec![0u8; 4096];
    loop {
        let info = sys::btf_obj_get_info_by_fd(btf_fd.as_fd(), &mut buf)?;
        let btf_size = info.btf_size as usize;
        if btf_size > buf.len() {
            buf.resize(btf_size, 0u8);
            continue;
        }
        buf.truncate(btf_size);
        break;
    }

    let btf = Btf::parse(&buf, Endianness::default()).map_err(ProgramError::Btf)?;

    let btf_id = btf
        .id_by_type_name_kind(func_name, BtfKind::Func)
        .map_err(ProgramError::Btf)?;

    Ok((btf_fd, btf_id))
}

define_link_wrapper!(
    /// The link used by [Extension] programs.
    ExtensionLink,
    /// The type returned by [Extension::attach]. Can be passed to [Extension::detach].
    ExtensionLinkId,
    FdLink,
    FdLinkId,
    Extension,
);
