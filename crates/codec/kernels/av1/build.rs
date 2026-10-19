//! Build the AV1 software decoder's SIMD assembly.
#[cfg(feature = "asm")]
#[path = "build/decoder.rs"]
mod decoder;

fn main() {
    #[cfg(feature = "asm")]
    {
        // Includes/macros are build inputs too. Cargo owns incremental reuse;
        // a second partial hash would miss edits to an included assembly file.
        println!("cargo:rerun-if-changed=src/decoder");
        decoder::build();
    }
}
