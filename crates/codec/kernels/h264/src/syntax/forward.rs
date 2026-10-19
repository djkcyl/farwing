// SPDX-License-Identifier: MIT
// Derived from oxideav-h264 0.1.8, Copyright (c) 2026 Karpelès Lab Inc.
// Retained syntax/metadata primitives used by this client; see COPYING.OxideAV in this crate.

pub fn forward_core_4x4(x: &[i32; 16]) -> [i32; 16] {
    // Row pass: H = Cf * X (each row of H is Cf * column of X).
    // Equivalently: for each column j, H[i, j] is the i-th forward
    // butterfly applied to (X[0,j], X[1,j], X[2,j], X[3,j]).
    let mut h = [0i32; 16];
    for j in 0..4 {
        let x0 = x[j];
        let x1 = x[4 + j];
        let x2 = x[8 + j];
        let x3 = x[12 + j];
        // H[0, j] = x0 + x1 + x2 + x3
        // H[1, j] = 2*x0 + x1 - x2 - 2*x3
        // H[2, j] = x0 - x1 - x2 + x3
        // H[3, j] = x0 - 2*x1 + 2*x2 - x3
        h[j] = x0 + x1 + x2 + x3;
        h[4 + j] = 2 * x0 + x1 - x2 - 2 * x3;
        h[8 + j] = x0 - x1 - x2 + x3;
        h[12 + j] = x0 - 2 * x1 + 2 * x2 - x3;
    }
    // Column pass: W = H * Cf^T. Same butterfly applied to each row
    // of H: for row i we compute the Cf-matrix products.
    let mut w = [0i32; 16];
    for i in 0..4 {
        let base = i * 4;
        let h0 = h[base];
        let h1 = h[base + 1];
        let h2 = h[base + 2];
        let h3 = h[base + 3];
        // W[i, 0] = h0 + h1 + h2 + h3
        // W[i, 1] = 2*h0 + h1 - h2 - 2*h3
        // W[i, 2] = h0 - h1 - h2 + h3
        // W[i, 3] = h0 - 2*h1 + 2*h2 - h3
        w[base] = h0 + h1 + h2 + h3;
        w[base + 1] = 2 * h0 + h1 - h2 - 2 * h3;
        w[base + 2] = h0 - h1 - h2 + h3;
        w[base + 3] = h0 - 2 * h1 + 2 * h2 - h3;
    }
    w
}

pub fn forward_hadamard_4x4(dc: &[i32; 16]) -> [i32; 16] {
    // H = [[1, 1, 1, 1],
    //      [1, 1,-1,-1],
    //      [1,-1,-1, 1],
    //      [1,-1, 1,-1]].
    let mut t = [0i32; 16];
    for j in 0..4 {
        t[j] = dc[j] + dc[4 + j] + dc[8 + j] + dc[12 + j];
        t[4 + j] = dc[j] + dc[4 + j] - dc[8 + j] - dc[12 + j];
        t[8 + j] = dc[j] - dc[4 + j] - dc[8 + j] + dc[12 + j];
        t[12 + j] = dc[j] - dc[4 + j] + dc[8 + j] - dc[12 + j];
    }
    let mut f = [0i32; 16];
    for i in 0..4 {
        let base = i * 4;
        let t0 = t[base];
        let t1 = t[base + 1];
        let t2 = t[base + 2];
        let t3 = t[base + 3];
        f[base] = t0 + t1 + t2 + t3;
        f[base + 1] = t0 + t1 - t2 - t3;
        f[base + 2] = t0 - t1 - t2 + t3;
        f[base + 3] = t0 - t1 + t2 - t3;
    }
    f
}
