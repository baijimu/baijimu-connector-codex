use std::ffi::OsStr;
use std::process::Command;

pub fn isolate_from_connector_environment(command: &mut Command) {
    for (name, _) in std::env::vars_os() {
        if is_connector_private_environment(&name) {
            command.env_remove(name);
        }
    }
}

fn is_connector_private_environment(name: &OsStr) -> bool {
    let name = name.to_string_lossy().to_ascii_uppercase();
    name == "CODEX_HOME" || name.starts_with("BAIJIMU_") || name.starts_with("CODEX_CONNECTOR_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_connector_private_environment_without_hiding_normal_user_environment() {
        assert!(is_connector_private_environment(OsStr::new("CODEX_HOME")));
        assert!(is_connector_private_environment(OsStr::new(
            "BAIJIMU_LOCAL_APP_EVENT_TOKEN_FILE"
        )));
        assert!(is_connector_private_environment(OsStr::new(
            "CODEX_CONNECTOR_PORT"
        )));
        assert!(!is_connector_private_environment(OsStr::new("PATH")));
        assert!(!is_connector_private_environment(OsStr::new("HTTPS_PROXY")));
        assert!(!is_connector_private_environment(OsStr::new(
            "OPENAI_API_KEY"
        )));
    }
}

/// Bounded external I/O. Redirect to private files rather than pipes: a descendant
/// inheriting stdout must not keep an output reader alive after its parent exits.
pub fn output(
    command: &mut Command,
    timeout: std::time::Duration,
) -> anyhow::Result<std::process::Output> {
    use anyhow::Context;
    use std::io::{Read, Seek, SeekFrom};
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let deadline = Instant::now() + timeout;
    let capture = Capture::new()?;
    let mut stdout = capture.file("stdout")?;
    let mut stderr = capture.file("stderr")?;
    command
        .stdin(Stdio::null())
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?);
    tree::configure(command);
    let mut child = command.spawn().context("启动受控子进程失败")?;
    let tree = match tree::Tree::attach(&child) {
        Ok(tree) => tree,
        Err(error) => {
            let _ = child.kill();
            reap(&mut child)?;
            return Err(error);
        }
    };
    const MAX_OUTPUT_BYTES: u64 = 16 * 1024 * 1024;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if stdout.metadata()?.len() > MAX_OUTPUT_BYTES
            || stderr.metadata()?.len() > MAX_OUTPUT_BYTES
        {
            tree.terminate()?;
            reap(&mut child)?;
            anyhow::bail!("子进程输出超过 16 MiB 限制，已终止本次操作的进程树");
        }
        if Instant::now() >= deadline {
            tree.terminate()?;
            reap(&mut child)?;
            anyhow::bail!(
                "子进程超过 {} 秒执行期限，已终止本次操作的进程树",
                timeout.as_secs_f64()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // Reap orphaned descendants before reading/removing the capture files.
    tree.terminate()?;
    let read = |file: &mut std::fs::File| -> anyhow::Result<Vec<u8>> {
        file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        file.take(MAX_OUTPUT_BYTES + 1).read_to_end(&mut bytes)?;
        anyhow::ensure!(
            bytes.len() as u64 <= MAX_OUTPUT_BYTES,
            "子进程输出超过 16 MiB 限制"
        );
        Ok(bytes)
    };
    let output = std::process::Output {
        status,
        stdout: read(&mut stdout)?,
        stderr: read(&mut stderr)?,
    };
    drop(stdout);
    drop(stderr);
    Ok(output)
}

fn reap(child: &mut std::process::Child) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while child.try_wait()?.is_none() {
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "终止子进程后未能在 2 秒内回收"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    Ok(())
}

struct Capture(std::path::PathBuf);
impl Capture {
    fn new() -> anyhow::Result<Self> {
        use rand::RngCore;
        let mut random = [0_u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut random);
        let directory = std::env::temp_dir().join(format!(
            "codex-command-{}-{:x}",
            std::process::id(),
            u128::from_le_bytes(random)
        ));
        #[allow(unused_mut)]
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&directory)?;
        Ok(Self(directory))
    }
    fn file(&self, name: &str) -> anyhow::Result<std::fs::File> {
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        Ok(options.open(self.0.join(name))?)
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(unix)]
mod tree {
    use super::*;
    use std::os::unix::process::CommandExt;
    pub fn configure(command: &mut Command) {
        command.process_group(0);
    }
    pub struct Tree(u32);
    impl Tree {
        pub fn attach(child: &std::process::Child) -> anyhow::Result<Self> {
            Ok(Self(child.id()))
        }
        pub fn terminate(&self) -> anyhow::Result<()> {
            let result = unsafe { libc::kill(-(self.0 as i32), libc::SIGKILL) };
            if result != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error.into());
                }
            }
            Ok(())
        }
    }
    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = self.terminate();
        }
    }
}

#[cfg(windows)]
mod tree {
    use super::*;
    use std::os::windows::{io::AsRawHandle, process::CommandExt};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD,
                THREADENTRY32,
            },
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
                SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            },
            Threading::{
                OpenThread, ResumeThread, CREATE_NO_WINDOW, CREATE_SUSPENDED, THREAD_SUSPEND_RESUME,
            },
        },
    };
    pub fn configure(command: &mut Command) {
        command.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
    }
    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    pub struct Tree(Handle);
    impl Tree {
        pub fn attach(child: &std::process::Child) -> anyhow::Result<Self> {
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    return Err(std::io::Error::last_os_error().into());
                }
                let tree = Self(Handle(job));
                let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &limits as *const _ as _,
                    std::mem::size_of_val(&limits) as u32,
                ) == 0
                    || AssignProcessToJobObject(job, child.as_raw_handle() as HANDLE) == 0
                {
                    return Err(std::io::Error::last_os_error().into());
                }
                // The child is suspended until assignment, so it cannot spawn an
                // untracked descendant in the spawn/assign interval.
                let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
                if snapshot == INVALID_HANDLE_VALUE {
                    return Err(std::io::Error::last_os_error().into());
                }
                let snapshot = Handle(snapshot);
                let mut entry: THREADENTRY32 = std::mem::zeroed();
                entry.dwSize = std::mem::size_of_val(&entry) as u32;
                let mut found = Thread32First(snapshot.0, &mut entry);
                while found != 0 {
                    if entry.th32OwnerProcessID == child.id() {
                        let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                        if thread.is_null() {
                            return Err(std::io::Error::last_os_error().into());
                        }
                        let thread = Handle(thread);
                        if ResumeThread(thread.0) == u32::MAX {
                            return Err(std::io::Error::last_os_error().into());
                        }
                        return Ok(tree);
                    }
                    found = Thread32Next(snapshot.0, &mut entry);
                }
                anyhow::bail!("找不到受控子进程的主线程");
            }
        }
        pub fn terminate(&self) -> anyhow::Result<()> {
            if unsafe { TerminateJobObject(self.0 .0, 1) } == 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn child() {
        let Ok(mode) = std::env::var("CODEX_PROCESS_TEST_MODE") else {
            return;
        };
        if mode == "descendant" {
            std::thread::sleep(Duration::from_millis(1400));
            std::fs::write(
                std::env::var_os("CODEX_PROCESS_TEST_MARKER").unwrap(),
                b"escaped",
            )
            .unwrap();
        } else if mode == "hang" || mode == "orphan" {
            let mut command = helper("descendant");
            // Deliberate orphan fixture: the runner must clean this descendant.
            #[allow(clippy::zombie_processes)]
            let _child = command.spawn().unwrap();
            println!("descendant started");
            if mode == "hang" {
                std::thread::sleep(Duration::from_secs(30));
            }
        } else if mode == "success" {
            println!("bounded output");
            eprintln!("bounded stderr");
        }
    }

    fn helper(mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "child_process::deadline_tests::child",
                "--nocapture",
            ])
            .env("CODEX_PROCESS_TEST_MODE", mode);
        command
    }

    #[test]
    fn captures_output_and_cleans_descendants_on_success_and_timeout() {
        let output = output(&mut helper("success"), Duration::from_secs(5)).unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("bounded output"));
        assert!(String::from_utf8_lossy(&output.stderr).contains("bounded stderr"));
        for mode in ["hang", "orphan"] {
            let scratch = Capture::new().unwrap();
            let marker = scratch.0.join("marker");
            let mut command = helper(mode);
            command.env("CODEX_PROCESS_TEST_MARKER", &marker);
            let started = Instant::now();
            let result = super::output(&mut command, Duration::from_millis(500));
            if mode == "hang" {
                assert!(result.unwrap_err().to_string().contains("执行期限"));
            } else {
                assert!(result.unwrap().status.success());
            }
            assert!(started.elapsed() < Duration::from_secs(3));
            std::thread::sleep(Duration::from_millis(1600));
            assert!(!marker.exists(), "descendant survived its owned operation");
        }
    }
}
