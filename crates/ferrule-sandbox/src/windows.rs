//! The Windows tier-1 backend: a restricted token inside a job object, no
//! admin needed. The design is Codex's unelevated backend
//! (`codex-rs/windows-sandbox-rs`, Apache-2.0), adapted; the reasoning is
//! in `docs/m26-isolation.md` §2.
//!
//! - Writes: the token is `WRITE_RESTRICTED` with one capability SID per
//!   writable root, and each root's DACL grants its SID. Every write needs
//!   both the user's access and a restricting SID's, so it only lands in a
//!   root. Reads are judged as for the user.
//! - Reads of ferrule's own secrets: Authenticated Users is deny-only in the
//!   sandbox token, and those paths carry a protected DACL that lets the
//!   user in only while that group is enabled ([`protect`]).
//! - Ferrule's process and threads carry the same condition, so the
//!   program can't read ferrule's memory or environment.
//! - The job kills the whole tree when the launcher goes, caps the
//!   process count and, optionally, memory.

use crate::launch::{self, process_sddl, protected_sddl, Spec};
use std::collections::BTreeMap;
use std::ffi::{c_void, OsStr, OsString};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, SetHandleInformation, ERROR_INSUFFICIENT_BUFFER,
    ERROR_SUCCESS, GENERIC_READ, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW,
    ConvertStringSecurityDescriptorToSecurityDescriptorW, ConvertStringSidToSidW,
    GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW, SetSecurityInfo,
    EXPLICIT_ACCESS_W, GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, SDDL_REVISION_1, SE_FILE_OBJECT,
    SE_KERNEL_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    CopySid, CreateRestrictedToken, CreateWellKnownSid, EqualSid, GetLengthSid,
    GetSecurityDescriptorDacl, GetTokenInformation, SetTokenInformation, TokenDefaultDacl,
    TokenGroups, TokenUser, WinAuthenticatedUserSid, WinRestrictedCodeSid, WinWorldSid, ACL,
    DACL_SECURITY_INFORMATION, DISABLE_MAX_PRIVILEGE, LUA_TOKEN,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_MAX_SID_SIZE,
    SID_AND_ATTRIBUTES, SUB_CONTAINERS_AND_OBJECTS_INHERIT, TOKEN_ADJUST_DEFAULT,
    TOKEN_ASSIGN_PRIMARY, TOKEN_DEFAULT_DACL, TOKEN_DUPLICATE, TOKEN_GROUPS, TOKEN_QUERY,
    TOKEN_USER, WELL_KNOWN_SID_TYPE, WRITE_RESTRICTED,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, DELETE, FILE_DELETE_CHILD, FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_EXECUTE,
    FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_JOB_MEMORY,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::StationsAndDesktops::{
    GetProcessWindowStation, GetThreadDesktop, GetUserObjectInformationW, UOI_NAME,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessAsUserW, DeleteProcThreadAttributeList, GetCurrentProcess, GetCurrentProcessId,
    GetCurrentThreadId, GetExitCodeProcess, InitializeProcThreadAttributeList, OpenProcessToken,
    OpenThread, UpdateProcThreadAttribute, WaitForSingleObject, EXTENDED_STARTUPINFO_PRESENT,
    INFINITE, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROC_THREAD_ATTRIBUTE_JOB_LIST, STARTF_USESTDHANDLES, STARTUPINFOEXW,
};

/// `SE_GROUP_ENABLED` and `SE_GROUP_LOGON_ID`, from `SystemServices`.
const GROUP_ENABLED: u32 = 0x4;
const GROUP_LOGON_ID: u32 = 0xC000_0000;
/// `WRITE_DAC` for a thread.
const THREAD_WRITE_DAC: u32 = 0x0004_0000;

/// What a writable root's capability SID may do: modify, never change the
/// DACL or the owner.
const ROOT_RIGHTS: u32 =
    FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE | FILE_DELETE_CHILD;

fn os_err(what: &str) -> String {
    format!("{what}: {}", io::Error::last_os_error())
}

fn win32_err(what: &str, code: u32) -> String {
    format!("{what}: {}", io::Error::from_raw_os_error(code as i32))
}

fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain([0]).collect()
}

/// A NUL-terminated wide string from `LocalAlloc`, as a `String`, freed.
unsafe fn take_local_wstr(p: *mut u16) -> String {
    let mut len = 0;
    while *p.add(len) != 0 {
        len += 1;
    }
    let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
    LocalFree(p.cast());
    s
}

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: we own the handle.
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// A `LocalAlloc`ed block (security descriptors, ACLs), freed on drop.
struct Local(*mut c_void);

impl Drop for Local {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: allocated by the API that returned it, owned here.
            unsafe { LocalFree(self.0) };
        }
    }
}

/// An owned SID.
#[derive(Clone)]
struct Sid(Vec<u8>);

impl Sid {
    fn ptr(&self) -> PSID {
        self.0.as_ptr() as PSID
    }

    /// Copies the SID at `p`.
    unsafe fn copy(p: PSID) -> Result<Self, String> {
        let len = GetLengthSid(p);
        let mut buf = vec![0u8; len as usize];
        if CopySid(len, buf.as_mut_ptr().cast(), p) == 0 {
            return Err(os_err("CopySid"));
        }
        Ok(Sid(buf))
    }

    fn well_known(kind: WELL_KNOWN_SID_TYPE) -> Result<Self, String> {
        let mut buf = vec![0u8; SECURITY_MAX_SID_SIZE as usize];
        let mut len = SECURITY_MAX_SID_SIZE;
        // SAFETY: the buffer holds the largest SID there is.
        if unsafe { CreateWellKnownSid(kind, null_mut(), buf.as_mut_ptr().cast(), &mut len) } == 0 {
            return Err(os_err("CreateWellKnownSid"));
        }
        buf.truncate(len as usize);
        Ok(Sid(buf))
    }

    fn parse(s: &str) -> Result<Self, String> {
        let mut p: PSID = null_mut();
        // SAFETY: `p` is LocalAlloc'd by the call and freed after the copy.
        unsafe {
            if ConvertStringSidToSidW(wide(s).as_ptr(), &mut p) == 0 {
                return Err(os_err(&format!("bad SID {s}")));
            }
            let _free = Local(p);
            Sid::copy(p)
        }
    }

    fn string(&self) -> Result<String, String> {
        let mut p: *mut u16 = null_mut();
        // SAFETY: the SID is valid; the string is LocalAlloc'd and freed.
        unsafe {
            if ConvertSidToStringSidW(self.ptr(), &mut p) == 0 {
                return Err(os_err("ConvertSidToStringSidW"));
            }
            Ok(take_local_wstr(p))
        }
    }

    fn eq(&self, other: PSID) -> bool {
        // SAFETY: both are valid SIDs.
        unsafe { EqualSid(self.ptr(), other) != 0 }
    }
}

// ---- tokens ----

fn own_token(access: u32) -> Result<Handle, String> {
    let mut h: HANDLE = null_mut();
    // SAFETY: plain call on our own process.
    if unsafe { OpenProcessToken(GetCurrentProcess(), access, &mut h) } == 0 {
        return Err(os_err("OpenProcessToken"));
    }
    Ok(Handle(h))
}

/// `GetTokenInformation` into a suitably aligned buffer.
fn token_info(token: HANDLE, class: i32) -> Result<Vec<u64>, String> {
    let mut len = 0u32;
    // SAFETY: a size query, then a call with a buffer of that size.
    unsafe {
        GetTokenInformation(token, class, null_mut(), 0, &mut len);
        if GetLastError() != ERROR_INSUFFICIENT_BUFFER {
            return Err(os_err("GetTokenInformation"));
        }
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        if GetTokenInformation(token, class, buf.as_mut_ptr().cast(), len, &mut len) == 0 {
            return Err(os_err("GetTokenInformation"));
        }
        Ok(buf)
    }
}

struct Identity {
    user: Sid,
    logon: Option<Sid>,
    /// Authenticated Users is enabled — what the conditional ACEs key on.
    au_enabled: bool,
}

fn identity(token: HANDLE) -> Result<Identity, String> {
    let au = Sid::well_known(WinAuthenticatedUserSid)?;
    let user_buf = token_info(token, TokenUser)?;
    let groups_buf = token_info(token, TokenGroups)?;
    // SAFETY: the buffers hold what GetTokenInformation wrote for these
    // classes; the SIDs point into them and are copied out.
    unsafe {
        let user = Sid::copy((*(user_buf.as_ptr() as *const TOKEN_USER)).User.Sid)?;
        let groups = &*(groups_buf.as_ptr() as *const TOKEN_GROUPS);
        let list = std::slice::from_raw_parts(groups.Groups.as_ptr(), groups.GroupCount as usize);
        let mut logon = None;
        let mut au_enabled = false;
        for g in list {
            if g.Attributes & GROUP_LOGON_ID == GROUP_LOGON_ID && logon.is_none() {
                logon = Some(Sid::copy(g.Sid)?);
            }
            if au.eq(g.Sid) && g.Attributes & GROUP_ENABLED != 0 {
                au_enabled = true;
            }
        }
        Ok(Identity {
            user,
            logon,
            au_enabled,
        })
    }
}

// ---- security descriptors ----

/// A security descriptor parsed from SDDL.
struct Sd(Local);

impl Sd {
    fn parse(sddl: &str) -> Result<Self, String> {
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: the descriptor is LocalAlloc'd and owned by `Local`.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide(sddl).as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                null_mut(),
            )
        } == 0
        {
            return Err(os_err(&format!("bad SDDL {sddl}")));
        }
        Ok(Sd(Local(sd)))
    }

    fn dacl(&self) -> Result<*mut ACL, String> {
        let (mut present, mut defaulted) = (0, 0);
        let mut dacl: *mut ACL = null_mut();
        // SAFETY: a valid descriptor; the DACL points into it.
        if unsafe { GetSecurityDescriptorDacl(self.0 .0, &mut present, &mut dacl, &mut defaulted) }
            == 0
            || present == 0
        {
            return Err(os_err("GetSecurityDescriptorDacl"));
        }
        Ok(dacl)
    }

    /// The DACL part as SDDL, the way Windows writes it back.
    fn dacl_sddl(&self) -> Result<String, String> {
        dacl_sddl(self.0 .0)
    }
}

fn dacl_sddl(sd: PSECURITY_DESCRIPTOR) -> Result<String, String> {
    let mut p: *mut u16 = null_mut();
    // SAFETY: a valid descriptor; the string is LocalAlloc'd and freed.
    unsafe {
        if ConvertSecurityDescriptorToStringSecurityDescriptorW(
            sd,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &mut p,
            null_mut(),
        ) == 0
        {
            return Err(os_err(
                "ConvertSecurityDescriptorToStringSecurityDescriptorW",
            ));
        }
        Ok(take_local_wstr(p))
    }
}

/// A file's current DACL: the descriptor (which owns it) and the pointer.
fn file_dacl(path: &Path) -> Result<(Local, *mut ACL), String> {
    let mut dacl: *mut ACL = null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: out-pointers; the descriptor is LocalAlloc'd, owned by `Local`.
    let rc = unsafe {
        GetNamedSecurityInfoW(
            wide(path).as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut sd,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(win32_err(
            &format!("reading the ACL of {}", path.display()),
            rc,
        ));
    }
    Ok((Local(sd), dacl))
}

/// Puts the protected DACL on `path` unless it's already there, then
/// checks ferrule can still open it and puts the old DACL back if not.
fn protect(path: &Path, id: &Identity) -> Result<(), String> {
    let want = Sd::parse(&protected_sddl(&id.user.string()?, path.is_dir()))?;
    let (old_sd, old_dacl) = file_dacl(path)?;
    if dacl_sddl(old_sd.0)? == want.dacl_sddl()? {
        return Ok(());
    }
    set_file_dacl(path, want.dacl()?, true)?;
    if let Err(e) = can_open(path) {
        let _ = set_file_dacl(path, old_dacl, false);
        return Err(format!(
            "protecting {} would lock ferrule out too ({e}); left as it was",
            path.display()
        ));
    }
    Ok(())
}

fn set_file_dacl(path: &Path, dacl: *const ACL, protected: bool) -> Result<(), String> {
    let mut info = DACL_SECURITY_INFORMATION;
    if protected {
        info |= PROTECTED_DACL_SECURITY_INFORMATION;
    }
    // SAFETY: a valid DACL owned by the caller.
    let rc = unsafe {
        SetNamedSecurityInfoW(
            wide(path).as_ptr(),
            SE_FILE_OBJECT,
            info,
            null_mut(),
            null_mut(),
            dacl,
            null(),
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(win32_err(
            &format!("setting the ACL of {}", path.display()),
            rc,
        ));
    }
    Ok(())
}

fn can_open(path: &Path) -> io::Result<()> {
    // SAFETY: plain open, closed right away.
    let h = unsafe {
        CreateFileW(
            wide(path).as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    drop(Handle(h));
    Ok(())
}

/// Adds an inheritable allow ACE for `sid` to `root`, unless its DACL
/// already names the SID. Setting it propagates to what's inside, which
/// takes a while on a big tree, once.
fn grant(root: &Path, sid: &Sid) -> Result<(), String> {
    let sid_str = sid.string()?;
    let (sd, dacl) = file_dacl(root)?;
    if dacl_sddl(sd.0)?.contains(&sid_str) {
        return Ok(());
    }
    let ea = EXPLICIT_ACCESS_W {
        grfAccessPermissions: ROOT_RIGHTS,
        grfAccessMode: GRANT_ACCESS,
        grfInheritance: SUB_CONTAINERS_AND_OBJECTS_INHERIT,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid.ptr().cast(),
        },
    };
    let mut new_acl: *mut ACL = null_mut();
    // SAFETY: one entry, the old DACL borrowed from `sd`; the new one is
    // LocalAlloc'd and owned by `_free`.
    let rc = unsafe { SetEntriesInAclW(1, &ea, dacl, &mut new_acl) };
    if rc != ERROR_SUCCESS {
        return Err(win32_err("SetEntriesInAclW", rc));
    }
    let _free = Local(new_acl.cast());
    if let Err(e) = set_file_dacl(root, new_acl, false) {
        // Propagation stops at a child it can't write; the root itself may
        // have taken the ACE, which is what counts.
        let (sd, _) = file_dacl(root)?;
        if !dacl_sddl(sd.0)?.contains(&sid_str) {
            return Err(e);
        }
        tracing::warn!("sandbox: granting {}: {e}", root.display());
    }
    Ok(())
}

// ---- capability SIDs ----

/// One random `S-1-5-21-…` per writable root, kept in `state` so a root's
/// ACE is written once.
fn capability_sids(roots: &[PathBuf], state: &Path) -> Result<Vec<Sid>, String> {
    let key = |p: &Path| p.to_string_lossy().to_lowercase();
    let mut map: BTreeMap<String, String> = std::fs::read_to_string(state)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let mut changed = false;
    for root in roots {
        if let std::collections::btree_map::Entry::Vacant(e) = map.entry(key(root)) {
            e.insert(random_sid());
            changed = true;
        }
    }
    if changed {
        if let Some(dir) = state.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = state.with_extension(format!("tmp{}", std::process::id()));
        let json = serde_json::to_string_pretty(&map).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, json)
            .and_then(|_| std::fs::rename(&tmp, state))
            .map_err(|e| format!("saving {}: {e}", state.display()))?;
    }
    roots.iter().map(|r| Sid::parse(&map[&key(r)])).collect()
}

fn random_sid() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut parts = [0u32; 4];
    for (i, part) in parts.iter_mut().enumerate() {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_usize(i);
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default(),
        );
        *part = h.finish() as u32;
    }
    format!(
        "S-1-5-21-{}-{}-{}-{}",
        parts[0], parts[1], parts[2], parts[3]
    )
}

fn state_file(spec: &Spec) -> PathBuf {
    if let Some(p) = std::env::var_os(launch::STATE_VAR).filter(|v| !v.is_empty()) {
        return p.into();
    }
    if let Some(p) = &spec.state_file {
        return p.clone();
    }
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("ferrule-sandbox")
        .join("windows-caps.json")
}

// ---- ferrule's own process ----

/// Shuts ferrule's process and threads to the sandbox token, and makes
/// that the default for what it creates next (the launcher included).
/// Nothing with Authenticated Users enabled notices. A no-op when the
/// current token lacks that group, since the condition would shut ferrule
/// out of itself.
pub fn harden_self() -> Result<(), String> {
    let token = own_token(TOKEN_QUERY | TOKEN_ADJUST_DEFAULT)?;
    let id = identity(token.0)?;
    if !id.au_enabled {
        return Ok(());
    }
    let sd = Sd::parse(&process_sddl(&id.user.string()?))?;
    let dacl = sd.dacl()?;
    // SAFETY: the pseudo-handle has every right on our own process.
    let rc = unsafe {
        SetSecurityInfo(
            GetCurrentProcess(),
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            dacl,
            null(),
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(win32_err("securing ferrule's process", rc));
    }
    for tid in own_threads()? {
        // SAFETY: a thread of ours, the handle closed after use.
        let h = unsafe { OpenThread(THREAD_WRITE_DAC, 0, tid) };
        if h.is_null() {
            continue;
        }
        let h = Handle(h);
        // SAFETY: as above, with WRITE_DAC on the thread.
        unsafe {
            SetSecurityInfo(
                h.0,
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                dacl,
                null(),
            )
        };
    }
    let default = TOKEN_DEFAULT_DACL { DefaultDacl: dacl };
    // SAFETY: the DACL outlives the call, which copies it.
    if unsafe {
        SetTokenInformation(
            token.0,
            TokenDefaultDacl,
            (&default as *const TOKEN_DEFAULT_DACL).cast(),
            std::mem::size_of::<TOKEN_DEFAULT_DACL>() as u32,
        )
    } == 0
    {
        return Err(os_err("setting ferrule's default DACL"));
    }
    Ok(())
}

fn own_threads() -> Result<Vec<u32>, String> {
    // SAFETY: a snapshot walked with a correctly sized entry.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snap == INVALID_HANDLE_VALUE {
            return Err(os_err("CreateToolhelp32Snapshot"));
        }
        let snap = Handle(snap);
        let pid = GetCurrentProcessId();
        let mut entry: THREADENTRY32 = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        let mut out = Vec::new();
        let mut ok = Thread32First(snap.0, &mut entry);
        while ok != 0 {
            if entry.th32OwnerProcessID == pid {
                out.push(entry.th32ThreadID);
            }
            ok = Thread32Next(snap.0, &mut entry);
        }
        Ok(out)
    }
}

// ---- the launch ----

/// Starts `program args…` under the spec's token in a new job, waits, and
/// returns its exit code.
pub fn launch(spec: &Spec, program: &Path, args: &[OsString]) -> Result<i32, String> {
    // Best effort: the program must not be able to open the launcher, which
    // holds the job and a normal token.
    if let Err(e) = harden_self() {
        tracing::debug!("sandbox launcher: {e}");
    }
    // The restricted token's handle gets this handle's rights, and setting
    // its default DACL needs TOKEN_ADJUST_DEFAULT.
    let token =
        own_token(TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_DEFAULT)?;
    let id = identity(token.0)?;

    if id.au_enabled {
        for path in &spec.protect {
            if let Err(e) = protect(path, &id) {
                eprintln!("ferrule sandbox launcher: {e}");
            }
        }
    }
    let caps = if spec.confine_writes {
        let caps = capability_sids(&spec.write_roots, &state_file(spec))?;
        for (root, sid) in spec.write_roots.iter().zip(&caps) {
            grant(root, sid)?;
        }
        caps
    } else {
        Vec::new()
    };

    let restricted = restricted_token(token.0, &id, &caps, spec.confine_writes)?;
    let job = job(spec)?;
    let (app, mut cmdline) = command_line(program, args)?;
    spawn_in_job(restricted.0, job.0, &app, &mut cmdline)
}

fn restricted_token(
    token: HANDLE,
    id: &Identity,
    caps: &[Sid],
    confine_writes: bool,
) -> Result<Handle, String> {
    let au = Sid::well_known(WinAuthenticatedUserSid)?;
    let disable = [SID_AND_ATTRIBUTES {
        Sid: au.ptr(),
        Attributes: 0,
    }];
    let mut restrict: Vec<Sid> = Vec::new();
    let mut flags = DISABLE_MAX_PRIVILEGE | LUA_TOKEN;
    if confine_writes {
        flags |= WRITE_RESTRICTED;
        restrict.extend(caps.iter().cloned());
        restrict.extend(id.logon.clone());
        restrict.push(Sid::well_known(WinWorldSid)?);
        // RESTRICTED (S-1-5-12): what the named-object directories and a
        // few registry keys grant restricted code, without which MSYS and
        // PowerShell can't create their named objects.
        restrict.push(Sid::well_known(WinRestrictedCodeSid)?);
    }
    let restrict_attrs: Vec<SID_AND_ATTRIBUTES> = restrict
        .iter()
        .map(|s| SID_AND_ATTRIBUTES {
            Sid: s.ptr(),
            Attributes: 0,
        })
        .collect();
    let mut new: HANDLE = null_mut();
    // SAFETY: the SID arrays outlive the call.
    if unsafe {
        CreateRestrictedToken(
            token,
            flags,
            disable.len() as u32,
            disable.as_ptr(),
            0,
            null(),
            restrict_attrs.len() as u32,
            if restrict_attrs.is_empty() {
                null()
            } else {
                restrict_attrs.as_ptr()
            },
            &mut new,
        )
    } == 0
    {
        return Err(os_err("CreateRestrictedToken"));
    }
    let new = Handle(new);
    // What the program creates (its processes, pipes, named objects) is
    // open to its own logon session and SYSTEM, not to the user's whole
    // token — so nothing but the tree can rewrite it.
    let owner = match &id.logon {
        Some(logon) => logon.string()?,
        None => id.user.string()?,
    };
    let sd = Sd::parse(&format!("D:(A;;GA;;;{owner})(A;;GA;;;SY)(A;;RC;;;OW)"))?;
    let default = TOKEN_DEFAULT_DACL {
        DefaultDacl: sd.dacl()?,
    };
    // SAFETY: the DACL outlives the call, which copies it.
    if unsafe {
        SetTokenInformation(
            new.0,
            TokenDefaultDacl,
            (&default as *const TOKEN_DEFAULT_DACL).cast(),
            std::mem::size_of::<TOKEN_DEFAULT_DACL>() as u32,
        )
    } == 0
    {
        return Err(os_err("setting the sandbox token's default DACL"));
    }
    Ok(new)
}

fn job(spec: &Spec) -> Result<Handle, String> {
    // SAFETY: an unnamed job; the limits struct is fully initialised.
    unsafe {
        let job = CreateJobObjectW(null(), null());
        if job.is_null() {
            return Err(os_err("CreateJobObjectW"));
        }
        let job = Handle(job);
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        let mut flags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
        if spec.process_limit > 0 {
            flags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
            info.BasicLimitInformation.ActiveProcessLimit = spec.process_limit;
        }
        if let Some(mb) = spec.memory_mb {
            flags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
            info.JobMemoryLimit = (mb as usize).saturating_mul(1024 * 1024);
        }
        info.BasicLimitInformation.LimitFlags = flags;
        if SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
        {
            return Err(os_err("SetInformationJobObject"));
        }
        Ok(job)
    }
}

/// The application path and the command line (NUL-terminated, mutable as
/// `CreateProcessW` wants). Batch files run through `cmd.exe /d /s /c`.
fn command_line(program: &Path, args: &[OsString]) -> Result<(Vec<u16>, Vec<u16>), String> {
    let path = std::env::var_os("PATH");
    let pathext = std::env::var("PATHEXT").ok();
    let resolved = launch::resolve_program(program, path.as_deref(), pathext.as_deref(), |p| {
        p.is_file()
    })
    .ok_or_else(|| format!("{}: program not found", program.display()))?;
    let wide_args: Vec<Vec<u16>> = args.iter().map(|a| a.encode_wide().collect()).collect();
    if launch::is_batch(&resolved) {
        for a in args {
            if !launch::batch_safe(&a.to_string_lossy()) {
                return Err(format!(
                    "{}: an argument to a batch file can't hold \", %, ! or a line break",
                    resolved.display()
                ));
            }
        }
        let cmd = std::env::var_os("ComSpec")
            .map(PathBuf::from)
            .filter(|p| p.is_file())
            .or_else(|| {
                std::env::var_os("SystemRoot")
                    .map(|r| PathBuf::from(r).join("System32").join("cmd.exe"))
            })
            .ok_or("can't find cmd.exe to run a batch file")?;
        let script: Vec<u16> = resolved.as_os_str().encode_wide().collect();
        let inner = launch::command_line(
            std::iter::once(script.as_slice()).chain(wide_args.iter().map(Vec::as_slice)),
        );
        let mut line: Vec<u16> = "cmd.exe /d /s /c \"".encode_utf16().collect();
        line.extend(inner);
        line.push(b'"' as u16);
        line.push(0);
        return Ok((wide(&cmd), line));
    }
    let argv0: Vec<u16> = resolved.as_os_str().encode_wide().collect();
    let mut line = launch::command_line(
        std::iter::once(argv0.as_slice()).chain(wide_args.iter().map(Vec::as_slice)),
    );
    line.push(0);
    Ok((wide(&resolved), line))
}

/// The launcher's own `station\desktop`, which the program must be told
/// or PowerShell fails to start under a restricted token.
fn desktop_name() -> Vec<u16> {
    fn name(h: HANDLE) -> Option<String> {
        let mut buf = [0u16; 256];
        let mut needed = 0u32;
        // SAFETY: a fixed buffer, its size in bytes.
        let ok = unsafe {
            GetUserObjectInformationW(
                h,
                UOI_NAME,
                buf.as_mut_ptr().cast(),
                std::mem::size_of_val(&buf) as u32,
                &mut needed,
            )
        };
        if ok == 0 {
            return None;
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    }
    // SAFETY: both return handles we don't own and mustn't close.
    let (station, desktop) = unsafe {
        (
            name(GetProcessWindowStation() as HANDLE),
            name(GetThreadDesktop(GetCurrentThreadId()) as HANDLE),
        )
    };
    let full = match (station, desktop) {
        (Some(s), Some(d)) => format!("{s}\\{d}"),
        _ => "winsta0\\default".to_string(),
    };
    wide(full)
}

fn spawn_in_job(
    token: HANDLE,
    job: HANDLE,
    app: &[u16],
    cmdline: &mut [u16],
) -> Result<i32, String> {
    // SAFETY: every pointer handed to the calls below lives until
    // CreateProcessAsUserW returns; the attribute list is deleted after.
    unsafe {
        let std_handles = [
            GetStdHandle(STD_INPUT_HANDLE),
            GetStdHandle(STD_OUTPUT_HANDLE),
            GetStdHandle(STD_ERROR_HANDLE),
        ];
        let mut inherit: Vec<HANDLE> = Vec::new();
        for &h in &std_handles {
            if !h.is_null()
                && h != INVALID_HANDLE_VALUE
                && !inherit.contains(&h)
                && SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) != 0
            {
                inherit.push(h);
            }
        }

        let attrs = if inherit.is_empty() { 1 } else { 2 };
        let mut size = 0usize;
        InitializeProcThreadAttributeList(null_mut(), attrs, 0, &mut size);
        let mut attr_buf = vec![0u64; size.div_ceil(8)];
        let list: LPPROC_THREAD_ATTRIBUTE_LIST = attr_buf.as_mut_ptr().cast();
        if InitializeProcThreadAttributeList(list, attrs, 0, &mut size) == 0 {
            return Err(os_err("InitializeProcThreadAttributeList"));
        }
        struct Attrs(LPPROC_THREAD_ATTRIBUTE_LIST);
        impl Drop for Attrs {
            fn drop(&mut self) {
                // SAFETY: initialised above.
                unsafe { DeleteProcThreadAttributeList(self.0) };
            }
        }
        let _attrs = Attrs(list);
        let jobs = [job];
        if UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
            jobs.as_ptr().cast(),
            std::mem::size_of_val(&jobs),
            null_mut(),
            null(),
        ) == 0
        {
            return Err(os_err("UpdateProcThreadAttribute(JOB_LIST)"));
        }
        if !inherit.is_empty()
            && UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                inherit.as_ptr().cast(),
                std::mem::size_of_val(inherit.as_slice()),
                null_mut(),
                null(),
            ) == 0
        {
            return Err(os_err("UpdateProcThreadAttribute(HANDLE_LIST)"));
        }

        let mut desktop = desktop_name();
        let mut si: STARTUPINFOEXW = std::mem::zeroed();
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        si.StartupInfo.lpDesktop = desktop.as_mut_ptr();
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = std_handles[0];
        si.StartupInfo.hStdOutput = std_handles[1];
        si.StartupInfo.hStdError = std_handles[2];
        si.lpAttributeList = list;

        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        if CreateProcessAsUserW(
            token,
            app.as_ptr(),
            cmdline.as_mut_ptr(),
            null(),
            null(),
            i32::from(!inherit.is_empty()),
            EXTENDED_STARTUPINFO_PRESENT,
            null(),
            null(),
            &si.StartupInfo,
            &mut pi,
        ) == 0
        {
            return Err(os_err(&format!(
                "starting {}",
                String::from_utf16_lossy(&app[..app.len() - 1])
            )));
        }
        let process = Handle(pi.hProcess);
        drop(Handle(pi.hThread));
        WaitForSingleObject(process.0, INFINITE);
        let mut code = 0u32;
        if GetExitCodeProcess(process.0, &mut code) == 0 {
            return Err(os_err("GetExitCodeProcess"));
        }
        Ok(code as i32)
    }
}

/// Whether `OpenProcess(PROCESS_VM_READ)` on `pid` works from here — the
/// Windows twin of reading `/proc/<pid>/environ`. For tests.
pub fn can_read_process(pid: u32) -> bool {
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_VM_READ};
    // SAFETY: plain open, closed right away.
    let h = unsafe { OpenProcess(PROCESS_VM_READ, 0, pid) };
    if h.is_null() {
        return false;
    }
    drop(Handle(h));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sddl_strings_parse_and_round_trip() {
        let user = "S-1-5-21-1-2-3-1001";
        for sddl in [
            protected_sddl(user, true),
            protected_sddl(user, false),
            process_sddl(user),
        ] {
            let sd = Sd::parse(&sddl).unwrap();
            // Written back the same way twice: what `protect` compares.
            let back = sd.dacl_sddl().unwrap();
            assert_eq!(Sd::parse(&back).unwrap().dacl_sddl().unwrap(), back);
        }
    }

    #[test]
    fn capability_sids_are_kept_per_root() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("caps.json");
        let a = PathBuf::from("C:\\a");
        let b = PathBuf::from("C:\\b");
        let first = capability_sids(&[a.clone(), b.clone()], &state).unwrap();
        let again = capability_sids(&[PathBuf::from("c:\\A")], &state).unwrap();
        assert_eq!(first[0].string().unwrap(), again[0].string().unwrap());
        assert_ne!(first[0].string().unwrap(), first[1].string().unwrap());
    }
}
