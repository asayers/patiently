use anyhow::{bail, ensure, Context};
use bpaf::Bpaf;
use rustix::fs::inotify;
use rustix::io::Errno;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Seek, Write};
use std::mem::MaybeUninit;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use tracing::*;

#[derive(Bpaf)]
#[bpaf(options)]
struct Opts {
    #[bpaf(short, long, fallback(1))]
    jobs: usize,
    #[bpaf(positional("COMMAND"))]
    cmd: Option<String>,
    #[bpaf(positional)]
    args: Vec<String>,
}

fn main() -> anyhow::Result<ExitCode> {
    let opts = opts().run();

    let level_filter = match std::env::var("RUST_LOG") {
        Ok(s) => s.parse()?,
        Err(_) => tracing_subscriber::filter::LevelFilter::WARN,
    };
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(level_filter)
        .without_time()
        .init();

    let qdir = QueueDir::open()?;

    match opts.cmd {
        None => {
            status(&qdir)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(cmd) => State::new(qdir)?.run(cmd, opts.args, opts.jobs),
    }
}

fn status(qdir: &QueueDir) -> anyhow::Result<()> {
    use std::fmt::Write;
    let mut tp = liveterm::TermPrinter::new(std::io::stdout());
    loop {
        let jobs = qdir.list()?;
        let mut totals = BTreeMap::<String, usize>::default();
        for &id in &jobs {
            match qdir.get_status(id) {
                Ok(status) => *totals.entry(status).or_default() += 1,
                Err(e) => warn!("{}: {e}", id.0),
            }
        }

        tp.clear()?;
        tp.buf.clear();
        for (status, count) in &totals {
            writeln!(tp.buf, "{:>10}: {count}", status.to_string())?;
        }
        tp.print()?;

        if totals.values().sum::<usize>() == 0 {
            break;
        }

        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    Ok(())
}

struct State {
    qdir: QueueDir,
    our_id: JobId,
    file: File,
    ahead_of_us: Vec<JobId>,
}

impl State {
    fn new(qdir: QueueDir) -> anyhow::Result<State> {
        let mut ahead_of_us;
        let mut our_id;
        let file = loop {
            ahead_of_us = qdir.list().context("List existing jobs")?;
            // Our ID is going to be one more than the latest existing job
            our_id = JobId(ahead_of_us.iter().map(|x| x.0).max().map_or(0, |x| x + 1));
            // Try to create a job file
            match qdir.try_create(our_id) {
                Ok(file) => break file, // We claimed this name
                Err(_) => continue,     // Someone else got there first.  Retry
            }
        };
        let _g = info_span!("", job_id = our_id.0).entered();
        info!("Claimed an ID");
        let mut state = State {
            qdir,
            ahead_of_us,
            our_id,
            file,
        };
        state.set_status("waiting")?;
        Ok(state)
    }
}

impl Drop for State {
    fn drop(&mut self) {
        let _ = self.qdir.remove(self.our_id);
    }
}

impl State {
    fn run(mut self, cmd: String, args: Vec<String>, jobs: usize) -> anyhow::Result<ExitCode> {
        let _g = info_span!("", job_id = self.our_id.0).entered();
        self.wait(jobs).context("While waiting for other jobs")?;
        self.set_status("running")?;
        let status = Command::new(cmd).args(args).status()?;
        let code = match status.code() {
            Some(x) => x as u8,
            None => 1, // stopped by a signal
        };
        Ok(ExitCode::from(code))
    }

    fn set_status(&mut self, status: &str) -> std::io::Result<()> {
        self.file.seek(std::io::SeekFrom::Start(0))?;
        self.file.write_all(status.as_bytes())?;
        Ok(())
    }

    fn wait(&mut self, max_jobs: usize) -> anyhow::Result<()> {
        if self.ahead_of_us.len() < max_jobs {
            // Just a fast-path to avoid setting up inotify and then immediately
            // dropping it
            return Ok(());
        }

        let ino_fd = inotify::init(inotify::CreateFlags::empty())?;
        let our_wd = match inotify::add_watch(
            &ino_fd,
            &self.qdir.file_for(self.our_id),
            inotify::WatchFlags::DELETE_SELF | inotify::WatchFlags::MOVE_SELF,
        ) {
            Ok(wd) => wd,
            Err(_) => bail!("Our own file was removed; cancelling"),
        };

        let mut ino_buf = [const { MaybeUninit::uninit() }; 1024];
        let mut evs = inotify::Reader::new(&ino_fd, &mut ino_buf);
        let mut watches = std::collections::HashSet::new();

        loop {
            info!(
                "Waiting for {} jobs to finish",
                self.ahead_of_us.len() + watches.len()
            );
            while watches.len() < max_jobs {
                match self.ahead_of_us.pop() {
                    Some(job) => {
                        match inotify::add_watch(
                            &ino_fd,
                            &self.qdir.file_for(job),
                            inotify::WatchFlags::DELETE_SELF,
                        ) {
                            Ok(wd) => {
                                watches.insert(wd);
                            }
                            Err(Errno::NOENT) => info!("{} is already gone", job.0),
                            Err(e) => warn!("Couldn't install a watch on {}: {e}", job.0),
                        }
                    }
                    None => return Ok(()),
                }
            }
            info!("Waiting for events on {} file(s)", watches.len());
            let ev = evs.next().context("Getting events")?;
            info!("Got event: {ev:?}");
            ensure!(ev
                .events()
                .intersects(inotify::ReadFlags::DELETE_SELF | inotify::ReadFlags::IGNORED));
            if ev.wd() == our_wd {
                bail!("Our own file was removed; cancelling");
            }
            watches.remove(&ev.wd());
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct JobId(usize);

pub struct QueueDir(PathBuf);

impl QueueDir {
    fn open() -> std::io::Result<QueueDir> {
        let qdir = std::env::var("QUEUE_DIR").map_or(PathBuf::from(".patiently"), PathBuf::from);
        std::fs::create_dir_all(&qdir)?;
        Ok(QueueDir(qdir))
    }

    fn list(&self) -> std::io::Result<Vec<JobId>> {
        let mut jobs = vec![];
        for dent in std::fs::read_dir(&self.0)? {
            let dent = dent?;
            let name = dent.file_name().to_string_lossy().into_owned();
            if !dent.file_type().is_ok_and(|ft| ft.is_file()) {
                warn!(
                    %name, rsn = "not a file",
                    "Ignoring unexpected entry in queue dir",
                );
                continue;
            }
            let Some(id) = dent
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<usize>().ok())
            else {
                warn!(
                    %name, rsn = "non-numeric filename",
                    "Ignoring unexpected entry in queue dir",
                );
                continue;
            };
            jobs.push(JobId(id));
        }
        jobs.sort_unstable();
        Ok(jobs)
    }

    fn file_for(&self, id: JobId) -> PathBuf {
        self.0.join(id.0.to_string())
    }

    fn try_create(&self, id: JobId) -> std::io::Result<File> {
        let path = self.0.join(id.0.to_string());
        let file = File::options().create_new(true).write(true).open(path)?;
        Ok(file)
    }

    fn remove(&self, id: JobId) -> std::io::Result<()> {
        std::fs::remove_file(self.0.join(id.0.to_string()))
    }

    fn get_status(&self, id: JobId) -> std::io::Result<String> {
        std::fs::read_to_string(self.0.join(id.0.to_string()))
    }
}
