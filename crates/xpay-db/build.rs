// sqlx::migrate! embeds the migrations at compile time, and the manifest is include_str!'d:
// rebuild when either changes.
fn main() {
    println!("cargo:rerun-if-changed=../../migrations");
    println!("cargo:rerun-if-changed=sql");
}
