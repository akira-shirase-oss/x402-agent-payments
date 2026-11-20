//! The log scan's controls: a log that holds a needle fails it, a needle the scan could
//! never see fails it, and a clean log passes. Without these a clean result proves nothing.

use xpay_db::logcheck::{assert_no_leaks, leaks};

#[test]
fn the_scan_catches_a_planted_leak() {
    let log = "{\"message\":\"signed in\",\"session\":\"5e551011deadbeef\"}\n";
    let caught =
        std::panic::catch_unwind(|| assert_no_leaks(log, &["5e551011deadbeef"], "control"));
    assert!(
        caught.is_err(),
        "the scan passed a log that holds a session id"
    );
}

#[test]
#[should_panic(expected = "cannot catch")]
fn a_needle_the_scan_cannot_see_fails_the_check() {
    // A needle spanning two lines can never appear inside one log line.
    assert_no_leaks("{\"message\":\"listening\"}\n", &["two\nlines"], "control");
}

#[test]
fn credential_urls_and_dev_passwords_are_leaks_and_a_clean_log_passes() {
    assert_eq!(
        leaks("url=postgres://app:hunter2@127.0.0.1:5432/db", &[]).len(),
        1
    );
    assert_eq!(
        leaks("url=postgres://127.0.0.1:5432/db and http://host:80/x", &[]),
        Vec::<String>::new()
    );
    assert_eq!(leaks("pw xpay-app-dev-password", &[]).len(), 1);
    assert_no_leaks(
        "{\"message\":\"listening\"}\n",
        &["s3cret-token"],
        "a clean log",
    );
}
