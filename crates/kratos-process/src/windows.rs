//! Job objects that contain this process for its lifetime and each child's process tree.
//!
//! Each unsafe block states the invariant that
//! makes its call sound; none of them reads or writes memory that Rust does not own for the whole
//! call.
#![allow(unsafe_code, reason = "the job object API has no safe wrapper in std")]

use std::io;
use std::mem::size_of;
use std::process::ExitStatus;
use std::sync::Mutex;

use tokio::process::{Child, Command};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, GetCurrentProcess, OpenThread, ResumeThread,
    THREAD_SUSPEND_RESUME,
};

/// The exit code every process in the tree gets when the owner ends it.
const ENDED_BY_OWNER: u32 = 1;

static CONTAINED: Mutex<bool> = Mutex::new(false);

/// Keeps future children in a kill-on-close job owned until this process exits.
///
/// Call before starting children. An incompatible enclosing job makes this fail.
pub fn contain_current_process() -> io::Result<()> {
    let mut contained = CONTAINED
        .lock()
        .map_err(|_| io::Error::other("the process containment lock is poisoned"))?;
    if *contained {
        return Ok(());
    }
    let job = new_job()?;
    // SAFETY: the job is open and GetCurrentProcess returns this process's valid pseudo-handle.
    if unsafe { AssignProcessToJobObject(job.0, GetCurrentProcess()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // Closing this job would kill us too. Windows closes the non-inherited handle at process exit.
    std::mem::forget(job);
    *contained = true;
    Ok(())
}

#[derive(Debug)]
pub struct Tree {
    job: OwnedHandle,
}

impl Tree {
    /// Starts the child suspended, puts it in a new kill-on-close job, then lets it run. A failure
    /// at any step ends the child before returning, so no process runs outside a job.
    pub fn spawn(command: &mut Command) -> io::Result<(Tree, Child)> {
        if !*CONTAINED
            .lock()
            .map_err(|_| io::Error::other("the process containment lock is poisoned"))?
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "call contain_current_process before spawning an owned child",
            ));
        }
        let job = new_job()?;
        command.creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW);
        let mut child = command.spawn()?;
        if let Err(error) = join_and_resume(&job, &child) {
            let _ = child.start_kill();
            return Err(error);
        }
        Ok((Tree { job }, child))
    }

    pub fn end(&mut self) -> io::Result<()> {
        // SAFETY: `self.job` is a job handle this value owns and keeps open.
        if unsafe { TerminateJobObject(self.job.0, ENDED_BY_OWNER) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub async fn wait(&mut self, child: &mut Child) -> io::Result<ExitStatus> {
        child.wait().await
    }
}

fn new_job() -> io::Result<OwnedHandle> {
    // SAFETY: null attributes and a null name ask for an unnamed job with default security.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(io::Error::last_os_error());
    }
    let job = OwnedHandle(job);
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    // Breaking away needs JOB_OBJECT_LIMIT_BREAKAWAY_OK, which is left unset.
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: `limits` lives until the call returns, and the length is its exact size.
    let set = unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if set == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(job)
}

fn join_and_resume(job: &OwnedHandle, child: &Child) -> io::Result<()> {
    let process = child
        .raw_handle()
        .ok_or_else(|| io::Error::other("the child exited before it joined its job"))?;
    // SAFETY: both handles are open: the job is owned by the caller, and tokio keeps the process
    // handle open while it tracks the child.
    if unsafe { AssignProcessToJobObject(job.0, process.cast()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let id = child
        .id()
        .ok_or_else(|| io::Error::other("the child exited before it started"))?;
    resume_threads(id)
}

/// A process created suspended has one thread, its main thread, and std does not hand out its
/// handle. A snapshot of the system's threads finds it by its owner's process id.
fn resume_threads(process_id: u32) -> io::Result<()> {
    // SAFETY: a thread snapshot takes no pointers; 0 means "every process".
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let snapshot = OwnedHandle(snapshot);
    let mut entry = THREADENTRY32 {
        dwSize: size_of::<THREADENTRY32>() as u32,
        cntUsage: 0,
        th32ThreadID: 0,
        th32OwnerProcessID: 0,
        tpBasePri: 0,
        tpDeltaPri: 0,
        dwFlags: 0,
    };
    let mut resumed = 0;
    // SAFETY: `entry` is a THREADENTRY32 whose dwSize is set, as the call requires, and it outlives
    // the call.
    let mut found = unsafe { Thread32First(snapshot.0, &raw mut entry) } != 0;
    while found {
        if entry.th32OwnerProcessID == process_id {
            resume_thread(entry.th32ThreadID)?;
            resumed += 1;
        }
        // SAFETY: as for Thread32First.
        found = unsafe { Thread32Next(snapshot.0, &raw mut entry) } != 0;
    }
    if resumed == 0 {
        return Err(io::Error::other(format!(
            "found no thread of process {process_id} to resume"
        )));
    }
    Ok(())
}

fn resume_thread(thread_id: u32) -> io::Result<()> {
    // SAFETY: OpenThread takes no pointers; a null result is checked before use.
    let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, thread_id) };
    if thread.is_null() {
        return Err(io::Error::last_os_error());
    }
    let thread = OwnedHandle(thread);
    // SAFETY: `thread` is open with THREAD_SUSPEND_RESUME access.
    if unsafe { ResumeThread(thread.0) } == u32::MAX {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[derive(Debug)]
struct OwnedHandle(HANDLE);

// SAFETY: a kernel handle is a process-wide index, valid from any thread. Nothing here hands out
// the raw value, and the handle is closed exactly once, by Drop.
unsafe impl Send for OwnedHandle {}
// SAFETY: as for Send; the calls made through a shared reference are thread-safe kernel calls.
unsafe impl Sync for OwnedHandle {}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle is open and owned by this value, so closing it once here is the
        // only close. For a job, the close ends every process still in it.
        unsafe { CloseHandle(self.0) };
    }
}
