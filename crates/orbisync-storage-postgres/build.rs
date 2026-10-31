//! Invalidates embedded migrations when the migration directory changes.

fn main() {
    // Track added migrations as well as edits to already embedded SQL files.
    println!("cargo:rerun-if-changed=../../migrations");
}
