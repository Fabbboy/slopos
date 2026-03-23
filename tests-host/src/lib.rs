// Host-side regression tests for the SlopOS graphics subsystem.
//
// These test the pure-logic components (pixel encode/decode, alpha blending,
// AA primitives, font parsing/rasterization) on the host using `cargo test`.

#[cfg(test)]
mod pixel_tests;

#[cfg(test)]
mod blend_tests;

#[cfg(test)]
mod canvas_ops_tests;

#[cfg(test)]
mod font_tests;
