//! In-process concurrency boundary. Operation reservations may span external I/O;
//! the data lock must only cover local reads or commits, never subprocess/network waits.
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::sync::{Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

static DATA: RwLock<()> = RwLock::new(());
static OPERATION: Mutex<Option<String>> = Mutex::new(None);
static INITIALIZATION: Mutex<Option<Result<(), String>>> = Mutex::new(None);

pub fn read() -> Result<RwLockReadGuard<'static, ()>> {
    match DATA.try_read() {
        Ok(guard) => Ok(guard),
        Err(std::sync::TryLockError::WouldBlock) => {
            Err(anyhow!("STATE_COMMITTING: 本地数据正在提交，请稍后重试"))
        }
        Err(std::sync::TryLockError::Poisoned(_)) => {
            Err(anyhow!("STATE_UNAVAILABLE: 本地数据状态锁异常"))
        }
    }
}

pub fn write() -> Result<RwLockWriteGuard<'static, ()>> {
    DATA.write().map_err(|_| anyhow!("本地数据状态锁异常"))
}

pub struct Operation;
impl Operation {
    pub fn begin(name: &str) -> Result<Self> {
        let mut operation = OPERATION
            .lock()
            .map_err(|_| anyhow!("管理操作状态锁异常"))?;
        if let Some(current) = operation.as_ref() {
            anyhow::bail!("OPERATION_IN_PROGRESS: {current} 正在进行，请完成后重试");
        }
        *operation = Some(name.to_string());
        Ok(Self)
    }
}
impl Drop for Operation {
    fn drop(&mut self) {
        if let Ok(mut operation) = OPERATION.lock() {
            *operation = None;
        }
    }
}

pub fn operation_state() -> Value {
    match OPERATION.lock() {
        Ok(operation) => json!({ "running": operation.is_some(), "name": *operation }),
        Err(_) => json!({ "error": "管理操作状态锁异常" }),
    }
}

pub fn initialization_state() -> Value {
    match INITIALIZATION.lock() {
        Ok(result) => match result.as_ref() {
            None => json!({ "status": "pending" }),
            Some(Ok(())) => json!({ "status": "ready" }),
            Some(Err(error)) => json!({ "status": "failed", "error": error }),
        },
        Err(_) => json!({ "status": "failed", "error": "本地数据初始化状态锁异常" }),
    }
}

pub fn local_data_ready() -> Result<bool> {
    Ok(initialization_state()["status"] == "ready"
        && crate::credential::local_data_ready()?
        && crate::codex_workspace::local_data_ready()?)
}

pub fn initialize_local_data() -> Result<()> {
    let _write = write()?;
    let result = (|| -> Result<()> {
        crate::credential::initialize_local_data()?;
        let profile = crate::credential::default_auth_profile_id()?;
        crate::codex_workspace::initialize_local_data(profile.as_deref())
    })();
    *INITIALIZATION
        .lock()
        .map_err(|_| anyhow!("本地数据初始化状态锁异常"))? = Some(
        result
            .as_ref()
            .map(|_| ())
            .map_err(|error| format!("{error:#}")),
    );
    result
}
