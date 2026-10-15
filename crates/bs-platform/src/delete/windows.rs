//! The Recycle Bin on Windows: moving items in, putting them back, emptying a
//! drive's bin, and deleting for good.
//!
//! Items go to the bin through `IFileOperation`, whose progress sink says
//! whether each item reached the bin or was deleted outright. Permanent
//! deletes open every item and folder with `FILE_FLAG_OPEN_REPARSE_POINT`, so a
//! link or junction is deleted as itself and never entered, and delete each
//! through the handle that was opened and checked.

#![allow(unsafe_code)]

use super::winpure;
use super::{
    Disposal, EmptyOutcome, Identity, Item, OpError, Outcome, PermanentReason, RefuseReason,
    ResolveError, Resolved, RestoreOutcome, RestoreToken, TokenInner,
};
use crate::bins::windows::{policy, query, user_sid};
use crate::drives::{Drive, DriveKind};
pub(crate) use bs_fsops::win::identity_of;
use bs_fsops::win::{final_path, identity_of_handle, info_by_handle, open_nofollow, wide};
use std::cell::{Cell, RefCell};
use std::ffi::{OsString, c_void};
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS};
use windows_sys::Win32::Storage::FileSystem::{
    DeleteFileW, FILE_ATTRIBUTE_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_STANDARD_INFO,
    FileStandardInfo, MoveFileExW,
};
use windows_sys::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance,
    CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows_sys::Win32::UI::Shell::{
    FOF_NO_UI, FOF_NORECURSEREPARSE, FOF_WANTNUKEWARNING, FOFX_RECYCLEONDELETE, FileOperation,
    SHCreateItemFromParsingName, SHERB_NOCONFIRMATION, SHERB_NOPROGRESSUI, SHERB_NOSOUND,
    SHEmptyRecycleBinW,
};
use windows_sys::core::GUID;

fn opfail(step: &'static str, error: &io::Error) -> OpError {
    OpError::io(step, error)
}

// -------------------------------------------------------------------- Sizes

/// The sizes of the files in an item, added up, without following links. A
/// file that cannot be read counts as nothing.
fn logical_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(meta) = std::fs::symlink_metadata(&current) else {
            continue;
        };
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            continue;
        }
        if meta.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&current) {
                stack.extend(entries.flatten().map(|e| e.path()));
            }
        } else {
            total = total.saturating_add(meta.len());
        }
    }
    total
}

// ---------------------------------------------------------------------- COM

type Hresult = i32;
const S_OK: Hresult = 0;
const E_NOINTERFACE: Hresult = 0x8000_4002_u32 as i32;
const RPC_E_CHANGED_MODE: Hresult = 0x8001_0106_u32 as i32;
const SIGDN_FILESYSPATH: i32 = 0x8005_8000_u32 as i32;

const IID_IUNKNOWN: GUID = GUID::from_u128(0x00000000_0000_0000_c000_000000000046);
const IID_ISHELLITEM: GUID = GUID::from_u128(0x43826d1e_e718_42ee_bc55_a1e261c37bfe);
const IID_IFILEOPERATION: GUID = GUID::from_u128(0x947aab5f_0a5c_4c13_b4d6_4bf7836fc9f8);
const IID_IFILEOPERATIONPROGRESSSINK: GUID =
    GUID::from_u128(0x04b0f1a7_9490_44bc_96e1_4296a31252e2);

type Slot = usize;

fn same_guid(a: &GUID, b: &GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

#[repr(C)]
struct UnknownVtbl {
    query_interface:
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> Hresult,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
}

/// `IShellItem`; the slots this code does not call are placeholders that keep
/// the layout.
#[repr(C)]
struct ShellItemVtbl {
    base: UnknownVtbl,
    bind_to_handler: Slot,
    get_parent: Slot,
    get_display_name: unsafe extern "system" fn(*mut c_void, i32, *mut *mut u16) -> Hresult,
    get_attributes: Slot,
    compare: Slot,
}

/// `IFileOperation`, in the order the interface declares its methods.
#[repr(C)]
struct FileOperationVtbl {
    base: UnknownVtbl,
    advise: Slot,
    unadvise: Slot,
    set_operation_flags: unsafe extern "system" fn(*mut c_void, u32) -> Hresult,
    set_progress_message: Slot,
    set_progress_dialog: Slot,
    set_properties: Slot,
    set_owner_window: Slot,
    apply_properties_to_item: Slot,
    apply_properties_to_items: Slot,
    rename_item: Slot,
    rename_items: Slot,
    move_item: Slot,
    move_items: Slot,
    copy_item: Slot,
    copy_items: Slot,
    delete_item: unsafe extern "system" fn(*mut c_void, *mut c_void, *mut c_void) -> Hresult,
    delete_items: Slot,
    new_item: Slot,
    perform_operations: unsafe extern "system" fn(*mut c_void) -> Hresult,
    get_any_operations_aborted: unsafe extern "system" fn(*mut c_void, *mut i32) -> Hresult,
}

/// Reads the vtable of a COM object.
///
/// # Safety
/// `object` must point to a live COM object whose interface has the vtable
/// layout `V`.
unsafe fn vtbl<'a, V>(object: *mut c_void) -> &'a V {
    // SAFETY: by the caller's contract the first word of the object is a
    // pointer to a vtable of type `V` that lives as long as the object.
    unsafe { &**object.cast::<*const V>() }
}

/// Drops one reference.
///
/// # Safety
/// `object` must be a live COM object this code holds a reference to.
unsafe fn release(object: *mut c_void) {
    if !object.is_null() {
        // SAFETY: by the caller's contract.
        unsafe { (vtbl::<UnknownVtbl>(object).release)(object) };
    }
}

/// The file-system path of a shell item.
///
/// # Safety
/// `item` must be a live `IShellItem`.
unsafe fn item_path(item: *mut c_void) -> Option<PathBuf> {
    if item.is_null() {
        return None;
    }
    let mut text: *mut u16 = std::ptr::null_mut();
    // SAFETY: by the caller's contract `item` is an `IShellItem`; `text`
    // receives a string the system allocates.
    let hr = unsafe {
        (vtbl::<ShellItemVtbl>(item).get_display_name)(item, SIGDN_FILESYSPATH, &mut text)
    };
    if hr < 0 || text.is_null() {
        return None;
    }
    let mut length = 0usize;
    // SAFETY: the system returned a NUL-terminated string.
    while unsafe { *text.add(length) } != 0 {
        length += 1;
    }
    // SAFETY: `text` points to `length` initialised UTF-16 units.
    let units = unsafe { std::slice::from_raw_parts(text, length) };
    let path = PathBuf::from(OsString::from_wide(units));
    // SAFETY: the string came from the shell's allocator and is freed once.
    unsafe { CoTaskMemFree(text.cast()) };
    Some(path)
}

/// What the shell reported for one deleted item.
#[derive(Clone, Debug)]
struct DeleteReport {
    hresult: Hresult,
    /// Where the item is in the bin; `None` when it was deleted outright.
    recycled: Option<PathBuf>,
    /// The shell reported an item in the bin, even if its path is unknown.
    reached_bin: bool,
}

/// The receiving end of the shell's progress calls. The shell only needs the
/// two calls that matter here; the rest accept and ignore.
#[repr(C)]
struct Sink {
    vtbl: *const SinkVtbl,
    refs: Cell<u32>,
    reports: RefCell<Vec<DeleteReport>>,
}

#[repr(C)]
struct SinkVtbl {
    base: UnknownVtbl,
    start_operations: unsafe extern "system" fn(*mut c_void) -> Hresult,
    finish_operations: unsafe extern "system" fn(*mut c_void, Hresult) -> Hresult,
    pre_rename_item:
        unsafe extern "system" fn(*mut c_void, u32, *mut c_void, *const u16) -> Hresult,
    post_rename_item: unsafe extern "system" fn(
        *mut c_void,
        u32,
        *mut c_void,
        *const u16,
        Hresult,
        *mut c_void,
    ) -> Hresult,
    pre_move_item: unsafe extern "system" fn(
        *mut c_void,
        u32,
        *mut c_void,
        *mut c_void,
        *const u16,
    ) -> Hresult,
    post_move_item: unsafe extern "system" fn(
        *mut c_void,
        u32,
        *mut c_void,
        *mut c_void,
        *const u16,
        Hresult,
        *mut c_void,
    ) -> Hresult,
    pre_copy_item: unsafe extern "system" fn(
        *mut c_void,
        u32,
        *mut c_void,
        *mut c_void,
        *const u16,
    ) -> Hresult,
    post_copy_item: unsafe extern "system" fn(
        *mut c_void,
        u32,
        *mut c_void,
        *mut c_void,
        *const u16,
        Hresult,
        *mut c_void,
    ) -> Hresult,
    pre_delete_item: unsafe extern "system" fn(*mut c_void, u32, *mut c_void) -> Hresult,
    post_delete_item:
        unsafe extern "system" fn(*mut c_void, u32, *mut c_void, Hresult, *mut c_void) -> Hresult,
    pre_new_item: unsafe extern "system" fn(*mut c_void, u32, *mut c_void, *const u16) -> Hresult,
    post_new_item: unsafe extern "system" fn(
        *mut c_void,
        u32,
        *mut c_void,
        *const u16,
        *const u16,
        u32,
        Hresult,
        *mut c_void,
    ) -> Hresult,
    update_progress: unsafe extern "system" fn(*mut c_void, u32, u32) -> Hresult,
    reset_timer: unsafe extern "system" fn(*mut c_void) -> Hresult,
    pause_timer: unsafe extern "system" fn(*mut c_void) -> Hresult,
    resume_timer: unsafe extern "system" fn(*mut c_void) -> Hresult,
}

unsafe extern "system" fn sink_query_interface(
    this: *mut c_void,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> Hresult {
    if out.is_null() {
        return E_NOINTERFACE;
    }
    // SAFETY: `iid` is a valid GUID pointer for the call, and `out` was checked.
    let wanted = unsafe { *iid };
    if same_guid(&wanted, &IID_IUNKNOWN) || same_guid(&wanted, &IID_IFILEOPERATIONPROGRESSSINK) {
        // SAFETY: `this` is the sink this vtable belongs to.
        unsafe { sink_add_ref(this) };
        // SAFETY: `out` is writable.
        unsafe { *out = this };
        S_OK
    } else {
        // SAFETY: `out` is writable.
        unsafe { *out = std::ptr::null_mut() };
        E_NOINTERFACE
    }
}

unsafe extern "system" fn sink_add_ref(this: *mut c_void) -> u32 {
    // SAFETY: the shell only calls this with the pointer it was given, which is
    // a live `Sink`.
    let sink = unsafe { &*this.cast::<Sink>() };
    sink.refs.set(sink.refs.get() + 1);
    sink.refs.get()
}

unsafe extern "system" fn sink_release(this: *mut c_void) -> u32 {
    // SAFETY: as in `sink_add_ref`.
    let sink = unsafe { &*this.cast::<Sink>() };
    let left = sink.refs.get().saturating_sub(1);
    sink.refs.set(left);
    if left == 0 {
        // SAFETY: the last reference is gone and the sink was made with
        // `Box::into_raw`, so it is freed once, here.
        drop(unsafe { Box::from_raw(this.cast::<Sink>()) });
    }
    left
}

unsafe extern "system" fn sink_post_delete_item(
    this: *mut c_void,
    _flags: u32,
    _item: *mut c_void,
    result: Hresult,
    newly_created: *mut c_void,
) -> Hresult {
    // SAFETY: as in `sink_add_ref`.
    let sink = unsafe { &*this.cast::<Sink>() };
    // SAFETY: when not null, `newly_created` is a live `IShellItem` for the
    // duration of the call.
    let recycled = unsafe { item_path(newly_created) };
    sink.reports.borrow_mut().push(DeleteReport {
        hresult: result,
        recycled,
        reached_bin: !newly_created.is_null(),
    });
    // Returning an error would cancel the rest of the operation.
    S_OK
}

unsafe extern "system" fn ok0(_: *mut c_void) -> Hresult {
    S_OK
}
unsafe extern "system" fn ok_finish(_: *mut c_void, _: Hresult) -> Hresult {
    S_OK
}
unsafe extern "system" fn ok_item_name(
    _: *mut c_void,
    _: u32,
    _: *mut c_void,
    _: *const u16,
) -> Hresult {
    S_OK
}
unsafe extern "system" fn ok_post_rename(
    _: *mut c_void,
    _: u32,
    _: *mut c_void,
    _: *const u16,
    _: Hresult,
    _: *mut c_void,
) -> Hresult {
    S_OK
}
unsafe extern "system" fn ok_pre_transfer(
    _: *mut c_void,
    _: u32,
    _: *mut c_void,
    _: *mut c_void,
    _: *const u16,
) -> Hresult {
    S_OK
}
unsafe extern "system" fn ok_post_transfer(
    _: *mut c_void,
    _: u32,
    _: *mut c_void,
    _: *mut c_void,
    _: *const u16,
    _: Hresult,
    _: *mut c_void,
) -> Hresult {
    S_OK
}
unsafe extern "system" fn ok_pre_delete(_: *mut c_void, _: u32, _: *mut c_void) -> Hresult {
    S_OK
}
unsafe extern "system" fn ok_post_new(
    _: *mut c_void,
    _: u32,
    _: *mut c_void,
    _: *const u16,
    _: *const u16,
    _: u32,
    _: Hresult,
    _: *mut c_void,
) -> Hresult {
    S_OK
}
unsafe extern "system" fn ok_progress(_: *mut c_void, _: u32, _: u32) -> Hresult {
    S_OK
}

static SINK_VTBL: SinkVtbl = SinkVtbl {
    base: UnknownVtbl {
        query_interface: sink_query_interface,
        add_ref: sink_add_ref,
        release: sink_release,
    },
    start_operations: ok0,
    finish_operations: ok_finish,
    pre_rename_item: ok_item_name,
    post_rename_item: ok_post_rename,
    pre_move_item: ok_pre_transfer,
    post_move_item: ok_post_transfer,
    pre_copy_item: ok_pre_transfer,
    post_copy_item: ok_post_transfer,
    pre_delete_item: ok_pre_delete,
    post_delete_item: sink_post_delete_item,
    pre_new_item: ok_item_name,
    post_new_item: ok_post_new,
    update_progress: ok_progress,
    reset_timer: ok0,
    pause_timer: ok0,
    resume_timer: ok0,
};

/// Keeps COM initialised on this thread for as long as it lives.
struct ComScope {
    uninitialize: bool,
}

impl ComScope {
    fn enter() -> Result<Self, Hresult> {
        // SAFETY: a null reserved pointer and valid flags.
        let hr = unsafe {
            CoInitializeEx(
                std::ptr::null(),
                (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
            )
        };
        if hr >= 0 {
            Ok(Self { uninitialize: true })
        } else if hr == RPC_E_CHANGED_MODE {
            // The thread already joined COM another way; use it as it is.
            Ok(Self {
                uninitialize: false,
            })
        } else {
            Err(hr)
        }
    }
}

impl Drop for ComScope {
    fn drop(&mut self) {
        if self.uninitialize {
            // SAFETY: balances the successful `CoInitializeEx` in `enter`.
            unsafe { CoUninitialize() };
        }
    }
}

/// Sends one item to the Recycle Bin through the shell, with no windows or
/// prompts, and returns what the shell reported.
fn recycle_through_shell(path: &Path) -> Result<DeleteReport, OpError> {
    let _com = ComScope::enter().map_err(|hr| OpError::code("start COM", hr))?;
    let mut operation: *mut c_void = std::ptr::null_mut();
    // SAFETY: the class and interface ids are valid and `operation` is writable.
    let hr = unsafe {
        CoCreateInstance(
            &FileOperation,
            std::ptr::null_mut(),
            CLSCTX_INPROC_SERVER,
            &IID_IFILEOPERATION,
            &mut operation,
        )
    };
    if hr < 0 || operation.is_null() {
        return Err(OpError::code("create the file operation", hr));
    }
    let result = recycle_with(operation, path);
    // SAFETY: `operation` is the reference `CoCreateInstance` gave this code.
    unsafe { release(operation) };
    result
}

fn recycle_with(operation: *mut c_void, path: &Path) -> Result<DeleteReport, OpError> {
    // SAFETY: `operation` is a live `IFileOperation`.
    let ops = unsafe { vtbl::<FileOperationVtbl>(operation) };
    // Recycle if possible, show nothing, and treat a link or junction as the
    // thing itself rather than a folder to look inside.
    //
    // `FOF_NO_UI` includes "no confirmation", which would let the shell delete
    // an item for good without asking when it cannot recycle it. The size and
    // setting checks before this call keep that from happening, but they
    // cannot know everything the shell weighs. `FOF_WANTNUKEWARNING` is the
    // documented way to keep a permanent delete from going unconfirmed: "Send a
    // warning if a file is being permanently destroyed during a delete
    // operation rather than recycled. This flag partially overrides
    // FOF_NOCONFIRMATION." (SHFILEOPSTRUCT, flag descriptions,
    // https://learn.microsoft.com/en-us/windows/win32/api/shellapi/ns-shellapi-shfileopstructa).
    // If the user declines, the shell cancels the delete and the item stays.
    let flags = FOFX_RECYCLEONDELETE | FOF_NO_UI | FOF_WANTNUKEWARNING | FOF_NORECURSEREPARSE;
    // SAFETY: `operation` is live and the flags are valid.
    let hr = unsafe { (ops.set_operation_flags)(operation, flags) };
    if hr < 0 {
        return Err(OpError::code("set the operation flags", hr));
    }
    let name = wide(path.as_os_str());
    let mut item: *mut c_void = std::ptr::null_mut();
    // SAFETY: `name` is NUL-terminated and outlives the call; `item` is writable.
    let hr = unsafe {
        SHCreateItemFromParsingName(
            name.as_ptr(),
            std::ptr::null_mut(),
            &IID_ISHELLITEM,
            &mut item,
        )
    };
    if hr < 0 || item.is_null() {
        return Err(OpError::code("find the item", hr));
    }
    let sink = Box::into_raw(Box::new(Sink {
        vtbl: &SINK_VTBL,
        refs: Cell::new(1),
        reports: RefCell::new(Vec::new()),
    }));
    let sink_object = sink.cast::<c_void>();

    // SAFETY: `operation`, `item` and the sink are live; the shell adds its own
    // references to the item and the sink while the operation is queued.
    let queued = unsafe { (ops.delete_item)(operation, item, sink_object) };
    let outcome = if queued < 0 {
        Err(OpError::code("queue the delete", queued))
    } else {
        // SAFETY: `operation` is live.
        let done = unsafe { (ops.perform_operations)(operation) };
        let mut aborted = 0i32;
        // SAFETY: `operation` is live and `aborted` is writable.
        unsafe { (ops.get_any_operations_aborted)(operation, &mut aborted) };
        // SAFETY: the sink is live: this code holds a reference.
        let reports = unsafe { (*sink).reports.borrow().clone() };
        match reports.last() {
            Some(report) if report.hresult >= 0 => Ok(report.clone()),
            Some(report) => Err(OpError::code("recycle", report.hresult)),
            None if done < 0 => Err(OpError::code("recycle", done)),
            None if aborted != 0 => Err(OpError::code("recycle (cancelled by the system)", done)),
            // The shell finished without saying what happened to the item.
            None => Err(OpError::code("recycle (no report from the system)", done)),
        }
    };
    // SAFETY: these are the references this code holds; the sink may outlive
    // this call if the shell still holds one, and frees itself then.
    unsafe {
        release(item);
        sink_release(sink_object);
    }
    outcome
}

// ------------------------------------------------------------- Recycle Bin

/// What a restore needs to know about an item in the Recycle Bin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RecycledToken {
    /// The `$R...` item in the bin.
    recycled: PathBuf,
    identity: Identity,
}

/// Finds the `$R` item for an original path by reading the `$I` records in the
/// user's folder on the drive. Used when the shell did not say where the item
/// went. The newest record wins.
fn find_in_bin(drive: &Drive, original: &Path) -> Option<PathBuf> {
    let sid = user_sid()?;
    let folder = crate::bins::winpure::recycle_folder(&drive.mount_point, sid)?;
    let mut best: Option<(u64, PathBuf)> = None;
    for entry in std::fs::read_dir(&folder).ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("$I") {
            continue;
        }
        let Ok(bytes) = read_small(&entry.path()) else {
            continue;
        };
        let Some(record) = winpure::parse_record(&bytes) else {
            continue;
        };
        if winpure::same_place(&record.original, &units_of(original))
            && best
                .as_ref()
                .is_none_or(|(when, _)| record.deleted_at >= *when)
        {
            best = Some((record.deleted_at, folder.join(format!("$R{}", &name[2..]))));
        }
    }
    best.map(|(_, path)| path)
}

/// Reads a `$I` file: they are a few hundred bytes.
fn read_small(path: &Path) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(64 * 1024)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Moves an item out of the bin back to `to`. The move fails rather than
/// replace anything, and the bin's record of the item is removed.
fn move_out_of_bin(recycled: &Path, to: &Path) -> io::Result<()> {
    let from_wide = wide(recycled.as_os_str());
    let to_wide = wide(to.as_os_str());
    // Flags of zero: the move fails rather than replace anything.
    // SAFETY: both names are NUL-terminated and outlive the call.
    if unsafe { MoveFileExW(from_wide.as_ptr(), to_wide.as_ptr(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if let Some(info) = winpure::info_for(&units_of(recycled)) {
        let info = wide(&OsString::from_wide(&info));
        // SAFETY: `info` is NUL-terminated and outlives the call.
        unsafe { DeleteFileW(info.as_ptr()) };
    }
    Ok(())
}

/// The 16-bit units of a path, for comparing places exactly.
fn units_of(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().collect()
}

// ------------------------------------------------------------------ Disposal

impl Disposal {
    pub(crate) fn resolve(&self, path: &Path) -> Result<Resolved, ResolveError> {
        let handle =
            open_nofollow(path, FILE_READ_ATTRIBUTES).map_err(|error| match error.kind() {
                io::ErrorKind::NotFound => ResolveError::NotFound,
                _ => ResolveError::CannotResolve,
            })?;
        let identity = identity_of_handle(&handle).map_err(|_| ResolveError::CannotResolve)?;
        let real = final_path(&handle).map_err(|_| ResolveError::CannotResolve)?;
        Ok(Resolved {
            real: PathBuf::from(real),
            identity,
            on_mount_point: false,
        })
    }

    pub(crate) fn bin_objection(&self, path: &Path, found: &Resolved) -> Option<PermanentReason> {
        let drive = winpure::drive_for(&self.drives, &found.real);
        let rules = drive.map(policy).unwrap_or_default();
        winpure::objection(drive, rules, logical_size(path))
    }

    pub(crate) fn trash_one(&self, item: &Item, now: &Resolved) -> Outcome {
        // Look again just before the shell is asked: it acts on the path.
        match identity_of(&item.path) {
            Ok(identity) if identity == now.identity => {}
            Ok(_) => {
                return Outcome::Refused {
                    reason: RefuseReason::ChangedSinceScan,
                };
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Outcome::Refused {
                    reason: RefuseReason::NotFound,
                };
            }
            Err(error) => {
                return Outcome::Failed {
                    error: opfail("look at the item", &error),
                };
            }
        }
        // Measured once: the same figure decides whether the bin takes the item
        // and is reported afterwards.
        let bytes = logical_size(&item.path);
        let drive = winpure::drive_for(&self.drives, &now.real);
        let rules = drive.map(policy).unwrap_or_default();
        if let Some(reason) = winpure::objection(drive, rules, bytes) {
            return Outcome::WouldBePermanent { reason };
        }
        let shared_files = single_file_links(&item.path);
        // The shell refuses the verbatim form (`\\?\C:\...`) that long paths and
        // canonical paths come in, so it is given the ordinary form. A path that
        // has no ordinary form is not handed over.
        // It is built from the path's own 16-bit units: as text, a name with a
        // lone surrogate would turn into a look-alike name, and the shell
        // would recycle the look-alike.
        let Some(shell_units) = winpure::shell_path(&units_of(&item.path)) else {
            return Outcome::Refused {
                reason: RefuseReason::CannotResolve,
            };
        };
        let shell_path = OsString::from_wide(&shell_units);
        let report = match recycle_through_shell(Path::new(&shell_path)) {
            Ok(report) => report,
            Err(error) => return Outcome::Failed { error },
        };
        let still_there = std::fs::symlink_metadata(&item.path).is_ok();
        if still_there {
            return Outcome::Failed {
                error: OpError::code("recycle (the item is still there)", report.hresult),
            };
        }
        if !report.reached_bin {
            // The shell removed it outright. Say so.
            return Outcome::Deleted {
                bytes,
                shared_files,
            };
        }
        let recycled = report
            .recycled
            .or_else(|| drive.and_then(|d| find_in_bin(d, &now.real)));
        let Some(recycled) = recycled else {
            return Outcome::Failed {
                error: OpError::code(
                    "recycle (the system did not say where the item went)",
                    report.hresult,
                ),
            };
        };
        let Ok(identity) = identity_of(&recycled) else {
            return Outcome::Failed {
                error: OpError::code("check the recycled item", report.hresult),
            };
        };
        // The shell acted on the path, not on what was checked. If what is in the
        // bin is not that item, something took its place in between: put it back
        // and report that. (An item found by its path in the bin is checked the
        // same way.)
        if identity != now.identity {
            return match move_out_of_bin(&recycled, &now.real) {
                Ok(()) => Outcome::Refused {
                    reason: RefuseReason::ChangedSinceScan,
                },
                Err(error) => Outcome::Failed {
                    error: opfail(
                        "put back an item that was replaced during the recycle",
                        &error,
                    ),
                },
            };
        }
        Outcome::MovedToBin {
            token: RestoreToken {
                original: now.real.clone(),
                inner: TokenInner::Recycled(RecycledToken { recycled, identity }),
            },
            bytes,
            // The bin reports file sizes, not the space they take.
            basis: super::ByteBasis::Logical,
            shared_files,
        }
    }

    pub(crate) fn delete_one(&self, item: &Item, now: &Resolved) -> Outcome {
        // The item is opened again and compared by file identity and final
        // path with what was judged.
        match bs_fsops::win::delete_item(&item.path, now.real.as_os_str(), now.identity) {
            Ok(removed) => Outcome::Deleted {
                bytes: removed.allocated,
                shared_files: removed.kept_by_other_names,
            },
            Err(bs_fsops::DeleteError::NotFound) => Outcome::Refused {
                reason: RefuseReason::NotFound,
            },
            Err(bs_fsops::DeleteError::Replaced) => Outcome::Refused {
                reason: RefuseReason::ChangedSinceScan,
            },
            Err(bs_fsops::DeleteError::Partial { removed, error }) => Outcome::Failed {
                error: OpError {
                    removed_bytes: removed.allocated,
                    removed_entries: removed.entries,
                    ..OpError::io("delete", &error)
                },
            },
            Err(bs_fsops::DeleteError::Look(error)) => Outcome::Failed {
                error: opfail("look at the item", &error),
            },
            Err(other) => Outcome::Failed {
                error: opfail("delete", &io::Error::other(other.to_string())),
            },
        }
    }

    pub(crate) fn restore_one(&self, token: &RestoreToken) -> RestoreOutcome {
        let TokenInner::Recycled(recycled) = &token.inner else {
            return RestoreOutcome::Refused {
                reason: RefuseReason::NotInBin,
            };
        };
        match self.try_restore(token, recycled) {
            Ok(path) => RestoreOutcome::Restored { path },
            Err(Err(reason)) => RestoreOutcome::Refused { reason },
            Err(Ok(error)) => RestoreOutcome::Failed { error },
        }
    }

    fn try_restore(
        &self,
        token: &RestoreToken,
        recycled: &RecycledToken,
    ) -> Result<PathBuf, Result<OpError, RefuseReason>> {
        let not_in_bin = || Err(Err(RefuseReason::NotInBin));
        // Only this user's folder in a bin may be restored from.
        let text = recycled.recycled.to_string_lossy().into_owned();
        let Some(sid) = user_sid() else {
            return not_in_bin();
        };
        let marker = format!(r"\$Recycle.Bin\{sid}\").to_lowercase();
        if !text.to_lowercase().contains(&marker) {
            return not_in_bin();
        }
        // The record is looked up under the item's own name, unit for unit.
        let Some(info_units) = winpure::info_for(&units_of(&recycled.recycled)) else {
            return not_in_bin();
        };
        let info_path = OsString::from_wide(&info_units);
        let record = read_small(Path::new(&info_path))
            .ok()
            .and_then(|bytes| winpure::parse_record(&bytes));
        // The record must say what the token says.
        match record {
            Some(record) if winpure::same_place(&record.original, &units_of(&token.original)) => {}
            _ => return not_in_bin(),
        }
        match identity_of(&recycled.recycled) {
            Ok(identity) if identity == recycled.identity => {}
            _ => return not_in_bin(),
        }
        let Some(parent) = token.original.parent() else {
            return not_in_bin();
        };
        if !std::fs::metadata(parent).is_ok_and(|m| m.is_dir()) {
            return Err(Err(RefuseReason::OriginalFolderMissing));
        }
        if std::fs::symlink_metadata(&token.original).is_ok() {
            return Err(Err(RefuseReason::OriginalExists));
        }
        // The move fails rather than replace anything.
        if let Err(error) = move_out_of_bin(&recycled.recycled, &token.original) {
            return match error.raw_os_error() {
                Some(code)
                    if code == ERROR_ALREADY_EXISTS as i32 || code == ERROR_FILE_EXISTS as i32 =>
                {
                    Err(Err(RefuseReason::OriginalExists))
                }
                _ => Err(Ok(opfail("move back", &error))),
            };
        }
        Ok(token.original.clone())
    }

    pub(crate) fn empty_one(&self, drive: &Drive) -> EmptyOutcome {
        // Emptying a drive's bin cannot be limited to a folder.
        if self.restriction.is_some() {
            return EmptyOutcome::Refused {
                reason: RefuseReason::OutsideRestriction,
            };
        }
        // Only fixed drives have a Recycle Bin; saying "emptied" for any other
        // would claim work that was not done.
        if drive.kind != DriveKind::Fixed {
            return EmptyOutcome::Refused {
                reason: RefuseReason::NoBinOnDrive,
            };
        }
        let (bytes, entries) = match query(&drive.mount_point) {
            Ok(figures) => figures,
            Err(code) => {
                return EmptyOutcome::Failed {
                    error: OpError::code("look at the Recycle Bin", code),
                };
            }
        };
        if entries == 0 {
            return EmptyOutcome::Emptied {
                bytes: 0,
                entries: 0,
            };
        }
        let root = wide(drive.mount_point.as_os_str());
        // SAFETY: `root` is NUL-terminated and outlives the call; no window is
        // used, so the handle is null.
        let hr = unsafe {
            SHEmptyRecycleBinW(
                std::ptr::null_mut(),
                root.as_ptr(),
                SHERB_NOCONFIRMATION | SHERB_NOPROGRESSUI | SHERB_NOSOUND,
            )
        };
        if hr < 0 {
            return EmptyOutcome::Failed {
                error: OpError::code("empty the Recycle Bin", hr),
            };
        }
        EmptyOutcome::Emptied { bytes, entries }
    }
}

/// For a single file, 1 when it has other names (hard links), else 0. Folders
/// are not examined.
fn single_file_links(path: &Path) -> u64 {
    let Ok(handle) = open_nofollow(path, FILE_READ_ATTRIBUTES) else {
        return 0;
    };
    let Ok(standard) = info_by_handle::<FILE_STANDARD_INFO>(&handle, FileStandardInfo) else {
        return 0;
    };
    u64::from(!standard.Directory && standard.NumberOfLinks > 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delete::Decision;
    use crate::drives::list_drives;
    use bs_knowledge::{Knowledge, Level};
    use std::fs;

    /// These tests act on the real Recycle Bin of the user running them, which
    /// is state shared with the whole machine, so they run only where the
    /// environment says the machine is disposable.
    fn disposable_host() -> bool {
        let yes = std::env::var_os("BYTESCAPE_DISPOSABLE_HOST").is_some_and(|v| v == "1");
        if !yes {
            eprintln!("skipped: BYTESCAPE_DISPOSABLE_HOST is not set to 1");
        }
        yes
    }

    /// The Recycle Bin and its stored settings (the limit and the delete-
    /// without-the-bin flag in the volume's BitBucket key) belong to the whole
    /// machine, and tests run side by side. A test that recycles anything, or
    /// changes those settings, holds this for its whole length, so it never
    /// sees another test's setting. Declare it before any settings guard, so
    /// the settings are put back before the next test gets its turn.
    static BIN: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn bin_turn() -> std::sync::MutexGuard<'static, ()> {
        // A test that failed while holding it has already put the settings
        // back (its guard ran while unwinding), so the lock is still good.
        BIN.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("bs-platform-del-{label}-{}", std::process::id()));
            fs::create_dir_all(&root).unwrap();
            Self(fs::canonicalize(root).unwrap())
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let named = self
                .0
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("bs-platform-del-"));
            if named
                && self
                    .0
                    .starts_with(fs::canonicalize(std::env::temp_dir()).unwrap())
            {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
    }

    /// A junction (no special rights needed), else a directory symbolic link.
    fn make_dir_link(link: &Path, target: &Path) -> bool {
        let junction = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .is_ok_and(|o| o.status.success());
        junction || std::os::windows::fs::symlink_dir(target, link).is_ok()
    }

    fn disposal(scratch: &Scratch) -> Disposal {
        let drives = list_drives().expect("drives");
        let env = super::super::environment_for(&drives);
        Disposal::new(Knowledge::new(&env), drives).restricted_to(&scratch.0)
    }

    #[test]
    fn a_file_is_recycled_and_restored() {
        if !disposable_host() {
            return;
        }
        let _bin = bin_turn();
        let scratch = Scratch::new("roundtrip");
        let file = scratch.0.join("recycle-me.txt");
        fs::write(&file, "contents").unwrap();
        let d = disposal(&scratch);
        let seen = identity_of(&file).unwrap();
        let outcomes = d.trash(&[Item::new(&file).seen(seen)], None);
        let Outcome::MovedToBin { token, .. } = &outcomes[0] else {
            panic!("expected the file in the Recycle Bin: {outcomes:?}");
        };
        assert!(!file.exists());
        // The result is the real place in plain form. The fixture path is the
        // canonical one, which carries the `\\?\` prefix, so the two are
        // spelled differently and are compared as places, then by identity
        // and contents.
        let RestoreOutcome::Restored { path } = d.restore(token) else {
            panic!("expected the file to be restored");
        };
        let text = path.to_string_lossy();
        assert!(!text.starts_with(r"\\?\"), "{text}");
        assert!(winpure::same_path(&text, &file.to_string_lossy()), "{text}");
        assert_eq!(identity_of(&path).unwrap(), seen);
        assert_eq!(fs::read_to_string(&path).unwrap(), "contents");
        assert_eq!(fs::read_to_string(&file).unwrap(), "contents");
    }

    #[test]
    fn a_link_is_deleted_as_a_link() {
        if !disposable_host() {
            return;
        }
        let scratch = Scratch::new("junction");
        let target = scratch.0.join("target");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("keep.txt"), "kept").unwrap();
        let link = scratch.0.join("link");
        if !make_dir_link(&link, &target) {
            eprintln!("skipped: cannot create a directory link here");
            return;
        }
        let d = disposal(&scratch);
        let seen = identity_of(&link).unwrap();
        let outcomes = d.delete_permanently(&[Item::new(&link).seen(seen)], None);
        assert!(
            matches!(outcomes[0], Outcome::Deleted { .. }),
            "{outcomes:?}"
        );
        assert!(!link.exists());
        assert_eq!(fs::read_to_string(target.join("keep.txt")).unwrap(), "kept");
    }

    #[test]
    fn a_folder_with_a_junction_is_deleted_without_entering_the_junction() {
        let scratch = Scratch::new("tree");
        let keep = scratch.0.join("keep");
        fs::create_dir_all(&keep).unwrap();
        fs::write(keep.join("precious.txt"), "precious").unwrap();
        let tree = scratch.0.join("tree");
        fs::create_dir_all(tree.join("sub")).unwrap();
        fs::write(tree.join("sub").join("a.txt"), "a").unwrap();
        if !make_dir_link(&tree.join("to-keep"), &keep) {
            eprintln!("skipped: cannot create a directory link here");
            return;
        }
        let drives = list_drives().expect("drives");
        let env = super::super::environment_for(&drives);
        let d = Disposal::new(Knowledge::new(&env), drives).restricted_to(&scratch.0);
        let seen = identity_of(&tree).unwrap();
        let outcomes = d.delete_permanently(&[Item::new(&tree).seen(seen)], None);
        assert!(
            matches!(outcomes[0], Outcome::Deleted { .. }),
            "{outcomes:?}"
        );
        assert!(!tree.exists());
        assert_eq!(
            fs::read_to_string(keep.join("precious.txt")).unwrap(),
            "precious"
        );
    }

    #[test]
    fn an_item_over_the_bin_limit_is_reported_before_anything_moves() {
        if !disposable_host() {
            return;
        }
        let _bin = bin_turn();
        // Lower the limit of the drive that holds the scratch folder for the
        // length of the test, and restore it afterwards.
        let scratch = Scratch::new("overflow");
        let file = scratch.0.join("big.bin");
        fs::write(&file, vec![0u8; 3 * 1024 * 1024]).unwrap();
        let d = disposal(&scratch);
        let Some(drive) = winpure::drive_for(d.drives(), &scratch.0) else {
            panic!("no drive holds the scratch folder");
        };
        let Some(guid) = crate::bins::winpure::volume_guid(drive.id.as_str()) else {
            eprintln!("skipped: the drive has no volume GUID");
            return;
        };
        // Also puts the settings back if the call below panics.
        let settings = SettingsGuard::new(crate::bins::winpure::bit_bucket_key(guid));
        settings.set("NukeOnDelete", 0);
        settings.set("MaxCapacity", 1);
        let outcomes = d.trash(&[Item::new(&file)], None);
        assert!(
            matches!(
                outcomes[0],
                Outcome::WouldBePermanent {
                    reason: PermanentReason::TooLargeForBin { .. }
                }
            ),
            "{outcomes:?}"
        );
        assert!(file.exists(), "the file must not have been touched");
    }

    /// Puts the volume's stored Recycle Bin settings back as they were.
    struct SettingsGuard {
        key: String,
        max: Option<u32>,
        nuke: Option<u32>,
    }

    impl SettingsGuard {
        fn new(key: String) -> Self {
            let max = crate::winreg::read_dword(&key, "MaxCapacity");
            let nuke = crate::winreg::read_dword(&key, "NukeOnDelete");
            Self { key, max, nuke }
        }

        fn set(&self, name: &str, value: u32) {
            crate::winreg::write_dword(&self.key, name, value).expect("write the setting");
        }
    }

    impl Drop for SettingsGuard {
        fn drop(&mut self) {
            for (name, old) in [("MaxCapacity", self.max), ("NukeOnDelete", self.nuke)] {
                match old {
                    Some(value) => {
                        let _ = crate::winreg::write_dword(&self.key, name, value);
                    }
                    None => crate::winreg::delete_value(&self.key, name),
                }
            }
        }
    }

    /// A junction goes to the Recycle Bin as itself and comes back as itself:
    /// what it points to is not recycled, not changed and not looked into.
    #[test]
    fn a_junction_is_recycled_and_restored_as_a_link() {
        if !disposable_host() {
            return;
        }
        let _bin = bin_turn();
        let scratch = Scratch::new("junction-recycle");
        let target = scratch.0.join("target");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("keep.txt"), "kept").unwrap();
        let link = scratch.0.join("link");
        if !make_dir_link(&link, &target) {
            eprintln!("skipped: cannot create a directory link here");
            return;
        }
        let d = disposal(&scratch);
        let seen = identity_of(&link).unwrap();
        let outcomes = d.trash(&[Item::new(&link).seen(seen)], None);
        let Outcome::MovedToBin { token, .. } = &outcomes[0] else {
            panic!("expected the junction in the Recycle Bin: {outcomes:?}");
        };
        assert!(fs::symlink_metadata(&link).is_err());
        assert_eq!(fs::read_to_string(target.join("keep.txt")).unwrap(), "kept");
        assert!(matches!(d.restore(token), RestoreOutcome::Restored { .. }));
        let restored = fs::symlink_metadata(&link).unwrap();
        assert!(
            std::os::windows::fs::MetadataExt::file_attributes(&restored)
                & FILE_ATTRIBUTE_REPARSE_POINT
                != 0,
            "the restored item must still be a link"
        );
        assert_eq!(fs::read_to_string(target.join("keep.txt")).unwrap(), "kept");
    }

    /// The precheck reads `MaxCapacity` as megabytes. This asks the shell
    /// itself, with nothing between, to recycle a file just under that limit.
    /// If the unit were wrong the shell would turn the file away: it would
    /// delete it outright (reported as `Deleted`) or put up its warning and
    /// wait for an answer, which the timeout turns into a failure.
    #[test]
    fn the_shell_recycles_a_file_just_under_the_limit_as_the_precheck_reads_it() {
        if !disposable_host() {
            return;
        }
        let _bin = bin_turn();
        let scratch = Scratch::new("limit-edge");
        let probe = disposal(&scratch);
        let Some(drive) = winpure::drive_for(probe.drives(), &scratch.0) else {
            panic!("no drive holds the scratch folder");
        };
        let Some(guid) = crate::bins::winpure::volume_guid(drive.id.as_str()) else {
            eprintln!("skipped: the drive has no volume GUID");
            return;
        };
        let settings = SettingsGuard::new(crate::bins::winpure::bit_bucket_key(guid));
        settings.set("NukeOnDelete", 0);
        settings.set("MaxCapacity", 4);
        let file = scratch.0.join("just-under.bin");
        // 4 MB less 4 KB: over the limit if it were read as kilobytes, under it
        // as megabytes.
        fs::write(&file, vec![1u8; 4 * 1024 * 1024 - 4096]).unwrap();
        let seen = identity_of(&file).unwrap();
        let item = Item::new(&file).seen(seen);
        assert_eq!(
            probe.precheck(std::slice::from_ref(&item), None).entries[0].decision,
            Decision::Proceed,
            "the precheck must let the file through"
        );

        let root = scratch.0.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let drives = list_drives().expect("drives");
            let env = super::super::environment_for(&drives);
            let d = Disposal::new(Knowledge::new(&env), drives).restricted_to(&root);
            let outcome = d.trash(&[item], None);
            let restored = match &outcome[0] {
                Outcome::MovedToBin { token, .. } => Some(d.restore(token)),
                _ => None,
            };
            let _ = sender.send((outcome, restored));
        });
        let Ok((outcome, restored)) = receiver.recv_timeout(std::time::Duration::from_secs(90))
        else {
            panic!(
                "the shell did not answer in 90 s: it is probably waiting on a warning \
                 about a delete it would not recycle, so the limit is read in the wrong unit"
            );
        };
        assert!(
            matches!(outcome[0], Outcome::MovedToBin { .. }),
            "the shell must recycle a file just under the limit: {outcome:?}"
        );
        assert!(matches!(restored, Some(RestoreOutcome::Restored { .. })));
        assert!(file.exists());
    }

    /// The cases the shell would delete outright (a file over the volume's
    /// limit, a volume set to delete without the bin) are turned away by the
    /// precheck, so the shell is never asked and its warning dialog never
    /// appears on a machine with nobody to answer it. A small file under a
    /// generous limit still goes to the bin.
    ///
    /// The one case this does not cover is an item the shell refuses to
    /// recycle for a reason the stored settings do not show. There the shell
    /// itself warns and the delete waits for an answer; declining leaves the
    /// item where it is.
    #[test]
    fn the_precheck_routes_the_overflow_cases_so_the_shell_is_never_asked() {
        if !disposable_host() {
            return;
        }
        let _bin = bin_turn();
        let scratch = Scratch::new("limits");
        let d = disposal(&scratch);
        let Some(drive) = winpure::drive_for(d.drives(), &scratch.0) else {
            panic!("no drive holds the scratch folder");
        };
        let Some(guid) = crate::bins::winpure::volume_guid(drive.id.as_str()) else {
            eprintln!("skipped: the drive has no volume GUID");
            return;
        };
        let settings = SettingsGuard::new(crate::bins::winpure::bit_bucket_key(guid));
        let make = |name: &str, bytes: usize| {
            let file = scratch.0.join(name);
            fs::write(&file, vec![1u8; bytes]).unwrap();
            file
        };

        // A limit of 1 (megabyte) turns a 3 MB file away before the shell.
        settings.set("NukeOnDelete", 0);
        settings.set("MaxCapacity", 1);
        let big = make("over-the-limit.bin", 3 * 1024 * 1024);
        let outcome = only_outcome(d.trash(&[Item::new(&big)], None));
        assert!(
            matches!(
                outcome,
                Outcome::WouldBePermanent {
                    reason: PermanentReason::TooLargeForBin { .. }
                }
            ),
            "{outcome:?}"
        );
        assert!(big.exists(), "the file must not have been touched");

        // A volume set to delete without the bin turns everything away.
        settings.set("MaxCapacity", 4096);
        settings.set("NukeOnDelete", 1);
        let small = make("nuke.bin", 10);
        let outcome = only_outcome(d.trash(&[Item::new(&small)], None));
        assert_eq!(
            outcome,
            Outcome::WouldBePermanent {
                reason: PermanentReason::BinDisabled
            }
        );
        assert!(small.exists());

        // A small file under the generous limit goes to the bin and back.
        settings.set("NukeOnDelete", 0);
        let outcome = only_outcome(d.trash(&[Item::new(&small)], None));
        let Outcome::MovedToBin { token, .. } = outcome else {
            panic!("expected the file in the Recycle Bin: {outcome:?}");
        };
        assert!(!small.exists());
        assert!(matches!(d.restore(&token), RestoreOutcome::Restored { .. }));
        assert!(small.exists());
    }

    fn only_outcome(mut outcomes: Vec<Outcome>) -> Outcome {
        assert_eq!(outcomes.len(), 1);
        outcomes.remove(0)
    }

    #[test]
    fn the_system_folders_and_drive_roots_are_blocked() {
        // Only looks at paths; nothing is changed.
        let d = Disposal::system().expect("drives");
        let system = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        let mut paths = vec![
            system.clone(),
            format!(r"{system}\System32"),
            format!(r"{}\", system.chars().take(2).collect::<String>()),
            r"C:\Program Files".to_owned(),
            r"C:\Windows\.".to_owned(),
            r"\\?\C:\Windows".to_owned(),
        ];
        for drive in d.drives() {
            paths.push(drive.mount_point.to_string_lossy().into_owned());
        }
        for path in paths {
            let plan = d.precheck(&[Item::new(&path)], None);
            assert_eq!(plan.strictest, Level::Blocked, "{path}");
            assert!(plan.selection_refused, "{path}");
        }
    }
}
