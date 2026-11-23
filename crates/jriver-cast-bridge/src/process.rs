//! MC's own process, through the Windows `tasklist` and `taskkill` programs
//! (SPEC.md "JRiver hung"): which MC processes Windows reports "Not
//! responding", and closing those by force. Only the image named by
//! `mcws.exe_path`, and only in this program's own Windows session, is
//! ever touched.

use std::{io, path::PathBuf, process::Stdio, sync::OnceLock, time::Duration};

use log::{debug, info};
use tokio::{process::Command, time::Instant};

/// How long `tasklist` or `taskkill` may take.
const TOOL_TIMEOUT: Duration = Duration::from_secs(10);
/// The "Not responding" query runs every second in the bridge's event loop,
/// so it may take less.
const QUERY_TIMEOUT: Duration = Duration::from_secs(2);
/// How long to wait for killed processes to be gone.
const EXIT_WAIT: Duration = Duration::from_secs(15);
/// `taskkill`'s exit code when the process is already gone.
const TASKKILL_NOT_FOUND: i32 = 128;

/// `%SystemRoot%\System32\<tool>`: never a same-named program elsewhere.
fn system_tool(tool: &str) -> PathBuf {
  let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
  PathBuf::from(root).join("System32").join(tool)
}

struct Output {
  code: Option<i32>,
  stdout: String,
  stderr: String,
}

/// Run a Windows tool without a console window.
async fn run(tool: &str, args: &[&str]) -> io::Result<Output> {
  run_for(tool, args, TOOL_TIMEOUT).await
}

async fn run_for(tool: &str, args: &[&str], limit: Duration) -> io::Result<Output> {
  let mut command = Command::new(system_tool(tool));
  command
    .args(args)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
  #[cfg(windows)]
  {
    /// `CREATE_NO_WINDOW`: the tray program has no console to lend.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
  }
  debug!("running {tool} {}", args.join(" "));
  let output = tokio::time::timeout(limit, command.output())
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, format!("{tool} took too long")))??;
  Ok(Output {
    code: output.status.code(),
    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
    stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
  })
}

fn failed(tool: &str, output: &Output) -> io::Error {
  io::Error::other(format!(
    "{tool} failed (exit code {:?}): {}",
    output.code, output.stderr
  ))
}

/// The rows of `tasklist /fo csv /nh` output as fields. Lines that aren't
/// CSV rows (the localized "no tasks" message) are skipped.
fn rows(output: &str) -> Vec<Vec<String>> {
  output
    .lines()
    .map(str::trim)
    .filter(|line| line.starts_with('"') && line.ends_with('"'))
    .map(|line| {
      line[1..line.len() - 1]
        .split("\",\"")
        .map(str::to_owned)
        .collect()
    })
    .collect()
}

/// PIDs (second field) of the rows.
fn parse_pids(output: &str) -> Vec<u32> {
  rows(output)
    .iter()
    .filter_map(|fields| fields.get(1)?.parse().ok())
    .collect()
}

/// `tasklist` with the given filters, as PIDs.
async fn tasklist(filters: &[String]) -> io::Result<Vec<u32>> {
  tasklist_for(filters, TOOL_TIMEOUT).await
}

async fn tasklist_for(filters: &[String], limit: Duration) -> io::Result<Vec<u32>> {
  let mut args = Vec::new();
  for filter in filters {
    args.extend(["/fi", filter.as_str()]);
  }
  args.extend(["/fo", "csv", "/nh"]);
  let output = run_for("tasklist.exe", &args, limit).await?;
  if output.code != Some(0) {
    return Err(failed("tasklist", &output));
  }
  Ok(parse_pids(&output.stdout))
}

/// This program's Windows session number (the fourth `tasklist` field of
/// its own row).
/// It never changes, so it's asked once.
async fn own_session() -> io::Result<String> {
  static SESSION: OnceLock<String> = OnceLock::new();
  if let Some(session) = SESSION.get() {
    return Ok(session.clone());
  }
  let session = ask_own_session().await?;
  Ok(SESSION.get_or_init(|| session).clone())
}

async fn ask_own_session() -> io::Result<String> {
  let pid = std::process::id().to_string();
  let filter = format!("PID eq {pid}");
  // On the every-second path until it has worked once: the short limit.
  let output = run_for(
    "tasklist.exe",
    &["/fi", &filter, "/fo", "csv", "/nh"],
    QUERY_TIMEOUT,
  )
  .await?;
  if output.code != Some(0) {
    return Err(failed("tasklist", &output));
  }
  rows(&output.stdout)
    .into_iter()
    .find(|fields| fields.get(1) == Some(&pid))
    .and_then(|fields| fields.get(3).cloned())
    .filter(|session| !session.is_empty() && session.bytes().all(|b| b.is_ascii_digit()))
    .ok_or_else(|| io::Error::other("tasklist didn't show this program's own session"))
}

/// PIDs of `image` (e.g. `Media Center 36.exe`) in this session that
/// Windows reports as "Not responding" (as Task Manager shows them).
pub async fn not_responding(image: &str) -> io::Result<Vec<u32>> {
  let session = own_session().await?;
  tasklist_for(
    &[
      format!("IMAGENAME eq {image}"),
      format!("SESSION eq {session}"),
      "STATUS eq NOT RESPONDING".to_owned(),
    ],
    QUERY_TIMEOUT,
  )
  .await
}

/// Is a process of `image` (e.g. `JellyfinMediaPlayer.exe`) running in this
/// program's session?
pub async fn running(image: &str) -> io::Result<bool> {
  let session = own_session().await?;
  let pids = tasklist_for(
    &[
      format!("IMAGENAME eq {image}"),
      format!("SESSION eq {session}"),
    ],
    QUERY_TIMEOUT,
  )
  .await?;
  Ok(!pids.is_empty())
}

/// Close `pids` by force and wait until they're gone.
pub async fn kill(pids: &[u32]) -> io::Result<()> {
  for pid in pids {
    info!("closing PID {pid} by force");
    let output = run("taskkill.exe", &["/f", "/pid", &pid.to_string()]).await?;
    // Already gone counts as closed; the wait below checks anyway.
    if !matches!(output.code, Some(0 | TASKKILL_NOT_FOUND)) {
      return Err(failed("taskkill", &output));
    }
  }
  let deadline = Instant::now() + EXIT_WAIT;
  for pid in pids {
    while !tasklist(&[format!("PID eq {pid}")]).await?.is_empty() {
      if Instant::now() >= deadline {
        return Err(io::Error::new(
          io::ErrorKind::TimedOut,
          format!("PID {pid} was still running {EXIT_WAIT:?} after taskkill"),
        ));
      }
      tokio::time::sleep(Duration::from_millis(250)).await;
    }
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn pids_come_from_csv_rows_only() {
    let output = "\"Media Center 36.exe\",\"11724\",\"Console\",\"1\",\"412,332 K\"\r\n\
                  \"Media Center 36.exe\",\"204\",\"Console\",\"1\",\"1,000 K\"\r\n";
    assert_eq!(parse_pids(output), vec![11724, 204]);
    assert!(
      parse_pids("INFO: No tasks are running which match the specified criteria.\r\n").is_empty()
    );
    assert!(parse_pids("").is_empty());
    assert_eq!(rows(output)[0][3], "1");
  }

  #[test]
  fn tools_come_from_system32() {
    let path = system_tool("tasklist.exe");
    assert!(path.ends_with(r"System32\tasklist.exe"));
    assert!(path.is_absolute());
  }

  #[cfg(windows)]
  #[tokio::test]
  async fn this_program_has_a_session_and_a_missing_image_is_never_hung() {
    let session = own_session().await.unwrap();
    assert!(session.bytes().all(|b| b.is_ascii_digit()));
    let image = "no-such-program-for-the-bridge-test.exe";
    assert!(not_responding(image).await.unwrap().is_empty());
    kill(&[]).await.unwrap();
  }
}
