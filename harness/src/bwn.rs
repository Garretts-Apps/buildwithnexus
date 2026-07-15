// Thin binary shim for the `bwn` alias — its own file (not src/main.rs)
// because cargo warns when one file backs multiple build targets.
fn main() {
    buildwithnexus::run();
}
