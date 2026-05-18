//! Best-effort mapping from MLX shader function names to canonical op kinds.
//!
//! Returned strings are intended for display in breakdown UIs. The table is
//! MLX-version-sensitive and intended to evolve as MLX adds or renames
//! shaders.

/// Pattern table for `resolve_kind`. Earlier entries take priority; a
/// later entry must not have an earlier entry as a strict prefix
/// (verified by `table_ordering_respects_specific_before_generic`).
const TABLE: &[(&str, &str)] = &[
    ("quantized_matmul_", "QuantizedMatmul"),
    ("gemm_", "Matmul"),
    ("gemv_", "Matmul"),
    ("sdpa_", "ScaledDotProductAttention"),
    ("softmax_", "Softmax"),
    ("rms_norm_", "RMSNorm"),
    ("layer_norm_", "LayerNorm"),
    ("gelu_", "GeLU"),
    ("silu_", "SiLU"),
    ("rope_", "RoPE"),
    ("conv2d_", "Conv2d"),
    ("conv_", "Conv"),
    ("reduce_", "Reduce"),
    ("scan_", "Scan"),
    ("sort_", "Sort"),
    ("copy_", "Copy"),
    ("quantize_", "Quantize"),
    ("dequantize_", "Dequantize"),
];

/// Map an MLX shader function name to a canonical op kind.
/// Returns `None` if no pattern matches.
pub fn resolve_kind(symbol: &str) -> Option<&'static str> {
    TABLE
        .iter()
        .find_map(|(prefix, kind)| symbol.starts_with(prefix).then_some(*kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gemm_maps_to_matmul() {
        assert_eq!(resolve_kind("gemm_t_n_bf16_64_64_32_2_2_8"), Some("Matmul"));
        assert_eq!(resolve_kind("gemv_t_bf16_64x64"), Some("Matmul"));
    }

    #[test]
    fn quantized_matmul_is_distinct() {
        assert_eq!(
            resolve_kind("quantized_matmul_w4a16_64x64"),
            Some("QuantizedMatmul")
        );
    }

    #[test]
    fn sdpa_maps_to_attention() {
        assert_eq!(
            resolve_kind("sdpa_vector_2pass_1_float16_64"),
            Some("ScaledDotProductAttention")
        );
    }

    #[test]
    fn norms_map_correctly() {
        assert_eq!(resolve_kind("rms_norm_float16"), Some("RMSNorm"));
        assert_eq!(resolve_kind("layer_norm_float16"), Some("LayerNorm"));
    }

    #[test]
    fn activations_map_correctly() {
        assert_eq!(resolve_kind("gelu_float16"), Some("GeLU"));
        assert_eq!(resolve_kind("silu_float16"), Some("SiLU"));
        assert_eq!(resolve_kind("softmax_float16_2"), Some("Softmax"));
    }

    #[test]
    fn unknown_returns_none() {
        assert_eq!(resolve_kind("K_f900_64x64x1"), None);
        assert_eq!(resolve_kind(""), None);
        assert_eq!(resolve_kind("totally_unknown_kernel"), None);
    }

    #[test]
    fn table_ordering_respects_specific_before_generic() {
        // Invariant: for any pair of entries (i, j) with i < j, entry j's prefix
        // must NOT start with entry i's prefix. If it did, entry j's symbol would
        // always be intercepted by entry i, making entry j dead code.
        //
        // Conversely, if entry i's prefix STARTS WITH entry j's prefix (j is
        // more generic), then i must come first — which is exactly the order
        // we enforce.
        //
        // The rule we actually need: for any pair where one prefix is a strict
        // prefix of the other, the more-specific (longer) one must come first.
        for (i, &(pi, _)) in TABLE.iter().enumerate() {
            for (j, &(pj, _)) in TABLE.iter().enumerate().skip(i + 1) {
                if pj.starts_with(pi) && pj != pi {
                    panic!(
                        "TABLE ordering invariant violated: entry {j} (\"{pj}\") \
                         starts with entry {i} (\"{pi}\"), so it would never be \
                         reached. The more-specific prefix \"{pj}\" must come \
                         before the generic \"{pi}\"."
                    );
                }
            }
        }
    }

    #[test]
    fn quantized_matmul_would_not_be_intercepted_by_a_matmul_generic_rule() {
        // Synthetic check: even if someone added ("matmul_", "Matmul") as the
        // FIRST entry (which is wrong), this test would still pass because
        // "quantized_matmul_w4a16" does not start with "matmul_". This test
        // documents that the existing entries don't collide for this input.
        // The real ordering guard is `table_ordering_respects_specific_before_generic`.
        assert!(!"quantized_matmul_w4a16_64x64".starts_with("matmul_"));
        assert!("quantized_matmul_w4a16_64x64".starts_with("quantized_matmul_"));
    }
}
