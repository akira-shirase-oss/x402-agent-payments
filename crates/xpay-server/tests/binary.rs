//! The real binary: it serves and drains on SIGTERM (exit 0), refuses the owner role
//! (exit 3), a schema newer or older than its own (exit 4) and a bad configuration with
//! every problem listed (exit 2), and its startup logs carry no secret.
#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "integration test helpers"
)]

use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::{Child, Command};
use xpay_db::logcheck::assert_no_leaks;
use xpay_db::testing::{Role, TestDb};

struct Running {
    child: Child,
    addr: String,
    lines: tokio::sync::mpsc::UnboundedReceiver<String>,
    seen: Vec<String>,
}

fn command(database_url: &str, extra: &[(&str, &str)]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_xpay-server"));
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("XPAY_DATABASE_URL", database_url)
        .env("XPAY_LISTEN", "127.0.0.1:0")
        .env("XPAY_LOG", "debug")
        .env("XPAY_STARTUP_TIMEOUT_MS", "5000")
        .envs(extra.iter().copied())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd
}

/// Runs to exit; the code and everything printed.
async fn run_to_exit(database_url: &str, extra: &[(&str, &str)]) -> (i32, String) {
    let out = tokio::time::timeout(
        Duration::from_secs(60),
        command(database_url, extra).output(),
    )
    .await
    .expect("the server did not exit")
    .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code().unwrap_or(-1), text)
}

async fn start(database_url: &str) -> Running {
    let mut child = command(database_url, &[]).spawn().unwrap();
    let (tx, mut lines) = tokio::sync::mpsc::unbounded_channel();
    for stream in [
        Box::new(child.stdout.take().unwrap()) as Box<dyn tokio::io::AsyncRead + Unpin + Send>,
        Box::new(child.stderr.take().unwrap()),
    ] {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stream).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let _ = tx.send(line);
            }
        });
    }
    let mut seen = Vec::new();
    let addr = tokio::time::timeout(Duration::from_secs(60), async {
        while let Some(line) = lines.recv().await {
            seen.push(line.clone());
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line)
                && v["fields"]["message"] == "listening"
            {
                return v["fields"]["listen"].as_str().unwrap().to_owned();
            }
        }
        panic!("the server exited before listening: {seen:?}");
    })
    .await
    .expect("the server never listened");
    Running {
        child,
        addr,
        lines,
        seen,
    }
}

async fn get(addr: &str, path: &str) -> (u16, String) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut out = String::new();
    tokio::time::timeout(Duration::from_secs(10), s.read_to_string(&mut out))
        .await
        .unwrap()
        .unwrap();
    let status = out.get(9..12).and_then(|c| c.parse().ok()).unwrap_or(0);
    (status, out)
}

#[tokio::test]
async fn the_server_serves_and_drains_on_sigterm_with_clean_logs() {
    let db = TestDb::new().await;
    let mut running = start(&db.url()).await;
    assert_eq!(get(&running.addr, "/livez").await.0, 200);
    let (ready, body) = get(&running.addr, "/readyz").await;
    assert_eq!(ready, 200, "{body}");
    assert_eq!(get(&running.addr, "/errorz").await.0, 200);
    assert_eq!(get(&running.addr, "/v1/me").await.0, 401);
    let pid = running.child.id().unwrap().to_string();
    assert!(
        std::process::Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .unwrap()
            .success()
    );
    let status = tokio::time::timeout(Duration::from_secs(30), running.child.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.code(), Some(0), "a clean drain exits 0");
    while let Ok(line) = running.lines.try_recv() {
        running.seen.push(line);
    }
    let log = running.seen.join("\n");
    assert_no_leaks(&log, &[&db.url()], "the server's startup and shutdown");
    assert!(log.contains("listening"), "{log}");
    db.cleanup().await;
}

#[tokio::test]
async fn the_server_refuses_the_owner_role_and_any_schema_but_its_own() {
    let db = TestDb::new().await;
    let (code, out) = run_to_exit(&db.url_as(Role::Owner), &[]).await;
    assert_eq!(code, 3, "{out}");
    assert!(out.contains("refusing to run"), "{out}");

    db.exec_as(
        Role::Owner,
        "INSERT INTO _sqlx_migrations (version, description, installed_on, success, checksum, execution_time)
         VALUES (99999, 'from a newer release', now(), true, '\\x00', 0)",
    )
    .await;
    let (code, out) = run_to_exit(&db.url(), &[]).await;
    assert_eq!(code, 4, "{out}");
    assert!(out.contains("newer"), "{out}");
    // A newer release's table makes the manifest fail too; the schema is the cause reported.
    db.exec_as(Role::Owner, "CREATE TABLE from_a_newer_release (id int); GRANT SELECT ON from_a_newer_release TO xpay_app").await;
    let (code, out) = run_to_exit(&db.url(), &[]).await;
    assert_eq!(code, 4, "{out}");
    db.cleanup().await;

    // Never migrated: behind, the cause before every missing table.
    let bare = TestDb::unmigrated().await;
    let (code, out) = run_to_exit(&bare.url(), &[]).await;
    assert_eq!(code, 4, "{out}");
    assert!(out.contains("behind"), "{out}");

    // A current schema whose protections were weakened: exit 3, naming the problem.
    let weak = TestDb::new().await;
    weak.exec_as(Role::Owner, "GRANT UPDATE ON account_events TO xpay_app")
        .await;
    let (code, out) = run_to_exit(&weak.url(), &[]).await;
    assert_eq!(code, 3, "{out}");
    assert!(out.contains("account_events"), "{out}");
    weak.cleanup().await;
    bare.cleanup().await;
}

#[tokio::test]
async fn a_bad_configuration_is_refused_with_every_problem() {
    let (code, out) = run_to_exit(
        "",
        &[("XPAY_MAX_CONNECTIONS", "0"), ("XPAY_LOG_FORMAT", "yaml")],
    )
    .await;
    assert_eq!(code, 2, "{out}");
    for expected in [
        "database URL is required",
        "XPAY_MAX_CONNECTIONS",
        "XPAY_LOG_FORMAT",
    ] {
        assert!(out.contains(expected), "{expected} missing from {out}");
    }
}
