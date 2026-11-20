//! The offline loop from the Rust binary, as a user runs it.

#[test]
fn the_loop_binary_pays_and_proves_every_failure_case() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_xpay-x402-loop"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    for case in [
        "402 -> pay -> 200",
        "replayed payment",
        "expired authorization",
        "wrong amount",
        "wrong recipient",
        "wrong network",
        "malformed header",
        "spend cap and allowlist",
        "facilitator timeout during settle",
        "x402 offline loop ok",
    ] {
        assert!(stdout.contains(case), "missing {case}:\n{stdout}");
    }
}

#[test]
fn the_mock_facilitator_binary_serves_supported() {
    use std::io::{BufRead as _, BufReader, Read as _, Write as _};
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_xpay-mock-facilitator"))
        .args(["--listen", "127.0.0.1:0"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let addr = line
        .trim()
        .strip_prefix("listening on ")
        .unwrap()
        .to_owned();
    let mut s = std::net::TcpStream::connect(&addr).unwrap();
    s.set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    write!(
        s,
        "GET /supported HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    assert!(
        resp.contains("\"eip155:84532\"") && resp.contains("\"exact\""),
        "{resp}"
    );
}
