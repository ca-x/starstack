//! Run native index inference in the same executable with a job-specific CPU budget.
use crate::{
    index,
    store::{self, Db},
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    io::{self, BufRead, BufReader, Read, Write},
    path::Path,
    process::{Command, Stdio},
    sync::Mutex,
};

const PROFILE: &str = "index_cpu_profile";
static NATIVE_BUILD: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Profile {
    Quiet,
    #[default]
    Balanced,
    Performance,
}

impl Profile {
    fn threads(self, available: usize) -> usize {
        let capacity = available.clamp(1, 16);
        match self {
            Self::Quiet => 1,
            Self::Balanced => capacity.div_ceil(2),
            Self::Performance => capacity,
        }
    }

    pub(crate) fn load(db: &mut Db) -> Result<Self> {
        Ok(match store::meta(db, PROFILE)?.as_deref() {
            Some("quiet") => Self::Quiet,
            Some("performance") => Self::Performance,
            _ => Self::Balanced,
        })
    }

    pub(crate) fn capacity() -> usize {
        std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
    }

    pub(crate) fn options() -> serde_json::Value {
        let capacity = Self::capacity();
        serde_json::json!({
            "quiet": Self::Quiet.threads(capacity),
            "balanced": Self::Balanced.threads(capacity),
            "performance": Self::Performance.threads(capacity),
        })
    }

    pub(crate) fn save(self, db: &mut Db) -> Result<()> {
        db.immediate_transaction(|db| {
            ensure!(index::backend(db)? == "builtin", "只有内置模型支持索引挡位");
            store::set_meta(
                db,
                PROFILE,
                match self {
                    Self::Quiet => "quiet",
                    Self::Balanced => "balanced",
                    Self::Performance => "performance",
                },
            )
        })
    }
}

#[derive(Serialize, Deserialize)]
enum Event {
    Phase(String),
    Progress(index::Progress),
    Finished(String),
    Failed { message: String, conflict: bool },
}

#[derive(Debug)]
pub(crate) struct Interrupted;

impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("内置索引工作进程中断，已保存断点")
    }
}

impl std::error::Error for Interrupted {}

pub(crate) fn build(
    db: &mut Db,
    rebuild: bool,
    phase: &dyn Fn(&str),
    observe: &dyn Fn(index::Progress),
) -> Result<String> {
    let rebuild = index::prepare_build(db, rebuild)?;
    if index::backend(db)? != "builtin" {
        return index::build_observed(db, rebuild, phase, observe);
    }
    phase("等待内置索引工作进程");
    let _lease = NATIVE_BUILD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let threads = Profile::load(db)?.threads(Profile::capacity()).to_string();
    let mut worker = Command::new(std::env::current_exe().context("找不到当前 StarStack 程序")?)
        .arg("index-worker")
        .arg("--data-dir")
        .arg(store::data_dir(db)?)
        .arg("--owner")
        .arg(store::owner(db)?)
        .env("CANDLE_NUM_THREADS", &threads)
        .env("RAYON_NUM_THREADS", &threads)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("无法启动内置索引工作进程")?;
    let _lifetime = worker.stdin.take().context("索引工作进程没有监督管道")?;
    let stdout = worker.stdout.take().context("索引工作进程没有输出管道")?;
    let read_worker = || -> Result<Option<Event>> {
        for line in BufReader::new(stdout).lines() {
            match serde_json::from_str(&line?)? {
                Event::Phase(text) => phase(&text),
                Event::Progress(progress) => observe(progress),
                event => return Ok(Some(event)),
            }
        }
        Ok(None)
    };
    let result = read_worker();
    if result.is_err() {
        let _ = worker.kill();
    }
    let status = worker.wait().context("无法等待内置索引工作进程")?;
    match result.context("无法读取内置索引进度")? {
        Some(Event::Finished(message)) if status.success() => Ok(message),
        Some(Event::Failed { conflict: true, .. }) => Err(index::BuildConflict.into()),
        Some(Event::Failed { message, .. }) => bail!("{message}"),
        _ => Err(Interrupted.into()),
    }
}

pub(crate) fn run(data_dir: &Path, owner: &str) -> Result<()> {
    // The supervising process holds stdin open. If it exits, stop this worker so a
    // restarted supervisor can resume without competing against a detached process.
    std::thread::spawn(|| {
        let _ = io::stdin().read(&mut [0]);
        std::process::exit(1);
    });
    let mut db = store::connect_as(data_dir, owner)?;
    let emit = |event: Event| {
        let mut out = io::stdout().lock();
        let _ = serde_json::to_writer(&mut out, &event);
        let _ = out.write_all(b"\n");
        let _ = out.flush();
    };
    let result = (|| {
        ensure!(index::backend(&mut db)? == "builtin", index::BuildConflict);
        let rebuild = index::pending_rebuild(&mut db)?;
        index::build_observed(
            &mut db,
            rebuild,
            &|step| emit(Event::Phase(step.into())),
            &|progress| emit(Event::Progress(progress)),
        )
    })();
    match result {
        Ok(message) => emit(Event::Finished(message)),
        Err(error) => emit(Event::Failed {
            conflict: error.is::<index::BuildConflict>(),
            message: format!("{error:#}"),
        }),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_respects_machine_capacity() {
        for (available, expected) in [(1, [1, 1, 1]), (5, [1, 3, 5]), (64, [1, 8, 16])] {
            assert_eq!(
                [Profile::Quiet, Profile::Balanced, Profile::Performance]
                    .map(|profile| profile.threads(available)),
                expected,
            );
        }
    }
}
