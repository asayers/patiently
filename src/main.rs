use anyhow::{bail, Context};
use bpaf::Bpaf;
use enum_map::{enum_map, Enum};
use inotify::{EventMask, Inotify, WatchMask};
use std::fmt;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::str::FromStr;
use tracing::*;

#[derive(Bpaf)]
#[bpaf(options)]
struct Opts {
    #[bpaf(short, long, fallback(1))]
    jobs: usize,
    #[bpaf(positional("COMMAND"))]
    cmd: Option<String>,
}

fn main() {
    if let Err(e) = main_2(opts().run()) {
        let es = e.chain().map(|x| x.to_string()).collect::<Vec<_>>();
        error!("{}", es.join(": "));
        process::exit(1);
    }
}

fn main_2(opts: Opts) -> anyhow::Result<()> {
    let level_filter = match std::env::var("RUST_LOG") {
        Ok(s) => s.parse()?,
        Err(_) => tracing_subscriber::filter::LevelFilter::WARN,
    };
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(level_filter)
        .without_time()
        .init();

    let qdir = std::env::var("QUEUE_DIR").map_or(PathBuf::from(".patiently"), PathBuf::from);
    std::fs::create_dir_all(&qdir)?;

    match opts.cmd {
        None => status(&qdir)?,
        Some(cmd) => {
            let mut state = State::new(qdir)?;
            let res = info_span!("", id = state.id)
                .in_scope(|| run_job(&mut state, cmd, opts.jobs))
                .context(state.id);
            if let Err(e) = res {
                // Make an attempt to mark the job as crashed, ignoring new errors
                let _ = state.change_status(Status::Crashed);
                return Err(e);
            }
        }
    }
    Ok(())
}

fn status(qdir: &Path) -> anyhow::Result<()> {
    use std::fmt::Write;
    let mut tp = liveterm::TermPrinter::new(std::io::stdout());
    loop {
        let jobs = list_jobs(&qdir)?;
        let mut totals = enum_map! { _ => 0 };
        let mut n_unfinished = 0;
        for (_, status) in &jobs {
            // println!("{id}: {status}");
            totals[*status] += 1;
            if !status.is_finished() {
                n_unfinished += 1;
            }
        }

        tp.clear()?;
        tp.buf.clear();
        for (status, count) in totals {
            writeln!(tp.buf, "{:>10}: {count}", status.to_string())?;
        }
        tp.print()?;

        if n_unfinished == 0 {
            break;
        }

        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    Ok(())
}

fn run_job(state: &mut State, cmd: String, jobs: usize) -> anyhow::Result<()> {
    state
        .wait_for_precursors(jobs)
        .context("While waiting for precursors")?;

    state.change_status(Status::Running)?;
    let exit_code = Command::new("bash").arg("-c").arg(cmd).status()?;

    let final_status = if exit_code.success() {
        Status::Finished
    } else {
        Status::Failed
    };
    state.change_status(final_status)?;
    Ok(())
}

struct State {
    qdir: PathBuf,
    id: usize,
    status: Status,
    // Reflects the status at the time new() was called.  May be stale.
    precursors: Vec<(usize, Status)>,
}

impl State {
    fn new(qdir: PathBuf) -> anyhow::Result<State> {
        let (id, precursors) = loop {
            let (id, precursors) = get_precusors(&qdir).context("Get precursors")?;
            // Try to create the queue file
            let path = qdir.join(format!("patiently.{id}.{}", Status::Waiting));
            let res = File::options().create_new(true).append(true).open(path);
            if res.is_ok() {
                // We claimed this name
                break (id, precursors);
            }
            // Someone else got there first.  Retry
        };
        // TODO: Check precursor flocks, set to "cancelled" if missing
        Ok(State {
            qdir,
            id,
            precursors,
            status: Status::Waiting,
        })
    }

    fn change_status(&mut self, to: Status) -> anyhow::Result<()> {
        let from = self.status;
        std::fs::rename(
            self.qdir.join(format!("patiently.{}.{from}", self.id)),
            self.qdir.join(format!("patiently.{}.{to}", self.id)),
        )
        .context("Changing status")?;
        info!(%from, %to, "Changing status");
        self.status = to;
        Ok(())
    }

    fn qfile(&self) -> PathBuf {
        self.qdir
            .join(format!("patiently.{}.{}", self.id, self.status))
    }

    fn wait_for_precursors(&mut self, max_jobs: usize) -> anyhow::Result<()> {
        info!("Waiting for {} jobs to finish", self.precursors.len());
        let mut inotify = loop {
            match Inotify::init() {
                Ok(x) => break x,
                Err(_) => std::thread::sleep(std::time::Duration::from_secs(1)),
            }
        };

        let mut inotify_buf = vec![0; 1024];
        let our_wd =
            match inotify.add_watch(&self.qfile(), WatchMask::DELETE_SELF | WatchMask::MOVE_SELF) {
                Ok(x) => x,
                Err(_) => {
                    // Our output file has already been deleted
                    warn!("Output file removed, exiting");
                    process::exit(0);
                }
            };
        let mut watches = std::collections::HashMap::new();

        loop {
            while watches.len() < max_jobs {
                match self.precursors.pop() {
                    Some((x, _)) => {
                        match inotify.add_watch(
                            &self.qdir.join(format!("patiently.{x}.{}", Status::Waiting)),
                            WatchMask::DELETE_SELF | WatchMask::MOVE_SELF,
                        ) {
                            Ok(wd) => {
                                watches.insert(wd, x);
                            }
                            Err(_) => {
                                if let Ok(wd) = inotify.add_watch(
                                    &self.qdir.join(format!("patiently.{x}.{}", Status::Running)),
                                    WatchMask::DELETE_SELF | WatchMask::MOVE_SELF,
                                ) {
                                    watches.insert(wd, x);
                                } else {
                                    // I guess it finished already?
                                }
                            }
                        }
                    }
                    None => return Ok(()),
                }
            }
            for ev in inotify
                .read_events_blocking(&mut inotify_buf)
                .context("Getting events")?
            {
                if ev.wd == our_wd {
                    warn!("Output file removed, exiting");
                    process::exit(0);
                }
                match ev.mask {
                    EventMask::IGNORED | EventMask::DELETE_SELF => {
                        watches.remove(&ev.wd);
                        continue;
                    }
                    EventMask::MOVE_SELF => {
                        let x = match watches.get(&ev.wd) {
                            Some(x) => x,
                            None => bail!("{:?}: Couldn't find watch", ev),
                        };
                        let exists =
                            |status| self.qdir.join(format!("patiently.{x}.{status}")).exists();
                        if exists(Status::Running) {
                            // The file just switched to "running" status.
                            // Keep watching it.
                        } else if exists(Status::Finished)
                            || exists(Status::Failed)
                            || exists(Status::Crashed)
                        {
                            // The file just switched to "finished"/"failed"
                            // status.  Remove the watch.
                            inotify.rm_watch(ev.wd).context("Removing watch")?;
                        } else if exists(Status::Waiting) {
                            bail!("File moved, but status is still waiting?");
                        } else {
                            // Someone has renamed the file to something we
                            // don't recognise.
                            inotify.rm_watch(ev.wd).context("Removing watch")?;
                        }
                    }
                    mask => bail!("Unexpected event mask {:?}", mask),
                }
            }
        }
    }
}

fn get_precusors(qdir: &Path) -> anyhow::Result<(usize, Vec<(usize, Status)>)> {
    let mut precursors = list_jobs(qdir)?;
    // Increment the ID regardless of the status
    let next_id = precursors.iter().map(|x| x.0).max().map_or(0, |x| x + 1);
    // Don't wait for completed jobs
    precursors.retain(|(_, status)| !status.is_finished());
    Ok((next_id, precursors))
}

fn list_jobs(qdir: &Path) -> anyhow::Result<Vec<(usize, Status)>> {
    let mut jobs = std::fs::read_dir(qdir)?
        .filter_map(|x| {
            let x = x.ok()?;
            match x.file_type() {
                Ok(ft) if ft.is_file() => (),
                _ => return None,
            }
            let name = x.file_name();
            let mut tokens = name.to_str()?.split('.');
            if tokens.next()? != "patiently" {
                return None;
            }
            let id: usize = tokens.next()?.parse().ok()?;
            let status: Status = tokens.next()?.parse().ok()?;
            Some((id, status))
        })
        .collect::<Vec<_>>();
    jobs.sort_unstable_by_key(|x| x.0);
    Ok(jobs)
}

#[derive(Copy, Clone, Enum)]
enum Status {
    Waiting,
    Running,
    Finished,
    Failed,
    Crashed,
}
impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Status::Waiting => f.write_str("waiting"),
            Status::Running => f.write_str("running"),
            Status::Finished => f.write_str("finished"),
            Status::Failed => f.write_str("failed"),
            Status::Crashed => f.write_str("crashed"),
        }
    }
}
impl FromStr for Status {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Status> {
        match s {
            "waiting" => Ok(Status::Waiting),
            "running" => Ok(Status::Running),
            "finished" => Ok(Status::Finished),
            "failed" => Ok(Status::Failed),
            "crashed" => Ok(Status::Crashed),
            _ => bail!("{s}: Unrecognised status"),
        }
    }
}
impl Status {
    fn is_finished(self) -> bool {
        match self {
            Status::Waiting | Status::Running => false,
            Status::Finished | Status::Failed | Status::Crashed => true,
        }
    }
}
