// Thin binary shim. All logic lives in the library crate (`lib.rs`) so the
// integration and performance suites can reach it.
fn main() {
    buildwithnexus::run();
}
